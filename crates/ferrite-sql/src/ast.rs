use ferrite_common::{ColumnDef, DataType, Schema};

/// A dotted name such as `users` or `public.users`. Parts are stored in
/// source order and are already case-folded by the lexer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectName(pub Vec<String>);

impl ObjectName {
    /// The last part, i.e. the object's own name.
    pub fn base(&self) -> &str {
        self.0.last().map(String::as_str).unwrap_or_default()
    }

    /// The qualifier, if the name was written as `qualifier.base`.
    pub fn qualifier(&self) -> Option<&str> {
        if self.0.len() >= 2 {
            self.0.get(self.0.len() - 2).map(String::as_str)
        } else {
            None
        }
    }

    /// Split into `(schema, name)`, falling back to `default_schema` when
    /// the name was unqualified.
    pub fn split<'a>(&'a self, default_schema: &'a str) -> (&'a str, &'a str) {
        (self.qualifier().unwrap_or(default_schema), self.base())
    }
}

/// Top-level statement. One SQL string may contain several, separated by
/// `;` (see [`crate::parse`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable(CreateTable),
    DropTable(DropTable),
    Query(Box<Query>),
    Insert(Insert),
    Update(Update),
    Delete(Delete),
    Begin,
    Commit,
    Rollback,
    CreateProcedure(CreateProcedure),
    DropProcedure(DropProcedure),
    Call(Call),
    CreateTrigger(CreateTrigger),
    DropTrigger(DropTrigger),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    pub if_not_exists: bool,
    pub name: ObjectName,
    pub columns: Vec<ColumnSpec>,
    pub constraints: Vec<TableConstraint>,
}

impl CreateTable {
    /// Project the parsed column list onto the shared
    /// [`ferrite_common::Schema`] the catalog and storage layers speak.
    ///
    /// A column is nullable unless it carries `NOT NULL` or takes part in
    /// a primary key; everything else in [`ColumnConstraint`] is metadata
    /// the planner handles, not part of the stored schema.
    pub fn to_schema(&self) -> Schema {
        let pk: Vec<&str> = self
            .constraints
            .iter()
            .flat_map(|c| match c {
                TableConstraint::PrimaryKey(cols) => cols.iter().map(String::as_str).collect(),
                TableConstraint::Unique(_) => Vec::new(),
            })
            .collect();
        Schema {
            columns: self
                .columns
                .iter()
                .map(|col| ColumnDef {
                    name: col.name.clone(),
                    data_type: col.data_type,
                    nullable: !col.constraints.iter().any(|c| {
                        matches!(c, ColumnConstraint::NotNull | ColumnConstraint::PrimaryKey)
                    }) && !pk.contains(&col.name.as_str()),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnSpec {
    pub name: String,
    pub data_type: DataType,
    pub constraints: Vec<ColumnConstraint>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ColumnConstraint {
    NotNull,
    Null,
    PrimaryKey,
    Unique,
    Default(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableConstraint {
    PrimaryKey(Vec<String>),
    Unique(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropTable {
    pub if_exists: bool,
    pub names: Vec<ObjectName>,
    pub cascade: bool,
}

/// A `SELECT`, including any leading `WITH` clause and trailing
/// `ORDER BY`/`LIMIT`/`OFFSET`, which bind to the whole set expression.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub with: Vec<Cte>,
    pub body: SetExpr,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cte {
    pub name: String,
    pub columns: Vec<String>,
    pub query: Box<Query>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetExpr {
    Select(Box<Select>),
    Query(Box<Query>),
    SetOp {
        op: SetOp,
        all: bool,
        left: Box<SetExpr>,
        right: Box<SetExpr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    Union,
    Intersect,
    Except,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub distinct: bool,
    pub projection: Vec<SelectItem>,
    pub from: Vec<TableWithJoins>,
    pub selection: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Wildcard,
    QualifiedWildcard(ObjectName),
    Expr { expr: Expr, alias: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableWithJoins {
    pub relation: TableFactor,
    pub joins: Vec<Join>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableFactor {
    Table {
        name: ObjectName,
        alias: Option<String>,
    },
    Derived {
        subquery: Box<Query>,
        alias: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub join_type: JoinType,
    pub relation: TableFactor,
    pub constraint: JoinConstraint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JoinConstraint {
    On(Expr),
    Using(Vec<String>),
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderByItem {
    pub expr: Expr,
    pub asc: bool,
    pub nulls_first: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    pub table: ObjectName,
    pub columns: Vec<String>,
    pub source: InsertSource,
    pub returning: Vec<SelectItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Query(Box<Query>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub table: ObjectName,
    pub alias: Option<String>,
    pub assignments: Vec<Assignment>,
    pub selection: Option<Expr>,
    pub returning: Vec<SelectItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: String,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    pub table: ObjectName,
    pub alias: Option<String>,
    pub selection: Option<Expr>,
    pub returning: Vec<SelectItem>,
}

/// A stored procedure. The body is a small imperative block rather than a
/// general-purpose PL: procedures exist mainly as the anchor of Ferrite's
/// identity-based security model (see `docs/architecture.md`), so they
/// need just enough control flow to inspect the caller and refuse.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateProcedure {
    pub or_replace: bool,
    pub name: ObjectName,
    pub params: Vec<ProcParam>,
    pub body: Vec<ProcStatement>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcParam {
    pub name: String,
    pub data_type: DataType,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProcStatement {
    Sql(Box<Statement>),
    Return(Option<Expr>),
    Raise(Expr),
    If {
        branches: Vec<(Expr, Vec<ProcStatement>)>,
        else_branch: Option<Vec<ProcStatement>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropProcedure {
    pub if_exists: bool,
    pub name: ObjectName,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub name: ObjectName,
    pub args: Vec<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTrigger {
    pub name: String,
    pub timing: TriggerTiming,
    pub events: Vec<TriggerEvent>,
    pub table: ObjectName,
    pub for_each_row: bool,
    pub condition: Option<Expr>,
    pub procedure: ObjectName,
    pub args: Vec<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerTiming {
    Before,
    After,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerEvent {
    Insert,
    Update,
    Delete,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropTrigger {
    pub if_exists: bool,
    pub name: String,
    pub table: ObjectName,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Literal),
    Column(ObjectName),
    Parameter(u32),
    UnaryOp {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    BinaryOp {
        left: Box<Expr>,
        op: BinaryOp,
        right: Box<Expr>,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        negated: bool,
        low: Box<Expr>,
        high: Box<Expr>,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    InSubquery {
        expr: Box<Expr>,
        subquery: Box<Query>,
        negated: bool,
    },
    Exists {
        subquery: Box<Query>,
        negated: bool,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
    },
    Case {
        operand: Option<Box<Expr>>,
        branches: Vec<(Expr, Expr)>,
        else_result: Option<Box<Expr>>,
    },
    Cast {
        expr: Box<Expr>,
        data_type: DataType,
    },
    Function(FunctionCall),
    Subquery(Box<Query>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Null,
    Boolean(bool),
    Int(i64),
    Float(f64),
    String(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Plus,
    Minus,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    And,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Plus,
    Minus,
    Multiply,
    Divide,
    Modulo,
    Concat,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    pub args: FunctionArgs,
    pub distinct: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FunctionArgs {
    /// The `*` of `count(*)`.
    Wildcard,
    List(Vec<Expr>),
}

/// Whether `name` is one of the five aggregate functions Ferrite v1
/// recognises. The parser treats every call the same way; this is the
/// hook the planner uses to split aggregates out of a projection.
pub fn is_aggregate(name: &str) -> bool {
    matches!(name, "count" | "sum" | "avg" | "min" | "max")
}
