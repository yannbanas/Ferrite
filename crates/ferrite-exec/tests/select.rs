//! End-to-end for the query half of SQL: joins, grouping, ordering, pattern
//! matching and de-duplication, from SQL text down to the rows that come
//! back.
//!
//! The planner's own tests check the *shape* of the plan. These check the
//! rows, which is where a pushdown that is subtly wrong about null semantics
//! shows up — the plan looks reasonable either way.

mod support;

use ferrite_common::{
    Catalog, DataType, Identity, Permission, Role, Row, Schema, StorageEngine, Value,
};
use ferrite_exec::{QueryResult, Session};
use ferrite_planner::Planner;
use ferrite_proc::ProcRegistry;

use support::{column, MemCatalog, MemStorage};

const OWNER: Identity = Identity([1u8; 32]);

/// One person with no post and no age, so both the outer-join and the
/// null-skipping aggregate paths have something to be wrong about.
const THREE_PEOPLE: &str =
    "INSERT INTO users VALUES (1, 'ada', 36), (2, 'grace', 45), (3, 'linus', NULL)";

/// Three posts by two of the three people: one author has several, one has
/// none.
const THREE_POSTS: &str =
    "INSERT INTO posts VALUES (10, 1, 'sketch'), (11, 1, 'notes'), (12, 2, 'compiler')";

fn users_schema() -> Schema {
    Schema {
        columns: vec![
            column("id", DataType::Int8, false),
            column("name", DataType::Text, true),
            column("age", DataType::Int4, true),
        ],
    }
}

fn posts_schema() -> Schema {
    Schema {
        columns: vec![
            column("id", DataType::Int8, false),
            column("author", DataType::Int8, false),
            column("title", DataType::Text, true),
        ],
    }
}

fn full_access() -> ProcRegistry {
    let mut registry = ProcRegistry::new();
    registry.grant_role(
        OWNER,
        Role {
            name: "owner".into(),
            permissions: vec![
                Permission::Select,
                Permission::Insert,
                Permission::Update,
                Permission::Delete,
                Permission::Execute,
            ],
        },
    );
    registry
}

struct Fixture {
    storage: MemStorage,
    catalog: MemCatalog,
    procs: ProcRegistry,
}

impl Fixture {
    /// Both tables created and empty.
    fn empty() -> Self {
        let storage = MemStorage::new();
        let catalog = MemCatalog::new();
        for (name, schema) in [("users", users_schema()), ("posts", posts_schema())] {
            let table = catalog.create_table("public", name, schema).unwrap();
            storage.create_table(0, table).unwrap();
        }
        Self {
            storage,
            catalog,
            procs: full_access(),
        }
    }

    fn loaded() -> Self {
        let fixture = Self::empty();
        fixture.run(THREE_PEOPLE);
        fixture.run(THREE_POSTS);
        fixture
    }

    fn run(&self, sql: &str) -> QueryResult {
        let statement = ferrite_sql::parse_statement(sql).expect("parse");
        let plan = Planner::new(&self.catalog, &self.catalog)
            .plan(&statement)
            .unwrap_or_else(|err| panic!("plan {sql:?}: {err}"));
        Session::new(&self.storage, &self.catalog, &self.procs, OWNER)
            .execute(1, &plan)
            .unwrap_or_else(|err| panic!("execute {sql:?}: {err}"))
    }

    fn rows(&self, sql: &str) -> Vec<Row> {
        match self.run(sql) {
            QueryResult::Rows { rows, .. } => rows,
            other => panic!("expected rows from {sql:?}, got {other:?}"),
        }
    }

    /// One column of the result, which is what most of these assertions are
    /// about.
    fn column(&self, sql: &str, position: usize) -> Vec<Value> {
        self.rows(sql)
            .iter()
            .map(|row| row.values[position].clone())
            .collect()
    }
}

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

#[test]
fn a_left_join_pads_a_row_with_no_match_with_nulls() {
    let fixture = Fixture::loaded();
    let rows = fixture.rows(
        "SELECT users.name, posts.title FROM users \
         LEFT JOIN posts ON posts.author = users.id \
         ORDER BY users.name, posts.title",
    );

    let names: Vec<Value> = rows.iter().map(|r| r.values[0].clone()).collect();
    let titles: Vec<Value> = rows.iter().map(|r| r.values[1].clone()).collect();
    assert_eq!(
        names,
        vec![text("ada"), text("ada"), text("grace"), text("linus")]
    );
    assert_eq!(
        titles,
        vec![text("notes"), text("sketch"), text("compiler"), Value::Null],
        "linus has no post, so his row is null-extended rather than dropped"
    );
}

