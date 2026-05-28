//! A small expression language for automation blocks, modelled loosely on
//! LlamaLab Automate. Values are `serde_json::Value` so JSON encode/decode
//! and dictionary/array access fall out naturally.
//!
//! Grammar (precedence low → high):
//!   ternary   : or ('?' expr ':' expr)?
//!   or        : and ('||' and)*
//!   and       : equality ('&&' equality)*
//!   equality  : comparison (('==' | '!=') comparison)*
//!   comparison: additive (('<' | '<=' | '>' | '>=') additive)*
//!   additive  : multiplicative (('+' | '-') multiplicative)*
//!   multiplic.: unary (('*' | '/' | '%') unary)*
//!   unary     : ('!' | '-') unary | postfix
//!   postfix   : primary ( '(' args ')' | '[' expr ']' | '.' ident )*
//!   primary   : number | string | 'true' | 'false' | 'null'
//!             | '$' ident | ident | '(' expr ')' | '[' args ']'
//!
//! `$name` reads a variable; bare `input` is a dictionary of the upstream
//! block's outputs (so `input.body`, `input.status_code`). `+` concatenates
//! when either operand is text, otherwise adds numerically.

use std::collections::BTreeMap;

use serde_json::Value;

pub(crate) struct EvalContext<'a> {
    pub(crate) variables: &'a BTreeMap<String, Value>,
    pub(crate) input: Value,
}

pub(crate) fn evaluate(src: &str, ctx: &EvalContext) -> Result<Value, String> {
    let expr = parse(src)?;
    eval(&expr, ctx)
}

/// Lex + parse without evaluating. Used to validate an expression at save
/// time so syntax errors surface in the editor, not at runtime.
pub(crate) fn validate(src: &str) -> Result<(), String> {
    parse(src).map(|_| ())
}

fn parse(src: &str) -> Result<Expr, String> {
    let tokens = lex(src)?;
    let mut parser = Parser { tokens, pos: 0 };
    let expr = parser.parse_expr()?;
    if parser.peek() != &Token::Eof {
        return Err(format!("unexpected trailing input near {:?}", parser.peek()));
    }
    Ok(expr)
}

// ---------------- Lexer ----------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f64),
    Str(String),
    Ident(String),
    Var(String),
    // punctuation / operators
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    AndAnd,
    OrOr,
    Bang,
    Question,
    Colon,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Eof,
}

