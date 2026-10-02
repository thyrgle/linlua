//! Lua values with 5.4 semantics: integers and floats are distinct
//! subtypes of number, tables keep insertion order, and only `nil`
//! and `false` are falsy.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::Stmt;
use crate::mem::OwnHandle;

/// A table key: integers (including float keys with an exact integer
/// value, normalized like Lua) and strings. Other key types are not
/// in the v1 dialect.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Int(i64),
    Str(String),
}

#[derive(Debug)]
pub struct Table {
    pub entries: Vec<(Key, Value)>,
}

impl Table {
    pub fn new() -> Self {
        Table {
            entries: Vec::new(),
        }
    }

    pub fn get(&self, key: &Key) -> Value {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or(Value::Nil)
    }

    pub fn set(&mut self, key: Key, value: Value) {
        if matches!(value, Value::Nil) {
            self.entries.retain(|(k, _)| k != &key);
            return;
        }
        for (k, v) in self.entries.iter_mut() {
            if k == &key {
                *v = value;
                return;
            }
        }
        self.entries.push((key, value));
    }

    /// The `#` border: n such that t[n] ~= nil and t[n + 1] == nil,
    /// counting up from 1 (the sequence case — all fixtures use it).
    pub fn border(&self) -> i64 {
        let mut n = 1i64;
        while !matches!(self.get(&Key::Int(n)), Value::Nil) {
            n += 1;
        }
        n - 1
    }
}

impl Default for Table {
    fn default() -> Self {
        Self::new()
    }
}

/// A function value: AST body plus the captured environment.
#[derive(Debug)]
pub struct Func {
    pub params: Vec<String>,
    pub body: Rc<Vec<Stmt>>,
    pub env: Rc<RefCell<crate::interp::Env>>,
}

#[derive(Debug, Clone)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    Num(f64),
    Str(Rc<String>),
    Table(Rc<RefCell<Table>>),
    /// An `-- @own` sequence in the linear-memory arena. Single
    /// owner: moves invalidate the source, references are read-only,
    /// and it cannot escape its scope.
    Own(OwnHandle),
    /// A read-only view of another value (`-- @ref`). Reads pass
    /// through; every write through a reference is an error — even
    /// after the reference is copied.
    Ref(Box<Value>),
    /// The tombstone left behind when an owned value moves.
    Moved,
    Func(Rc<Func>),
}