#[test]
fn an_inner_join_drops_the_row_with_no_match() {
    let fixture = Fixture::loaded();
    assert_eq!(
        fixture.column(
            "SELECT users.name FROM users JOIN posts ON posts.author = users.id \
             ORDER BY users.name, posts.title",
            0
        ),
        vec![text("ada"), text("ada"), text("grace")]
    );
}

/// The case pushdown gets silently wrong: sinking this predicate into the
/// `posts` scan would leave every unmatched user standing, null-extended, so
/// the answer would be three rows instead of one.
#[test]
fn a_where_on_the_null_extended_side_of_a_left_join_removes_the_unmatched_rows() {
    let fixture = Fixture::loaded();
    assert_eq!(
        fixture.column(
            "SELECT users.name FROM users LEFT JOIN posts ON posts.author = users.id \
             WHERE posts.title = 'compiler'",
            0
        ),
        vec![text("grace")]
    );
}

/// The mirror image: the same restriction written in `ON` keeps every left
/// row and only stops it from matching.
#[test]
fn an_on_restriction_on_the_null_extended_side_keeps_every_left_row() {
    let fixture = Fixture::loaded();
    let rows = fixture.rows(
        "SELECT users.name, posts.title FROM users \
         LEFT JOIN posts ON posts.author = users.id AND posts.title = 'compiler' \
         ORDER BY users.name",
    );

    let names: Vec<Value> = rows.iter().map(|r| r.values[0].clone()).collect();
    let titles: Vec<Value> = rows.iter().map(|r| r.values[1].clone()).collect();
    assert_eq!(names, vec![text("ada"), text("grace"), text("linus")]);
    assert_eq!(titles, vec![Value::Null, text("compiler"), Value::Null]);
}

#[test]
fn a_predicate_on_the_preserved_side_of_a_left_join_still_filters_it() {
    let fixture = Fixture::loaded();
    assert_eq!(
        fixture.column(
            "SELECT users.name FROM users LEFT JOIN posts ON posts.author = users.id \
             WHERE users.id = 3",
            0
        ),
        vec![text("linus")]
    );
}

#[test]
fn a_comma_separated_from_list_joins_on_the_where_clause() {
    let fixture = Fixture::loaded();
    assert_eq!(
        fixture.column(
            "SELECT users.name FROM users, posts WHERE posts.author = users.id \
             ORDER BY users.name, posts.title",
            0
        ),
        vec![text("ada"), text("ada"), text("grace")]
    );
}

#[test]
fn group_by_produces_one_row_per_group_with_its_own_aggregates() {
    let fixture = Fixture::loaded();
    let rows = fixture.rows(
        "SELECT author, count(*), min(id), max(id) FROM posts GROUP BY author ORDER BY author",
    );

    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].values,
        vec![
            Value::Int8(1),
            Value::Int8(2),
            Value::Int8(10),
            Value::Int8(11)
        ]
    );
    assert_eq!(
        rows[1].values,
        vec![
            Value::Int8(2),
            Value::Int8(1),
            Value::Int8(12),
            Value::Int8(12)
        ]
    );
}

#[test]
fn grouping_over_a_join_counts_within_each_group() {
    let fixture = Fixture::loaded();
    let rows = fixture.rows(
        "SELECT users.name, count(posts.id) FROM users \
         LEFT JOIN posts ON posts.author = users.id \
         GROUP BY users.name ORDER BY users.name",
    );

    assert_eq!(
        rows.iter().map(|r| r.values.clone()).collect::<Vec<_>>(),
        vec![
            vec![text("ada"), Value::Int8(2)],
            vec![text("grace"), Value::Int8(1)],
            // The null-extended row contributes no post to count.
            vec![text("linus"), Value::Int8(0)],
        ]
    );
}

#[test]
fn having_drops_whole_groups_after_they_are_computed() {
    let fixture = Fixture::loaded();
    assert_eq!(
        fixture.column(
            "SELECT author FROM posts GROUP BY author HAVING count(*) > 1",
            0
        ),
        vec![Value::Int8(1)]
    );
}

#[test]
fn count_distinguishes_rows_from_non_null_values_from_distinct_values() {
    let fixture = Fixture::loaded();
    let rows =
        fixture.rows("SELECT count(*), count(age), count(DISTINCT author) FROM users, posts");

    assert_eq!(
        rows[0].values,
        vec![Value::Int8(9), Value::Int8(6), Value::Int8(2)],
        "three users crossed with three posts, and linus has no age"
    );
}

