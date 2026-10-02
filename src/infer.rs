//! Ownership inference: unannotated sequence-table declarations that
//! provably never escape allocate into the arena, exactly as if they
//! carried `-- @own`.
//!
//! A declaration qualifies when, within its function body:
//! - the name is never returned (even as part of an expression),
//! - never aliased (`local b = a` / `b = a`),
//! - never stored into a container (`t[i] = a`, `{a}`),
//! - never captured by a closure (any use inside a nested function),
//! - never reassigned.
//!
//! Reads are always fine: element access, `#a`, iteration, and
//! passing to calls (calls borrow). Inference is output-invisible —
//! the differential suite runs inferred programs against `lua5.4`,
//! where the annotations do not exist at all.

use crate::ast::*;

#[derive(Debug, Default, Clone, Copy)]
pub struct Inference {
    pub own: usize,
    pub gc: usize,
}

/// Applies verdicts in place: qualifying declarations gain `-- @own`.
pub fn infer(chunk: &mut Chunk) -> Inference {
    process_body(chunk)
}

fn process_body(stmts: &mut [Stmt]) -> Inference {
    let mut result = Inference::default();

    // Candidates: unannotated single-name locals bound to a pure
    // sequence literal. Later redeclarations of a name replace the
    // earlier candidate.
    let mut candidates: Vec<(String, usize)> = Vec::new(); // (name, stmt index)
    for (i, stmt) in stmts.iter().enumerate() {
        if let Stmt::Local {
            names,
            inits,
            ann: None,
        } = stmt
        {
            if names.len() == 1 && inits.len() == 1 && is_sequence_literal(&inits[0]) {
                let (name0, _) = &names[0];
                candidates.retain(|(n, _)| n != name0);
                candidates.push((name0.clone(), i));
            }
        }
    }
    if candidates.is_empty() {
        // Still recurse into nested function bodies.
        for stmt in stmts.iter_mut() {
            recurse_nested(stmt, &mut result);
        }
        return result;
    }

    // Disqualify: any bare use, capture, alias, store, reassignment —
    // anywhere except the candidate's own declaration.
    let mut dead: Vec<usize> = Vec::new(); // indexes into candidates
    for (ci, (name, own_idx)) in candidates.iter().enumerate() {
        if body_disqualifies(stmts, name, *own_idx) {
            dead.push(ci);
        }
    }

    // Apply: surviving candidates gain @own.
    for (ci, (_, stmt_idx)) in candidates.iter().enumerate() {
        if dead.contains(&ci) {
            result.gc += 1;
            continue;
        }
        if let Stmt::Local { ann, .. } = &mut stmts[*stmt_idx] {
            *ann = Some(crate::lexer::Annotation::Own);
            result.own += 1;
        }
    }

    // Recurse into nested function bodies (their locals are their own).
    for stmt in stmts.iter_mut() {
        recurse_nested(stmt, &mut result);
    }
    result
}

fn recurse_nested(stmt: &mut Stmt, result: &mut Inference) {
    match stmt {
        Stmt::Fn { body, .. } => {
            let r = process_body(body);
            result.own += r.own;
            result.gc += r.gc;
        }
        Stmt::Local { inits, .. } => {
            for init in inits {
                if let Expr::Function { body, .. } = init {
                    let r = process_body(body);
                    result.own += r.own;
                    result.gc += r.gc;
                }
            }
        }
        _ => {}
    }
}

/// Whether the name's declaration must stay GC: any bare use (value
/// position), any capture inside a nested function, any reassignment.
/// Element reads, `#a`, call arguments, and iteration do not
/// disqualify.
fn body_disqualifies(stmts: &[Stmt], name: &str, skip: usize) -> bool {
    for (i, stmt) in stmts.iter().enumerate() {
        if i == skip {
            continue;
        }
        if stmt_disqualifies(stmt, name) {
            return true;
        }
    }
    false
}

