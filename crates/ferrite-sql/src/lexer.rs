use crate::ParseError;

/// A keyword recognised by the Ferrite lexer.
///
/// Keywords are split into reserved and non-reserved (see
/// [`Keyword::is_reserved`]). Non-reserved keywords may still be used as
/// bare identifiers, which keeps common column names such as `text`,
/// `key` or `row` usable without quoting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Keyword {
    Add,
    After,
    All,
    Alter,
    And,
    As,
    Asc,
    Before,
    Begin,
    Between,
    Bigint,
    Bool,
    Boolean,
    By,
    Call,
    Cascade,
    Case,
    Cast,
    Column,
    Commit,
    Create,
    Collate,
    Cross,
    Default,
    Delete,
    Desc,
    Distinct,
    Double,
    Drop,
    Each,
    Else,
    Elsif,
    End,
    Except,
    Execute,
    Exists,
    False,
    First,
    Float8,
    For,
    From,
    Full,
    Function,
    Group,
    Having,
    If,
    Ilike,
    In,
    Index,
    Inner,
    Insert,
    Int,
    Int4,
    Int8,
    Integer,
    Intersect,
    Into,
    Is,
    Join,
    Json,
    Jsonb,
    Key,
    Last,
    Left,
    Like,
    Limit,
    Not,
    Null,
    Nulls,
    Offset,
    On,
    Or,
    Order,
    Outer,
    Precision,
    Primary,
    Procedure,
    Raise,
    Replace,
    Restrict,
    Return,
    Returning,
    Right,
    Rollback,
    Row,
    Select,
    Set,
    Start,
    Statement,
    Table,
    Text,
    Then,
    Timestamp,
    Timestamptz,
    Transaction,
    Trigger,
    True,
    Union,
    Unique,
    Update,
    Using,
    Uuid,
    Values,
    Varchar,
    When,
    Where,
    With,
    Work,
}

impl Keyword {
    /// Reserved keywords can never appear where a bare identifier is
    /// expected; non-reserved ones can. Type names and a handful of
    /// noise words are deliberately non-reserved.
    pub fn is_reserved(self) -> bool {
        !matches!(
            self,
            Keyword::Add
                | Keyword::Bigint
                | Keyword::Bool
                | Keyword::Boolean
                | Keyword::Cascade
                | Keyword::Column
                | Keyword::Double
                | Keyword::Each
                | Keyword::First
                | Keyword::Float8
                | Keyword::Function
                | Keyword::Index
                | Keyword::Int
                | Keyword::Int4
                | Keyword::Int8
                | Keyword::Integer
                | Keyword::Json
                | Keyword::Jsonb
                | Keyword::Key
                | Keyword::Last
                | Keyword::Nulls
                | Keyword::Precision
                | Keyword::Procedure
                | Keyword::Replace
                | Keyword::Restrict
                | Keyword::Row
                | Keyword::Statement
                | Keyword::Text
                | Keyword::Timestamp
                | Keyword::Timestamptz
                | Keyword::Transaction
                | Keyword::Trigger
                | Keyword::Uuid
                | Keyword::Varchar
                | Keyword::Work
        )
    }

