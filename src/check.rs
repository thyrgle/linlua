//! The static type checker: Luau-style annotations checked at
//! compile time, erased at runtime.
//!
//! Unannotated code is inferred where the initializer makes the type
//! obvious (literals, arithmetic, concat, calls to typed functions)
//! and treated as `any` elsewhere — `any` is compatible with
//! everything. Types never change what a program runs; they change
//! what mistakes it can catch.

use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::*;
use crate::lexer::NumLit;

/// One type diagnostic.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeError {
    pub subject: String,
    pub message: String,
}

impl TypeError {
    pub fn render(&self) -> String {
        format!("type error in `{}`: {}", self.subject, self.message)
    }
}

fn err(subject: impl Into<String>, message: impl Into<String>) -> TypeError {
    TypeError {
        subject: subject.into(),
        message: message.into(),
    }
}

/// The checker's type language. Structural enough for v1: `Any`
/// collapses unions.
#[derive(Debug, Clone, PartialEq)]
enum Ty {
    Nil,
    Number,
    String,
    Boolean,
    Any,
    Array(Box<Ty>),
}

impl Ty {
    fn from_ann(ann: &TypeAnn) -> Ty {
        match ann {
            TypeAnn::Nil => Ty::Nil,
            TypeAnn::Number => Ty::Number,
            TypeAnn::String => Ty::String,
            TypeAnn::Boolean => Ty::Boolean,
            TypeAnn::Any => Ty::Any,
            TypeAnn::Array(t) => Ty::Array(Box::new(Ty::from_ann(t))),
        }
    }

    fn name(&self) -> String {
        match self {
            Ty::Nil => "nil".into(),
            Ty::Number => "number".into(),
            Ty::String => "string".into(),
            Ty::Boolean => "boolean".into(),
            Ty::Any => "any".into(),
            Ty::Array(t) => format!("{{{}}}", t.name()),
        }
    }

    /// Assignability: `any` matches both ways; equality otherwise.
    fn accepts(&self, value: &Ty) -> bool {
        self == &Ty::Any || value == &Ty::Any || self == value
    }
}

/// One lexical scope: names to types, chained by shared parent.
#[derive(Debug, Default)]
struct Scope {
    vars: HashMap<String, Ty>,
    parent: Option<Rc<Scope>>,
}

impl Scope {
    fn get(&self, name: &str) -> Option<&Ty> {
        if let Some(t) = self.vars.get(name) {
            return Some(t);
        }
        self.parent.as_ref().and_then(|p| p.get(name))
    }

    fn declare(&mut self, name: String, ty: Ty) {
        self.vars.insert(name, ty);
    }

    fn child(&self) -> Scope {
        Scope {
            vars: HashMap::new(),
            parent: Some(Rc::new(Scope {
                vars: self.vars.clone(),
                parent: self.parent.clone(),
            })),
        }
    }
}

/// Checks a chunk; returns every type error.
pub fn check_program(chunk: &[Stmt]) -> Vec<TypeError> {
    let mut scope = Scope::default();
    // Builtin globals.
    scope.declare("print".into(), Ty::Any);
    scope.declare("tostring".into(), Ty::Any);
    scope.declare("type".into(), Ty::Any);
    scope.declare("tonumber".into(), Ty::Any);
    let mut math = Ty::Any;
    let _ = &mut math;
    scope.declare("math".into(), Ty::Any);
    let mut errors = Vec::new();
    check_block(chunk, &mut scope, &mut errors, None);
    errors
}

fn check_block(
    stmts: &[Stmt],
    scope: &mut Scope,
    errors: &mut Vec<TypeError>,
    fn_ret: Option<&Ty>,
) {
    for stmt in stmts {
        check_stmt(stmt, scope, errors, fn_ret);
    }
}

