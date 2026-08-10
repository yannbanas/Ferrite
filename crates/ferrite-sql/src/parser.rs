use ferrite_common::DataType;

use crate::ast::*;
use crate::lexer::{Keyword, Lexer, SpannedToken, Token};
use crate::ParseError;

/// Hard cap on grammar recursion. Deeply nested input is the one way a
/// recursive-descent parser can take down the process without ever
/// calling `panic!`, so nesting is bounded rather than trusted.
const MAX_DEPTH: u32 = 100;

/// Parse a SQL string into zero or more statements.
///
/// Statements are separated by `;`; a trailing `;` is optional and an
/// input that is empty or only comments yields an empty vector.
pub fn parse(sql: &str) -> Result<Vec<Statement>, ParseError> {
    let tokens = Lexer::new(sql).tokenize()?;
    let mut parser = Parser {
        tokens,
        pos: 0,
        depth: 0,
    };
    parser.parse_statements()
}

/// Parse a SQL string that must contain exactly one statement.
pub fn parse_statement(sql: &str) -> Result<Statement, ParseError> {
    let mut statements = parse(sql)?;
    if statements.len() == 1 {
        Ok(statements.remove(0))
    } else {
        Err(ParseError {
            message: format!("expected exactly one statement, found {}", statements.len()),
            offset: 0,
        })
    }
}

struct Parser {
    tokens: Vec<SpannedToken>,
    pos: usize,
    depth: u32,
}

impl Parser {
    fn peek(&self) -> &Token {
        match self.tokens.get(self.pos) {
            Some(t) => &t.token,
            None => &Token::Eof,
        }
    }

    fn peek_at(&self, ahead: usize) -> &Token {
        match self.tokens.get(self.pos + ahead) {
            Some(t) => &t.token,
            None => &Token::Eof,
        }
    }

    fn offset(&self) -> usize {
        match self.tokens.get(self.pos) {
            Some(t) => t.offset,
            None => self.tokens.last().map(|t| t.offset).unwrap_or(0),
        }
    }

    fn advance(&mut self) {
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
    }

