//! The linlua AST: the Luau-flavored subset the v1 interpreter runs.
//!
//! Statements end at block boundaries (no semicolons needed); the
//! parser tolerates them.

use crate::lexer::{Annotation, NumLit};

/// A type annotation: Luau-flavored. `nil`, `number`, `string`,
/// `boolean`, `any`, and `{T}` arrays. Checked statically, erased at
/// runtime — annotations never change what a program does.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeAnn {
    Nil,
    Number,
    String,
    Boolean,
    Any,
    Array(Box<TypeAnn>),
}

impl TypeAnn {
    /// The annotation's source name.
    pub fn name(&self) -> String {
        match self {
            TypeAnn::Nil => "nil".into(),
            TypeAnn::Number => "number".into(),
            TypeAnn::String => "string".into(),
            TypeAnn::Boolean => "boolean".into(),
            TypeAnn::Any => "any".into(),
            TypeAnn::Array(t) => format!("{{{}}}", t.name()),
        }
    }
}

/// A parsed source file: top-level statements (a chunk).
pub type Chunk = Vec<Stmt>;

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `local a: number = 1, b = 2` — values pair positionally;
    /// missing initializers are nil, extras drop. Each name may carry
    /// a Luau-style type annotation (checked statically, erased at
    /// runtime). A memory annotation (`-- @own` / `-- @ref` on the
    /// preceding comment line) opts the single-name form into the
    /// linear-memory layer.
    Local {
        names: Vec<(String, Option<TypeAnn>)>,
        inits: Vec<Expr>,
        ann: Option<Annotation>,
    },
    /// `name = expr` (globals and existing locals; fields and indices
    /// are in [Expr::Assign]-style suffix form).
    Assign {
        target: Target,
        value: Expr,
    },
    /// `function name(a: number): string body end` — kept as a
    /// statement so `local function` can bind before the body runs.
    /// Parameter and return annotations are erased at runtime.
    Fn {
        name: String,
        params: Vec<(String, Option<TypeAnn>)>,
        ret: Option<TypeAnn>,
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
        params: Vec<(String, Option<TypeAnn>)>,
        ret: Option<TypeAnn>,
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
