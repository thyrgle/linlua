//! The linlua parser: tokens to AST via recursive descent.
//!
//! Precedence follows Lua's reference ladder, tightest bind last:
//! `or`, `and`, comparisons, `|`, `~`, `&`, shifts, `..` (right),
//! `+ -`, `* / // %`, unary, `^` (right).
//!
//! Expression statements are restricted to calls and assignments, as
//! in Lua.

use crate::ast::*;
use crate::lexer::{Annotation, Tok, Token};

pub struct Parser {
    toks: Vec<Token>,
    pos: usize,
    annotations: Vec<(usize, Annotation)>,
}

/// A parse error: message plus absolute byte offset.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub offset: usize,
    pub message: String,
}

impl Parser {
    pub fn new(toks: Vec<Token>, annotations: Vec<(usize, Annotation)>) -> Self {
        Parser {
            toks,
            pos: 0,
            annotations,
        }
    }

    /// The annotation attached to the statement starting at
    /// `stmt_start`: a comment sitting between the previous token and
    /// the statement. One annotation applies to one statement.
    /// A type annotation: builtin names, or `{T}` for arrays.
    fn parse_type(&mut self) -> Result<TypeAnn, ParseError> {
        if self.eat(&Tok::LBrace) {
            let inner = self.parse_type()?;
            self.expect(&Tok::RBrace)?;
            return Ok(TypeAnn::Array(Box::new(inner)));
        }
        // `nil` is a keyword, not a name.
        if self.eat(&Tok::Nil) {
            return Ok(TypeAnn::Nil);
        }
        let name = self.expect_name()?;
        match name.as_str() {
            "nil" => Ok(TypeAnn::Nil),
            "number" => Ok(TypeAnn::Number),
            "string" => Ok(TypeAnn::String),
            "boolean" => Ok(TypeAnn::Boolean),
            "any" => Ok(TypeAnn::Any),
            other => Err(self.err(&format!(
                "unknown type `{other}` (v1 knows nil, number, string, boolean, any, {{T}})"
            ))),
        }
    }

    fn pending_annotation(&self, stmt_start: usize) -> Option<Annotation> {
        let prev_end = if self.pos == 0 {
            0
        } else {
            self.toks[self.pos - 1].end
        };
        self.annotations
            .iter()
            .rev()
            .find(|(end, _)| *end >= prev_end && *end <= stmt_start)
            .map(|(_, a)| *a)
    }

    pub fn parse_chunk(&mut self) -> Result<Chunk, ParseError> {
        let mut out = Vec::new();
        while !self.at_eof_marker() {
            out.push(self.statement()?);
        }
        Ok(out)
    }

    // ---- tokens ----

    fn at(&self, tok: &Tok) -> bool {
        self.toks.get(self.pos).map(|t| &t.tok) == Some(tok)
    }

    fn at_eof_marker(&self) -> bool {
        self.pos >= self.toks.len()
    }

    fn bump(&mut self) -> Token {
        match self.toks.get(self.pos) {
            Some(t) => {
                let t = t.clone();
                self.pos += 1;
                t
            }
            None => Token {
                tok: Tok::Nil,
                start: 0,
                end: 0,
            },
        }
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.at(tok) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: &Tok) -> Result<Token, ParseError> {
        if self.at(tok) {
            Ok(self.bump())
        } else {
            Err(self.err(&format!("expected {tok:?}, found {}", self.found())))
        }
    }

    fn found(&self) -> String {
        match self.toks.get(self.pos) {
            Some(t) => format!("{:?}", t.tok),
            None => "end of input".to_string(),
        }
    }

    fn err(&self, message: &str) -> ParseError {
        let offset = self
            .toks
            .get(self.pos)
            .map(|t| t.start)
            .unwrap_or_else(|| self.toks.last().map(|t| t.end).unwrap_or(0));
        ParseError {
            offset,
            message: message.to_string(),
        }
    }

    // ---- statements ----