impl Value {
    /// Lua truthiness: only `nil` and `false` are falsy — `0` is
    /// truthy, unlike JavaScript.
    pub fn truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }

    /// The value as a table key, normalizing exact float keys.
    pub fn as_key(&self) -> Option<Key> {
        match self {
            Value::Int(i) => Some(Key::Int(*i)),
            Value::Num(f) if f.fract() == 0.0 && f.abs() < 9.3e18 => Some(Key::Int(*f as i64)),
            Value::Str(s) => Some(Key::Str((**s).clone())),
            _ => None,
        }
    }

    /// The integer value for bitwise ops and integer contexts: ints
    /// pass through; floats must have an exact integer representation
    /// ("number has no integer representation" otherwise, like Lua).
    pub fn as_lua_int(&self) -> Result<i64, String> {
        match self {
            Value::Int(i) => Ok(*i),
            Value::Num(f)
                if f.fract() == 0.0
                    && *f >= -9.223372036854776e18
                    && *f <= 9.223372036854776e18 =>
            {
                Ok(*f as i64)
            }
            other => Err(format!(
                "number has no integer representation ({})",
                other.tostring()
            )),
        }
    }

    pub fn as_num(&self) -> Result<f64, String> {
        match self {
            Value::Int(i) => Ok(*i as f64),
            Value::Num(f) => Ok(*f),
            other => Err(format!(
                "attempt to perform arithmetic on a {} value",
                other.type_name()
            )),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "boolean",
            Value::Int(_) | Value::Num(_) => "number",
            Value::Str(_) => "string",
            Value::Table(_) => "table",
            Value::Own(_) => "table",
            Value::Ref(inner) => inner.type_name(),
            Value::Moved => "moved value",
            Value::Func(_) => "function",
        }
    }

    /// Peels read-only references; the interpreter works on the
    /// underlying value after this.
    pub fn unref(&self) -> &Value {
        match self {
            Value::Ref(inner) => inner.unref(),
            other => other,
        }
    }

    /// Lua's `tostring`: integers plainly, floats as `%.14g` with a
    /// `.0` suffix when the result would look like an integer, strings
    /// bare, everything else as `type: 0x…` (v1 uses a stable id).
    pub fn tostring(&self) -> String {
        match self {
            Value::Nil => "nil".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Num(f) => fmt_float_g14(*f),
            Value::Str(s) => (**s).clone(),
            Value::Table(t) => {
                let p = Rc::as_ptr(t) as *const () as usize;
                format!("table: 0x{p:08x}")
            }
            Value::Own(h) => format!("table: 0x{:08x}", h.0),
            Value::Ref(inner) => inner.tostring(),
            Value::Moved => "moved value".to_string(),
            Value::Func(f) => {
                let p = Rc::as_ptr(f) as *const () as usize;
                format!("function: 0x{p:08x}")
            }
        }
    }

    /// Equality per Lua: numbers compare across subtypes; strings,
    /// tables and functions by content/identity.
    pub fn lua_eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Nil, Value::Nil) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Int(a), Value::Num(b)) | (Value::Num(b), Value::Int(a)) => *a as f64 == *b,
            (Value::Num(a), Value::Num(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Table(a), Value::Table(b)) => Rc::ptr_eq(a, b),
            (Value::Own(a), Value::Own(b)) => a == b,
            (Value::Own(a), Value::Table(b)) | (Value::Table(b), Value::Own(a)) => {
                // An owned value never equals a GC table.
                let _ = (a, b);
                false
            }
            (Value::Ref(a), b) => a.lua_eq(b),
            (a, Value::Ref(b)) => a.lua_eq(b),
            (Value::Func(a), Value::Func(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// Formats a float like Lua 5.4's `%.14g`, appending `.0` when the
/// result would be indistinguishable from an integer. `inf`, `-inf`
/// and `nan` pass through with their 'n' keeping the suffix away —
/// the same trick Lua's `tostring` uses.
pub fn fmt_float_g14(x: f64) -> String {
    if !x.is_finite() {
        if x.is_nan() {
            return "nan".to_string();
        }
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let s = format_g(x, 14);
    if !s.contains(['.', 'e', 'E', 'n']) {
        format!("{s}.0")
    } else {
        s
    }
}

/// C's `%.*g`: exponent form when the decimal exponent is < -4 or
/// >= precision, fixed otherwise; trailing zeros stripped.
pub fn format_g(x: f64, precision: usize) -> String {
    if x == 0.0 {
        return if x.is_sign_negative() {
            "-0".to_string()
        } else {
            "0".to_string()
        };
    }
    // The decimal exponent, from Rust's shortest scientific form.
    let sci = format!("{:e}", x);
    let exp: i32 = sci
        .split('e')
        .nth(1)
        .and_then(|e| e.parse().ok())
        .unwrap_or(0);
    let p = precision as i32;
    if exp < -4 || exp >= p {
        // %e with precision-1 fraction digits, zeros stripped.
        let s = format!("{:.*e}", (p - 1).max(0) as usize, x);
        // Rust: "1.234e20" → C: "1.234e+20"; strip fraction zeros.
        let (mant, e) = s.split_once('e').unwrap_or((s.as_str(), ""));
        let mant = strip_fraction_zeros(mant);
        let sign = if e.starts_with('-') { '-' } else { '+' };
        let digits = e.trim_start_matches(['+', '-']);
        format!("{mant}e{sign}{digits:0>2}")
    } else {
        // %f with (precision - 1 - exp) decimals, zeros stripped.
        let decimals = (p - 1 - exp).max(0) as usize;
        let s = format!("{:.*}", decimals, x);
        strip_fraction_zeros(&s)
    }
}

fn strip_fraction_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    let trimmed = s.trim_end_matches('0');
    trimmed.trim_end_matches('.').to_string()
}
