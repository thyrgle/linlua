//! The linlua AST: the Luau-flavored subset the v1 interpreter runs.
//!
//! Statements end at block boundaries (no semicolons needed); the
//! parser tolerates them.

use crate::lexer::{Annotation, NumLit};

/// A parsed source file: top-level statements (a chunk).
pub type Chunk = Vec<Stmt>;

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `local a, b = e1, e2` — values pair positionally; missing
    /// initializers are nil, extras drop. An annotation (`-- @own` /
    /// `-- @ref` on the preceding comment line) opts the single-name
    /// form into the linear-memory layer. No types yet (M3 adds
    /// Luau-style annotations).
    Local {
        names: Vec<String>,
        inits: Vec<Expr>,
        ann: Option<Annotation>,
    },
    /// `name = expr` (globals and existing locals; fields and indices
    /// are in [Expr::Assign]-style suffix form).
    Assign {
        target: Target,
        value: Expr,
    },
    /// `function name(a, b) body end` — desugars to
    /// `name = function(a, b) body end` sugar-level; kept as a
    /// statement so `local function` can bind before the body runs.
    Fn {
        name: String,
        params: Vec<String>,
        body: Vec<Stmt>,
        is_local: bool,
    },
    Expr(Expr),
    If {
        branches: Vec<(Expr, Vec<Stmt>)>,
        otherwise: Option<Vec<Stmt>>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    Repeat {
        body: Vec<Stmt>,
        until: Expr,
    },
    /// `for i = start, limit[, step] do body end` — the numeric loop.
    /// start/limit/step evaluate once; `i` is fresh per iteration.
    ForNum {
        var: String,
        start: Expr,
        limit: Expr,
        step: Option<Expr>,
        body: Vec<Stmt>,
    },
    /// `for k[, v] in expr do body end` — the generic loop. v1
    /// iterates insertion-ordered pairs of a table (the `pairs` shape)
    /// or 1..#t (the `ipairs` shape), decided at runtime.
    ForIn {
        vars: Vec<String>,
        expr: Expr,
        body: Vec<Stmt>,
    },
    Return(Option<Expr>),
    Break,
    /// `do body end` — a bare block.
    Do(Vec<Stmt>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Name(String),
    /// `t.k`
    Field {
        obj: Expr,
        name: String,
    },
    /// `t[k]`
    Index {
        obj: Expr,
        index: Expr,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Nil,
    True,
    False,
    Num(NumLit),
    Str(String),
    /// `...` varargs — parsed, rejected by the v1 interpreter.
    Vararg,
    Ident(String),
    /// `{a, b, k = v, [expr] = v}` — array items keep order; fields
    /// and keyed entries land in the table too.
    Table(Vec<TableField>),
    Function {
        params: Vec<String>,
        body: Vec<Stmt>,
    },
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    Index {
        obj: Box<Expr>,
        index: Box<Expr>,
    },
    /// `t.k`
    Field {
        obj: Box<Expr>,
        name: String,
    },
    /// `(expr)` — grouping, kept so `(-2)^2` and `(f)(x)` behave.
    Paren(Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableField {
    Item(Expr),
    Keyed { key: Expr, value: Expr },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    Len,
    /// `~` bitwise not
    BNot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
    Concat,
    Eq,
    NotEq,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
    Band,
    Bor,
    BXor,
    Shl,
    Shr,
}
