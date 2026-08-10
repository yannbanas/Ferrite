use ferrite_sql::parse;
use proptest::prelude::*;

fn ident() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["a", "b", "c", "id", "users", "t1"]).prop_map(str::to_string)
}

fn literal() -> impl Strategy<Value = String> {
    prop_oneof![
        any::<i32>().prop_map(|n| n.to_string()),
        Just("3.5".to_string()),
        Just("'text'".to_string()),
        Just("true".to_string()),
        Just("null".to_string()),
        (1u32..8).prop_map(|n| format!("${n}")),
    ]
}

fn expr() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![ident(), literal()];
    leaf.prop_recursive(4, 32, 3, |inner| {
        prop_oneof![
            (
                inner.clone(),
                inner.clone(),
                prop::sample::select(vec![
                    "+", "-", "*", "/", "%", "=", "<>", "<", ">", "<=", ">=", "AND", "OR", "||"
                ])
            )
                .prop_map(|(l, r, op)| format!("({l} {op} {r})")),
            inner.clone().prop_map(|e| format!("NOT ({e})")),
            inner.clone().prop_map(|e| format!("-({e})")),
            inner.clone().prop_map(|e| format!("({e}) IS NOT NULL")),
            (inner.clone(), inner.clone())
                .prop_map(|(a, b)| format!("({a}) BETWEEN ({b}) AND ({b})")),
            inner.clone().prop_map(|e| format!("({e}) IN (1, 2, 3)")),
            inner
                .clone()
                .prop_map(|e| format!("CASE WHEN ({e}) THEN 1 ELSE 2 END")),
            inner.prop_map(|e| format!("count({e})")),
        ]
    })
}

fn statement() -> impl Strategy<Value = String> {
    prop_oneof![
        (ident(), expr()).prop_map(|(t, e)| format!("SELECT * FROM {t} WHERE {e}")),
        (ident(), ident(), expr())
            .prop_map(|(t, c, e)| format!("SELECT {c}, count(*) FROM {t} GROUP BY {c} HAVING {e}")),
        (ident(), ident(), expr()).prop_map(|(a, b, e)| format!(
            "SELECT * FROM {a} LEFT JOIN {b} ON {e} ORDER BY 1 DESC LIMIT 10"
        )),
        (ident(), expr(), expr())
            .prop_map(|(t, a, b)| format!("INSERT INTO {t} (a, b) VALUES ({a}, {b}) RETURNING *")),
        (ident(), ident(), expr(), expr())
            .prop_map(|(t, c, v, w)| format!("UPDATE {t} SET {c} = {v} WHERE {w}")),
        (ident(), expr()).prop_map(|(t, e)| format!("DELETE FROM {t} WHERE {e}")),
        (ident(), expr()).prop_map(|(t, e)| format!(
            "CREATE PROCEDURE p(x BIGINT) AS BEGIN \
             IF {e} THEN RAISE 'no'; ELSE DELETE FROM {t}; END IF; RETURN; END"
        )),
        ident().prop_map(|t| format!(
            "CREATE TABLE {t} (id UUID PRIMARY KEY, v TEXT NOT NULL, n INT DEFAULT 0)"
        )),
        ident().prop_map(|t| format!("DROP TABLE IF EXISTS {t} CASCADE")),
        Just("BEGIN".to_string()),
        Just("COMMIT".to_string()),
        Just("ROLLBACK".to_string()),
    ]
}

proptest! {
    /// Generated queries inside the supported subset must always parse.
    #[test]
    fn generated_statements_parse(stmts in prop::collection::vec(statement(), 1..4)) {
        let sql = stmts.join("; ");
        prop_assert!(parse(&sql).is_ok(), "failed to parse: {sql}");
    }

    /// Arbitrary text must produce a `Result`, never a panic. The harness
    /// fails the test if the parser unwinds.
    #[test]
    fn arbitrary_text_never_panics(sql in ".{0,400}") {
        let _ = parse(&sql);
    }

    /// Soup made from real tokens explores far deeper into the grammar
    /// than random characters do.
    #[test]
    fn token_soup_never_panics(
        pieces in prop::collection::vec(
            prop::sample::select(vec![
                "SELECT", "FROM", "WHERE", "JOIN", "ON", "GROUP", "BY", "HAVING", "ORDER",
                "LIMIT", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE",
                "TABLE", "PROCEDURE", "TRIGGER", "BEGIN", "END", "IF", "THEN", "RAISE",
                "CASE", "WHEN", "CAST", "AS", "NOT", "NULL", "IN", "BETWEEN", "LIKE",
                "EXISTS", "UNION", "WITH", "(", ")", ",", ";", "*", "=", "$1", "'x'", "1",
                "a", "\"q\"", "--", "/*", "*/", "1e", ".",
            ]),
            0..60,
        ),
    ) {
        let _ = parse(&pieces.join(" "));
        let _ = parse(&pieces.concat());
    }
}
