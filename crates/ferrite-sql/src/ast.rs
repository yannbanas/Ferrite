use ferrite_common::{ColumnDef, ColumnDefault, DataType, FerriteError, Schema, Value};

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
    AlterTable(AlterTable),
    DropTable(DropTable),
    CreateIndex(CreateIndex),
    DropIndex(DropIndex),
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
    ///
    /// Fails when a `DEFAULT` clause is outside the set Ferrite v1 stores
    /// — see [`ColumnSpec::to_column_def`]. The constants it does keep are
    /// still untyped at this point: a quoted string written into a
    /// `TIMESTAMP` column is `Value::Text` until the caller coerces it
    /// (`ferrite_planner::typecheck_defaults`).
    pub fn to_schema(&self) -> Result<Schema, FerriteError> {
        let pk: Vec<&str> = self
            .constraints
            .iter()
            .flat_map(|c| match c {
                TableConstraint::PrimaryKey(cols) => cols.iter().map(String::as_str).collect(),
                TableConstraint::Unique(_) => Vec::new(),
            })
            .collect();
        Ok(Schema {
            columns: self
                .columns
                .iter()
                .map(|col| col.to_column_def(pk.contains(&col.name.as_str())))
                .collect::<Result<_, _>>()?,
        })
    }
}

/// `ALTER TABLE [IF EXISTS] name <action>`. Ferrite v1 has one action —
/// see [`AlterTableAction`].
#[derive(Debug, Clone, PartialEq)]
pub struct AlterTable {
    pub if_exists: bool,
    pub name: ObjectName,
    pub action: AlterTableAction,
}

/// The `ALTER TABLE` actions Ferrite v1 covers.
///
/// `ADD COLUMN` only. `DROP COLUMN`, `RENAME`, `ALTER COLUMN TYPE`,
/// `SET`/`DROP DEFAULT`, `SET`/`DROP NOT NULL` and constraint actions are
/// **not** covered and are a parse error naming the action, never a
/// silently accepted no-op. `ADD COLUMN` is what an application's
/// migration mechanism actually runs; the rest can follow once the storage
/// layer has a story for rewriting existing rows.
#[derive(Debug, Clone, PartialEq)]
pub enum AlterTableAction {
    AddColumn {
        if_not_exists: bool,
        column: ColumnSpec,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnSpec {
    pub name: String,
    pub data_type: DataType,
    pub constraints: Vec<ColumnConstraint>,
}

impl ColumnSpec {
    /// Project one parsed column onto a [`ferrite_common::ColumnDef`].
    /// `primary_key` says whether a table-level `PRIMARY KEY` names it,
    /// which makes it non-nullable just as the column-level spelling does.
    pub fn to_column_def(&self, primary_key: bool) -> Result<ColumnDef, FerriteError> {
        let nullable = !primary_key
            && !self
                .constraints
                .iter()
                .any(|c| matches!(c, ColumnConstraint::NotNull | ColumnConstraint::PrimaryKey));
        let default = self
            .constraints
            .iter()
            .find_map(|c| match c {
                ColumnConstraint::Default(expr) => Some(expr),
                _ => None,
            })
            .map(|expr| column_default(&self.name, expr))
            .transpose()?;
        Ok(ColumnDef {
            name: self.name.clone(),
            data_type: self.data_type,
            nullable,
            default,
        })
    }
}

/// Project a parsed `DEFAULT` expression onto the small set Ferrite
/// stores.
///
/// Accepted: any literal (including a signed number), and the current
/// timestamp written either as a bare `CURRENT_TIMESTAMP` or as a
/// zero-argument `now()` / `current_timestamp()`. Everything else —
/// arithmetic, a call to any other function, a reference to another column
/// — is refused here rather than dropped, because a `DEFAULT` that is
/// quietly not applied puts a `NULL` where the application expects a
/// value, and on a nullable column nothing ever reports it.
fn column_default(column: &str, expr: &Expr) -> Result<ColumnDefault, FerriteError> {
    let refused = || {
        FerriteError::InvalidDefinition(format!(
            "the DEFAULT of column `{column}` is not a literal or CURRENT_TIMESTAMP"
        ))
    };
    match expr {
        Expr::Literal(literal) => Ok(ColumnDefault::Constant(literal_value(literal))),
        Expr::UnaryOp { op, expr } => match (op, expr.as_ref()) {
            (UnaryOp::Plus, Expr::Literal(literal)) => {
                Ok(ColumnDefault::Constant(literal_value(literal)))
            }
            (UnaryOp::Minus, Expr::Literal(Literal::Int(n))) => Ok(ColumnDefault::Constant(
                literal_value(&Literal::Int(n.checked_neg().ok_or_else(refused)?)),
            )),
            (UnaryOp::Minus, Expr::Literal(Literal::Float(f))) => {
                Ok(ColumnDefault::Constant(Value::Float8(-f)))
            }
            _ => Err(refused()),
        },
        Expr::Function(call) if is_current_timestamp(&call.name) => match &call.args {
            FunctionArgs::List(args) if args.is_empty() => Ok(ColumnDefault::CurrentTimestamp),
            _ => Err(refused()),
        },
        // `CURRENT_TIMESTAMP` has no parentheses in the SQL standard, so
        // the expression parser sees a bare name and reads it as a column
        // reference; there is no column in scope in a `DEFAULT`.
        Expr::Column(name) if name.qualifier().is_none() && is_current_timestamp(name.base()) => {
            Ok(ColumnDefault::CurrentTimestamp)
        }
        _ => Err(refused()),
    }
}

fn is_current_timestamp(name: &str) -> bool {
    matches!(name, "now" | "current_timestamp")
}

/// Integer literals take the narrowest type that fits, matching what the
/// planner does with a literal in a `VALUES` list.
fn literal_value(literal: &Literal) -> Value {
    match literal {
        Literal::Null => Value::Null,
        Literal::Boolean(b) => Value::Boolean(*b),
        Literal::Int(n) => match i32::try_from(*n) {
            Ok(small) => Value::Int4(small),
            Err(_) => Value::Int8(*n),
        },
        Literal::Float(f) => Value::Float8(*f),
        Literal::String(s) => Value::Text(s.clone()),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ColumnConstraint {
    NotNull,
    Null,
    PrimaryKey,
    Unique,
    Default(Expr),
    /// `COLLATE name` written on the column itself. Recorded so the DDL
    /// parses, but Ferrite stores no per-column collation: it does not
    /// change the column's type and it does not make a `UNIQUE` on that
    /// column case-insensitive. See `docs/pawchat-sql-audit.md`.
    Collate(String),
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

/// `CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON table (cols)`. There is
/// no access-method clause (`USING gin`, …): `docs/architecture.md` keeps
/// B-tree only in v1.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndex {
    pub if_not_exists: bool,
    pub unique: bool,
    pub name: String,
    pub table: ObjectName,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropIndex {
    pub if_exists: bool,
    pub name: String,
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
        /// `ILIKE` rather than `LIKE`.
        case_insensitive: bool,
    },
    /// `expr COLLATE name`. Only the collation SQLite calls `NOCASE` has
    /// a meaning in Ferrite; the planner rejects any other name rather
    /// than dropping it, since a dropped collation silently changes which
    /// rows a query returns.
    Collate {
        expr: Box<Expr>,
        collation: String,
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