    /// Lower-case spelling, used to render the keyword back in error
    /// messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Keyword::Add => "add",
            Keyword::After => "after",
            Keyword::All => "all",
            Keyword::Alter => "alter",
            Keyword::And => "and",
            Keyword::As => "as",
            Keyword::Asc => "asc",
            Keyword::Before => "before",
            Keyword::Begin => "begin",
            Keyword::Between => "between",
            Keyword::Bigint => "bigint",
            Keyword::Bool => "bool",
            Keyword::Boolean => "boolean",
            Keyword::By => "by",
            Keyword::Call => "call",
            Keyword::Cascade => "cascade",
            Keyword::Case => "case",
            Keyword::Cast => "cast",
            Keyword::Collate => "collate",
            Keyword::Column => "column",
            Keyword::Commit => "commit",
            Keyword::Create => "create",
            Keyword::Cross => "cross",
            Keyword::Default => "default",
            Keyword::Delete => "delete",
            Keyword::Desc => "desc",
            Keyword::Distinct => "distinct",
            Keyword::Double => "double",
            Keyword::Drop => "drop",
            Keyword::Each => "each",
            Keyword::Else => "else",
            Keyword::Elsif => "elsif",
            Keyword::End => "end",
            Keyword::Except => "except",
            Keyword::Execute => "execute",
            Keyword::Exists => "exists",
            Keyword::False => "false",
            Keyword::First => "first",
            Keyword::Float8 => "float8",
            Keyword::For => "for",
            Keyword::From => "from",
            Keyword::Full => "full",
            Keyword::Function => "function",
            Keyword::Group => "group",
            Keyword::Having => "having",
            Keyword::If => "if",
            Keyword::Ilike => "ilike",
            Keyword::In => "in",
            Keyword::Index => "index",
            Keyword::Inner => "inner",
            Keyword::Insert => "insert",
            Keyword::Int => "int",
            Keyword::Int4 => "int4",
            Keyword::Int8 => "int8",
            Keyword::Integer => "integer",
            Keyword::Intersect => "intersect",
            Keyword::Into => "into",
            Keyword::Is => "is",
            Keyword::Join => "join",
            Keyword::Json => "json",
            Keyword::Jsonb => "jsonb",
            Keyword::Key => "key",
            Keyword::Last => "last",
            Keyword::Left => "left",
            Keyword::Like => "like",
            Keyword::Limit => "limit",
            Keyword::Not => "not",
            Keyword::Null => "null",
            Keyword::Nulls => "nulls",
            Keyword::Offset => "offset",
            Keyword::On => "on",
            Keyword::Or => "or",
            Keyword::Order => "order",
            Keyword::Outer => "outer",
            Keyword::Precision => "precision",
            Keyword::Primary => "primary",
            Keyword::Procedure => "procedure",
            Keyword::Raise => "raise",
            Keyword::Replace => "replace",
            Keyword::Restrict => "restrict",
            Keyword::Return => "return",
            Keyword::Returning => "returning",
            Keyword::Right => "right",
            Keyword::Rollback => "rollback",
            Keyword::Row => "row",
            Keyword::Select => "select",
            Keyword::Set => "set",
            Keyword::Start => "start",
            Keyword::Statement => "statement",
            Keyword::Table => "table",
            Keyword::Text => "text",
            Keyword::Then => "then",
            Keyword::Timestamp => "timestamp",
            Keyword::Timestamptz => "timestamptz",
            Keyword::Transaction => "transaction",
            Keyword::Trigger => "trigger",
            Keyword::True => "true",
            Keyword::Union => "union",
            Keyword::Unique => "unique",
            Keyword::Update => "update",
            Keyword::Using => "using",
            Keyword::Uuid => "uuid",
            Keyword::Values => "values",
            Keyword::Varchar => "varchar",
            Keyword::When => "when",
            Keyword::Where => "where",
            Keyword::With => "with",
            Keyword::Work => "work",
        }
    }
}

