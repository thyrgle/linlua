//! The linlua tree-walking interpreter: Lua 5.4 semantics on the v1
//! subset, including integer/float arithmetic subtyping, floored
//! division and modulo, insertion-ordered tables, and closures over
//! lexical environments.
//!
//! `print` is the output channel (tab-separated like Lua's), so the
//! differential harness compares this interpreter byte-for-byte with
//! a real `lua5.4`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::*;
use crate::lexer::Annotation;
use crate::mem::{Arenas, OwnHandle};
use crate::value::{Func, Key, Table, Value};

/// A runtime error, Lua-flavored.
#[derive(Debug, Clone, PartialEq)]
pub struct InterpError {
    pub message: String,
}

impl From<String> for InterpError {
    fn from(message: String) -> Self {
        InterpError { message }
    }
}

fn bail(message: impl Into<String>) -> InterpError {
    InterpError {
        message: message.into(),
    }
}

pub type R<T> = Result<T, InterpError>;

/// A lexical scope: names to values, chained to the enclosing scope.
#[derive(Debug)]
pub struct Env {
    pub vars: HashMap<String, Value>,
    pub parent: Option<Rc<RefCell<Env>>>,
}

impl Env {
    pub fn get(&self, name: &str) -> Option<Value> {
        if let Some(v) = self.vars.get(name) {
            return Some(v.clone());
        }
        self.parent.as_ref().and_then(|p| p.borrow().get(name))
    }

    pub fn assign(&mut self, name: &str, value: Value) -> bool {
        if self.vars.contains_key(name) {
            self.vars.insert(name.to_string(), value);
            return true;
        }
        if let Some(p) = &self.parent {
            return p.borrow_mut().assign(name, value);
        }
        false
    }
}

pub struct Interp<'o> {
    out: &'o mut dyn std::io::Write,
    /// Globals: plain assignment to an undeclared name.
    globals: Rc<RefCell<Table>>,
    /// Linear memory for `-- @own` tables.
    arenas: Rc<RefCell<Arenas>>,
}

/// Control flow out of a statement.
enum Flow {
    Normal,
    Break,
    Return(Value),
}

impl<'o> Interp<'o> {
    pub fn new(out: &'o mut dyn std::io::Write) -> Self {
        let mut math = Table::new();
        math.set(Key::Str("pi".into()), Value::Num(std::f64::consts::PI));
        math.set(Key::Str("huge".into()), Value::Num(f64::INFINITY));
        math.set(Key::Str("maxinteger".into()), Value::Int(i64::MAX));
        math.set(Key::Str("mininteger".into()), Value::Int(i64::MIN));
        let mut globals = Table::new();
        globals.set(
            Key::Str("math".into()),
            Value::Table(Rc::new(RefCell::new(math))),
        );
        Interp {
            out,
            globals: Rc::new(RefCell::new(globals)),
            arenas: Rc::new(RefCell::new(Arenas::default())),
        }
    }

    pub fn arenas(&self) -> Rc<RefCell<Arenas>> {
        Rc::clone(&self.arenas)
    }

    pub fn run(&mut self, chunk: &[Stmt]) -> R<()> {
        let env = Rc::new(RefCell::new(Env {
            vars: HashMap::new(),
            parent: None,
        }));
        match self.exec_block(chunk, &env)? {
            Flow::Return(_) => Ok(()),
            _ => Ok(()),
        }
    }

    // ---- statements ----