fn check_stmt(stmt: &Stmt, scope: &mut Scope, errors: &mut Vec<TypeError>, fn_ret: Option<&Ty>) {
    match stmt {
        Stmt::Local {
            names,
            inits,
            ann: _,
        } => {
            let checked: Vec<Ty> = inits.iter().map(|e| check_expr(e, scope, errors)).collect();
            for (i, (name, ann)) in names.iter().enumerate() {
                let declared = ann.as_ref().map(Ty::from_ann);
                let inferred = checked.get(i).cloned().unwrap_or(Ty::Nil);
                if let Some(want) = &declared {
                    if !want.accepts(&inferred) {
                        errors.push(err(
                            name.clone(),
                            format!("cannot assign {} to `{}`", inferred.name(), want.name()),
                        ));
                    }
                }
                // The declared (or inferred) type sticks to the name.
                scope.declare(name.clone(), declared.unwrap_or(inferred));
            }
        }
        Stmt::Assign { target, value } => {
            let vty = check_expr(value, scope, errors);
            match target {
                Target::Name(name) => {
                    if let Some(want) = scope.get(name).cloned() {
                        if !want.accepts(&vty) {
                            errors.push(err(
                                name.clone(),
                                format!("cannot assign {} to `{}`", vty.name(), want.name()),
                            ));
                        }
                    }
                }
                Target::Field { obj, .. } => {
                    check_expr(obj, scope, errors);
                }
                Target::Index { obj, index } => {
                    let oty = check_expr(obj, scope, errors);
                    let ity = check_expr(index, scope, errors);
                    // Numeric index into a typed array must be a number.
                    if let Ty::Array(elem) = &oty {
                        if !matches!(ity, Ty::Number | Ty::Any) {
                            errors.push(err(
                                format!("{}[...]", obj_subject(obj)),
                                format!("array index must be a number, found {}", ity.name()),
                            ));
                        }
                        // Storing a wrong element type into a typed array.
                        if let Target::Index { .. } = target {
                            if !elem.accepts(&vty) {
                                errors.push(err(
                                    format!("{}[...]", obj_subject(obj)),
                                    format!(
                                        "cannot store {} in an array of {}",
                                        vty.name(),
                                        elem.name()
                                    ),
                                ));
                            }
                        }
                    }
                }
            }
        }
        Stmt::Fn {
            name,
            params,
            ret,
            body,
            is_local: _,
        } => {
            // The function's own name binds first (recursion); its
            // value type is opaque for v1 (no arrow types yet).
            scope.declare(name.clone(), Ty::Any);
            let mut inner = scope.child();
            for (p, ann) in params {
                let ty = ann.as_ref().map(Ty::from_ann).unwrap_or(Ty::Any);
                inner.declare(p.clone(), ty);
            }
            let expected = ret.as_ref().map(Ty::from_ann);
            check_block(body, &mut inner, errors, expected.as_ref());
        }
        Stmt::Expr(e) => {
            check_expr(e, scope, errors);
        }
        Stmt::If {
            branches,
            otherwise,
        } => {
            for (cond, body) in branches {
                check_expr(cond, scope, errors);
                let mut inner = scope.child();
                check_block(body, &mut inner, errors, fn_ret);
            }
            if let Some(body) = otherwise {
                let mut inner = scope.child();
                check_block(body, &mut inner, errors, fn_ret);
            }
        }
        Stmt::While { cond, body } => {
            check_expr(cond, scope, errors);
            let mut inner = scope.child();
            check_block(body, &mut inner, errors, fn_ret);
        }
        Stmt::Repeat { body, until } => {
            let mut inner = scope.child();
            check_block(body, &mut inner, errors, fn_ret);
            check_expr(until, &mut inner, errors);
        }
        Stmt::ForNum {
            var,
            start,
            limit,
            step,
            body,
        } => {
            let bounds: Vec<&Expr> = [start, limit]
                .iter()
                .map(|e| &**e)
                .chain(step.as_ref())
                .collect();
            for e in bounds {
                let ty = check_expr(e, scope, errors);
                if !matches!(ty, Ty::Number | Ty::Any) {
                    errors.push(err(
                        "'for' bounds",
                        format!("loop bounds must be numbers, found {}", ty.name()),
                    ));
                }
            }
            let mut inner = scope.child();
            inner.declare(var.clone(), Ty::Number);
            check_block(body, &mut inner, errors, fn_ret);
        }
        Stmt::ForIn { vars, expr, body } => {
            let ity = check_expr(expr, scope, errors);
            let (kty, vty) = match &ity {
                Ty::Array(elem) => (Ty::Number, (**elem).clone()),
                _ => (Ty::Any, Ty::Any),
            };
            let mut inner = scope.child();
            if !vars.is_empty() {
                inner.declare(vars[0].clone(), kty);
            }
            if vars.len() > 1 {
                inner.declare(vars[1].clone(), vty);
            }
            for extra in &vars[2..] {
                inner.declare(extra.clone(), Ty::Nil);
            }
            check_block(body, &mut inner, errors, fn_ret);
        }
        Stmt::Return(Some(e)) => {
            let ty = check_expr(e, scope, errors);
            if let Some(want) = fn_ret {
                if !want.accepts(&ty) {
                    errors.push(err(
                        "return",
                        format!(
                            "cannot return {} from a function of {}",
                            ty.name(),
                            want.name()
                        ),
                    ));
                }
            }
        }
        Stmt::Return(None) => {
            if let Some(want) = fn_ret {
                if want != &Ty::Nil && want != &Ty::Any {
                    errors.push(err(
                        "return",
                        format!("a bare return in a function of {} returns nil", want.name()),
                    ));
                }
            }
        }
        Stmt::Break => {}
        Stmt::Do(body) => {
            let mut inner = scope.child();
            check_block(body, &mut inner, errors, fn_ret);
        }
    }
}