fn stmt_disqualifies(stmt: &Stmt, name: &str) -> bool {
    match stmt {
        Stmt::Local {
            names,
            inits,
            ann: _,
        } => {
            // Re-shadowing the name disqualifies (conservative).
            if names.iter().any(|(n, _)| n == name) {
                return true;
            }
            inits
                .iter()
                .any(|e| expr_bare_use(e, name, false) || expr_captures(e, name))
        }
        Stmt::Assign { target, value } => {
            match target {
                Target::Name(n) if n == name => true, // reassigned
                // Aliasing: `b = a`.
                Target::Name(_) => expr_bare_use(value, name, false) || expr_captures(value, name),
                // Container store: `t[i] = a` (the target's object
                // subtree may read freely).
                Target::Field { obj, name: _ } => {
                    expr_captures(obj, name)
                        || expr_bare_use(value, name, false)
                        || expr_captures(value, name)
                }
                Target::Index { obj, index } => {
                    expr_captures(obj, name)
                        || expr_bare_use(index, name, false)
                        || expr_bare_use(value, name, false)
                        || expr_captures(value, name)
                }
            }
        }
        Stmt::Fn {
            name: fname,
            params,
            ret: _,
            body,
            is_local: _,
        } => {
            fname == name
                || params.iter().any(|(p, _)| p == name)
                || body.iter().any(|s| occurs_at_all(s, name))
        }
        Stmt::Expr(e) => expr_bare_use(e, name, false) || expr_captures(e, name),
        Stmt::If {
            branches,
            otherwise,
        } => {
            branches.iter().any(|(cond, body)| {
                expr_bare_use(cond, name, false)
                    || expr_captures(cond, name)
                    || body.iter().any(|s| stmt_disqualifies(s, name))
            }) || otherwise
                .as_ref()
                .map(|b| b.iter().any(|s| stmt_disqualifies(s, name)))
                .unwrap_or(false)
        }
        Stmt::While { cond, body } => {
            expr_bare_use(cond, name, false)
                || expr_captures(cond, name)
                || body.iter().any(|s| stmt_disqualifies(s, name))
        }
        Stmt::Repeat { body, until } => {
            expr_bare_use(until, name, false)
                || expr_captures(until, name)
                || body.iter().any(|s| stmt_disqualifies(s, name))
        }
        Stmt::ForNum {
            var,
            start,
            limit,
            step,
            body,
        } => {
            var == name
                || {
                    let bounds: Vec<&Expr> =
                        [start, limit].into_iter().chain(step.as_ref()).collect();
                    bounds
                        .iter()
                        .any(|e| expr_bare_use(e, name, false) || expr_captures(e, name))
                }
                || body.iter().any(|s| stmt_disqualifies(s, name))
        }
        Stmt::ForIn { vars, expr, body } => {
            vars.iter().any(|v| v == name)
                // Iterating the name itself is a read.
                || expr_captures(expr, name)
                || body.iter().any(|s| stmt_disqualifies(s, name))
        }
        Stmt::Return(Some(e)) => {
            // A bare return escapes; returning an element reads.
            expr_bare_use(e, name, false) || expr_captures(e, name)
        }
        Stmt::Return(None) | Stmt::Break => false,
        Stmt::Do(body) => body.iter().any(|s| stmt_disqualifies(s, name)),
    }
}

/// The name in a VALUE position: aliased, stored, computed on, or
/// returned bare. Element reads (`a[i]`, `a.k`), `#a`, call
/// arguments, and iteration are reads and pass.
fn expr_bare_use(e: &Expr, name: &str, in_function: bool) -> bool {
    if in_function {
        return expr_occurs(e, name);
    }
    match e {
        Expr::Ident(n) => n == name,
        // Reads through the name (`a[i]`, `a.k`) are fine; a bare use
        // of the name anywhere else in the subtree is not.
        Expr::Index { obj, index } => {
            if matches!(&**obj, Expr::Ident(n) if n == name) {
                expr_bare_use(index, name, false)
            } else {
                expr_bare_use(obj, name, false) || expr_bare_use(index, name, false)
            }
        }
        Expr::Field { obj, .. } => {
            !matches!(&**obj, Expr::Ident(_)) && expr_bare_use(obj, name, false)
        }
        // Passing the name to a call is a borrow (`f(a)` is fine);
        // computing on it inside an argument is not (`f(x + a)`).
        Expr::Call { callee, args } => {
            expr_captures(callee, name)
                || args.iter().any(|a| {
                    !matches!(a, Expr::Ident(n) if n == name) && expr_bare_use(a, name, false)
                })
        }
        Expr::Paren(inner) => expr_bare_use(inner, name, false),
        // `#a` is a read.
        Expr::Unary(UnOp::Len, inner) => {
            !matches!(&**inner, Expr::Ident(n) if n == name) && expr_bare_use(inner, name, false)
        }
        Expr::Unary(_, inner) => expr_bare_use(inner, name, false),
        Expr::Binary(_, l, r) => expr_bare_use(l, name, false) || expr_bare_use(r, name, false),
        Expr::Table(fields) => fields.iter().any(|f| match f {
            TableField::Item(item) => expr_bare_use(item, name, false),
            TableField::Keyed { key, value } => {
                expr_bare_use(key, name, false) || expr_bare_use(value, name, false)
            }
        }),
        Expr::Function { .. } => expr_occurs(e, name),
        _ => false,
    }
}

/// Any occurrence of the name in the expression tree.
fn expr_occurs(e: &Expr, name: &str) -> bool {
    let mut found = false;
    scan_expr(e, name, &mut found);
    found
}