    fn exec_block(&mut self, stmts: &[Stmt], env: &Rc<RefCell<Env>>) -> R<Flow> {
        for stmt in stmts {
            match self.exec_stmt(stmt, env)? {
                Flow::Normal => {}
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    fn exec_stmt(&mut self, stmt: &Stmt, env: &Rc<RefCell<Env>>) -> R<Flow> {
        match stmt {
            Stmt::Local { names, inits, ann } => {
                // An annotation opts the single-name form into linear
                // memory; multi-name declarations stay dynamic.
                if ann.is_some() && names.len() != 1 {
                    return Err(bail("annotate one name at a time"));
                }
                match ann {
                    Some(Annotation::Own) => {
                        let name = &names[0];
                        let handle = self.alloc_own(&inits[0], env)?;
                        env.borrow_mut()
                            .vars
                            .insert(name.clone(), Value::Own(handle));
                        return Ok(Flow::Normal);
                    }
                    Some(Annotation::Ref) => {
                        let name = &names[0];
                        let v = self.eval(&inits[0], env)?;
                        env.borrow_mut()
                            .vars
                            .insert(name.clone(), Value::Ref(Box::new(v)));
                        return Ok(Flow::Normal);
                    }
                    None => {}
                }
                let values: Vec<Value> = inits
                    .iter()
                    .map(|e| self.eval(e, env))
                    .collect::<R<Vec<_>>>()?;
                // Aliasing an owned value is a move: the source
                // becomes a tombstone (before any binding borrows).
                let values: Vec<Value> = values
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| match (&v, inits.get(i)) {
                        (Value::Own(_), Some(Expr::Ident(src))) => {
                            let src = src.clone();
                            if names.get(i).map(|n| n == &src) != Some(true) {
                                env.borrow_mut().assign(&src, Value::Moved);
                            }
                            v
                        }
                        _ => v,
                    })
                    .collect();
                let mut scope = env.borrow_mut();
                for (i, name) in names.iter().enumerate() {
                    let v = values.get(i).cloned().unwrap_or(Value::Nil);
                    scope.vars.insert(name.clone(), v);
                }
                Ok(Flow::Normal)
            }
            Stmt::Assign { target, value } => {
                let v = self.eval(value, env)?;
                // Aliasing an owned value is a move: the source
                // becomes a tombstone.
                if let (Value::Own(_), Expr::Ident(src)) = (&v, value) {
                    if matches!(target, Target::Name(dst) if dst != src) {
                        env.borrow_mut().assign(src, Value::Moved);
                    }
                }
                self.assign(target, v, env)?;
                Ok(Flow::Normal)
            }
            Stmt::Fn {
                name,
                params,
                body,
                is_local,
            } => {
                let f = Value::Func(Rc::new(Func {
                    params: params.clone(),
                    body: Rc::new(body.clone()),
                    env: Rc::clone(env),
                }));
                if *is_local {
                    // `local function f` binds the name before the
                    // body runs — recursion works.
                    env.borrow_mut().vars.insert(name.clone(), f);
                } else {
                    self.globals.borrow_mut().set(Key::Str(name.clone()), f);
                }
                Ok(Flow::Normal)
            }
            Stmt::Expr(e) => {
                self.eval(e, env)?;
                Ok(Flow::Normal)
            }
            Stmt::If {
                branches,
                otherwise,
            } => {
                for (cond, body) in branches {
                    if self.eval(cond, env)?.truthy() {
                        let inner = self.child(env);
                        return self.exec_block(body, &inner);
                    }
                }
                if let Some(body) = otherwise {
                    let inner = self.child(env);
                    return self.exec_block(body, &inner);
                }
                Ok(Flow::Normal)
            }
            Stmt::While { cond, body } => {
                while self.eval(cond, env)?.truthy() {
                    let inner = self.child(env);
                    match self.exec_block(body, &inner)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Normal => {}
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::Repeat { body, until } => {
                loop {
                    // The condition sees the body's locals (Lua
                    // scoping) — share one scope.
                    match self.exec_block(body, env)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Normal => {}
                    }
                    if self.eval(until, env)?.truthy() {
                        break;
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::ForNum {
                var,
                start,
                limit,
                step,
                body,
            } => {
                let start = self.eval(start, env)?;
                let limit = self.eval(limit, env)?;
                let step = match step {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Int(1),
                };
                let has_float = matches!(start, Value::Num(_))
                    || matches!(limit, Value::Num(_))
                    || matches!(step, Value::Num(_));
                if has_float {
                    let mut i = start.as_num()?;
                    let limit = limit.as_num()?;
                    let step = step.as_num()?;
                    if step == 0.0 {
                        return Err(bail("'for' step is zero"));
                    }
                    while (step > 0.0 && i <= limit) || (step < 0.0 && i >= limit) {
                        let inner = self.child(env);
                        inner.borrow_mut().vars.insert(var.clone(), Value::Num(i));
                        match self.exec_block(body, &inner)? {
                            Flow::Break => break,
                            Flow::Return(v) => return Ok(Flow::Return(v)),
                            Flow::Normal => {}
                        }
                        i += step;
                    }
                } else {
                    let mut i = start.as_lua_int()?;
                    let limit = limit.as_lua_int()?;
                    let step = step.as_lua_int()?;
                    if step == 0 {
                        return Err(bail("'for' step is zero"));
                    }
                    while (step > 0 && i <= limit) || (step < 0 && i >= limit) {
                        let inner = self.child(env);
                        inner.borrow_mut().vars.insert(var.clone(), Value::Int(i));
                        match self.exec_block(body, &inner)? {
                            Flow::Break => break,
                            Flow::Return(v) => return Ok(Flow::Return(v)),
                            Flow::Normal => {}
                        }
                        i = i.wrapping_add(step);
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::ForIn { vars, expr, body } => {
                // v1 iterator protocol: `pairs(t)` / `ipairs(t)` (or a
                // bare table, linlua sugar). pairs walks insertion
                // order — for sequences that matches Lua's array-part
                // order; ipairs walks 1..#t.
                let (t, ipairs_only) = match expr {
                    Expr::Call { callee, args } => {
                        let fn_name = match &**callee {
                            Expr::Ident(n) => n.as_str(),
                            _ => "",
                        };
                        if fn_name != "pairs" && fn_name != "ipairs" {
                            return Err(bail(
                                "v1 `for..in` iterates pairs(t)/ipairs(t) or a bare table",
                            ));
                        }
                        if args.len() != 1 {
                            return Err(bail(format!("{fn_name} takes one argument")));
                        }
                        let t = match self.eval(&args[0], env)? {
                            Value::Table(t) => t,
                            Value::Own(h) => self.materialize(h),
                            other => {
                                return Err(bail(format!(
                                    "attempt to iterate over a {} value",
                                    other.type_name()
                                )))
                            }
                        };
                        (t, fn_name == "ipairs")
                    }
                    _ => match self.eval(expr, env)? {
                        Value::Table(t) => (t, false),
                        Value::Own(h) => (self.materialize(h), false),
                        other => {
                            return Err(bail(format!(
                                "attempt to iterate over a {} value",
                                other.type_name()
                            )))
                        }
                    },
                };
                let entries: Vec<(Key, Value)> = t.borrow().entries.clone();
                let entries: Vec<(Key, Value)> = if ipairs_only {
                    entries
                        .into_iter()
                        .filter(|(k, _)| matches!(k, Key::Int(i) if *i >= 1 && *i <= t.borrow().border()))
                        .collect()
                } else {
                    entries
                };
                for (k, v) in entries {
                    let inner = self.child(env);
                    {
                        let mut scope = inner.borrow_mut();
                        match vars.len() {
                            0 => {}
                            1 => {
                                scope.vars.insert(vars[0].clone(), key_value(&k));
                            }
                            _ => {
                                scope.vars.insert(vars[0].clone(), key_value(&k));
                                scope.vars.insert(vars[1].clone(), v);
                                for extra in &vars[2..] {
                                    scope.vars.insert(extra.clone(), Value::Nil);
                                }
                            }
                        }
                    }
                    match self.exec_block(body, &inner)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Normal => {}
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::Return(e) => {
                let value = match e {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Nil,
                };
                // Ownership cannot escape: an owned value returned
                // from its scope has no owner left behind.
                if matches!(value, Value::Own(_)) {
                    return Err(bail("an @own value cannot escape via return"));
                }
                Ok(Flow::Return(value))
            }
            Stmt::Break => Ok(Flow::Break),
            Stmt::Do(body) => {
                let inner = self.child(env);
                self.exec_block(body, &inner)
            }
        }
    }

    fn child(&self, env: &Rc<RefCell<Env>>) -> Rc<RefCell<Env>> {
        Rc::new(RefCell::new(Env {
            vars: HashMap::new(),
            parent: Some(Rc::clone(env)),
        }))
    }

    fn assign(&mut self, target: &Target, value: Value, env: &Rc<RefCell<Env>>) -> R<()> {
        match target {
            Target::Name(name) => {
                let mut e = env.borrow_mut();
                if e.assign(name, value.clone()) {
                    return Ok(());
                }
                drop(e);
                self.globals.borrow_mut().set(Key::Str(name.clone()), value);
                Ok(())
            }
            Target::Field { obj, name } => {
                let o = self.eval(obj, env)?;
                self.table_set(o, &Key::Str(name.clone()), value)
            }
            Target::Index { obj, index } => {
                let o = self.eval(obj, env)?;
                let i = self.eval(index, env)?;
                let key = i
                    .as_key()
                    .ok_or_else(|| bail("table index is nil or unsupported"))?;
                self.table_set(o, &key, value)
            }
        }
    }

    fn table_set(&mut self, obj: Value, key: &Key, value: Value) -> R<()> {
        // Writes through a borrowed reference fail loudly — before
        // the reference is peeled.
        if matches!(obj, Value::Ref(_)) {
            return Err(bail("attempt to write through a borrowed reference"));
        }
        // Storing an owned value anywhere is an ownership violation:
        // GC containers would hide the owner, owned containers would
        // nest ownership (depth-one).
        if let Value::Own(_) = value {
            return Err(bail(
                "an @own value cannot be stored in a container (it must stay bound to its name)",
            ));
        }
        match obj.unref() {
            Value::Table(t) => {
                t.borrow_mut().set(key.clone(), value);
                Ok(())
            }
            Value::Own(h) => match key {
                Key::Int(i) => self.arenas.borrow_mut().set(*h, *i, value).map_err(bail),
                Key::Str(k) => Err(bail(format!("an @own table is a sequence (no key `{k}`)"))),
            },
            other => Err(bail(format!(
                "attempt to index a {} value",
                other.type_name()
            ))),
        }
    }

    /// Allocates a table literal into the arena (`-- @own`): the
    /// initializer must be a pure sequence of items.
    fn alloc_own(&mut self, init: &Expr, env: &Rc<RefCell<Env>>) -> R<OwnHandle> {
        let fields = match init {
            Expr::Table(fields) => fields,
            _ => return Err(bail("-- @own requires a table literal initializer")),
        };
        let mut items = Vec::new();
        for f in fields {
            match f {
                TableField::Item(e) => {
                    let v = self.eval(e, env)?;
                    if let Value::Own(_) = v {
                        return Err(bail("an @own value cannot nest in another @own value"));
                    }
                    items.push(v);
                }
                TableField::Keyed { .. } => {
                    return Err(bail(
                        "an @own table must be a pure sequence (no keyed fields)",
                    ));
                }
            }
        }
        Ok(self.arenas.borrow_mut().alloc(items))
    }

    // ---- expressions ----

    fn eval(&mut self, expr: &Expr, env: &Rc<RefCell<Env>>) -> R<Value> {
        match expr {
            Expr::Nil => Ok(Value::Nil),
            Expr::True => Ok(Value::Bool(true)),
            Expr::False => Ok(Value::Bool(false)),
            Expr::Num(n) => Ok(match n {
                crate::lexer::NumLit::Int(i) => Value::Int(*i),
                crate::lexer::NumLit::Float(f) => Value::Num(*f),
            }),
            Expr::Str(s) => Ok(Value::Str(Rc::new(s.clone()))),
            Expr::Vararg => Err(bail("varargs are not in the v1 dialect")),
            Expr::Ident(name) => {
                if let Some(v) = env.borrow().get(name) {
                    if matches!(v, Value::Moved) {
                        return Err(bail(format!("use of a moved value (`{name}`)")));
                    }
                    return Ok(v);
                }
                Ok(self.globals.borrow().get(&Key::Str(name.clone())))
            }
            Expr::Table(fields) => {
                let t = Rc::new(RefCell::new(Table::new()));
                let mut next = 1i64;
                for f in fields {
                    match f {
                        TableField::Item(e) => {
                            let v = self.eval(e, env)?;
                            if !matches!(v, Value::Nil) {
                                t.borrow_mut().set(Key::Int(next), v);
                            }
                            next += 1;
                        }
                        TableField::Keyed { key, value } => {
                            let k = self.eval(key, env)?;
                            let v = self.eval(value, env)?;
                            let key = k
                                .as_key()
                                .ok_or_else(|| bail("table index is nil or unsupported"))?;
                            t.borrow_mut().set(key, v);
                        }
                    }
                }
                Ok(Value::Table(t))
            }
            Expr::Function { params, body } => Ok(Value::Func(Rc::new(Func {
                params: params.clone(),
                body: Rc::new(body.clone()),
                env: Rc::clone(env),
            }))),
            Expr::Paren(e) => self.eval(e, env),
            Expr::Unary(op, e) => {
                let v = self.eval(e, env)?;
                self.unary(*op, v)
            }
            Expr::Binary(op, l, r) => match op {
                BinOp::And => {
                    let l = self.eval(l, env)?;
                    if l.truthy() {
                        self.eval(r, env)
                    } else {
                        Ok(l)
                    }
                }
                BinOp::Or => {
                    let l = self.eval(l, env)?;
                    if l.truthy() {
                        Ok(l)
                    } else {
                        self.eval(r, env)
                    }
                }
                _ => {
                    let l = self.eval(l, env)?;
                    let r = self.eval(r, env)?;
                    self.binary(*op, l, r)
                }
            },
            Expr::Index { obj, index } => {
                let o = self.eval(obj, env)?;
                let i = self.eval(index, env)?;
                let key = i
                    .as_key()
                    .ok_or_else(|| bail("table index is nil or unsupported"))?;
                self.index(o, &key)
            }
            Expr::Field { obj, name } => {
                let o = self.eval(obj, env)?;
                self.index(o, &Key::Str(name.clone()))
            }
            // (Own/Ref handling lives in `index`.)
            Expr::Call { callee, args } => {
                // Builtins dispatch by name.
                if let Expr::Ident(n) = &**callee {
                    if n == "print" || n == "tostring" || n == "type" || n == "tonumber" {
                        let mut vals = Vec::new();
                        for a in args {
                            vals.push(self.eval(a, env)?);
                        }
                        return self.builtin(n, vals);
                    }
                }
                let f = self.eval(callee, env)?;
                let mut vals = Vec::new();
                for a in args {
                    let v = self.eval(a, env)?;
                    // Passing an owned value borrows it: the callee
                    // reads through a reference, and any write through
                    // that reference fails loudly.
                    vals.push(match v {
                        Value::Own(_) => Value::Ref(Box::new(v)),
                        other => other,
                    });
                }
                self.call(f, vals)
            }
        }
    }

    /// The v1 builtin library: `print` (tab-separated, like Lua),
    /// `tostring`, `type`, and `tonumber`.
    fn builtin(&mut self, name: &str, vals: Vec<Value>) -> R<Value> {
        match name {
            "print" => {
                let line = vals
                    .iter()
                    .map(|v| v.tostring())
                    .collect::<Vec<_>>()
                    .join("\t");
                writeln!(self.out, "{line}").map_err(|_| bail("write failed"))?;
                Ok(Value::Nil)
            }
            "tostring" => {
                let v = vals.first().cloned().unwrap_or(Value::Nil);
                Ok(Value::Str(Rc::new(v.tostring())))
            }
            "type" => {
                let v = vals.first().cloned().unwrap_or(Value::Nil);
                Ok(Value::Str(Rc::new(v.type_name().to_string())))
            }
            "tonumber" => {
                let v = vals.first().cloned().unwrap_or(Value::Nil);
                match v {
                    Value::Int(_) | Value::Num(_) => Ok(v),
                    Value::Str(s) => {
                        let t = s.trim();
                        if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                            if let Ok(i) = i64::from_str_radix(hex, 16) {
                                return Ok(Value::Int(i));
                            }
                        }
                        t.parse::<i64>()
                            .map(Value::Int)
                            .or_else(|_| t.parse::<f64>().map(Value::Num))
                            .map_err(|_| bail("passed string is not a valid number"))
                            .or(Ok(Value::Nil))
                    }
                    _ => Ok(Value::Nil),
                }
            }
            _ => Err(bail(format!("unknown builtin `{name}`"))),
        }
    }

    /// Copies an owned sequence into a fresh insertion-ordered table
    /// for iteration (1-based keys, arena order).
    fn materialize(&mut self, h: OwnHandle) -> Rc<RefCell<Table>> {
        let items = self.arenas.borrow().items(h);
        let t = Rc::new(RefCell::new(Table::new()));
        for (i, v) in items.into_iter().enumerate() {
            t.borrow_mut().set(Key::Int(i as i64 + 1), v);
        }
        t
    }

    fn index(&mut self, obj: Value, key: &Key) -> R<Value> {
        match obj.unref() {
            Value::Table(t) => Ok(t.borrow().get(key)),
            Value::Own(h) => match key {
                Key::Int(i) => Ok(self.arenas.borrow().get(*h, *i)),
                Key::Str(k) => Err(bail(format!("an @own table is a sequence (no key `{k}`)"))),
            },
            other => Err(bail(format!(
                "attempt to index a {} value",
                other.type_name()
            ))),
        }
    }

    fn call(&mut self, f: Value, args: Vec<Value>) -> R<Value> {
        let func = match &f {
            Value::Func(func) => Rc::clone(func),
            other => {
                return Err(bail(format!(
                    "attempt to call a {} value",
                    other.type_name()
                )))
            }
        };
        let inner = Rc::new(RefCell::new(Env {
            vars: HashMap::new(),
            parent: Some(Rc::clone(&func.env)),
        }));
        for (i, p) in func.params.iter().enumerate() {
            let v = args.get(i).cloned().unwrap_or(Value::Nil);
            inner.borrow_mut().vars.insert(p.clone(), v);
        }
        match self.exec_block(&func.body, &inner)? {
            Flow::Return(v) => Ok(v),
            _ => Ok(Value::Nil),
        }
    }

    fn unary(&mut self, op: UnOp, v: Value) -> R<Value> {
        match op {
            UnOp::Not => Ok(Value::Bool(!v.truthy())),
            UnOp::Neg => match v {
                Value::Int(i) => Ok(Value::Int(i.wrapping_neg())),
                Value::Num(f) => Ok(Value::Num(-f)),
                other => Err(bail(format!(
                    "attempt to perform arithmetic on a {} value",
                    other.type_name()
                ))),
            },
            UnOp::Len => match v.unref() {
                Value::Str(s) => Ok(Value::Int(s.len() as i64)),
                Value::Table(t) => Ok(Value::Int(t.borrow().border())),
                Value::Own(h) => Ok(Value::Int(self.arenas.borrow().len(*h) as i64)),
                other => Err(bail(format!(
                    "attempt to get length of a {} value",
                    other.type_name()
                ))),
            },
            UnOp::BNot => {
                let i = v.as_lua_int()?;
                Ok(Value::Int(!i))
            }
        }
    }

    fn binary(&mut self, op: BinOp, l: Value, r: Value) -> R<Value> {
        use BinOp::*;
        match op {
            Eq => Ok(Value::Bool(l.lua_eq(&r))),
            NotEq => Ok(Value::Bool(!l.lua_eq(&r))),
            Lt | Gt | Le | Ge => self.compare(op, &l, &r),
            Concat => {
                let ls = self.tostring_op(&l)?;
                let rs = self.tostring_op(&r)?;
                Ok(Value::Str(Rc::new(format!("{ls}{rs}"))))
            }
            Add | Sub | Mul | Div | IDiv | Mod | Pow => self.arith(op, &l, &r),
            Band | Bor | BXor | Shl | Shr => {
                let a = l.as_lua_int()?;
                let b = r.as_lua_int()?;
                Ok(Value::Int(match op {
                    Band => a & b,
                    Bor => a | b,
                    BXor => a ^ b,
                    Shl => a.wrapping_shl(b as u32),
                    Shr => a.wrapping_shr(b as u32),
                    _ => unreachable!(),
                }))
            }
            And | Or => unreachable!("short-circuited in eval"),
        }
    }

    fn tostring_op(&self, v: &Value) -> R<String> {
        match v.unref() {
            Value::Nil | Value::Bool(_) | Value::Table(_) | Value::Func(_) | Value::Own(_) => Err(
                bail(format!("attempt to concatenate a {} value", v.type_name())),
            ),
            Value::Int(_) | Value::Num(_) | Value::Str(_) => Ok(v.tostring()),
            Value::Moved => Err(bail("attempt to concatenate a moved value")),
            // unref() already peeled references.
            Value::Ref(_) => unreachable!(),
        }
    }

    fn compare(&mut self, op: BinOp, l: &Value, r: &Value) -> R<Value> {
        // Numbers compare across subtypes; strings bytewise; mixing
        // number and string is an error, like Lua.
        let ord = match (l, r) {
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Int(a), Value::Num(b)) => (*a as f64)
                .partial_cmp(b)
                .ok_or_else(|| bail("cannot compare NaN"))?,
            (Value::Num(a), Value::Int(b)) => a
                .partial_cmp(&(*b as f64))
                .ok_or_else(|| bail("cannot compare NaN"))?,
            (Value::Num(a), Value::Num(b)) => {
                a.partial_cmp(b).ok_or_else(|| bail("cannot compare NaN"))?
            }
            (Value::Str(a), Value::Str(b)) => a.as_str().cmp(b.as_str()),
            _ => {
                return Err(bail(format!(
                    "attempt to compare {} with {}",
                    l.type_name(),
                    r.type_name()
                )))
            }
        };
        use std::cmp::Ordering::*;
        Ok(Value::Bool(match op {
            BinOp::Lt => ord == Less,
            BinOp::Gt => ord == Greater,
            BinOp::Le => ord != Greater,
            BinOp::Ge => ord != Less,
            _ => unreachable!(),
        }))
    }

    /// Lua 5.4 arithmetic: int×int stays int (wrapping) except `/`
    /// (always float) and `^` (always float); any float makes the
    /// result float. `%` is floored.
    fn arith(&mut self, op: BinOp, l: &Value, r: &Value) -> R<Value> {
        // Integer fast path.
        let li = matches!(l, Value::Int(_));
        let ri = matches!(r, Value::Int(_));
        if li && ri && !matches!(op, BinOp::Div | BinOp::Pow) {
            let a = match l {
                Value::Int(i) => *i,
                _ => unreachable!(),
            };
            let b = match r {
                Value::Int(i) => *i,
                _ => unreachable!(),
            };
            return Ok(Value::Int(match op {
                BinOp::Add => a.wrapping_add(b),
                BinOp::Sub => a.wrapping_sub(b),
                BinOp::Mul => a.wrapping_mul(b),
                BinOp::IDiv => {
                    if b == 0 {
                        return Err(bail("attempt to perform 'n//0'"));
                    }
                    // Floor division with wrap-around edges, like Lua.
                    if a == i64::MIN && b == -1 {
                        i64::MIN
                    } else {
                        let q = a / b;
                        let r = a % b;
                        if r != 0 && ((r < 0) != (b < 0)) {
                            q - 1
                        } else {
                            q
                        }
                    }
                }
                BinOp::Mod => {
                    if b == 0 {
                        return Err(bail("attempt to perform 'n%%0'"));
                    }
                    if a == i64::MIN && b == -1 {
                        0
                    } else {
                        // Floored: the result takes the divisor's sign.
                        let r = a % b;
                        if r != 0 && ((r < 0) != (b < 0)) {
                            r + b
                        } else {
                            r
                        }
                    }
                }
                _ => unreachable!(),
            }));
        }

        let a = l.as_num()?;
        let b = r.as_num()?;
        match op {
            BinOp::Div => Ok(Value::Num(a / b)),
            BinOp::Pow => Ok(Value::Num(a.powf(b))),
            BinOp::IDiv => Ok(Value::Num((a / b).floor())),
            BinOp::Mod => Ok(Value::Num(a - (a / b).floor() * b)),
            BinOp::Add => Ok(Value::Num(a + b)),
            BinOp::Sub => Ok(Value::Num(a - b)),
            BinOp::Mul => Ok(Value::Num(a * b)),
            _ => unreachable!(),
        }
    }
}

/// The value bound to the loop key variable in `for k, v in t`.
fn key_value(k: &Key) -> Value {
    match k {
        Key::Int(i) => Value::Int(*i),
        Key::Str(s) => Value::Str(Rc::new(s.clone())),
    }
}