#[test]
fn sum_avg_min_and_max_ignore_nulls() {
    let fixture = Fixture::loaded();
    let rows = fixture.rows("SELECT sum(age), avg(age), min(age), max(age) FROM users");

    assert_eq!(
        rows[0].values,
        vec![
            Value::Int8(81),
            Value::Float8(40.5),
            Value::Int4(36),
            Value::Int4(45)
        ]
    );
}

/// `count(*)` over nothing is one row holding zero; `GROUP BY` over nothing
/// is no rows at all. Easy to conflate in a single code path.
#[test]
fn an_aggregate_over_an_empty_table_produces_a_row_but_a_group_by_does_not() {
    let fixture = Fixture::empty();

    let rows = fixture.rows("SELECT count(*) FROM users");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values, vec![Value::Int8(0)]);

    assert!(fixture
        .rows("SELECT id, count(*) FROM users GROUP BY id")
        .is_empty());
}

/// Every aggregate but `count` is null over an empty input, rather than zero.
#[test]
fn sum_over_an_empty_table_is_null_not_zero() {
    let fixture = Fixture::empty();
    assert_eq!(
        fixture.rows("SELECT sum(age), max(age) FROM users")[0].values,
        vec![Value::Null, Value::Null]
    );
}

#[test]
fn order_by_really_orders_and_puts_nulls_where_asked() {
    let fixture = Fixture::loaded();
    let names = |sql: &str| fixture.column(sql, 0);

    assert_eq!(
        names("SELECT name FROM users ORDER BY age"),
        vec![text("ada"), text("grace"), text("linus")],
        "ascending sorts nulls last"
    );
    assert_eq!(
        names("SELECT name FROM users ORDER BY age DESC"),
        vec![text("linus"), text("grace"), text("ada")],
        "descending sorts nulls first"
    );
    assert_eq!(
        names("SELECT name FROM users ORDER BY age ASC NULLS FIRST"),
        vec![text("linus"), text("ada"), text("grace")]
    );
    assert_eq!(
        names("SELECT name FROM users ORDER BY age DESC NULLS LAST"),
        vec![text("grace"), text("ada"), text("linus")]
    );
}

#[test]
fn order_by_can_sort_on_a_column_the_select_list_does_not_carry() {
    let fixture = Fixture::loaded();
    assert_eq!(
        fixture.column("SELECT name FROM users ORDER BY id DESC", 0),
        vec![text("linus"), text("grace"), text("ada")]
    );
}

#[test]
fn order_by_accepts_a_select_list_alias_and_a_position() {
    let fixture = Fixture::loaded();
    assert_eq!(
        fixture.column("SELECT name AS who FROM users ORDER BY who DESC", 0),
        vec![text("linus"), text("grace"), text("ada")]
    );
    assert_eq!(
        fixture.column("SELECT name FROM users ORDER BY 1 DESC", 0),
        vec![text("linus"), text("grace"), text("ada")]
    );
}

#[test]
fn like_matches_wildcards_and_not_like_inverts_it() {
    let fixture = Fixture::loaded();
    let names = |sql: &str| fixture.column(sql, 0);

    assert_eq!(
        names("SELECT name FROM users WHERE name LIKE 'a%'"),
        vec![text("ada")]
    );
    assert_eq!(
        names("SELECT name FROM users WHERE name LIKE '_da'"),
        vec![text("ada")]
    );
    assert_eq!(
        names("SELECT name FROM users WHERE name LIKE '%a%' ORDER BY name"),
        vec![text("ada"), text("grace")]
    );
    assert_eq!(
        names("SELECT name FROM users WHERE name NOT LIKE '%a%'"),
        vec![text("linus")]
    );
    assert_eq!(
        names("SELECT title FROM posts WHERE title LIKE '%o%e%' ORDER BY title"),
        vec![text("compiler"), text("notes")]
    );
}

#[test]
fn distinct_collapses_repeated_rows() {
    let fixture = Fixture::loaded();
    assert_eq!(
        fixture.column("SELECT DISTINCT author FROM posts ORDER BY author", 0),
        vec![Value::Int8(1), Value::Int8(2)]
    );
}

#[test]
fn offset_skips_rows_after_the_sort_not_before_it() {
    let fixture = Fixture::loaded();
    assert_eq!(
        fixture.column(
            "SELECT name FROM users ORDER BY id DESC LIMIT 1 OFFSET 1",
            0
        ),
        vec![text("grace")]
    );
}

#[test]
fn arithmetic_and_concatenation_work_in_a_projection() {
    let fixture = Fixture::loaded();
    let rows = fixture.rows("SELECT name || '!', age * 2 FROM users ORDER BY id LIMIT 1");
    assert_eq!(rows[0].values, vec![text("ada!"), Value::Int4(72)]);
}