/// Any occurrence inside a nested function body is a capture.
fn expr_captures(e: &Expr, name: &str) -> bool {
    match e {
        Expr::Function {
            params,
            ret: _,
            body,
        } => params.iter().any(|(p, _)| p == name) || body.iter().any(|s| occurs_at_all(s, name)),
        Expr::Paren(inner) => expr_captures(inner, name),
        Expr::Unary(_, inner) => expr_captures(inner, name),
        Expr::Binary(_, l, r) => expr_captures(l, name) || expr_captures(r, name),
        Expr::Call { callee, args } => {
            expr_captures(callee, name) || args.iter().any(|a| expr_captures(a, name))
        }
        Expr::Index { obj, index } => expr_captures(obj, name) || expr_captures(index, name),
        Expr::Field { obj, .. } => expr_captures(obj, name),
        Expr::Table(fields) => fields.iter().any(|f| match f {
            TableField::Item(item) => expr_captures(item, name),
            TableField::Keyed { key, value } => {
                expr_captures(key, name) || expr_captures(value, name)
            }
        }),
        _ => false,
    }
}

/// Any occurrence of the name at all.
fn occurs_at_all(stmt: &Stmt, name: &str) -> bool {
    let mut found = false;
    scan_stmt(stmt, name, &mut found);
    found
}

fn scan_stmt(stmt: &Stmt, name: &str, found: &mut bool) {
    if *found {
        return;
    }
    match stmt {
        Stmt::Local {
            names,
            inits,
            ann: _,
        } => {
            if names.iter().any(|(n, _)| n == name) {
                *found = true;
                return;
            }
            for e in inits {
                scan_expr(e, name, found);
            }
        }
        Stmt::Assign { target, value } => {
            match target {
                Target::Name(n) => {
                    if n == name {
                        *found = true;
                        return;
                    }
                }
                Target::Field { obj, .. } => scan_expr(obj, name, found),
                Target::Index { obj, index } => {
                    scan_expr(obj, name, found);
                    scan_expr(index, name, found);
                }
            }
            scan_expr(value, name, found);
        }
        Stmt::Fn {
            name: n,
            params,
            body,
            ..
        } => {
            if n == name || params.iter().any(|(p, _)| p == name) {
                *found = true;
                return;
            }
            for s in body {
                scan_stmt(s, name, found);
            }
        }
        Stmt::Expr(e) => scan_expr(e, name, found),
        Stmt::If {
            branches,
            otherwise,
        } => {
            for (cond, body) in branches {
                scan_expr(cond, name, found);
                for s in body {
                    scan_stmt(s, name, found);
                }
            }
            if let Some(b) = otherwise {
                for s in b {
                    scan_stmt(s, name, found);
                }
            }
        }
        Stmt::While { cond, body } => {
            scan_expr(cond, name, found);
            for s in body {
                scan_stmt(s, name, found);
            }
        }
        Stmt::Repeat { body, until } => {
            for s in body {
                scan_stmt(s, name, found);
            }
            scan_expr(until, name, found);
        }
        Stmt::ForNum {
            var,
            start,
            limit,
            step,
            body,
        } => {
            if var == name {
                *found = true;
                return;
            }
            let bounds: Vec<&Expr> = [start, limit].into_iter().chain(step.as_ref()).collect();
            for e in bounds {
                scan_expr(e, name, found);
            }
            for s in body {
                scan_stmt(s, name, found);
            }
        }
        Stmt::ForIn { vars, expr, body } => {
            if vars.iter().any(|v| v == name) {
                *found = true;
                return;
            }
            scan_expr(expr, name, found);
            for s in body {
                scan_stmt(s, name, found);
            }
        }
        Stmt::Return(Some(e)) => scan_expr(e, name, found),
        Stmt::Return(None) | Stmt::Break => {}
        Stmt::Do(body) => {
            for s in body {
                scan_stmt(s, name, found);
            }
        }
    }
}

fn scan_expr(e: &Expr, name: &str, found: &mut bool) {
    if *found {
        return;
    }
    match e {
        Expr::Ident(n) if n == name => *found = true,
        Expr::Paren(inner) | Expr::Unary(_, inner) => scan_expr(inner, name, found),
        Expr::Binary(_, l, r) => {
            scan_expr(l, name, found);
            scan_expr(r, name, found);
        }
        Expr::Call { callee, args } => {
            scan_expr(callee, name, found);
            for a in args {
                scan_expr(a, name, found);
            }
        }
        Expr::Index { obj, index } => {
            scan_expr(obj, name, found);
            scan_expr(index, name, found);
        }
        Expr::Field { obj, .. } => scan_expr(obj, name, found),
        Expr::Table(fields) => {
            for f in fields {
                match f {
                    TableField::Item(item) => scan_expr(item, name, found),
                    TableField::Keyed { key, value } => {
                        scan_expr(key, name, found);
                        scan_expr(value, name, found);
                    }
                }
            }
        }
        Expr::Function {
            params,
            ret: _,
            body,
        } => {
            if params.iter().any(|(p, _)| p == name) {
                *found = true;
                return;
            }
            for s in body {
                scan_stmt(s, name, found);
            }
        }
        _ => {}
    }
}

/// A pure sequence literal: `{e1, e2, ...}` with no keyed fields.
fn is_sequence_literal(e: &Expr) -> bool {
    match e {
        Expr::Table(fields) => fields.iter().all(|f| matches!(f, TableField::Item(_))),
        _ => false,
    }
}