fn lex(src: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '+' => {
                out.push(Token::Plus);
                i += 1;
            }
            '-' => {
                out.push(Token::Minus);
                i += 1;
            }
            '*' => {
                out.push(Token::Star);
                i += 1;
            }
            '/' => {
                out.push(Token::Slash);
                i += 1;
            }
            '%' => {
                out.push(Token::Percent);
                i += 1;
            }
            '(' => {
                out.push(Token::LParen);
                i += 1;
            }
            ')' => {
                out.push(Token::RParen);
                i += 1;
            }
            '[' => {
                out.push(Token::LBracket);
                i += 1;
            }
            ']' => {
                out.push(Token::RBracket);
                i += 1;
            }
            ',' => {
                out.push(Token::Comma);
                i += 1;
            }
            '.' => {
                out.push(Token::Dot);
                i += 1;
            }
            '?' => {
                out.push(Token::Question);
                i += 1;
            }
            ':' => {
                out.push(Token::Colon);
                i += 1;
            }
            '=' => {
                if chars.get(i + 1) == Some(&'=') {
                    out.push(Token::EqEq);
                    i += 2;
                } else {
                    return Err("'=' must be '==' for comparison".to_string());
                }
            }
            '!' => {
                if chars.get(i + 1) == Some(&'=') {
                    out.push(Token::NotEq);
                    i += 2;
                } else {
                    out.push(Token::Bang);
                    i += 1;
                }
            }
            '<' => {
                if chars.get(i + 1) == Some(&'=') {
                    out.push(Token::LtEq);
                    i += 2;
                } else {
                    out.push(Token::Lt);
                    i += 1;
                }
            }
            '>' => {
                if chars.get(i + 1) == Some(&'=') {
                    out.push(Token::GtEq);
                    i += 2;
                } else {
                    out.push(Token::Gt);
                    i += 1;
                }
            }
            '&' => {
                if chars.get(i + 1) == Some(&'&') {
                    out.push(Token::AndAnd);
                    i += 2;
                } else {
                    return Err("'&' must be '&&'".to_string());
                }
            }
            '|' => {
                if chars.get(i + 1) == Some(&'|') {
                    out.push(Token::OrOr);
                    i += 2;
                } else {
                    return Err("'|' must be '||'".to_string());
                }
            }
            '"' | '\'' => {
                let quote = c;
                i += 1;
                let mut s = String::new();
                let mut closed = false;
                while i < chars.len() {
                    let ch = chars[i];
                    if ch == '\\' {
                        // escape sequence
                        i += 1;
                        match chars.get(i) {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some('r') => s.push('\r'),
                            Some('\\') => s.push('\\'),
                            Some('"') => s.push('"'),
                            Some('\'') => s.push('\''),
                            Some(other) => {
                                s.push('\\');
                                s.push(*other);
                            }
                            None => return Err("unterminated escape".to_string()),
                        }
                        i += 1;
                    } else if ch == quote {
                        closed = true;
                        i += 1;
                        break;
                    } else {
                        s.push(ch);
                        i += 1;
                    }
                }
                if !closed {
                    return Err("unterminated string".to_string());
                }
                out.push(Token::Str(s));
            }
            '$' => {
                i += 1;
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                if i == start {
                    return Err("expected variable name after '$'".to_string());
                }
                out.push(Token::Var(chars[start..i].iter().collect()));
            }
            _ if c.is_ascii_digit() || (c == '.' ) => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                let n: f64 = text
                    .parse()
                    .map_err(|_| format!("invalid number '{text}'"))?;
                out.push(Token::Number(n));
            }
            _ if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                out.push(Token::Ident(chars[start..i].iter().collect()));
            }
            other => return Err(format!("unexpected character '{other}'")),
        }
    }
    out.push(Token::Eof);
    Ok(out)
}

// ---------------- AST ----------------

#[derive(Debug, Clone)]
enum Expr {
    Literal(Value),
    Var(String),
    Ident(String),
    Unary(UnaryOp, Box<Expr>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
    Index(Box<Expr>, Box<Expr>),
    Member(Box<Expr>, String),
    Array(Vec<Expr>),
}

#[derive(Debug, Clone, Copy)]
enum UnaryOp {
    Not,
    Neg,
}

#[derive(Debug, Clone, Copy)]
enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

// ---------------- Parser ----------------

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn advance(&mut self) -> Token {
        let t = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        self.pos += 1;
        t
    }