fn keyword_from_str(word: &str) -> Option<Keyword> {
    let kw = match word {
        "add" => Keyword::Add,
        "after" => Keyword::After,
        "all" => Keyword::All,
        "alter" => Keyword::Alter,
        "and" => Keyword::And,
        "as" => Keyword::As,
        "asc" => Keyword::Asc,
        "before" => Keyword::Before,
        "begin" => Keyword::Begin,
        "between" => Keyword::Between,
        "bigint" => Keyword::Bigint,
        "bool" => Keyword::Bool,
        "boolean" => Keyword::Boolean,
        "by" => Keyword::By,
        "call" => Keyword::Call,
        "cascade" => Keyword::Cascade,
        "case" => Keyword::Case,
        "cast" => Keyword::Cast,
        "collate" => Keyword::Collate,
        "column" => Keyword::Column,
        "commit" => Keyword::Commit,
        "create" => Keyword::Create,
        "cross" => Keyword::Cross,
        "default" => Keyword::Default,
        "delete" => Keyword::Delete,
        "desc" => Keyword::Desc,
        "distinct" => Keyword::Distinct,
        "double" => Keyword::Double,
        "drop" => Keyword::Drop,
        "each" => Keyword::Each,
        "else" => Keyword::Else,
        "elsif" => Keyword::Elsif,
        "end" => Keyword::End,
        "except" => Keyword::Except,
        "execute" => Keyword::Execute,
        "exists" => Keyword::Exists,
        "false" => Keyword::False,
        "first" => Keyword::First,
        "float8" => Keyword::Float8,
        "for" => Keyword::For,
        "from" => Keyword::From,
        "full" => Keyword::Full,
        "function" => Keyword::Function,
        "group" => Keyword::Group,
        "having" => Keyword::Having,
        "if" => Keyword::If,
        "ilike" => Keyword::Ilike,
        "in" => Keyword::In,
        "index" => Keyword::Index,
        "inner" => Keyword::Inner,
        "insert" => Keyword::Insert,
        "int" => Keyword::Int,
        "int4" => Keyword::Int4,
        "int8" => Keyword::Int8,
        "integer" => Keyword::Integer,
        "intersect" => Keyword::Intersect,
        "into" => Keyword::Into,
        "is" => Keyword::Is,
        "join" => Keyword::Join,
        "json" => Keyword::Json,
        "jsonb" => Keyword::Jsonb,
        "key" => Keyword::Key,
        "last" => Keyword::Last,
        "left" => Keyword::Left,
        "like" => Keyword::Like,
        "limit" => Keyword::Limit,
        "not" => Keyword::Not,
        "null" => Keyword::Null,
        "nulls" => Keyword::Nulls,
        "offset" => Keyword::Offset,
        "on" => Keyword::On,
        "or" => Keyword::Or,
        "order" => Keyword::Order,
        "outer" => Keyword::Outer,
        "precision" => Keyword::Precision,
        "primary" => Keyword::Primary,
        "procedure" => Keyword::Procedure,
        "raise" => Keyword::Raise,
        "replace" => Keyword::Replace,
        "restrict" => Keyword::Restrict,
        "return" => Keyword::Return,
        "returning" => Keyword::Returning,
        "right" => Keyword::Right,
        "rollback" => Keyword::Rollback,
        "row" => Keyword::Row,
        "select" => Keyword::Select,
        "set" => Keyword::Set,
        "start" => Keyword::Start,
        "statement" => Keyword::Statement,
        "table" => Keyword::Table,
        "text" => Keyword::Text,
        "then" => Keyword::Then,
        "timestamp" => Keyword::Timestamp,
        "timestamptz" => Keyword::Timestamptz,
        "transaction" => Keyword::Transaction,
        "trigger" => Keyword::Trigger,
        "true" => Keyword::True,
        "union" => Keyword::Union,
        "unique" => Keyword::Unique,
        "update" => Keyword::Update,
        "using" => Keyword::Using,
        "uuid" => Keyword::Uuid,
        "values" => Keyword::Values,
        "varchar" => Keyword::Varchar,
        "when" => Keyword::When,
        "where" => Keyword::Where,
        "with" => Keyword::With,
        "work" => Keyword::Work,
        _ => return None,
    };
    Some(kw)
}

/// A lexical token. Identifiers are already case-folded: unquoted ones
/// are lower-cased (Postgres folds down, not up), quoted ones keep their
/// original spelling.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Keyword(Keyword),
    Ident(String),
    /// Raw numeric lexeme; converted to `i64`/`f64` by the parser so that
    /// out-of-range literals surface as a parse error, not a panic.
    Number(String),
    /// Decoded string literal content, with `''` already unescaped.
    StringLit(String),
    /// `$1`-style placeholder for the extended query protocol.
    Param(u32),
    Comma,
    Semi,
    LParen,
    RParen,
    Dot,
    Star,
    Plus,
    Minus,
    Slash,
    Percent,
    Concat,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Eof,
}