    fn block(&mut self) -> Result<Vec<Stmt>, ParseError> {
        let mut out = Vec::new();
        loop {
            if self.at_eof_marker() {
                return Ok(out);
            }
            match self.toks.get(self.pos).map(|t| &t.tok) {
                Some(Tok::End) | Some(Tok::Else) | Some(Tok::Elseif) | Some(Tok::Until) => {
                    return Ok(out)
                }
                None => return Ok(out),
                _ => {}
            }
            let stmt = self.statement()?;
            let ret_like = matches!(stmt, Stmt::Return(_) | Stmt::Break);
            out.push(stmt);
            self.eat(&Tok::Semi);
            if ret_like {
                // `return`/`break` must end their block.
                return Ok(out);
            }
        }
    }

    fn statement(&mut self) -> Result<Stmt, ParseError> {
        if self.at_eof_marker() {
            return Err(self.err("unexpected end of input"));
        }
        match &self.toks[self.pos].tok {
            Tok::Semi => {
                self.pos += 1;
                Ok(Stmt::Do(Vec::new()))
            }
            Tok::DColon => {
                // `::name::` label — parse and drop (goto is not in
                // the v1 interpreter).
                self.pos += 1;
                self.expect_name()?;
                self.expect(&Tok::DColon)?;
                Ok(Stmt::Do(Vec::new()))
            }
            Tok::Goto => {
                self.pos += 1;
                self.expect_name()?;
                Ok(Stmt::Do(Vec::new()))
            }
            Tok::Local => {
                // The annotation (if any) sits between the previous
                // token and this statement — capture before advancing.
                let ann = self.pending_annotation(self.toks[self.pos].start);
                self.pos += 1;
                if self.eat(&Tok::Function) {
                    let name = self.expect_name()?;
                    let (params, ret, body) = self.fn_parts()?;
                    return Ok(Stmt::Fn {
                        name,
                        params,
                        ret,
                        body,
                        is_local: true,
                    });
                }
                let mut names = Vec::new();
                loop {
                    let name = self.expect_name()?;
                    let ty = if self.eat(&Tok::Colon) {
                        Some(self.parse_type()?)
                    } else {
                        None
                    };
                    names.push((name, ty));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                let mut inits = Vec::new();
                if self.eat(&Tok::Assign) {
                    inits.push(self.expr()?);
                    while self.eat(&Tok::Comma) {
                        inits.push(self.expr()?);
                    }
                }
                Ok(Stmt::Local { names, inits, ann })
            }
            Tok::If => self.if_stmt(),
            Tok::While => {
                self.pos += 1;
                let cond = self.expr()?;
                self.expect(&Tok::Do)?;
                let body = self.block()?;
                self.expect(&Tok::End)?;
                Ok(Stmt::While { cond, body })
            }
            Tok::Repeat => {
                self.pos += 1;
                let body = self.block()?;
                self.expect(&Tok::Until)?;
                let until = self.expr()?;
                Ok(Stmt::Repeat { body, until })
            }
            Tok::For => {
                self.pos += 1;
                let first = self.expect_name()?;
                if self.eat(&Tok::Assign) {
                    let start = self.expr()?;
                    self.expect(&Tok::Comma)?;
                    let limit = self.expr()?;
                    let step = if self.eat(&Tok::Comma) {
                        Some(self.expr()?)
                    } else {
                        None
                    };
                    self.expect(&Tok::Do)?;
                    let body = self.block()?;
                    self.expect(&Tok::End)?;
                    return Ok(Stmt::ForNum {
                        var: first,
                        start,
                        limit,
                        step,
                        body,
                    });
                }
                let mut vars = vec![first];
                while self.eat(&Tok::Comma) {
                    vars.push(self.expect_name()?);
                }
                self.expect(&Tok::In)?;
                let expr = self.expr()?;
                self.expect(&Tok::Do)?;
                let body = self.block()?;
                self.expect(&Tok::End)?;
                Ok(Stmt::ForIn { vars, expr, body })
            }
            Tok::Function => {
                self.pos += 1;
                let mut name = self.expect_name()?;
                let mut field: Option<String> = None;
                while self.at(&Tok::Dot) {
                    self.pos += 1;
                    let part = self.expect_name()?;
                    field = Some(match field {
                        None => part,
                        Some(prev) => format!("{prev}.{part}"),
                    });
                    name = match field.take() {
                        Some(f) => f,
                        None => unreachable!(),
                    };
                }
                if self.at(&Tok::Colon) {
                    return Err(self.err("method syntax `obj:m()` is not in the v1 dialect"));
                }
                let (params, ret, body) = self.fn_parts()?;
                if name.contains('.') {
                    return Err(self.err(
                        "dotted function names (`function t.f()`) are not in the v1 dialect",
                    ));
                }
                Ok(Stmt::Fn {
                    name,
                    params,
                    ret,
                    body,
                    is_local: false,
                })
            }
            Tok::Return => {
                self.pos += 1;
                // `return` with no value when the block ends here.
                match &self.toks[self.pos].tok {
                    Tok::End | Tok::Else | Tok::Elseif | Tok::Until => Ok(Stmt::Return(None)),
                    _ if self.at_eof_marker() => Ok(Stmt::Return(None)),
                    _ => Ok(Stmt::Return(Some(self.expr()?))),
                }
            }
            Tok::Break => {
                self.pos += 1;
                Ok(Stmt::Break)
            }
            Tok::Do => {
                self.pos += 1;
                let body = self.block()?;
                self.expect(&Tok::End)?;
                Ok(Stmt::Do(body))
            }
            _ => {
                // Expression statement: assignment or a call.
                let start_pos = self.pos;
                let e = self.suffixed_expr()?;
                match &e {
                    Expr::Call { .. } => Ok(Stmt::Expr(e)),
                    _ => {
                        if self.eat(&Tok::Assign) {
                            let value = self.expr()?;
                            let target = expr_to_target(e, start_pos)?;
                            Ok(Stmt::Assign { target, value })
                        } else {
                            Err(self.err("expression statements must be calls or assignments"))
                        }
                    }
                }
            }
        }
    }

    fn if_stmt(&mut self) -> Result<Stmt, ParseError> {
        self.pos += 1; // if
        let mut branches = Vec::new();
        let cond = self.expr()?;
        self.expect(&Tok::Then)?;
        let body = self.block()?;
        branches.push((cond, body));
        let mut otherwise = None;
        loop {
            if self.eat(&Tok::Elseif) {
                let cond = self.expr()?;
                self.expect(&Tok::Then)?;
                let body = self.block()?;
                branches.push((cond, body));
                continue;
            }
            if self.eat(&Tok::Else) {
                otherwise = Some(self.block()?);
            }
            self.expect(&Tok::End)?;
            return Ok(Stmt::If {
                branches,
                otherwise,
            });
        }
    }

    fn expect_name(&mut self) -> Result<String, ParseError> {
        match &self.toks[self.pos].tok {
            Tok::Name(n) => {
                let n = n.clone();
                self.pos += 1;
                Ok(n)
            }
            _ => Err(self.err(&format!("expected a name, found {}", self.found()))),
        }
    }

    fn fn_parts(&mut self) -> Result<FnParts, ParseError> {
        self.expect(&Tok::LParen)?;
        let mut params = Vec::new();
        if !self.at(&Tok::RParen) {
            loop {
                if self.at(&Tok::DDot) {
                    return Err(self.err("varargs `...` are not in the v1 dialect"));
                }
                let name = self.expect_name()?;
                let ty = if self.eat(&Tok::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                params.push((name, ty));
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(&Tok::RParen)?;
        let ret = if self.eat(&Tok::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let body = self.block()?;
        self.expect(&Tok::End)?;
        Ok((params, ret, body))
    }

    // ---- expressions ----

    pub fn expr(&mut self) -> Result<Expr, ParseError> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<Expr, ParseError> {
        let mut l = self.and_expr()?;
        while self.at(&Tok::Or) {
            self.pos += 1;
            let r = self.and_expr()?;
            l = Expr::Binary(BinOp::Or, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn and_expr(&mut self) -> Result<Expr, ParseError> {
        let mut l = self.cmp_expr()?;
        while self.at(&Tok::And) {
            self.pos += 1;
            let r = self.cmp_expr()?;
            l = Expr::Binary(BinOp::And, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn cmp_expr(&mut self) -> Result<Expr, ParseError> {
        let l = self.bor_expr()?;
        let op = match self.toks.get(self.pos).map(|t| &t.tok) {
            Some(Tok::Lt) => BinOp::Lt,
            Some(Tok::Gt) => BinOp::Gt,
            Some(Tok::Le) => BinOp::Le,
            Some(Tok::Ge) => BinOp::Ge,
            Some(Tok::NotEq) => BinOp::NotEq,
            Some(Tok::Eq) => BinOp::Eq,
            _ => return Ok(l),
        };
        self.pos += 1;
        let r = self.bor_expr()?;
        Ok(Expr::Binary(op, Box::new(l), Box::new(r)))
    }

    fn bor_expr(&mut self) -> Result<Expr, ParseError> {
        let mut l = self.bxor_expr()?;
        while self.at(&Tok::Pipe) {
            self.pos += 1;
            let r = self.bxor_expr()?;
            l = Expr::Binary(BinOp::Bor, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn bxor_expr(&mut self) -> Result<Expr, ParseError> {
        let mut l = self.band_expr()?;
        while self.at(&Tok::Tilde) {
            self.pos += 1;
            let r = self.band_expr()?;
            l = Expr::Binary(BinOp::BXor, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn band_expr(&mut self) -> Result<Expr, ParseError> {
        let mut l = self.shift_expr()?;
        while self.at(&Tok::Amp) {
            self.pos += 1;
            let r = self.shift_expr()?;
            l = Expr::Binary(BinOp::Band, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn shift_expr(&mut self) -> Result<Expr, ParseError> {
        let mut l = self.concat_expr()?;
        loop {
            let op = if self.at(&Tok::Shl) {
                BinOp::Shl
            } else if self.at(&Tok::Shr) {
                BinOp::Shr
            } else {
                break;
            };
            self.pos += 1;
            let r = self.concat_expr()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    /// `..` is right-associative.
    fn concat_expr(&mut self) -> Result<Expr, ParseError> {
        let l = self.add_expr()?;
        if self.at(&Tok::DDot) {
            self.pos += 1;
            let r = self.concat_expr()?;
            return Ok(Expr::Binary(BinOp::Concat, Box::new(l), Box::new(r)));
        }
        Ok(l)
    }

    fn add_expr(&mut self) -> Result<Expr, ParseError> {
        let mut l = self.mul_expr()?;
        loop {
            let op = if self.at(&Tok::Plus) {
                BinOp::Add
            } else if self.at(&Tok::Minus) {
                BinOp::Sub
            } else {
                break;
            };
            self.pos += 1;
            let r = self.mul_expr()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn mul_expr(&mut self) -> Result<Expr, ParseError> {
        let mut l = self.unary_expr()?;
        loop {
            let op = match self.toks.get(self.pos).map(|t| &t.tok) {
                Some(Tok::Star) => BinOp::Mul,
                Some(Tok::Slash) => BinOp::Div,
                Some(Tok::DSlash) => BinOp::IDiv,
                Some(Tok::Percent) => BinOp::Mod,
                _ => break,
            };
            self.pos += 1;
            let r = self.unary_expr()?;
            l = Expr::Binary(op, Box::new(l), Box::new(r));
        }
        Ok(l)
    }

    fn unary_expr(&mut self) -> Result<Expr, ParseError> {
        let op = match self.toks.get(self.pos).map(|t| &t.tok) {
            Some(Tok::Not) => UnOp::Not,
            Some(Tok::Minus) => UnOp::Neg,
            Some(Tok::Hash) => UnOp::Len,
            Some(Tok::Tilde) => UnOp::BNot,
            _ => return self.pow_expr(),
        };
        self.pos += 1;
        let e = self.unary_expr()?;
        Ok(Expr::Unary(op, Box::new(e)))
    }

    /// `^` is right-associative and binds tighter than unary on the
    /// right (`-2^2 == -(2^2)`), looser on the left.
    fn pow_expr(&mut self) -> Result<Expr, ParseError> {
        let base = self.suffixed_expr()?;
        if self.at(&Tok::Caret) {
            self.pos += 1;
            let exp = self.unary_expr()?;
            return Ok(Expr::Binary(BinOp::Pow, Box::new(base), Box::new(exp)));
        }
        Ok(base)
    }

    /// A primary expression plus call/index suffixes.
    fn suffixed_expr(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.primary_expr()?;
        loop {
            if self.at(&Tok::Dot) {
                self.pos += 1;
                let name = self.expect_name()?;
                e = Expr::Field {
                    obj: Box::new(e),
                    name,
                };
            } else if self.at(&Tok::LBracket) {
                self.pos += 1;
                let index = self.expr()?;
                self.expect(&Tok::RBracket)?;
                e = Expr::Index {
                    obj: Box::new(e),
                    index: Box::new(index),
                };
            } else if self.at(&Tok::LParen) {
                self.pos += 1;
                let mut args = Vec::new();
                if !self.at(&Tok::RParen) {
                    loop {
                        args.push(self.expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RParen)?;
                e = Expr::Call {
                    callee: Box::new(e),
                    args,
                };
            } else {
                return Ok(e);
            }
        }
    }

    fn primary_expr(&mut self) -> Result<Expr, ParseError> {
        match self.bump().tok {
            Tok::Nil => Ok(Expr::Nil),
            Tok::True => Ok(Expr::True),
            Tok::False => Ok(Expr::False),
            Tok::Num(n) => Ok(Expr::Num(n)),
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::DDot => Ok(Expr::Vararg),
            Tok::Name(n) => Ok(Expr::Ident(n)),
            Tok::Function => {
                let (params, ret, body) = self.fn_parts()?;
                Ok(Expr::Function { params, ret, body })
            }
            Tok::LBrace => self.table_expr(),
            Tok::LParen => {
                let e = self.expr()?;
                self.expect(&Tok::RParen)?;
                Ok(Expr::Paren(Box::new(e)))
            }
            other => Err(self.err(&format!("expected an expression, found {other:?}"))),
        }
    }

    fn table_expr(&mut self) -> Result<Expr, ParseError> {
        let mut fields = Vec::new();
        loop {
            if self.eat(&Tok::RBrace) {
                return Ok(Expr::Table(fields));
            }
            let field = if self.at(&Tok::LBracket) {
                self.pos += 1;
                let key = self.expr()?;
                self.expect(&Tok::RBracket)?;
                self.expect(&Tok::Assign)?;
                let value = self.expr()?;
                TableField::Keyed { key, value }
            } else if matches!(self.toks.get(self.pos).map(|t| &t.tok), Some(Tok::Name(_)))
                && matches!(
                    self.toks.get(self.pos + 1).map(|t| &t.tok),
                    Some(Tok::Assign)
                )
            {
                let name = self.expect_name()?;
                self.pos += 1; // =
                let value = self.expr()?;
                TableField::Keyed {
                    key: Expr::Str(name),
                    value,
                }
            } else {
                TableField::Item(self.expr()?)
            };
            fields.push(field);
            if !self.eat(&Tok::Comma) && !self.eat(&Tok::Semi) {
                self.expect(&Tok::RBrace)?;
                return Ok(Expr::Table(fields));
            }
        }
    }
}

/// The parts of a `function` header: typed params, optional return
/// annotation, body.
type FnParts = (Vec<(String, Option<TypeAnn>)>, Option<TypeAnn>, Vec<Stmt>);

/// A parsed `name.k` chain (or bare name) becomes an assignment
/// target.
fn expr_to_target(e: Expr, offset: usize) -> Result<Target, ParseError> {
    let err = |msg: &str| ParseError {
        offset,
        message: msg.to_string(),
    };
    match e {
        Expr::Ident(n) => Ok(Target::Name(n)),
        Expr::Field { obj, name } => Ok(Target::Field { obj: *obj, name }),
        Expr::Index { obj, index } => Ok(Target::Index {
            obj: *obj,
            index: *index,
        }),
        Expr::Paren(inner) => expr_to_target(*inner, offset),
        _ => Err(err("not an assignment target")),
    }
}

/// Parses a whole chunk or returns the first fatal error. Memory
/// annotations (`-- @own` / `-- @ref`) are recovered from comments and
/// attached to the declarations they precede.
pub fn parse_chunk(source: &str) -> Result<Chunk, ParseError> {
    let mut lexer = crate::lexer::Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| ParseError {
        offset: e.offset,
        message: e.message,
    })?;
    let annotations = std::mem::take(&mut lexer.annotations);
    if std::env::var("ANNDBG").is_ok() {
        eprintln!("annotations: {:?}", annotations);
    }
    Parser::new(tokens, annotations).parse_chunk()
}