    fn expect(&mut self, expected: &Token) -> Result<(), String> {
        if self.peek() == expected {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected {expected:?}, found {:?}", self.peek()))
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_ternary()
    }

    fn parse_ternary(&mut self) -> Result<Expr, String> {
        let cond = self.parse_or()?;
        if self.peek() == &Token::Question {
            self.advance();
            let then_branch = self.parse_expr()?;
            self.expect(&Token::Colon)?;
            let else_branch = self.parse_expr()?;
            Ok(Expr::Ternary(
                Box::new(cond),
                Box::new(then_branch),
                Box::new(else_branch),
            ))
        } else {
            Ok(cond)
        }
    }

    fn parse_or(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and()?;
        while self.peek() == &Token::OrOr {
            self.advance();
            let right = self.parse_and()?;
            left = Expr::Binary(BinaryOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_equality()?;
        while self.peek() == &Token::AndAnd {
            self.advance();
            let right = self.parse_equality()?;
            left = Expr::Binary(BinaryOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_equality(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_comparison()?;
        loop {
            let op = match self.peek() {
                Token::EqEq => BinaryOp::Eq,
                Token::NotEq => BinaryOp::Ne,
                _ => break,
            };
            self.advance();
            let right = self.parse_comparison()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_additive()?;
        loop {
            let op = match self.peek() {
                Token::Lt => BinaryOp::Lt,
                Token::LtEq => BinaryOp::Le,
                Token::Gt => BinaryOp::Gt,
                Token::GtEq => BinaryOp::Ge,
                _ => break,
            };
            self.advance();
            let right = self.parse_additive()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinaryOp::Add,
                Token::Minus => BinaryOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_multiplicative()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinaryOp::Mul,
                Token::Slash => BinaryOp::Div,
                Token::Percent => BinaryOp::Rem,
                _ => break,
            };
            self.advance();
            let right = self.parse_unary()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Token::Bang => {
                self.advance();
                Ok(Expr::Unary(UnaryOp::Not, Box::new(self.parse_unary()?)))
            }
            Token::Minus => {
                self.advance();
                Ok(Expr::Unary(UnaryOp::Neg, Box::new(self.parse_unary()?)))
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_primary()?;
        loop {
            match self.peek() {
                Token::LBracket => {
                    self.advance();
                    let index = self.parse_expr()?;
                    self.expect(&Token::RBracket)?;
                    expr = Expr::Index(Box::new(expr), Box::new(index));
                }
                Token::Dot => {
                    self.advance();
                    let name = match self.advance() {
                        Token::Ident(s) => s,
                        other => return Err(format!("expected member name, found {other:?}")),
                    };
                    expr = Expr::Member(Box::new(expr), name);
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.advance() {
            Token::Number(n) => Ok(Expr::Literal(num(n))),
            Token::Str(s) => Ok(Expr::Literal(Value::String(s))),
            Token::Var(name) => Ok(Expr::Var(name)),
            Token::Ident(name) => {
                if name == "true" {
                    Ok(Expr::Literal(Value::Bool(true)))
                } else if name == "false" {
                    Ok(Expr::Literal(Value::Bool(false)))
                } else if name == "null" {
                    Ok(Expr::Literal(Value::Null))
                } else if self.peek() == &Token::LParen {
                    self.advance();
                    let args = self.parse_args(&Token::RParen)?;
                    Ok(Expr::Call(name, args))
                } else {
                    Ok(Expr::Ident(name))
                }
            }
            Token::LParen => {
                let inner = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                Ok(inner)
            }
            Token::LBracket => {
                let items = self.parse_args(&Token::RBracket)?;
                Ok(Expr::Array(items))
            }
            other => Err(format!("unexpected token {other:?}")),
        }
    }

    fn parse_args(&mut self, close: &Token) -> Result<Vec<Expr>, String> {
        let mut args = Vec::new();
        if self.peek() == close {
            self.advance();
            return Ok(args);
        }
        loop {
            args.push(self.parse_expr()?);
            match self.peek() {
                Token::Comma => {
                    self.advance();
                }
                t if t == close => {
                    self.advance();
                    break;
                }
                other => return Err(format!("expected ',' or {close:?}, found {other:?}")),
            }
        }
        Ok(args)
    }
}

// ---------------- Evaluator ----------------

fn eval(expr: &Expr, ctx: &EvalContext) -> Result<Value, String> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Var(name) => Ok(ctx.variables.get(name).cloned().unwrap_or(Value::Null)),
        Expr::Ident(name) => match name.as_str() {
            "input" => Ok(ctx.input.clone()),
            other => Err(format!("unknown identifier '{other}' (use $name for variables)")),
        },
        Expr::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(eval(item, ctx)?);
            }
            Ok(Value::Array(out))
        }
        Expr::Unary(op, inner) => {
            let v = eval(inner, ctx)?;
            match op {
                UnaryOp::Not => Ok(Value::Bool(!truthy(&v))),
                UnaryOp::Neg => Ok(num(-to_number(&v))),
            }
        }
        Expr::Ternary(cond, a, b) => {
            if truthy(&eval(cond, ctx)?) {
                eval(a, ctx)
            } else {
                eval(b, ctx)
            }
        }
        Expr::Binary(op, l, r) => eval_binary(*op, l, r, ctx),
        Expr::Index(base, idx) => {
            let base = eval(base, ctx)?;
            let idx = eval(idx, ctx)?;
            Ok(index_value(&base, &idx))
        }
        Expr::Member(base, name) => {
            let base = eval(base, ctx)?;
            Ok(index_value(&base, &Value::String(name.clone())))
        }
        Expr::Call(name, args) => {
            let mut values = Vec::with_capacity(args.len());
            for a in args {
                values.push(eval(a, ctx)?);
            }
            call_function(name, &values)
        }
    }
}

fn eval_binary(op: BinaryOp, l: &Expr, r: &Expr, ctx: &EvalContext) -> Result<Value, String> {
    // Short-circuiting logical operators.
    match op {
        BinaryOp::And => {
            let left = eval(l, ctx)?;
            if !truthy(&left) {
                return Ok(left);
            }
            return eval(r, ctx);
        }
        BinaryOp::Or => {
            let left = eval(l, ctx)?;
            if truthy(&left) {
                return Ok(left);
            }
            return eval(r, ctx);
        }
        _ => {}
    }

    let left = eval(l, ctx)?;
    let right = eval(r, ctx)?;
    match op {
        BinaryOp::Add => {
            // Concatenate if either side is a string, else add numerically.
            if left.is_string() || right.is_string() {
                Ok(Value::String(format!("{}{}", to_text(&left), to_text(&right))))
            } else {
                Ok(num(to_number(&left) + to_number(&right)))
            }
        }
        BinaryOp::Sub => Ok(num(to_number(&left) - to_number(&right))),
        BinaryOp::Mul => Ok(num(to_number(&left) * to_number(&right))),
        BinaryOp::Div => Ok(num(to_number(&left) / to_number(&right))),
        BinaryOp::Rem => Ok(num(to_number(&left) % to_number(&right))),
        BinaryOp::Eq => Ok(Value::Bool(values_equal(&left, &right))),
        BinaryOp::Ne => Ok(Value::Bool(!values_equal(&left, &right))),
        BinaryOp::Lt => Ok(Value::Bool(compare(&left, &right) == Some(std::cmp::Ordering::Less))),
        BinaryOp::Le => Ok(Value::Bool(matches!(
            compare(&left, &right),
            Some(std::cmp::Ordering::Less) | Some(std::cmp::Ordering::Equal)
        ))),
        BinaryOp::Gt => Ok(Value::Bool(
            compare(&left, &right) == Some(std::cmp::Ordering::Greater),
        )),
        BinaryOp::Ge => Ok(Value::Bool(matches!(
            compare(&left, &right),
            Some(std::cmp::Ordering::Greater) | Some(std::cmp::Ordering::Equal)
        ))),
        BinaryOp::And | BinaryOp::Or => unreachable!(),
    }
}

// ---------------- Value helpers ----------------

pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn to_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => s.trim().parse().unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

/// Stringify a value the way text concatenation and the `++`-style output
/// expect: strings raw, numbers without a trailing `.0`, bools as
/// true/false, null as empty, containers as compact JSON.
pub(crate) fn to_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => format_number(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn format_number(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f == f.trunc() && f.is_finite() && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

/// Build a numeric Value, preferring an integer representation when the
/// result is whole so JSON encoding produces `1` rather than `1.0`.
/// Non-finite values (NaN/Inf, which JSON can't represent) become null.
fn num(f: f64) -> Value {
    if !f.is_finite() {
        Value::Null
    } else if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
        Value::from(f as i64)
    } else {
        Value::from(f)
    }
}

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(_), _) | (_, Value::Number(_))
            if !a.is_string() && !b.is_string() && !a.is_null() && !b.is_null() =>
        {
            to_number(a) == to_number(b)
        }
        _ => a == b,
    }
}

fn compare(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    if a.is_string() && b.is_string() {
        return a.as_str().unwrap().partial_cmp(b.as_str().unwrap());
    }
    to_number(a).partial_cmp(&to_number(b))
}

fn index_value(base: &Value, idx: &Value) -> Value {
    match base {
        Value::Object(map) => {
            let key = to_text(idx);
            map.get(&key).cloned().unwrap_or(Value::Null)
        }
        Value::Array(arr) => {
            let i = to_number(idx);
            if i.is_nan() || i < 0.0 {
                return Value::Null;
            }
            arr.get(i as usize).cloned().unwrap_or(Value::Null)
        }
        Value::String(s) => {
            let i = to_number(idx);
            if i.is_nan() || i < 0.0 {
                return Value::Null;
            }
            s.chars()
                .nth(i as usize)
                .map(|c| Value::String(c.to_string()))
                .unwrap_or(Value::Null)
        }
        _ => Value::Null,
    }
}

// ---------------- Functions ----------------

fn call_function(name: &str, args: &[Value]) -> Result<Value, String> {
    let arg = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
    match name {
        // --- JSON ---
        "jsonEncode" => Ok(Value::String(
            serde_json::to_string(&arg(0)).map_err(|e| e.to_string())?,
        )),
        "jsonDecode" => {
            let s = to_text(&arg(0));
            if s.trim().is_empty() {
                return Err(
                    "jsonDecode: input is empty (the upstream block may not have run yet)"
                        .to_string(),
                );
            }
            serde_json::from_str(&s).map_err(|e| format!("jsonDecode: {e}"))
        }
        // --- String ---
        "upper" | "upperCase" => Ok(Value::String(to_text(&arg(0)).to_uppercase())),
        "lower" | "lowerCase" => Ok(Value::String(to_text(&arg(0)).to_lowercase())),
        "trim" => Ok(Value::String(to_text(&arg(0)).trim().to_string())),
        "len" => Ok(num(value_len(&arg(0)) as f64)),
        "contains" => {
            let haystack = arg(0);
            let needle = arg(1);
            Ok(Value::Bool(match &haystack {
                Value::Array(a) => a.iter().any(|x| values_equal(x, &needle)),
                Value::Object(o) => o.contains_key(&to_text(&needle)),
                _ => to_text(&haystack).contains(&to_text(&needle)),
            }))
        }
        "indexOf" => {
            let hay = to_text(&arg(0));
            let needle = to_text(&arg(1));
            Ok(num(
                hay.find(&needle).map(|b| hay[..b].chars().count() as i64).unwrap_or(-1) as f64,
            ))
        }
        "replace" | "replaceAll" => {
            let s = to_text(&arg(0));
            let from = to_text(&arg(1));
            let to = to_text(&arg(2));
            Ok(Value::String(s.replace(&from, &to)))
        }
        "split" => {
            let s = to_text(&arg(0));
            let sep = to_text(&arg(1));
            let parts: Vec<Value> = if sep.is_empty() {
                s.chars().map(|c| Value::String(c.to_string())).collect()
            } else {
                s.split(&sep).map(|p| Value::String(p.to_string())).collect()
            };
            Ok(Value::Array(parts))
        }
        "join" => {
            let sep = to_text(&arg(1));
            match arg(0) {
                Value::Array(a) => Ok(Value::String(
                    a.iter().map(to_text).collect::<Vec<_>>().join(&sep),
                )),
                other => Ok(Value::String(to_text(&other))),
            }
        }
        "substr" => {
            let s = to_text(&arg(0));
            let chars: Vec<char> = s.chars().collect();
            let start = (to_number(&arg(1)).max(0.0) as usize).min(chars.len());
            let end = if args.len() >= 3 {
                (to_number(&arg(2)).max(0.0) as usize).min(chars.len())
            } else {
                chars.len()
            };
            if end <= start {
                Ok(Value::String(String::new()))
            } else {
                Ok(Value::String(chars[start..end].iter().collect()))
            }
        }
        // --- Math ---
        "abs" => Ok(num(to_number(&arg(0)).abs())),
        "round" => Ok(num(to_number(&arg(0)).round())),
        "floor" => Ok(num(to_number(&arg(0)).floor())),
        "ceil" => Ok(num(to_number(&arg(0)).ceil())),
        "trunc" => Ok(num(to_number(&arg(0)).trunc())),
        "sqrt" => Ok(num(to_number(&arg(0)).sqrt())),
        "pow" => Ok(num(to_number(&arg(0)).powf(to_number(&arg(1))))),
        "min" => Ok(num(args.iter().map(to_number).fold(f64::INFINITY, f64::min))),
        "max" => Ok(num(
            args.iter().map(to_number).fold(f64::NEG_INFINITY, f64::max),
        )),
        "random" => Ok(num(simple_random())),
        // --- Encoding ---
        "urlEncode" => Ok(Value::String(url_encode(&to_text(&arg(0))))),
        "base64Encode" => Ok(Value::String(base64_encode(to_text(&arg(0)).as_bytes()))),
        // --- Conversion ---
        "number" => {
            let n = to_number(&arg(0));
            if n.is_nan() {
                Ok(Value::Null)
            } else {
                Ok(num(n))
            }
        }
        "text" | "string" => Ok(Value::String(to_text(&arg(0)))),
        "bool" => Ok(Value::Bool(truthy(&arg(0)))),
        "type" => Ok(Value::String(
            match arg(0) {
                Value::Null => "null",
                Value::Bool(_) => "boolean",
                Value::Number(_) => "number",
                Value::String(_) => "text",
                Value::Array(_) => "array",
                Value::Object(_) => "dictionary",
            }
            .to_string(),
        )),
        // --- Utility ---
        "coalesce" => Ok(args
            .iter()
            .find(|v| !v.is_null())
            .cloned()
            .unwrap_or(Value::Null)),
        "now" => Ok(num(crate::time::now_ms() as f64)),
        "keys" => match arg(0) {
            Value::Object(o) => Ok(Value::Array(
                o.keys().map(|k| Value::String(k.clone())).collect(),
            )),
            _ => Ok(Value::Array(vec![])),
        },
        "values" => match arg(0) {
            Value::Object(o) => Ok(Value::Array(o.values().cloned().collect())),
            Value::Array(a) => Ok(Value::Array(a)),
            _ => Ok(Value::Array(vec![])),
        },
        other => Err(format!("unknown function '{other}'")),
    }
}

fn value_len(v: &Value) -> usize {
    match v {
        Value::String(s) => s.chars().count(),
        Value::Array(a) => a.len(),
        Value::Object(o) => o.len(),
        Value::Null => 0,
        _ => to_text(v).chars().count(),
    }
}

// A cheap, dependency-free PRNG seeded from the wall clock. Good enough for
// automation jitter; not for anything security-sensitive.
fn simple_random() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // xorshift-ish mix
    let mut x = nanos as u64 ^ 0x9E37_79B9_7F4A_7C15;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    (x as f64 / u64::MAX as f64).fract()
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with(vars: Vec<(&str, Value)>, input: Value) -> BTreeMap<String, Value> {
        let _ = input;
        vars.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    fn eval_str(src: &str, vars: &BTreeMap<String, Value>, input: Value) -> Value {
        let ctx = EvalContext {
            variables: vars,
            input,
        };
        evaluate(src, &ctx).expect("eval ok")
    }

    #[test]
    fn arithmetic_and_precedence() {
        let vars = BTreeMap::new();
        assert_eq!(eval_str("1 + 2 * 3", &vars, Value::Null), serde_json::json!(7));
        assert_eq!(eval_str("(1 + 2) * 3", &vars, Value::Null), serde_json::json!(9));
        assert_eq!(eval_str("10 % 3", &vars, Value::Null), serde_json::json!(1));
        // whole-number results encode as integers, not 7.0
        assert_eq!(eval_str("jsonEncode(3 + 4)", &vars, Value::Null), Value::String("7".to_string()));
    }

    #[test]
    fn string_concat_with_plus() {
        let vars = BTreeMap::new();
        assert_eq!(
            eval_str("\"a\" + \"b\" + 1", &vars, Value::Null),
            Value::String("ab1".to_string())
        );
    }

    #[test]
    fn variables_and_input() {
        let vars = ctx_with(vec![("counter", Value::from(5.0))], Value::Null);
        assert_eq!(eval_str("$counter + 1", &vars, Value::Null), serde_json::json!(6));
        let input = serde_json::json!({ "status_code": 200, "body": "ok" });
        assert_eq!(
            eval_str("input.status_code", &vars, input.clone()),
            Value::from(200)
        );
        assert_eq!(
            eval_str("input.body == \"ok\"", &vars, input),
            Value::Bool(true)
        );
    }

    #[test]
    fn ternary_and_logical() {
        let vars = ctx_with(vec![("x", Value::from(3.0))], Value::Null);
        assert_eq!(
            eval_str("$x > 2 ? \"big\" : \"small\"", &vars, Value::Null),
            Value::String("big".to_string())
        );
        assert_eq!(eval_str("true && false", &vars, Value::Null), Value::Bool(false));
        assert_eq!(eval_str("!false", &vars, Value::Null), Value::Bool(true));
    }

    #[test]
    fn json_functions() {
        let vars = ctx_with(
            vec![("data", serde_json::json!({ "a": 1, "b": [2, 3] }))],
            Value::Null,
        );
        assert_eq!(
            eval_str("jsonEncode($data)", &vars, Value::Null),
            Value::String("{\"a\":1,\"b\":[2,3]}".to_string())
        );
        assert_eq!(
            eval_str("jsonDecode(\"[1,2,3]\")[1]", &vars, Value::Null),
            Value::from(2)
        );
    }

    #[test]
    fn string_functions() {
        let vars = BTreeMap::new();
        assert_eq!(
            eval_str("upper(\"hi\")", &vars, Value::Null),
            Value::String("HI".to_string())
        );
        assert_eq!(eval_str("len(\"hello\")", &vars, Value::Null), serde_json::json!(5));
        assert_eq!(
            eval_str("replace(\"a-b-c\", \"-\", \":\")", &vars, Value::Null),
            Value::String("a:b:c".to_string())
        );
        assert_eq!(
            eval_str("split(\"a,b\", \",\")[0]", &vars, Value::Null),
            Value::String("a".to_string())
        );
    }

    #[test]
    fn decodes_and_navigates_nested_json() {
        // Mirrors the open-meteo "cache today's temperature" flow: the HTTP
        // body arrives as a string under input.body; jsonDecode + nested
        // member/index access pulls out the number.
        let vars = BTreeMap::new();
        let body = r#"{"daily":{"time":["2026-05-28"],"temperature_2m_max":[29.7]}}"#;
        let input = serde_json::json!({ "body": body });
        assert_eq!(
            eval_str(
                "jsonDecode(input.body).daily.temperature_2m_max[0]",
                &vars,
                input.clone(),
            ),
            serde_json::json!(29.7)
        );
        // …and the > comparison the IF block performs on it.
        assert_eq!(
            eval_str(
                "jsonDecode(input.body).daily.temperature_2m_max[0] > 15",
                &vars,
                input,
            ),
            Value::Bool(true)
        );
    }

    #[test]
    fn unknown_function_errors() {
        let vars = BTreeMap::new();
        let ctx = EvalContext {
            variables: &vars,
            input: Value::Null,
        };
        assert!(evaluate("bogus(1)", &ctx).is_err());
    }
}