fn obj_subject(obj: &Expr) -> String {
    match obj {
        Expr::Ident(n) => n.clone(),
        _ => "...".into(),
    }
}

fn check_expr(e: &Expr, scope: &mut Scope, errors: &mut Vec<TypeError>) -> Ty {
    match e {
        Expr::Nil => Ty::Nil,
        Expr::True | Expr::False => Ty::Boolean,
        Expr::Num(_) => Ty::Number,
        Expr::Str(_) => Ty::String,
        Expr::Vararg => Ty::Any,
        Expr::Ident(name) => scope.get(name).cloned().unwrap_or(Ty::Any),
        Expr::Table(fields) => {
            let mut elem: Option<Ty> = None;
            let mut mixed = false;
            let mut has_keyed = false;
            for f in fields {
                match f {
                    TableField::Item(item) => {
                        let ty = check_expr(item, scope, errors);
                        elem = match elem {
                            None => Some(ty),
                            Some(prev) if prev == ty => Some(prev),
                            _ => {
                                mixed = true;
                                Some(Ty::Any)
                            }
                        };
                    }
                    TableField::Keyed { key, value } => {
                        check_expr(key, scope, errors);
                        check_expr(value, scope, errors);
                        has_keyed = true;
                    }
                }
            }
            // Keyed fields make it a dict — opaque for v1 (no
            // record types yet). Pure item sequences stay arrays:
            // mixed elements degrade to {any}.
            if has_keyed {
                Ty::Any
            } else {
                Ty::Array(Box::new(if mixed {
                    Ty::Any
                } else {
                    elem.unwrap_or(Ty::Any)
                }))
            }
        }
        Expr::Function { params, ret, body } => {
            let mut inner = scope.child();
            for (p, ann) in params {
                let ty = ann.as_ref().map(Ty::from_ann).unwrap_or(Ty::Any);
                inner.declare(p.clone(), ty);
            }
            let expected = ret.as_ref().map(Ty::from_ann);
            check_block(body, &mut inner, errors, expected.as_ref());
            ret.as_ref().map(Ty::from_ann).unwrap_or(Ty::Any)
        }
        Expr::Paren(inner) => check_expr(inner, scope, errors),
        Expr::Unary(op, inner) => {
            let ty = check_expr(inner, scope, errors);
            match op {
                UnOp::Not => Ty::Boolean,
                UnOp::Len => {
                    if matches!(ty, Ty::String | Ty::Array(_) | Ty::Any) {
                        Ty::Number
                    } else {
                        errors.push(err("#", format!("cannot take the length of {}", ty.name())));
                        Ty::Number
                    }
                }
                UnOp::Neg | UnOp::BNot => {
                    if !matches!(ty, Ty::Number | Ty::Any) {
                        errors.push(err("-", format!("cannot negate {}", ty.name())));
                    }
                    Ty::Number
                }
            }
        }
        Expr::Binary(op, l, r) => {
            let lt = check_expr(l, scope, errors);
            let rt = check_expr(r, scope, errors);
            match op {
                BinOp::Concat => {
                    for (side, ty) in [("<left>", &lt), ("<right>", &rt)] {
                        if !matches!(ty, Ty::String | Ty::Number | Ty::Any) {
                            errors.push(err(
                                "..",
                                format!("cannot concatenate {} ({})", ty.name(), side),
                            ));
                        }
                    }
                    Ty::String
                }
                BinOp::Add
                | BinOp::Sub
                | BinOp::Mul
                | BinOp::Div
                | BinOp::IDiv
                | BinOp::Mod
                | BinOp::Pow => {
                    for (side, ty) in [("<left>", &lt), ("<right>", &rt)] {
                        if !matches!(ty, Ty::Number | Ty::Any) {
                            errors.push(err(
                                arith_subject(op),
                                format!("cannot apply arithmetic to {} ({})", ty.name(), side),
                            ));
                        }
                    }
                    Ty::Number
                }
                BinOp::Eq | BinOp::NotEq => Ty::Boolean,
                BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                    // Same-family comparisons only: Lua runtime-errors
                    // on number-vs-string ordering.
                    let family = |t: &Ty| match t {
                        Ty::Number => Some("number"),
                        Ty::String => Some("string"),
                        _ => None,
                    };
                    let mixed = match (family(&lt), family(&rt)) {
                        (Some(a), Some(b)) => a != b,
                        _ => false,
                    };
                    let unordered = !matches!(lt, Ty::Number | Ty::String | Ty::Any)
                        || !matches!(rt, Ty::Number | Ty::String | Ty::Any);
                    if mixed || unordered {
                        errors.push(err(
                            "comparison",
                            format!("cannot compare {} with {}", lt.name(), rt.name()),
                        ));
                    }
                    Ty::Boolean
                }
                BinOp::And | BinOp::Or => {
                    if lt == rt {
                        lt
                    } else {
                        Ty::Any
                    }
                }
                BinOp::Band | BinOp::Bor | BinOp::BXor | BinOp::Shl | BinOp::Shr => {
                    for (side, ty) in [("<left>", &lt), ("<right>", &rt)] {
                        if !matches!(ty, Ty::Number | Ty::Any) {
                            errors.push(err(
                                "bitwise",
                                format!(
                                    "bitwise operands must be numbers ({}: {})",
                                    side,
                                    ty.name()
                                ),
                            ));
                        }
                    }
                    Ty::Number
                }
            }
        }
        Expr::Index { obj, index } => {
            let oty = check_expr(obj, scope, errors);
            let ity = check_expr(index, scope, errors);
            match &oty {
                Ty::Array(elem) => {
                    if !matches!(ity, Ty::Number | Ty::Any) {
                        errors.push(err(
                            format!("{}[...]", obj_subject(obj)),
                            format!("array index must be a number, found {}", ity.name()),
                        ));
                    }
                    (**elem).clone()
                }
                _ => Ty::Any,
            }
        }
        Expr::Field { obj, name } => {
            check_expr(obj, scope, errors);
            let _ = name;
            Ty::Any
        }
        Expr::Call { callee, args } => {
            // A call to a name with a declared return type gets it.
            let ret = match &**callee {
                Expr::Ident(name) => scope.get(name).cloned().unwrap_or(Ty::Any),
                _ => Ty::Any,
            };
            for a in args {
                check_expr(a, scope, errors);
            }
            ret
        }
    }
}

fn arith_subject(op: &BinOp) -> &'static str {
    match op {
        BinOp::Add => "'+'",
        BinOp::Sub => "'-'",
        BinOp::Mul => "'*'",
        BinOp::Div => "'/'",
        BinOp::IDiv => "'//'",
        BinOp::Mod => "'%'",
        BinOp::Pow => "'^'",
        _ => "arithmetic",
    }
}

#[allow(unused)]
fn unused(num: NumLit) {}