impl Token {
    pub fn describe(&self) -> String {
        match self {
            Token::Keyword(k) => format!("keyword `{}`", k.as_str()),
            Token::Ident(i) => format!("identifier `{i}`"),
            Token::Number(n) => format!("number `{n}`"),
            Token::StringLit(_) => "string literal".to_string(),
            Token::Param(n) => format!("parameter `${n}`"),
            Token::Comma => "`,`".to_string(),
            Token::Semi => "`;`".to_string(),
            Token::LParen => "`(`".to_string(),
            Token::RParen => "`)`".to_string(),
            Token::Dot => "`.`".to_string(),
            Token::Star => "`*`".to_string(),
            Token::Plus => "`+`".to_string(),
            Token::Minus => "`-`".to_string(),
            Token::Slash => "`/`".to_string(),
            Token::Percent => "`%`".to_string(),
            Token::Concat => "`||`".to_string(),
            Token::Eq => "`=`".to_string(),
            Token::NotEq => "`<>`".to_string(),
            Token::Lt => "`<`".to_string(),
            Token::LtEq => "`<=`".to_string(),
            Token::Gt => "`>`".to_string(),
            Token::GtEq => "`>=`".to_string(),
            Token::Eof => "end of input".to_string(),
        }
    }
}

/// A token plus the byte offset it starts at, so parse errors can point
/// into the original query text.
#[derive(Debug, Clone, PartialEq)]
pub struct SpannedToken {
    pub token: Token,
    pub offset: usize,
}