    fn err<T>(&self, message: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError {
            message: message.into(),
            offset: self.offset(),
        })
    }

    fn expected<T>(&self, what: &str) -> Result<T, ParseError> {
        self.err(format!("expected {what}, found {}", self.peek().describe()))
    }

    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.err("query nesting is too deep");
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == token {
            self.advance();
            true
        } else {
            false
        }
    }

    fn eat_keyword(&mut self, kw: Keyword) -> bool {
        if self.peek() == &Token::Keyword(kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn eat_keywords(&mut self, kws: &[Keyword]) -> bool {
        for (i, kw) in kws.iter().enumerate() {
            if self.peek_at(i) != &Token::Keyword(*kw) {
                return false;
            }
        }
        self.pos += kws.len();
        true
    }

    fn expect(&mut self, token: &Token) -> Result<(), ParseError> {
        if self.eat(token) {
            Ok(())
        } else {
            self.expected(&token.describe())
        }
    }

    fn expect_keyword(&mut self, kw: Keyword) -> Result<(), ParseError> {
        if self.eat_keyword(kw) {
            Ok(())
        } else {
            self.expected(&format!("keyword `{}`", kw.as_str()))
        }
    }

    fn parse_identifier(&mut self) -> Result<String, ParseError> {
        match self.peek().clone() {
            Token::Ident(name) => {
                self.advance();
                Ok(name)
            }
            Token::Keyword(kw) if !kw.is_reserved() => {
                self.advance();
                Ok(kw.as_str().to_string())
            }
            _ => self.expected("an identifier"),
        }
    }

    /// An identifier that may also be an alias; returns `None` when the
    /// next token clearly starts another clause.
    fn parse_optional_alias(&mut self) -> Result<Option<String>, ParseError> {
        if self.eat_keyword(Keyword::As) {
            return Ok(Some(self.parse_identifier()?));
        }
        match self.peek() {
            Token::Ident(_) => Ok(Some(self.parse_identifier()?)),
            Token::Keyword(kw) if !kw.is_reserved() => Ok(Some(self.parse_identifier()?)),
            _ => Ok(None),
        }
    }

    fn is_identifier_at(&self, ahead: usize) -> bool {
        match self.peek_at(ahead) {
            Token::Ident(_) => true,
            Token::Keyword(kw) => !kw.is_reserved(),
            _ => false,
        }
    }

    fn parse_object_name(&mut self) -> Result<ObjectName, ParseError> {
        let mut parts = vec![self.parse_identifier()?];
        while self.peek() == &Token::Dot && self.is_identifier_at(1) {
            self.advance();
            parts.push(self.parse_identifier()?);
        }
        if parts.len() > 2 {
            return self.err("names qualified by more than one level are not supported");
        }
        Ok(ObjectName(parts))
    }

    fn parse_statements(&mut self) -> Result<Vec<Statement>, ParseError> {
        let mut out = Vec::new();
        loop {
            while self.eat(&Token::Semi) {}
            if self.peek() == &Token::Eof {
                return Ok(out);
            }
            out.push(self.parse_statement()?);
            if self.peek() == &Token::Eof {
                return Ok(out);
            }
            if !self.eat(&Token::Semi) {
                return self.expected("`;` between statements");
            }
        }
    }

    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        match self.peek() {
            Token::Keyword(Keyword::Create) => self.parse_create(),
            Token::Keyword(Keyword::Alter) => self.parse_alter(),
            Token::Keyword(Keyword::Drop) => self.parse_drop(),
            Token::Keyword(Keyword::Select) | Token::Keyword(Keyword::With) | Token::LParen => {
                Ok(Statement::Query(Box::new(self.parse_query()?)))
            }
            Token::Keyword(Keyword::Insert) => self.parse_insert(),
            Token::Keyword(Keyword::Update) => self.parse_update(),
            Token::Keyword(Keyword::Delete) => self.parse_delete(),
            Token::Keyword(Keyword::Call) => self.parse_call(),
            Token::Keyword(Keyword::Begin) => {
                self.advance();
                let _ = self.eat_keyword(Keyword::Transaction) || self.eat_keyword(Keyword::Work);
                Ok(Statement::Begin)
            }
            Token::Keyword(Keyword::Start) => {
                self.advance();
                self.expect_keyword(Keyword::Transaction)?;
                Ok(Statement::Begin)
            }
            Token::Keyword(Keyword::Commit) | Token::Keyword(Keyword::End) => {
                self.advance();
                let _ = self.eat_keyword(Keyword::Transaction) || self.eat_keyword(Keyword::Work);
                Ok(Statement::Commit)
            }
            Token::Keyword(Keyword::Rollback) => {
                self.advance();
                let _ = self.eat_keyword(Keyword::Transaction) || self.eat_keyword(Keyword::Work);
                Ok(Statement::Rollback)
            }
            _ => self.expected("the start of a statement"),
        }
    }

    fn parse_create(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::Create)?;
        let or_replace = self.eat_keywords(&[Keyword::Or, Keyword::Replace]);
        match self.peek() {
            Token::Keyword(Keyword::Table) if !or_replace => self.parse_create_table(),
            Token::Keyword(Keyword::Procedure) | Token::Keyword(Keyword::Function) => {
                self.parse_create_procedure(or_replace)
            }
            Token::Keyword(Keyword::Trigger) if !or_replace => self.parse_create_trigger(),
            Token::Keyword(Keyword::Index) | Token::Keyword(Keyword::Unique) if !or_replace => {
                self.parse_create_index()
            }
            _ => self.expected("`TABLE`, `INDEX`, `PROCEDURE` or `TRIGGER`"),
        }
    }

    fn parse_create_index(&mut self) -> Result<Statement, ParseError> {
        let unique = self.eat_keyword(Keyword::Unique);
        self.expect_keyword(Keyword::Index)?;
        let if_not_exists = self.eat_keywords(&[Keyword::If, Keyword::Not, Keyword::Exists]);
        let name = self.parse_identifier()?;
        self.expect_keyword(Keyword::On)?;
        let table = self.parse_object_name()?;
        let columns = self.parse_column_list()?;
        Ok(Statement::CreateIndex(CreateIndex {
            if_not_exists,
            unique,
            name,
            table,
            columns,
        }))
    }

    fn parse_create_table(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::Table)?;
        let if_not_exists = self.eat_keywords(&[Keyword::If, Keyword::Not, Keyword::Exists]);
        let name = self.parse_object_name()?;
        self.expect(&Token::LParen)?;
        let mut columns = Vec::new();
        let mut constraints = Vec::new();
        loop {
            if self.eat_keywords(&[Keyword::Primary, Keyword::Key]) {
                constraints.push(TableConstraint::PrimaryKey(self.parse_column_list()?));
            } else if self.eat_keyword(Keyword::Unique) {
                constraints.push(TableConstraint::Unique(self.parse_column_list()?));
            } else {
                columns.push(self.parse_column_spec()?);
            }
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        self.expect(&Token::RParen)?;
        if columns.is_empty() {
            return self.err("a table needs at least one column");
        }
        Ok(Statement::CreateTable(CreateTable {
            if_not_exists,
            name,
            columns,
            constraints,
        }))
    }

    /// `ALTER TABLE [IF EXISTS] name ADD [COLUMN] [IF NOT EXISTS] col type …`
    ///
    /// Only `ADD COLUMN` is covered. Every other action is named in the
    /// error rather than skipped, so a migration that Ferrite cannot run
    /// stops instead of appearing to have run.
    fn parse_alter(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::Alter)?;
        self.expect_keyword(Keyword::Table)?;
        let if_exists = self.eat_keywords(&[Keyword::If, Keyword::Exists]);
        let name = self.parse_object_name()?;

        if !self.eat_keyword(Keyword::Add) {
            return self.err(
                "the only ALTER TABLE action Ferrite v1 supports is ADD COLUMN \
                 (no DROP COLUMN, RENAME, ALTER COLUMN or constraint actions)",
            );
        }
        let _ = self.eat_keyword(Keyword::Column);
        let if_not_exists = self.eat_keywords(&[Keyword::If, Keyword::Not, Keyword::Exists]);
        let column = self.parse_column_spec()?;

        Ok(Statement::AlterTable(AlterTable {
            if_exists,
            name,
            action: AlterTableAction::AddColumn {
                if_not_exists,
                column,
            },
        }))
    }

    fn parse_column_list(&mut self) -> Result<Vec<String>, ParseError> {
        self.expect(&Token::LParen)?;
        let mut names = vec![self.parse_identifier()?];
        while self.eat(&Token::Comma) {
            names.push(self.parse_identifier()?);
        }
        self.expect(&Token::RParen)?;
        Ok(names)
    }

    fn parse_column_spec(&mut self) -> Result<ColumnSpec, ParseError> {
        let name = self.parse_identifier()?;
        let data_type = self.parse_data_type()?;
        let mut constraints = Vec::new();
        loop {
            if self.eat_keywords(&[Keyword::Not, Keyword::Null]) {
                constraints.push(ColumnConstraint::NotNull);
            } else if self.eat_keyword(Keyword::Null) {
                constraints.push(ColumnConstraint::Null);
            } else if self.eat_keywords(&[Keyword::Primary, Keyword::Key]) {
                constraints.push(ColumnConstraint::PrimaryKey);
            } else if self.eat_keyword(Keyword::Unique) {
                constraints.push(ColumnConstraint::Unique);
            } else if self.eat_keyword(Keyword::Default) {
                constraints.push(ColumnConstraint::Default(self.parse_expr()?));
            } else {
                break;
            }
        }
        Ok(ColumnSpec {
            name,
            data_type,
            constraints,
        })
    }

    fn parse_data_type(&mut self) -> Result<DataType, ParseError> {
        let kw = match self.peek() {
            Token::Keyword(kw) => *kw,
            _ => return self.expected("a type name"),
        };
        let ty = match kw {
            Keyword::Bool | Keyword::Boolean => DataType::Boolean,
            Keyword::Int | Keyword::Int4 | Keyword::Integer => DataType::Int4,
            Keyword::Bigint | Keyword::Int8 => DataType::Int8,
            Keyword::Float8 => DataType::Float8,
            Keyword::Double => {
                self.advance();
                self.expect_keyword(Keyword::Precision)?;
                return Ok(DataType::Float8);
            }
            Keyword::Text => DataType::Text,
            Keyword::Varchar => {
                self.advance();
                if self.eat(&Token::LParen) {
                    if !matches!(self.peek(), Token::Number(_)) {
                        return self.expected("a length");
                    }
                    self.advance();
                    self.expect(&Token::RParen)?;
                }
                return Ok(DataType::Text);
            }
            Keyword::Timestamp | Keyword::Timestamptz => DataType::Timestamp,
            Keyword::Uuid => DataType::Uuid,
            Keyword::Json | Keyword::Jsonb => DataType::Json,
            _ => return self.expected("a type name"),
        };
        self.advance();
        Ok(ty)
    }

    fn parse_drop(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::Drop)?;
        match self.peek() {
            Token::Keyword(Keyword::Table) => {
                self.advance();
                let if_exists = self.eat_keywords(&[Keyword::If, Keyword::Exists]);
                let mut names = vec![self.parse_object_name()?];
                while self.eat(&Token::Comma) {
                    names.push(self.parse_object_name()?);
                }
                let cascade = if self.eat_keyword(Keyword::Cascade) {
                    true
                } else {
                    let _ = self.eat_keyword(Keyword::Restrict);
                    false
                };
                Ok(Statement::DropTable(DropTable {
                    if_exists,
                    names,
                    cascade,
                }))
            }
            Token::Keyword(Keyword::Procedure) | Token::Keyword(Keyword::Function) => {
                self.advance();
                let if_exists = self.eat_keywords(&[Keyword::If, Keyword::Exists]);
                let name = self.parse_object_name()?;
                Ok(Statement::DropProcedure(DropProcedure { if_exists, name }))
            }
            Token::Keyword(Keyword::Trigger) => {
                self.advance();
                let if_exists = self.eat_keywords(&[Keyword::If, Keyword::Exists]);
                let name = self.parse_identifier()?;
                self.expect_keyword(Keyword::On)?;
                let table = self.parse_object_name()?;
                Ok(Statement::DropTrigger(DropTrigger {
                    if_exists,
                    name,
                    table,
                }))
            }
            Token::Keyword(Keyword::Index) => {
                self.advance();
                let if_exists = self.eat_keywords(&[Keyword::If, Keyword::Exists]);
                let name = self.parse_identifier()?;
                Ok(Statement::DropIndex(DropIndex { if_exists, name }))
            }
            _ => self.expected("`TABLE`, `INDEX`, `PROCEDURE` or `TRIGGER`"),
        }
    }

    fn parse_query(&mut self) -> Result<Query, ParseError> {
        self.enter()?;
        let with = if self.eat_keyword(Keyword::With) {
            let mut ctes = Vec::new();
            loop {
                let name = self.parse_identifier()?;
                let columns = if self.peek() == &Token::LParen {
                    self.parse_column_list()?
                } else {
                    Vec::new()
                };
                self.expect_keyword(Keyword::As)?;
                self.expect(&Token::LParen)?;
                let query = Box::new(self.parse_query()?);
                self.expect(&Token::RParen)?;
                ctes.push(Cte {
                    name,
                    columns,
                    query,
                });
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            ctes
        } else {
            Vec::new()
        };

        let body = self.parse_set_expr()?;

        let mut order_by = Vec::new();
        if self.eat_keywords(&[Keyword::Order, Keyword::By]) {
            loop {
                let expr = self.parse_expr()?;
                let asc = if self.eat_keyword(Keyword::Desc) {
                    false
                } else {
                    let _ = self.eat_keyword(Keyword::Asc);
                    true
                };
                let nulls_first = if self.eat_keyword(Keyword::Nulls) {
                    if self.eat_keyword(Keyword::First) {
                        Some(true)
                    } else if self.eat_keyword(Keyword::Last) {
                        Some(false)
                    } else {
                        return self.expected("`FIRST` or `LAST`");
                    }
                } else {
                    None
                };
                order_by.push(OrderByItem {
                    expr,
                    asc,
                    nulls_first,
                });
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
        }

        let limit = if self.eat_keyword(Keyword::Limit) {
            if self.eat_keyword(Keyword::All) {
                None
            } else {
                Some(self.parse_expr()?)
            }
        } else {
            None
        };
        let offset = if self.eat_keyword(Keyword::Offset) {
            Some(self.parse_expr()?)
        } else {
            None
        };

        self.leave();
        Ok(Query {
            with,
            body,
            order_by,
            limit,
            offset,
        })
    }

    fn parse_set_expr(&mut self) -> Result<SetExpr, ParseError> {
        self.enter()?;
        let mut left = self.parse_set_term()?;
        loop {
            let op = match self.peek() {
                Token::Keyword(Keyword::Union) => SetOp::Union,
                Token::Keyword(Keyword::Intersect) => SetOp::Intersect,
                Token::Keyword(Keyword::Except) => SetOp::Except,
                _ => break,
            };
            self.advance();
            let all = self.eat_keyword(Keyword::All);
            let right = self.parse_set_term()?;
            left = SetExpr::SetOp {
                op,
                all,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
        self.leave();
        Ok(left)
    }

    fn parse_set_term(&mut self) -> Result<SetExpr, ParseError> {
        if self.peek() == &Token::LParen {
            self.advance();
            let query = self.parse_query()?;
            self.expect(&Token::RParen)?;
            return Ok(SetExpr::Query(Box::new(query)));
        }
        Ok(SetExpr::Select(Box::new(self.parse_select()?)))
    }

    fn parse_select(&mut self) -> Result<Select, ParseError> {
        self.expect_keyword(Keyword::Select)?;
        let distinct = self.eat_keyword(Keyword::Distinct);
        let mut projection = vec![self.parse_select_item()?];
        while self.eat(&Token::Comma) {
            projection.push(self.parse_select_item()?);
        }

        let mut from = Vec::new();
        if self.eat_keyword(Keyword::From) {
            loop {
                from.push(self.parse_table_with_joins()?);
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
        }

        let selection = if self.eat_keyword(Keyword::Where) {
            Some(self.parse_expr()?)
        } else {
            None
        };

        let mut group_by = Vec::new();
        if self.eat_keywords(&[Keyword::Group, Keyword::By]) {
            loop {
                group_by.push(self.parse_expr()?);
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
        }

        let having = if self.eat_keyword(Keyword::Having) {
            Some(self.parse_expr()?)
        } else {
            None
        };

        if having.is_some() && group_by.is_empty() {
            return self.err("`HAVING` requires a `GROUP BY` clause");
        }

        Ok(Select {
            distinct,
            projection,
            from,
            selection,
            group_by,
            having,
        })
    }

    fn parse_select_item(&mut self) -> Result<SelectItem, ParseError> {
        if self.eat(&Token::Star) {
            return Ok(SelectItem::Wildcard);
        }
        if matches!(self.peek(), Token::Ident(_))
            && self.peek_at(1) == &Token::Dot
            && self.peek_at(2) == &Token::Star
        {
            let qualifier = self.parse_identifier()?;
            self.advance();
            self.advance();
            return Ok(SelectItem::QualifiedWildcard(ObjectName(vec![qualifier])));
        }
        let expr = self.parse_expr()?;
        let alias = self.parse_optional_alias()?;
        Ok(SelectItem::Expr { expr, alias })
    }

    fn parse_table_with_joins(&mut self) -> Result<TableWithJoins, ParseError> {
        let relation = self.parse_table_factor()?;
        let mut joins = Vec::new();
        loop {
            let join_type = if self.eat_keyword(Keyword::Cross) {
                self.expect_keyword(Keyword::Join)?;
                JoinType::Cross
            } else if self.eat_keyword(Keyword::Inner) {
                self.expect_keyword(Keyword::Join)?;
                JoinType::Inner
            } else if self.eat_keyword(Keyword::Left) {
                let _ = self.eat_keyword(Keyword::Outer);
                self.expect_keyword(Keyword::Join)?;
                JoinType::Left
            } else if self.eat_keyword(Keyword::Right) {
                let _ = self.eat_keyword(Keyword::Outer);
                self.expect_keyword(Keyword::Join)?;
                JoinType::Right
            } else if self.eat_keyword(Keyword::Full) {
                let _ = self.eat_keyword(Keyword::Outer);
                self.expect_keyword(Keyword::Join)?;
                JoinType::Full
            } else if self.eat_keyword(Keyword::Join) {
                JoinType::Inner
            } else {
                break;
            };

            let relation = self.parse_table_factor()?;
            let constraint = if self.eat_keyword(Keyword::On) {
                JoinConstraint::On(self.parse_expr()?)
            } else if self.eat_keyword(Keyword::Using) {
                JoinConstraint::Using(self.parse_column_list()?)
            } else {
                JoinConstraint::None
            };

            if join_type == JoinType::Cross {
                if constraint != JoinConstraint::None {
                    return self.err("`CROSS JOIN` does not take a join condition");
                }
            } else if constraint == JoinConstraint::None {
                return self.err("this join requires an `ON` or `USING` clause");
            }

            joins.push(Join {
                join_type,
                relation,
                constraint,
            });
        }
        Ok(TableWithJoins { relation, joins })
    }

    fn parse_table_factor(&mut self) -> Result<TableFactor, ParseError> {
        if self.eat(&Token::LParen) {
            let subquery = Box::new(self.parse_query()?);
            self.expect(&Token::RParen)?;
            let alias = match self.parse_optional_alias()? {
                Some(alias) => alias,
                None => return self.err("a subquery in `FROM` must have an alias"),
            };
            return Ok(TableFactor::Derived { subquery, alias });
        }
        let name = self.parse_object_name()?;
        let alias = self.parse_optional_alias()?;
        Ok(TableFactor::Table { name, alias })
    }

    fn parse_insert(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::Insert)?;
        self.expect_keyword(Keyword::Into)?;
        let table = self.parse_object_name()?;
        let columns = if self.peek() == &Token::LParen && !self.starts_subquery_after_lparen() {
            self.parse_column_list()?
        } else {
            Vec::new()
        };

        let source = if self.eat_keyword(Keyword::Values) {
            let mut rows = Vec::new();
            loop {
                self.expect(&Token::LParen)?;
                let mut row = vec![self.parse_expr()?];
                while self.eat(&Token::Comma) {
                    row.push(self.parse_expr()?);
                }
                self.expect(&Token::RParen)?;
                if !columns.is_empty() && row.len() != columns.len() {
                    return self.err(format!(
                        "`VALUES` row has {} expressions but {} columns were named",
                        row.len(),
                        columns.len()
                    ));
                }
                rows.push(row);
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            InsertSource::Values(rows)
        } else {
            InsertSource::Query(Box::new(self.parse_query()?))
        };

        let returning = self.parse_returning()?;
        Ok(Statement::Insert(Insert {
            table,
            columns,
            source,
            returning,
        }))
    }

    fn starts_subquery_after_lparen(&self) -> bool {
        matches!(
            self.peek_at(1),
            Token::Keyword(Keyword::Select) | Token::Keyword(Keyword::With)
        )
    }

    fn parse_returning(&mut self) -> Result<Vec<SelectItem>, ParseError> {
        if !self.eat_keyword(Keyword::Returning) {
            return Ok(Vec::new());
        }
        let mut items = vec![self.parse_select_item()?];
        while self.eat(&Token::Comma) {
            items.push(self.parse_select_item()?);
        }
        Ok(items)
    }

    fn parse_update(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::Update)?;
        let table = self.parse_object_name()?;
        let alias = self.parse_optional_alias()?;
        self.expect_keyword(Keyword::Set)?;
        let mut assignments = Vec::new();
        loop {
            let column = self.parse_identifier()?;
            self.expect(&Token::Eq)?;
            let value = self.parse_expr()?;
            assignments.push(Assignment { column, value });
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        let selection = if self.eat_keyword(Keyword::Where) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let returning = self.parse_returning()?;
        Ok(Statement::Update(Update {
            table,
            alias,
            assignments,
            selection,
            returning,
        }))
    }

    fn parse_delete(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::Delete)?;
        self.expect_keyword(Keyword::From)?;
        let table = self.parse_object_name()?;
        let alias = self.parse_optional_alias()?;
        let selection = if self.eat_keyword(Keyword::Where) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let returning = self.parse_returning()?;
        Ok(Statement::Delete(Delete {
            table,
            alias,
            selection,
            returning,
        }))
    }

    fn parse_call(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::Call)?;
        let name = self.parse_object_name()?;
        self.expect(&Token::LParen)?;
        let mut args = Vec::new();
        if self.peek() != &Token::RParen {
            args.push(self.parse_expr()?);
            while self.eat(&Token::Comma) {
                args.push(self.parse_expr()?);
            }
        }
        self.expect(&Token::RParen)?;
        Ok(Statement::Call(Call { name, args }))
    }

    fn parse_create_procedure(&mut self, or_replace: bool) -> Result<Statement, ParseError> {
        if !self.eat_keyword(Keyword::Procedure) {
            self.expect_keyword(Keyword::Function)?;
        }
        let name = self.parse_object_name()?;
        self.expect(&Token::LParen)?;
        let mut params = Vec::new();
        if self.peek() != &Token::RParen {
            loop {
                let param_name = self.parse_identifier()?;
                let data_type = self.parse_data_type()?;
                params.push(ProcParam {
                    name: param_name,
                    data_type,
                });
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
        }
        self.expect(&Token::RParen)?;
        let _ = self.eat_keyword(Keyword::As);
        self.expect_keyword(Keyword::Begin)?;
        let body = self.parse_proc_block(&[Keyword::End])?;
        if body.is_empty() {
            return self.err("a procedure body must contain at least one statement");
        }
        self.expect_keyword(Keyword::End)?;
        Ok(Statement::CreateProcedure(CreateProcedure {
            or_replace,
            name,
            params,
            body,
        }))
    }

    /// Parse `<stmt>; <stmt>; ...` up to one of `terminators`, which is
    /// left unconsumed. Every statement in a block ends with `;`.
    fn parse_proc_block(
        &mut self,
        terminators: &[Keyword],
    ) -> Result<Vec<ProcStatement>, ParseError> {
        self.enter()?;
        let mut body = Vec::new();
        loop {
            if let Token::Keyword(kw) = self.peek() {
                if terminators.contains(kw) {
                    break;
                }
            }
            if self.peek() == &Token::Eof {
                return self.expected("`END` closing the procedure body");
            }
            body.push(self.parse_proc_statement()?);
            self.expect(&Token::Semi)?;
        }
        self.leave();
        Ok(body)
    }

    fn parse_proc_statement(&mut self) -> Result<ProcStatement, ParseError> {
        self.enter()?;
        let stmt = match self.peek() {
            Token::Keyword(Keyword::Return) => {
                self.advance();
                if self.peek() == &Token::Semi {
                    ProcStatement::Return(None)
                } else {
                    ProcStatement::Return(Some(self.parse_expr()?))
                }
            }
            Token::Keyword(Keyword::Raise) => {
                self.advance();
                ProcStatement::Raise(self.parse_expr()?)
            }
            Token::Keyword(Keyword::If) => {
                self.advance();
                let mut branches = Vec::new();
                let condition = self.parse_expr()?;
                self.expect_keyword(Keyword::Then)?;
                let block =
                    self.parse_proc_block(&[Keyword::Elsif, Keyword::Else, Keyword::End])?;
                branches.push((condition, block));
                let mut else_branch = None;
                loop {
                    if self.eat_keyword(Keyword::Elsif) {
                        let condition = self.parse_expr()?;
                        self.expect_keyword(Keyword::Then)?;
                        let block =
                            self.parse_proc_block(&[Keyword::Elsif, Keyword::Else, Keyword::End])?;
                        branches.push((condition, block));
                    } else if self.eat_keyword(Keyword::Else) {
                        else_branch = Some(self.parse_proc_block(&[Keyword::End])?);
                    } else {
                        break;
                    }
                }
                self.expect_keyword(Keyword::End)?;
                self.expect_keyword(Keyword::If)?;
                ProcStatement::If {
                    branches,
                    else_branch,
                }
            }
            _ => ProcStatement::Sql(Box::new(self.parse_statement()?)),
        };
        self.leave();
        Ok(stmt)
    }

    fn parse_create_trigger(&mut self) -> Result<Statement, ParseError> {
        self.expect_keyword(Keyword::Trigger)?;
        let name = self.parse_identifier()?;
        let timing = if self.eat_keyword(Keyword::Before) {
            TriggerTiming::Before
        } else if self.eat_keyword(Keyword::After) {
            TriggerTiming::After
        } else {
            return self.expected("`BEFORE` or `AFTER`");
        };
        let mut events = Vec::new();
        loop {
            let event = if self.eat_keyword(Keyword::Insert) {
                TriggerEvent::Insert
            } else if self.eat_keyword(Keyword::Update) {
                TriggerEvent::Update
            } else if self.eat_keyword(Keyword::Delete) {
                TriggerEvent::Delete
            } else {
                return self.expected("`INSERT`, `UPDATE` or `DELETE`");
            };
            if events.contains(&event) {
                return self.err("duplicate trigger event");
            }
            events.push(event);
            if !self.eat_keyword(Keyword::Or) {
                break;
            }
        }
        self.expect_keyword(Keyword::On)?;
        let table = self.parse_object_name()?;
        let mut for_each_row = false;
        if self.eat_keywords(&[Keyword::For, Keyword::Each]) {
            if self.eat_keyword(Keyword::Row) {
                for_each_row = true;
            } else if !self.eat_keyword(Keyword::Statement) {
                return self.expected("`ROW` or `STATEMENT`");
            }
        }
        let condition = if self.eat_keyword(Keyword::When) {
            self.expect(&Token::LParen)?;
            let expr = self.parse_expr()?;
            self.expect(&Token::RParen)?;
            Some(expr)
        } else {
            None
        };
        self.expect_keyword(Keyword::Execute)?;
        if !self.eat_keyword(Keyword::Procedure) {
            self.expect_keyword(Keyword::Function)?;
        }
        let procedure = self.parse_object_name()?;
        self.expect(&Token::LParen)?;
        let mut args = Vec::new();
        if self.peek() != &Token::RParen {
            args.push(self.parse_expr()?);
            while self.eat(&Token::Comma) {
                args.push(self.parse_expr()?);
            }
        }
        self.expect(&Token::RParen)?;
        Ok(Statement::CreateTrigger(CreateTrigger {
            name,
            timing,
            events,
            table,
            for_each_row,
            condition,
            procedure,
            args,
        }))
    }

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        self.parse_subexpr(0)
    }

    fn parse_subexpr(&mut self, min_precedence: u8) -> Result<Expr, ParseError> {
        self.enter()?;
        let mut left = self.parse_prefix()?;
        loop {
            let precedence = match self.peek_precedence() {
                Some(p) if p > min_precedence => p,
                _ => break,
            };
            left = self.parse_infix(left, precedence)?;
        }
        self.leave();
        Ok(left)
    }

    fn peek_precedence(&self) -> Option<u8> {
        let p = match self.peek() {
            Token::Keyword(Keyword::Or) => 1,
            Token::Keyword(Keyword::And) => 2,
            Token::Keyword(Keyword::Is)
            | Token::Keyword(Keyword::In)
            | Token::Keyword(Keyword::Between)
            | Token::Keyword(Keyword::Like) => 4,
            Token::Keyword(Keyword::Not) => match self.peek_at(1) {
                Token::Keyword(Keyword::In)
                | Token::Keyword(Keyword::Between)
                | Token::Keyword(Keyword::Like) => 4,
                _ => return None,
            },
            Token::Eq | Token::NotEq | Token::Lt | Token::LtEq | Token::Gt | Token::GtEq => 5,
            Token::Concat => 6,
            Token::Plus | Token::Minus => 7,
            Token::Star | Token::Slash | Token::Percent => 8,
            _ => return None,
        };
        Some(p)
    }

    fn parse_infix(&mut self, left: Expr, precedence: u8) -> Result<Expr, ParseError> {
        if self.eat_keyword(Keyword::Is) {
            let negated = self.eat_keyword(Keyword::Not);
            self.expect_keyword(Keyword::Null)?;
            return Ok(Expr::IsNull {
                expr: Box::new(left),
                negated,
            });
        }

        let negated = if self.peek() == &Token::Keyword(Keyword::Not) {
            self.advance();
            true
        } else {
            false
        };

        if self.eat_keyword(Keyword::Between) {
            let low = self.parse_subexpr(5)?;
            self.expect_keyword(Keyword::And)?;
            let high = self.parse_subexpr(5)?;
            return Ok(Expr::Between {
                expr: Box::new(left),
                negated,
                low: Box::new(low),
                high: Box::new(high),
            });
        }
        if self.eat_keyword(Keyword::Like) {
            let pattern = self.parse_subexpr(precedence)?;
            return Ok(Expr::Like {
                expr: Box::new(left),
                pattern: Box::new(pattern),
                negated,
            });
        }
        if self.eat_keyword(Keyword::In) {
            self.expect(&Token::LParen)?;
            if matches!(
                self.peek(),
                Token::Keyword(Keyword::Select) | Token::Keyword(Keyword::With)
            ) {
                let subquery = Box::new(self.parse_query()?);
                self.expect(&Token::RParen)?;
                return Ok(Expr::InSubquery {
                    expr: Box::new(left),
                    subquery,
                    negated,
                });
            }
            let mut list = Vec::new();
            if self.peek() != &Token::RParen {
                list.push(self.parse_expr()?);
                while self.eat(&Token::Comma) {
                    list.push(self.parse_expr()?);
                }
            }
            self.expect(&Token::RParen)?;
            return Ok(Expr::InList {
                expr: Box::new(left),
                list,
                negated,
            });
        }
        if negated {
            return self.expected("`IN`, `BETWEEN` or `LIKE` after `NOT`");
        }

        let op = match self.peek() {
            Token::Keyword(Keyword::Or) => BinaryOp::Or,
            Token::Keyword(Keyword::And) => BinaryOp::And,
            Token::Eq => BinaryOp::Eq,
            Token::NotEq => BinaryOp::NotEq,
            Token::Lt => BinaryOp::Lt,
            Token::LtEq => BinaryOp::LtEq,
            Token::Gt => BinaryOp::Gt,
            Token::GtEq => BinaryOp::GtEq,
            Token::Concat => BinaryOp::Concat,
            Token::Plus => BinaryOp::Plus,
            Token::Minus => BinaryOp::Minus,
            Token::Star => BinaryOp::Multiply,
            Token::Slash => BinaryOp::Divide,
            Token::Percent => BinaryOp::Modulo,
            _ => return self.expected("an operator"),
        };
        self.advance();
        let right = self.parse_subexpr(precedence)?;
        Ok(Expr::BinaryOp {
            left: Box::new(left),
            op,
            right: Box::new(right),
        })
    }

    fn parse_prefix(&mut self) -> Result<Expr, ParseError> {
        self.enter()?;
        let expr = self.parse_prefix_inner()?;
        self.leave();
        Ok(expr)
    }

    fn parse_prefix_inner(&mut self) -> Result<Expr, ParseError> {
        match self.peek().clone() {
            Token::Minus => {
                self.advance();
                Ok(Expr::UnaryOp {
                    op: UnaryOp::Minus,
                    expr: Box::new(self.parse_subexpr(8)?),
                })
            }
            Token::Plus => {
                self.advance();
                Ok(Expr::UnaryOp {
                    op: UnaryOp::Plus,
                    expr: Box::new(self.parse_subexpr(8)?),
                })
            }
            Token::Keyword(Keyword::Not) => {
                self.advance();
                Ok(Expr::UnaryOp {
                    op: UnaryOp::Not,
                    expr: Box::new(self.parse_subexpr(3)?),
                })
            }
            Token::Keyword(Keyword::Null) => {
                self.advance();
                Ok(Expr::Literal(Literal::Null))
            }
            Token::Keyword(Keyword::True) => {
                self.advance();
                Ok(Expr::Literal(Literal::Boolean(true)))
            }
            Token::Keyword(Keyword::False) => {
                self.advance();
                Ok(Expr::Literal(Literal::Boolean(false)))
            }
            Token::Keyword(Keyword::Exists) => {
                self.advance();
                self.expect(&Token::LParen)?;
                let subquery = Box::new(self.parse_query()?);
                self.expect(&Token::RParen)?;
                Ok(Expr::Exists {
                    subquery,
                    negated: false,
                })
            }
            Token::Keyword(Keyword::Cast) => {
                self.advance();
                self.expect(&Token::LParen)?;
                let expr = Box::new(self.parse_expr()?);
                self.expect_keyword(Keyword::As)?;
                let data_type = self.parse_data_type()?;
                self.expect(&Token::RParen)?;
                Ok(Expr::Cast { expr, data_type })
            }
            Token::Keyword(Keyword::Case) => self.parse_case(),
            Token::StringLit(s) => {
                self.advance();
                Ok(Expr::Literal(Literal::String(s)))
            }
            Token::Number(n) => {
                self.advance();
                self.number_literal(&n)
            }
            Token::Param(n) => {
                self.advance();
                Ok(Expr::Parameter(n))
            }
            Token::LParen => {
                self.advance();
                if matches!(
                    self.peek(),
                    Token::Keyword(Keyword::Select) | Token::Keyword(Keyword::With)
                ) {
                    let query = Box::new(self.parse_query()?);
                    self.expect(&Token::RParen)?;
                    return Ok(Expr::Subquery(query));
                }
                let expr = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                Ok(expr)
            }
            Token::Ident(_) => {
                let name = self.parse_object_name()?;
                if self.peek() == &Token::LParen {
                    if name.0.len() != 1 {
                        return self.err("qualified function names are not supported");
                    }
                    return self.parse_function_call(name.base().to_string());
                }
                Ok(Expr::Column(name))
            }
            Token::Keyword(kw) if !kw.is_reserved() => {
                let name = self.parse_object_name()?;
                Ok(Expr::Column(name))
            }
            _ => self.expected("an expression"),
        }
    }

    fn parse_function_call(&mut self, name: String) -> Result<Expr, ParseError> {
        self.expect(&Token::LParen)?;
        if self.eat(&Token::Star) {
            self.expect(&Token::RParen)?;
            return Ok(Expr::Function(FunctionCall {
                name,
                args: FunctionArgs::Wildcard,
                distinct: false,
            }));
        }
        let distinct = self.eat_keyword(Keyword::Distinct);
        let mut args = Vec::new();
        if self.peek() != &Token::RParen {
            args.push(self.parse_expr()?);
            while self.eat(&Token::Comma) {
                args.push(self.parse_expr()?);
            }
        }
        self.expect(&Token::RParen)?;
        if distinct && args.is_empty() {
            return self.err("`DISTINCT` requires at least one argument");
        }
        Ok(Expr::Function(FunctionCall {
            name,
            args: FunctionArgs::List(args),
            distinct,
        }))
    }

    fn parse_case(&mut self) -> Result<Expr, ParseError> {
        self.expect_keyword(Keyword::Case)?;
        let operand = if self.peek() == &Token::Keyword(Keyword::When) {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };
        let mut branches = Vec::new();
        while self.eat_keyword(Keyword::When) {
            let condition = self.parse_expr()?;
            self.expect_keyword(Keyword::Then)?;
            let result = self.parse_expr()?;
            branches.push((condition, result));
        }
        if branches.is_empty() {
            return self.expected("at least one `WHEN` branch");
        }
        let else_result = if self.eat_keyword(Keyword::Else) {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect_keyword(Keyword::End)?;
        Ok(Expr::Case {
            operand,
            branches,
            else_result,
        })
    }

    fn number_literal(&self, raw: &str) -> Result<Expr, ParseError> {
        if raw.contains('.') || raw.contains('e') || raw.contains('E') {
            match raw.parse::<f64>() {
                Ok(f) if f.is_finite() => Ok(Expr::Literal(Literal::Float(f))),
                _ => self.err("numeric literal out of range"),
            }
        } else {
            match raw.parse::<i64>() {
                Ok(i) => Ok(Expr::Literal(Literal::Int(i))),
                Err(_) => self.err("integer literal out of range for `bigint`"),
            }
        }
    }
}