/// Turns SQL text into tokens.
///
/// Every failure mode (unterminated literal, unterminated block comment,
/// stray character, oversized parameter index) is reported as a
/// [`ParseError`]; the lexer never panics on arbitrary input.
pub struct Lexer<'a> {
    src: &'a str,
    chars: Vec<(usize, char)>,
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Self {
            src,
            chars: src.char_indices().collect(),
            pos: 0,
        }
    }

    /// Tokenize the whole input, always ending with [`Token::Eof`].
    pub fn tokenize(mut self) -> Result<Vec<SpannedToken>, ParseError> {
        let mut out = Vec::new();
        loop {
            self.skip_trivia()?;
            let offset = self.offset();
            if self.pos >= self.chars.len() {
                out.push(SpannedToken {
                    token: Token::Eof,
                    offset,
                });
                return Ok(out);
            }
            let token = self.next_token()?;
            out.push(SpannedToken { token, offset });
        }
    }

    fn offset(&self) -> usize {
        match self.chars.get(self.pos) {
            Some((byte, _)) => *byte,
            None => self.src.len(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).map(|(_, c)| *c)
    }

    fn peek_at(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).map(|(_, c)| *c)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn err<T>(&self, offset: usize, message: impl Into<String>) -> Result<T, ParseError> {
        Err(ParseError {
            message: message.into(),
            offset,
        })
    }

    fn skip_trivia(&mut self) -> Result<(), ParseError> {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.pos += 1;
                }
                Some('-') if self.peek_at(1) == Some('-') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                Some('/') if self.peek_at(1) == Some('*') => {
                    let start = self.offset();
                    self.pos += 2;
                    let mut depth = 1usize;
                    while depth > 0 {
                        match self.peek() {
                            None => {
                                return self.err(start, "unterminated block comment");
                            }
                            Some('*') if self.peek_at(1) == Some('/') => {
                                self.pos += 2;
                                depth -= 1;
                            }
                            Some('/') if self.peek_at(1) == Some('*') => {
                                self.pos += 2;
                                depth += 1;
                            }
                            Some(_) => self.pos += 1,
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn next_token(&mut self) -> Result<Token, ParseError> {
        let start = self.offset();
        let c = match self.peek() {
            Some(c) => c,
            None => return Ok(Token::Eof),
        };

        if c == '\'' {
            return self.lex_string(start);
        }
        if c == '"' {
            return self.lex_quoted_ident(start);
        }
        if c.is_ascii_digit() {
            return self.lex_number(start);
        }
        if c == '$' {
            return self.lex_param(start);
        }
        if is_ident_start(c) {
            return Ok(self.lex_word());
        }

        self.pos += 1;
        let token = match c {
            ',' => Token::Comma,
            ';' => Token::Semi,
            '(' => Token::LParen,
            ')' => Token::RParen,
            '.' => Token::Dot,
            '*' => Token::Star,
            '+' => Token::Plus,
            '-' => Token::Minus,
            '/' => Token::Slash,
            '%' => Token::Percent,
            '=' => Token::Eq,
            '|' => {
                if self.peek() == Some('|') {
                    self.pos += 1;
                    Token::Concat
                } else {
                    return self.err(start, "unexpected character `|` (did you mean `||`?)");
                }
            }
            '!' => {
                if self.peek() == Some('=') {
                    self.pos += 1;
                    Token::NotEq
                } else {
                    return self.err(start, "unexpected character `!` (did you mean `!=`?)");
                }
            }
            '<' => match self.peek() {
                Some('>') => {
                    self.pos += 1;
                    Token::NotEq
                }
                Some('=') => {
                    self.pos += 1;
                    Token::LtEq
                }
                _ => Token::Lt,
            },
            '>' => {
                if self.peek() == Some('=') {
                    self.pos += 1;
                    Token::GtEq
                } else {
                    Token::Gt
                }
            }
            other => {
                return self.err(start, format!("unexpected character `{other}`"));
            }
        };
        Ok(token)
    }

    fn lex_string(&mut self, start: usize) -> Result<Token, ParseError> {
        self.pos += 1;
        let mut value = String::new();
        loop {
            match self.bump() {
                None => return self.err(start, "unterminated string literal"),
                Some('\'') => {
                    if self.peek() == Some('\'') {
                        self.pos += 1;
                        value.push('\'');
                    } else {
                        return Ok(Token::StringLit(value));
                    }
                }
                Some(c) => value.push(c),
            }
        }
    }

    fn lex_quoted_ident(&mut self, start: usize) -> Result<Token, ParseError> {
        self.pos += 1;
        let mut value = String::new();
        loop {
            match self.bump() {
                None => return self.err(start, "unterminated quoted identifier"),
                Some('"') => {
                    if self.peek() == Some('"') {
                        self.pos += 1;
                        value.push('"');
                    } else if value.is_empty() {
                        return self.err(start, "zero-length quoted identifier");
                    } else {
                        return Ok(Token::Ident(value));
                    }
                }
                Some(c) => value.push(c),
            }
        }
    }

    fn lex_number(&mut self, start: usize) -> Result<Token, ParseError> {
        let begin = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some('.') && matches!(self.peek_at(1), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some('e') | Some('E')) {
            let exp_digit = match self.peek_at(1) {
                Some('+') | Some('-') => matches!(self.peek_at(2), Some(c) if c.is_ascii_digit()),
                Some(c) => c.is_ascii_digit(),
                None => false,
            };
            if exp_digit {
                self.pos += 2;
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
        }
        if matches!(self.peek(), Some(c) if is_ident_start(c)) {
            return self.err(start, "invalid numeric literal");
        }
        let text: String = self.chars[begin..self.pos]
            .iter()
            .map(|(_, c)| *c)
            .collect();
        Ok(Token::Number(text))
    }

    fn lex_param(&mut self, start: usize) -> Result<Token, ParseError> {
        self.pos += 1;
        let begin = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if begin == self.pos {
            return self.err(start, "expected a digit after `$`");
        }
        let text: String = self.chars[begin..self.pos]
            .iter()
            .map(|(_, c)| *c)
            .collect();
        match text.parse::<u32>() {
            Ok(n) => Ok(Token::Param(n)),
            Err(_) => self.err(start, "parameter index out of range"),
        }
    }

    fn lex_word(&mut self) -> Token {
        let begin = self.pos;
        while matches!(self.peek(), Some(c) if is_ident_continue(c)) {
            self.pos += 1;
        }
        let raw: String = self.chars[begin..self.pos]
            .iter()
            .map(|(_, c)| *c)
            .collect();
        let folded = raw.to_lowercase();
        match keyword_from_str(&folded) {
            Some(kw) => Token::Keyword(kw),
            None => Token::Ident(folded),
        }
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || (!c.is_ascii() && c.is_alphabetic())
}

fn is_ident_continue(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c == '$'
}
