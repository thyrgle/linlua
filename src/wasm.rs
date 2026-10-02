//! The linlua WASM backend: the strict dialect in linear memory.
//!
//! The dialect: numbers with Lua's integer/float subtyping (i64/f64
//! locals, per-expression subtype propagation matching the
//! interpreter's arithmetic rules), booleans, static strings,
//! sequences (length header at +0, 1-based elements at +8), numeric
//! loops (both subtypes), while/repeat, if/elseif/else, and direct
//! calls to top-level functions (params typed by unifying call
//! sites; recursive-only functions default their numeric params to
//! i64).
//!
//! Not in the dialect (rejected at compile time): closures and
//! anonymous functions, first-class functions, varargs, generic
//! `for..in`, globals as variables, string concatenation and string
//! comparison, nil, mixed-subtype sequences, multi-target
//! assignment, keyed tables.
//!
//! Every fixture is checksum-gated: the compiled module runs in Node
//! with Lua-style number formatting, and the output must match the
//! interpreter and `lua5.4` byte-for-byte.

use std::collections::HashMap;

use wasm_encoder::{
    BlockType, CodeSection, ConstExpr, DataSection, ExportSection, Function, FunctionSection,
    ImportSection, Instruction as Ins, MemArg, MemorySection, Module, TypeSection, ValType,
};

use crate::ast::*;
use crate::lexer::NumLit;

/// A compile error.
#[derive(Debug, Clone, PartialEq)]
pub struct WasmError {
    pub message: String,
}

fn err(what: impl Into<String>) -> WasmError {
    WasmError {
        message: format!("WASM backend (strict dialect): {}", what.into()),
    }
}

type R<T> = Result<T, WasmError>;

/// The value language of the dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    I64,
    F64,
    Bool,
    Str,
    ArrI64,
    ArrF64,
}

impl Ty {
    fn val(&self) -> ValType {
        match self {
            Ty::I64 => ValType::I64,
            Ty::F64 => ValType::F64,
            Ty::Bool | Ty::Str | Ty::ArrI64 | Ty::ArrF64 => ValType::I32,
        }
    }

    fn is_num(&self) -> bool {
        matches!(self, Ty::I64 | Ty::F64)
    }

    fn is_arr(&self) -> bool {
        matches!(self, Ty::ArrI64 | Ty::ArrF64)
    }

    fn elem(&self) -> Ty {
        match self {
            Ty::ArrI64 => Ty::I64,
            Ty::ArrF64 => Ty::F64,
            other => *other,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Ty::I64 => "integer",
            Ty::F64 => "float",
            Ty::Bool => "boolean",
            Ty::Str => "string",
            Ty::ArrI64 => "integer array",
            Ty::ArrF64 => "float array",
        }
    }
}

/// A function signature: parameter types plus the return type.
#[derive(Debug, Clone)]
struct Sig {
    params: Vec<Ty>,
    ret: Ty,
}

/// The module builder: shared tables (signatures, string literals,
/// function indices) plus a per-function compiler.
struct ModuleBuilder {
    sigs: HashMap<String, Sig>,
    strings: HashMap<String, u32>,
    data_next: u32,
    data_blob: Vec<(u32, Vec<u8>)>,
    /// user function name -> (index, sig)
    fn_indices: HashMap<String, (u32, Sig)>,
    // infrastructure indices
    alloc_idx: u32,
    pow_idx: u32,
    log_i64_idx: u32,
    log_f64_idx: u32,
    log_bool_idx: u32,
    log_str_idx: u32,
    log_tab_idx: u32,
    log_flush_idx: u32,
}

/// Per-function code generator.
struct FnCompiler<'b> {
    b: &'b mut ModuleBuilder,
    locals: HashMap<String, (u32, Ty)>,
    n_locals: u32,
    /// ValTypes of every local (params first) for the locals section.
    local_types: Vec<ValType>,
    code: Vec<Ins<'static>>,
    depth: u32,
    /// (break label base, loop label base)
    loops: Vec<(u32, u32)>,
}

impl<'b> FnCompiler<'b> {
    fn fresh(&mut self) -> u32 {
        let i = self.n_locals;
        self.n_locals += 1;
        self.local_types.push(ValType::I32);
        i
    }

    fn fresh_ty(&mut self, ty: ValType) -> u32 {
        let i = self.n_locals;
        self.n_locals += 1;
        self.local_types.push(ty);
        i
    }

    fn emit(&mut self, ins: Ins<'static>) {
        self.code.push(ins);
    }

    fn err(&self, what: &str) -> WasmError {
        err(what)
    }

    fn str_lit(&mut self, s: &str) -> u32 {
        if let Some(ptr) = self.b.strings.get(s) {
            return *ptr;
        }
        let ptr = self.b.data_next;
        let bytes = s.as_bytes();
        let mut blob = Vec::with_capacity(8 + bytes.len());
        // {len: u32, pad: u32, bytes} — readers decode at +8.
        blob.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        blob.extend_from_slice(&[0, 0, 0, 0]);
        blob.extend_from_slice(bytes);
        self.b.data_blob.push((ptr, blob));
        self.b.data_next += 8 + bytes.len() as u32;
        self.b.strings.insert(s.to_string(), ptr);
        ptr
    }

    // ---- statements ----

    fn compile_stmts(&mut self, stmts: &[Stmt]) -> R<()> {
        for s in stmts {
            self.compile_stmt(s)?;
        }
        Ok(())
    }

    fn compile_stmt(&mut self, stmt: &Stmt) -> R<()> {
        match stmt {
            Stmt::Local {
                names,
                inits,
                ann: _,
            } => {
                if names.len() != inits.len() {
                    return Err(
                        self.err("every `local` needs an initializer (no nil in the dialect)")
                    );
                }
                for ((name, ty_ann), init) in names.iter().zip(inits.iter()) {
                    let _ = ty_ann; // erased: the dialect infers subtypes
                    let ty = self.infer_expr(init)?;
                    let slot = self.fresh_ty(ty.val());
                    self.compile_expr_into(init, ty)?;
                    self.emit(Ins::LocalSet(slot));
                    self.locals.insert(name.clone(), (slot, ty));
                }
                Ok(())
            }
            Stmt::Assign { target, value } => match target {
                Target::Name(name) => {
                    let (slot, want) = self.locals.get(name).copied().ok_or_else(|| {
                        self.err(
                            "assignment to an undeclared name (globals are not in the dialect)",
                        )
                    })?;
                    let got = self.infer_expr(value)?;
                    if got != want {
                        return Err(self.err(&format!(
                            "reassignment of `{}` changes its subtype ({} -> {})",
                            name,
                            want.name(),
                            got.name()
                        )));
                    }
                    self.compile_expr_into(value, want)?;
                    self.emit(Ins::LocalSet(slot));
                    Ok(())
                }
                _ => self.index_store(target, value),
            },
            Stmt::Fn { .. } => {
                Err(self.err("nested function definitions are not in the dialect (top-level only)"))
            }
            Stmt::Expr(e) => match e {
                Expr::Call { .. } => {
                    self.compile_expr(e)?;
                    self.emit(Ins::Drop);
                    Ok(())
                }
                _ => Err(self.err("expression statements must be calls")),
            },
            Stmt::If {
                branches,
                otherwise,
            } => {
                // elseif chains are nested Ifs: if c1 { b1 } else { if
                // c2 { b2 } else { otherwise } } — every If gets its
                // own End.
                self.compile_if_chain(branches, otherwise)?;
                Ok(())
            }
            Stmt::While { cond, body } => {
                let base = self.depth;
                self.emit(Ins::Block(BlockType::Empty));
                self.emit(Ins::Loop(BlockType::Empty));
                self.depth += 2;
                self.loops.push((base, base + 1));
                self.compile_cond(cond)?;
                self.emit(Ins::I32Eqz);
                self.emit(Ins::BrIf(1)); // exit
                self.compile_stmts(body)?;
                self.emit(Ins::Br(0)); // loop
                self.depth -= 2;
                self.emit(Ins::End);
                self.emit(Ins::End);
                self.loops.pop();
                Ok(())
            }
            Stmt::Repeat { body, until } => {
                let base = self.depth;
                self.emit(Ins::Block(BlockType::Empty));
                self.emit(Ins::Loop(BlockType::Empty));
                self.depth += 2;
                self.loops.push((base, base + 1));
                self.compile_stmts(body)?;
                self.compile_cond(until)?;
                self.emit(Ins::I32Eqz);
                self.emit(Ins::BrIf(0)); // loop while false
                self.depth -= 2;
                self.emit(Ins::End);
                self.emit(Ins::End);
                self.loops.pop();
                Ok(())
            }
            Stmt::ForNum {
                var,
                start,
                limit,
                step,
                body,
            } => {
                let step_expr = step.clone().unwrap_or(Expr::Num(NumLit::Int(1)));
                let sty = self.infer_expr(&step_expr)?;
                let lty = self.infer_expr(limit)?;
                if !sty.is_num() || !lty.is_num() {
                    return Err(self.err("'for' bounds must be numbers"));
                }
                // One subtype for the whole loop: mixing promotes.
                let sty = if sty == Ty::F64 || lty == Ty::F64 {
                    Ty::F64
                } else {
                    Ty::I64
                };
                let int_loop = sty == Ty::I64;
                let (zero, gt, le, ge, add): (Ins, Ins, Ins, Ins, Ins) = if int_loop {
                    (
                        Ins::I64Const(0),
                        Ins::I64GtS,
                        Ins::I64LeS,
                        Ins::I64GeS,
                        Ins::I64Add,
                    )
                } else {
                    (
                        Ins::F64Const(0.0.into()),
                        Ins::F64Gt,
                        Ins::F64Le,
                        Ins::F64Ge,
                        Ins::F64Add,
                    )
                };

                self.compile_expr_into(start, sty)?;
                let i = self.fresh_ty(sty.val());
                self.emit(Ins::LocalSet(i));
                self.compile_expr_into(limit, sty)?;
                let limit_slot = self.fresh_ty(sty.val());
                self.emit(Ins::LocalSet(limit_slot));
                self.compile_expr_into(&step_expr, sty)?;
                let step_slot = self.fresh_ty(sty.val());
                self.emit(Ins::LocalSet(step_slot));
                self.locals.insert(var.clone(), (i, sty));

                let base = self.depth;
                self.emit(Ins::Block(BlockType::Empty));
                self.emit(Ins::Loop(BlockType::Empty));
                self.depth += 2;
                self.loops.push((base, base + 1));

                // continue? = (step > 0) ? i <= limit : i >= limit
                self.emit(Ins::LocalGet(step_slot));
                self.emit(zero);
                self.emit(gt);
                self.emit(Ins::If(BlockType::Result(ValType::I32)));
                self.depth += 1;
                self.emit(Ins::LocalGet(i));
                self.emit(Ins::LocalGet(limit_slot));
                self.emit(le);
                self.emit(Ins::Else);
                self.emit(Ins::LocalGet(i));
                self.emit(Ins::LocalGet(limit_slot));
                self.emit(ge);
                self.emit(Ins::End);
                self.depth -= 1;
                self.emit(Ins::I32Eqz);
                self.emit(Ins::BrIf(1)); // exit

                self.compile_stmts(body)?;

                self.emit(Ins::LocalGet(i));
                self.emit(Ins::LocalGet(step_slot));
                self.emit(add);
                self.emit(Ins::LocalSet(i));
                self.emit(Ins::Br(0));
                self.depth -= 2;
                self.emit(Ins::End);
                self.emit(Ins::End);
                self.loops.pop();
                self.locals.remove(var);
                Ok(())
            }
            Stmt::ForIn { .. } => {
                Err(self.err("`for..in` is not in the dialect (use a numeric for over #t)"))
            }
            Stmt::Return(e) => {
                match e {
                    Some(e) => {
                        self.compile_expr(e)?;
                    }
                    None => return Err(self.err("bare return (no nil in the dialect)")),
                }
                self.emit(Ins::Return);
                Ok(())
            }
            Stmt::Break => {
                let (break_base, _) = self
                    .loops
                    .last()
                    .copied()
                    .ok_or_else(|| self.err("break outside a loop"))?;
                let d = self.depth - break_base - 1;
                self.emit(Ins::Br(d));
                Ok(())
            }
            Stmt::Do(body) => self.compile_stmts(body),
        }
    }

    /// An if/elseif/else chain as nested If blocks.
    fn compile_if_chain(
        &mut self,
        branches: &[(Expr, Vec<Stmt>)],
        otherwise: &Option<Vec<Stmt>>,
    ) -> R<()> {
        match branches.split_first() {
            None => {
                if let Some(body) = otherwise {
                    self.compile_stmts(body)?;
                }
                Ok(())
            }
            Some(((cond, body), rest)) => {
                self.compile_cond(cond)?;
                self.emit(Ins::If(BlockType::Empty));
                self.depth += 1;
                self.compile_stmts(body)?;
                self.emit(Ins::Else);
                self.compile_if_chain(rest, otherwise)?;
                self.emit(Ins::End);
                self.depth -= 1;
                Ok(())
            }
        }
    }

    /// `t[i] = v` — a local array, integer index, matching element
    /// subtype. Stores take [addr, value].
    fn index_store(&mut self, target: &Target, value: &Expr) -> R<()> {
        let (obj, index) = match target {
            Target::Index { obj, index } => (obj, index),
            Target::Field { .. } => return Err(self.err("field stores are not in the dialect")),
            Target::Name(_) => unreachable!("names handled by the caller"),
        };
        let arr_name = match obj {
            Expr::Ident(n) => n.clone(),
            _ => return Err(self.err("stores into non-locals are not in the dialect")),
        };
        let (slot, ty) = self
            .locals
            .get(&arr_name)
            .copied()
            .ok_or_else(|| self.err("store into an undeclared name"))?;
        if !matches!(ty, Ty::ArrI64 | Ty::ArrF64) {
            return Err(self.err("index store on a non-array"));
        }
        let elem = ty.elem();
        let ity = self.infer_expr(index)?;
        if ity != Ty::I64 {
            return Err(self.err("array indices must be integers"));
        }
        let vty = self.infer_expr(value)?;
        if vty != elem {
            return Err(self.err(&format!(
                "cannot store {} in an array of {}",
                vty.name(),
                elem.name()
            )));
        }
        // [addr, value], one base push: addr = slot + (i-1)*8, with a
        // bounds trap (dialect sequences have a fixed length — growing
        // is the documented divergence).
        self.emit(Ins::LocalGet(slot));
        self.compile_expr_into(index, Ty::I64)?;
        self.emit(Ins::I64Const(1));
        self.emit(Ins::I64Sub);
        let i = self.fresh_ty(ValType::I64);
        self.emit(Ins::LocalSet(i));
        // bounds: i < 0 or i >= len -> trap
        self.emit(Ins::LocalGet(i));
        self.emit(Ins::I64Const(0));
        self.emit(Ins::I64LtS);
        self.emit(Ins::LocalGet(i));
        self.emit(Ins::LocalGet(slot));
        self.emit(Ins::I32Load(MemArg {
            offset: 0,
            align: 2,
            memory_index: 0,
        }));
        self.emit(Ins::I64ExtendI32U);
        self.emit(Ins::I64GeS);
        self.emit(Ins::I32Or);
        self.emit(Ins::If(BlockType::Empty));
        self.emit(Ins::Unreachable);
        self.emit(Ins::End);
        self.emit(Ins::LocalGet(i));
        self.emit(Ins::I32WrapI64);
        self.emit(Ins::I32Const(8));
        self.emit(Ins::I32Mul);
        self.emit(Ins::I32Add);
        self.compile_expr_into(value, elem)?;
        let (store, align) = match elem {
            Ty::I64 => (
                Ins::I64Store(MemArg {
                    offset: 8,
                    align: 3,
                    memory_index: 0,
                }),
                3u32,
            ),
            _ => (
                Ins::F64Store(MemArg {
                    offset: 8,
                    align: 3,
                    memory_index: 0,
                }),
                3,
            ),
        };
        let _ = align;
        self.emit(store);
        Ok(())
    }

    fn compile_cond(&mut self, e: &Expr) -> R<()> {
        let ty = self.infer_expr(e)?;
        if ty != Ty::Bool {
            return Err(self.err(&format!(
                "conditions must be booleans in the dialect (found {})",
                ty.name()
            )));
        }
        self.compile_expr_into(e, Ty::Bool)
    }

    // ---- expressions ----

    fn infer_expr(&mut self, e: &Expr) -> R<Ty> {
        match e {
            Expr::Nil => Err(self.err("nil is not in the dialect")),
            Expr::True | Expr::False => Ok(Ty::Bool),
            Expr::Num(NumLit::Int(_)) => Ok(Ty::I64),
            Expr::Num(NumLit::Float(_)) => Ok(Ty::F64),
            Expr::Str(_) => Ok(Ty::Str),
            Expr::Vararg => Err(self.err("varargs")),
            Expr::Ident(name) => self.locals.get(name).map(|(_, t)| *t).ok_or_else(|| {
                self.err(&format!(
                    "unknown name `{name}` (globals are not in the dialect)"
                ))
            }),
            Expr::Table(fields) => {
                if fields.is_empty() {
                    return Err(self.err("empty table literals have no element type"));
                }
                let mut elem: Option<Ty> = None;
                for f in fields {
                    match f {
                        TableField::Item(item) => {
                            let ty = self.infer_expr(item)?;
                            match elem {
                                None => elem = Some(ty),
                                Some(prev) if prev == ty => {}
                                _ => {
                                    return Err(self.err("sequences must have one element subtype"))
                                }
                            }
                        }
                        TableField::Keyed { .. } => {
                            return Err(
                                self.err("keyed tables are not in the dialect (sequences only)")
                            );
                        }
                    }
                }
                Ok(match elem {
                    Some(Ty::F64) => Ty::ArrF64,
                    Some(Ty::I64) => Ty::ArrI64,
                    _ => return Err(self.err("sequences must hold numbers")),
                })
            }
            Expr::Function { .. } => Err(self.err("anonymous functions")),
            Expr::Paren(inner) => self.infer_expr(inner),
            Expr::Unary(op, inner) => {
                let ty = self.infer_expr(inner)?;
                match op {
                    UnOp::Not => {
                        if ty != Ty::Bool {
                            return Err(self.err("`not` needs a boolean"));
                        }
                        Ok(Ty::Bool)
                    }
                    UnOp::Len => match ty {
                        Ty::Str | Ty::ArrI64 | Ty::ArrF64 => Ok(Ty::I64),
                        other => {
                            Err(self.err(&format!("cannot take the length of a {}", other.name())))
                        }
                    },
                    UnOp::Neg => match ty {
                        Ty::I64 | Ty::F64 => Ok(ty),
                        other => Err(self.err(&format!("cannot negate a {}", other.name()))),
                    },
                    UnOp::BNot => {
                        if ty == Ty::I64 {
                            Ok(Ty::I64)
                        } else {
                            Err(self.err("`~` needs an integer"))
                        }
                    }
                }
            }
            Expr::Binary(op, l, r) => {
                let lt = self.infer_expr(l)?;
                let rt = self.infer_expr(r)?;
                match op {
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::IDiv | BinOp::Mod => {
                        self.num_pair(lt, rt)?;
                        Ok(if lt == Ty::I64 && rt == Ty::I64 {
                            Ty::I64
                        } else {
                            Ty::F64
                        })
                    }
                    BinOp::Div | BinOp::Pow => {
                        self.num_pair(lt, rt)?;
                        Ok(Ty::F64)
                    }
                    BinOp::Concat => Err(self.err("string concatenation")),
                    BinOp::Eq | BinOp::NotEq => {
                        if lt != rt {
                            return Err(self.err(&format!(
                                "cannot compare {} with {}",
                                lt.name(),
                                rt.name()
                            )));
                        }
                        if lt == Ty::Str {
                            return Err(self.err("string comparison"));
                        }
                        if lt.is_arr() || matches!(lt, Ty::ArrI64 | Ty::ArrF64) {
                            return Err(self.err("array comparison"));
                        }
                        Ok(Ty::Bool)
                    }
                    BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                        if lt != rt || !lt.is_num() {
                            return Err(self.err(&format!(
                                "ordering comparisons need two numbers of one subtype ({} vs {})",
                                lt.name(),
                                rt.name()
                            )));
                        }
                        Ok(Ty::Bool)
                    }
                    BinOp::And | BinOp::Or => {
                        if lt != Ty::Bool || rt != Ty::Bool {
                            return Err(self.err("`and`/`or` need boolean operands in the dialect"));
                        }
                        Ok(Ty::Bool)
                    }
                    BinOp::Band | BinOp::Bor | BinOp::BXor | BinOp::Shl | BinOp::Shr => {
                        if lt != Ty::I64 || rt != Ty::I64 {
                            return Err(self.err("bitwise ops need integers"));
                        }
                        Ok(Ty::I64)
                    }
                }
            }
            Expr::Index { obj, index } => {
                let on = match &**obj {
                    Expr::Ident(n) => n.clone(),
                    _ => return Err(self.err("indexing non-locals")),
                };
                let (_, ty) = self
                    .locals
                    .get(&on)
                    .copied()
                    .ok_or_else(|| self.err("indexing an unknown name"))?;
                let elem = match ty {
                    Ty::ArrI64 => Ty::I64,
                    Ty::ArrF64 => Ty::F64,
                    _ => return Err(self.err("indexing a non-array")),
                };
                let ity = self.infer_expr(index)?;
                if ity != Ty::I64 {
                    return Err(self.err("array indices must be integers"));
                }
                Ok(elem)
            }
            Expr::Field { obj, name } => {
                // The math constants are the dialect's only fields.
                if matches!(&**obj, Expr::Ident(n) if n == "math") {
                    return match name.as_str() {
                        "pi" | "huge" => Ok(Ty::F64),
                        "maxinteger" | "mininteger" => Ok(Ty::I64),
                        other => Err(self.err(&format!("unknown math constant `{other}`"))),
                    };
                }
                Err(self.err("field access (`t.k`)"))
            }
            Expr::Call { callee, args } => {
                let name = match &**callee {
                    Expr::Ident(n) => n.clone(),
                    _ => return Err(self.err("non-direct calls")),
                };
                if name == "print" {
                    for a in args {
                        let ty = self.infer_expr(a)?;
                        match ty {
                            Ty::I64 | Ty::F64 | Ty::Bool | Ty::Str => {}
                            other => {
                                return Err(self.err(&format!("print cannot take {}", other.name())))
                            }
                        }
                    }
                    return Ok(Ty::I64); // nil stand-in
                }
                let sig = self
                    .b
                    .sigs
                    .get(&name)
                    .cloned()
                    .ok_or_else(|| self.err(&format!("unknown function `{name}`")))?;
                if args.len() != sig.params.len() {
                    return Err(self.err(&format!(
                        "`{name}` called with {} arguments, expected {}",
                        args.len(),
                        sig.params.len()
                    )));
                }
                for (a, want) in args.iter().zip(&sig.params) {
                    let got = self.infer_expr(a)?;
                    if got != *want {
                        return Err(self.err(&format!(
                            "`{name}` argument is {}, expected {}",
                            got.name(),
                            want.name()
                        )));
                    }
                }
                Ok(sig.ret)
            }
        }
    }

    fn num_pair(&mut self, lt: Ty, rt: Ty) -> R<()> {
        if !lt.is_num() || !rt.is_num() {
            return Err(self.err(&format!(
                "arithmetic needs numbers ({} and {})",
                lt.name(),
                rt.name()
            )));
        }
        Ok(())
    }

    /// Emits an expression, coercing i64 -> f64 when the promoted
    /// subtype needs it (the type rules were checked in infer).
    fn compile_expr_into(&mut self, e: &Expr, want: Ty) -> R<()> {
        let got = self.infer_expr(e)?;
        self.compile_expr(e)?;
        if got != want {
            match (got, want) {
                (Ty::I64, Ty::F64) => {
                    self.emit(Ins::F64ConvertI64S);
                }
                _ => {
                    return Err(self.err(&format!(
                        "internal: cannot coerce {} to {}",
                        got.name(),
                        want.name()
                    )))
                }
            }
        }
        Ok(())
    }

    fn compile_expr(&mut self, e: &Expr) -> R<Ty> {
        match e {
            Expr::True => {
                self.emit(Ins::I32Const(1));
                Ok(Ty::Bool)
            }
            Expr::False => {
                self.emit(Ins::I32Const(0));
                Ok(Ty::Bool)
            }
            Expr::Num(NumLit::Int(v)) => {
                self.emit(Ins::I64Const(*v));
                Ok(Ty::I64)
            }
            Expr::Num(NumLit::Float(v)) => {
                self.emit(Ins::F64Const((*v).into()));
                Ok(Ty::F64)
            }
            Expr::Str(s) => {
                let ptr = self.str_lit(s);
                self.emit(Ins::I32Const(ptr as i32));
                Ok(Ty::Str)
            }
            Expr::Ident(name) => {
                let (slot, ty) = self
                    .locals
                    .get(name)
                    .copied()
                    .ok_or_else(|| self.err(&format!("unknown name `{name}`")))?;
                self.emit(Ins::LocalGet(slot));
                Ok(ty)
            }
            Expr::Table(fields) => {
                let mut elem = Ty::I64;
                if let Some(TableField::Item(first)) = fields.first() {
                    elem = self.infer_expr(first)?;
                }
                let n = fields.len();
                // $alloc(8 + n*8) -> ptr
                self.emit(Ins::I32Const(8 + 8 * n as i32));
                self.emit(Ins::Call(self.b.alloc_idx));
                let ptr = self.fresh();
                self.emit(Ins::LocalTee(ptr));
                self.emit(Ins::I32Const(n as i32));
                self.emit(Ins::I32Store(MemArg {
                    offset: 0,
                    align: 2,
                    memory_index: 0,
                }));
                for (i, f) in fields.iter().enumerate() {
                    let item = match f {
                        TableField::Item(item) => item,
                        TableField::Keyed { .. } => return Err(self.err("keyed tables")),
                    };
                    // [addr, value] per store; the value via a temp.
                    self.compile_expr_into(item, elem)?;
                    let v = self.fresh_ty(elem.val());
                    self.emit(Ins::LocalSet(v));
                    self.emit(Ins::LocalGet(ptr));
                    self.emit(Ins::LocalGet(v));
                    match elem {
                        Ty::I64 => self.emit(Ins::I64Store(MemArg {
                            offset: (8 + 8 * i) as u64,
                            align: 3,
                            memory_index: 0,
                        })),
                        _ => self.emit(Ins::F64Store(MemArg {
                            offset: (8 + 8 * i) as u64,
                            align: 3,
                            memory_index: 0,
                        })),
                    }
                }
                self.emit(Ins::LocalGet(ptr));
                Ok(match elem {
                    Ty::F64 => Ty::ArrF64,
                    _ => Ty::ArrI64,
                })
            }
            Expr::Paren(inner) => self.compile_expr(inner),
            Expr::Unary(op, inner) => {
                // Neg needs the zero UNDER the operand: emit it first.
                if *op == UnOp::Neg {
                    let ty = self.infer_expr(inner)?;
                    match ty {
                        Ty::I64 => self.emit(Ins::I64Const(0)),
                        _ => self.emit(Ins::F64Const(0.0.into())),
                    }
                    self.compile_expr_into(inner, ty)?;
                    self.emit(match ty {
                        Ty::I64 => Ins::I64Sub,
                        _ => Ins::F64Sub,
                    });
                    return Ok(ty);
                }
                let ty = self.compile_expr(inner)?;
                let _ = &ty;
                match op {
                    UnOp::Not => {
                        self.emit(Ins::I32Eqz);
                        Ok(Ty::Bool)
                    }
                    UnOp::Len => {
                        // header at +0
                        self.emit(Ins::I32Load(MemArg {
                            offset: 0,
                            align: 2,
                            memory_index: 0,
                        }));
                        self.emit(Ins::I64ExtendI32U);
                        Ok(Ty::I64)
                    }
                    UnOp::Neg => unreachable!("handled above"),
                    UnOp::BNot => {
                        // ~x = x ^ -1; [x] -> [-1, x] needs a swap.
                        let v = self.fresh_ty(ValType::I64);
                        self.emit(Ins::LocalSet(v));
                        self.emit(Ins::I64Const(-1));
                        self.emit(Ins::LocalGet(v));
                        self.emit(Ins::I64Xor);
                        Ok(Ty::I64)
                    }
                }
            }
            Expr::Binary(op, l, r) => {
                let lt = self.infer_expr(l)?;
                let rt = self.infer_expr(r)?;
                match op {
                    BinOp::And => {
                        // a and b: a truthy -> b else a (booleans)
                        self.compile_expr_into(l, Ty::Bool)?;
                        self.emit(Ins::If(BlockType::Result(ValType::I32)));
                        self.depth += 1;
                        self.compile_expr_into(r, Ty::Bool)?;
                        self.emit(Ins::Else);
                        self.emit(Ins::I32Const(0));
                        self.emit(Ins::End);
                        self.depth -= 1;
                        Ok(Ty::Bool)
                    }
                    BinOp::Or => {
                        self.compile_expr_into(l, Ty::Bool)?;
                        self.emit(Ins::If(BlockType::Result(ValType::I32)));
                        self.depth += 1;
                        self.emit(Ins::I32Const(1));
                        self.emit(Ins::Else);
                        self.compile_expr_into(r, Ty::Bool)?;
                        self.emit(Ins::End);
                        self.depth -= 1;
                        Ok(Ty::Bool)
                    }
                    BinOp::Eq | BinOp::NotEq => {
                        self.compile_expr_into(l, lt)?;
                        self.compile_expr_into(r, rt)?;
                        match lt {
                            Ty::I64 => self.emit(Ins::I64Eq),
                            Ty::F64 => self.emit(Ins::F64Eq),
                            _ => self.emit(Ins::I32Eq),
                        }
                        if *op == BinOp::NotEq {
                            self.emit(Ins::I32Eqz);
                        }
                        Ok(Ty::Bool)
                    }
                    BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                        self.compile_expr_into(l, lt)?;
                        self.compile_expr_into(r, rt)?;
                        let ins = match (lt, op) {
                            (Ty::I64, BinOp::Lt) => Ins::I64LtS,
                            (Ty::I64, BinOp::Gt) => Ins::I64GtS,
                            (Ty::I64, BinOp::Le) => Ins::I64LeS,
                            (Ty::I64, _) => Ins::I64GeS,
                            (_, BinOp::Lt) => Ins::F64Lt,
                            (_, BinOp::Gt) => Ins::F64Gt,
                            (_, BinOp::Le) => Ins::F64Le,
                            (_, _) => Ins::F64Ge,
                        };
                        self.emit(ins);
                        Ok(Ty::Bool)
                    }
                    BinOp::Add
                    | BinOp::Sub
                    | BinOp::Mul
                    | BinOp::Div
                    | BinOp::IDiv
                    | BinOp::Mod
                    | BinOp::Pow => self.compile_arith(*op, lt, rt, l, r),
                    BinOp::Band | BinOp::Bor | BinOp::BXor | BinOp::Shl | BinOp::Shr => {
                        self.compile_expr_into(l, Ty::I64)?;
                        self.compile_expr_into(r, Ty::I64)?;
                        self.emit(match op {
                            BinOp::Band => Ins::I64And,
                            BinOp::Bor => Ins::I64Or,
                            BinOp::BXor => Ins::I64Xor,
                            BinOp::Shl => Ins::I64Shl,
                            _ => Ins::I64ShrS,
                        });
                        Ok(Ty::I64)
                    }
                    BinOp::Concat => Err(self.err("string concatenation")),
                }
            }
            Expr::Index { obj, index } => {
                let on = match &**obj {
                    Expr::Ident(n) => n.clone(),
                    _ => return Err(self.err("indexing non-locals")),
                };
                let (slot, ty) = self
                    .locals
                    .get(&on)
                    .copied()
                    .ok_or_else(|| self.err("indexing an unknown name"))?;
                let elem = ty.elem();
                // [addr] via slot + (i-1)*8, then the load at +8.
                self.compile_expr_into(index, Ty::I64)?;
                let i = self.fresh_ty(ValType::I64);
                self.emit(Ins::LocalSet(i));
                self.emit(Ins::LocalGet(slot));
                self.emit(Ins::LocalGet(i));
                self.emit(Ins::I64Const(1));
                self.emit(Ins::I64Sub);
                self.emit(Ins::I32WrapI64);
                self.emit(Ins::I32Const(8));
                self.emit(Ins::I32Mul);
                self.emit(Ins::I32Add);
                match elem {
                    Ty::I64 => self.emit(Ins::I64Load(MemArg {
                        offset: 8,
                        align: 3,
                        memory_index: 0,
                    })),
                    _ => self.emit(Ins::F64Load(MemArg {
                        offset: 8,
                        align: 3,
                        memory_index: 0,
                    })),
                }
                Ok(elem)
            }
            Expr::Call { callee, args } => {
                let name = match &**callee {
                    Expr::Ident(n) => n.clone(),
                    _ => return Err(self.err("non-direct calls")),
                };
                if name == "print" {
                    for (i, a) in args.iter().enumerate() {
                        let ty = self.infer_expr(a)?;
                        if i > 0 {
                            self.emit(Ins::Call(self.b.log_tab_idx));
                        }
                        self.compile_expr_into(a, ty)?;
                        self.emit(match ty {
                            Ty::I64 => Ins::Call(self.b.log_i64_idx),
                            Ty::F64 => Ins::Call(self.b.log_f64_idx),
                            Ty::Bool => Ins::Call(self.b.log_bool_idx),
                            _ => Ins::Call(self.b.log_str_idx),
                        });
                    }
                    self.emit(Ins::Call(self.b.log_flush_idx));
                    // nil stand-in for statement-position Drop
                    self.emit(Ins::I64Const(0));
                    return Ok(Ty::I64);
                }
                let (_, sig) = self
                    .b
                    .fn_indices
                    .get(&name)
                    .ok_or_else(|| self.err(&format!("unknown function `{name}`")))?
                    .clone();
                for (a, want) in args.iter().zip(&sig.params) {
                    self.compile_expr_into(a, *want)?;
                }
                let idx = self.b.fn_indices[&name].0;
                self.emit(Ins::Call(idx));
                Ok(sig.ret)
            }
            Expr::Nil | Expr::Vararg | Expr::Function { .. } => Err(self.err("not in the dialect")),
            Expr::Field { obj, name } => {
                if matches!(&**obj, Expr::Ident(n) if n == "math") {
                    match name.as_str() {
                        "pi" => {
                            self.emit(Ins::F64Const(std::f64::consts::PI.into()));
                            return Ok(Ty::F64);
                        }
                        "huge" => {
                            self.emit(Ins::F64Const(f64::INFINITY.into()));
                            return Ok(Ty::F64);
                        }
                        "maxinteger" => {
                            self.emit(Ins::I64Const(i64::MAX));
                            return Ok(Ty::I64);
                        }
                        "mininteger" => {
                            self.emit(Ins::I64Const(i64::MIN));
                            return Ok(Ty::I64);
                        }
                        _ => {}
                    }
                }
                Err(self.err("field access"))
            }
        }
    }

    fn compile_arith(&mut self, op: BinOp, lt: Ty, rt: Ty, l: &Expr, r: &Expr) -> R<Ty> {
        let _ = (lt, rt);
        // `/` and `^` are always float; int×int stays int otherwise.
        let result = if matches!(op, BinOp::Div | BinOp::Pow) {
            Ty::F64
        } else if self.infer_expr(l)? == Ty::I64 && self.infer_expr(r)? == Ty::I64 {
            Ty::I64
        } else {
            Ty::F64
        };
        self.compile_expr_into(l, result)?;
        self.compile_expr_into(r, result)?;
        let float = result == Ty::F64;
        match op {
            BinOp::Add => self.emit(if float { Ins::F64Add } else { Ins::I64Add }),
            BinOp::Sub => self.emit(if float { Ins::F64Sub } else { Ins::I64Sub }),
            BinOp::Mul => self.emit(if float { Ins::F64Mul } else { Ins::I64Mul }),
            BinOp::Div => self.emit(Ins::F64Div),
            BinOp::IDiv => {
                if float {
                    // a // b = floor(a / b)
                    self.emit(Ins::F64Div);
                    self.emit(Ins::F64Floor);
                } else {
                    self.emit_int_floored_div();
                }
            }
            BinOp::Mod => {
                if float {
                    // [a, b] -> a - floor(a / b) * b: keep a under the
                    // computation, then subtract.
                    let a = self.fresh_ty(ValType::F64);
                    let b = self.fresh_ty(ValType::F64);
                    self.emit(Ins::LocalSet(b));
                    self.emit(Ins::LocalSet(a));
                    self.emit(Ins::LocalGet(a));
                    self.emit(Ins::LocalGet(a));
                    self.emit(Ins::LocalGet(b));
                    self.emit(Ins::F64Div);
                    self.emit(Ins::F64Floor);
                    self.emit(Ins::LocalGet(b));
                    self.emit(Ins::F64Mul);
                    self.emit(Ins::F64Sub);
                } else {
                    self.emit_int_floored_mod();
                }
            }
            BinOp::Pow => {
                self.emit(Ins::Call(self.b.pow_idx));
            }
            _ => return Err(self.err("arithmetic")),
        }
        Ok(result)
    }

    /// [a, b] -> Lua's floored integer division, with wrap edges.
    fn emit_int_floored_div(&mut self) {
        let a = self.fresh_ty(ValType::I64);
        let b = self.fresh_ty(ValType::I64);
        let q = self.fresh_ty(ValType::I64);
        self.emit(Ins::LocalSet(b));
        self.emit(Ins::LocalSet(a));
        // q = trunc(a / b)
        self.emit(Ins::LocalGet(a));
        self.emit(Ins::LocalGet(b));
        self.emit(Ins::I64DivS);
        self.emit(Ins::LocalSet(q));
        // exact when a == q * b
        self.emit(Ins::LocalGet(q));
        self.emit(Ins::LocalGet(b));
        self.emit(Ins::I64Mul);
        self.emit(Ins::LocalGet(a));
        self.emit(Ins::I64Eq);
        self.emit(Ins::If(BlockType::Result(ValType::I64)));
        self.depth += 1;
        self.emit(Ins::LocalGet(q));
        self.emit(Ins::Else);
        // signs differ -> floor is one lower
        self.emit(Ins::LocalGet(a));
        self.emit(Ins::LocalGet(b));
        self.emit(Ins::I64Xor);
        self.emit(Ins::I64Const(0));
        self.emit(Ins::I64LtS);
        self.emit(Ins::If(BlockType::Result(ValType::I64)));
        self.depth += 1;
        self.emit(Ins::LocalGet(q));
        self.emit(Ins::I64Const(1));
        self.emit(Ins::I64Sub);
        self.emit(Ins::Else);
        self.emit(Ins::LocalGet(q));
        self.emit(Ins::End);
        self.depth -= 1;
        self.emit(Ins::End);
        self.depth -= 1;
    }

    /// [a, b] -> Lua's floored integer modulo: the result takes the
    /// divisor's sign.
    fn emit_int_floored_mod(&mut self) {
        let a = self.fresh_ty(ValType::I64);
        let b = self.fresh_ty(ValType::I64);
        let r = self.fresh_ty(ValType::I64);
        self.emit(Ins::LocalSet(b));
        self.emit(Ins::LocalSet(a));
        self.emit(Ins::LocalGet(a));
        self.emit(Ins::LocalGet(b));
        self.emit(Ins::I64RemS);
        self.emit(Ins::LocalSet(r));
        // r == 0 -> 0; signs differ -> r + b; else r
        self.emit(Ins::LocalGet(r));
        self.emit(Ins::I64Eqz);
        self.emit(Ins::If(BlockType::Result(ValType::I64)));
        self.depth += 1;
        self.emit(Ins::I64Const(0));
        self.emit(Ins::Else);
        self.emit(Ins::LocalGet(r));
        self.emit(Ins::LocalGet(b));
        self.emit(Ins::I64Xor);
        self.emit(Ins::I64Const(0));
        self.emit(Ins::I64LtS);
        self.emit(Ins::If(BlockType::Result(ValType::I64)));
        self.depth += 1;
        self.emit(Ins::LocalGet(r));
        self.emit(Ins::LocalGet(b));
        self.emit(Ins::I64Add);
        self.emit(Ins::Else);
        self.emit(Ins::LocalGet(r));
        self.emit(Ins::End);
        self.depth -= 1;
        self.emit(Ins::End);
        self.depth -= 1;
    }
}

/// Import/defined index layout:
/// 0..=5 imports: logi, logf, logb, logstr, logtab, logflush
/// 6 import: pow(f64, f64) -> f64 (host Math.pow)
/// 7 defined: $alloc(n: i32) -> i32 (bump)
/// 8.. user functions, $run last.
const ALLOC_IDX: u32 = 7;
const FIRST_USER: u32 = 8;

/// Compiles the strict dialect to a WebAssembly module.
pub fn compile(source: &str) -> Result<Vec<u8>, WasmError> {
    let chunk = crate::parser::parse_chunk(source).map_err(|e| WasmError {
        message: format!("parse error: {}", e.message),
    })?;

    // Top-level functions in definition order; the rest is $run.
    let mut fns: Vec<(String, Vec<String>, Vec<Stmt>)> = Vec::new();
    let mut main: Vec<Stmt> = Vec::new();
    for stmt in &chunk {
        match stmt {
            Stmt::Fn {
                name, params, body, ..
            } => fns.push((
                name.clone(),
                params.iter().map(|(n, _)| n.clone()).collect(),
                body.clone(),
            )),
            other => main.push(other.clone()),
        }
    }

    // Signature unification: param types from call sites (shallow
    // arg typing; unconstrained params default to i64), return type
    // from the body's first return.
    let mut param_tys: HashMap<String, Vec<Option<Ty>>> = HashMap::new();
    for (name, params, _) in &fns {
        param_tys.insert(name.clone(), vec![None; params.len()]);
    }
    // The main chunk's shallow locals: names bound to literals, so
    // `total(a)` types the array through the name.
    let mut main_scope: HashMap<String, Ty> = HashMap::new();
    for stmt in &chunk {
        if let Stmt::Local {
            names,
            inits,
            ann: _,
        } = stmt
        {
            for ((n, _), init) in names.iter().zip(inits.iter()) {
                if let Some(ty) = shallow_type(init) {
                    main_scope.insert(n.clone(), ty);
                }
            }
        }
    }
    for (fname, args) in collect_calls(&chunk) {
        if let Some(slots) = param_tys.get_mut(&fname) {
            for (i, arg) in args.iter().enumerate() {
                if i >= slots.len() {
                    continue;
                }
                let ty = match arg {
                    Expr::Ident(n) => main_scope.get(n).copied(),
                    other => shallow_type(other),
                };
                if let Some(ty) = ty {
                    match slots[i] {
                        None => slots[i] = Some(ty),
                        Some(prev) if prev == ty => {}
                        _ => {
                            return Err(err(format!(
                                "`{fname}` is called with different argument subtypes"
                            )))
                        }
                    }
                }
            }
        }
    }
    let mut sigs: HashMap<String, Sig> = HashMap::new();
    for (name, params, body) in &fns {
        let slots = &param_tys[name];
        let params_ty: Vec<Ty> = params
            .iter()
            .enumerate()
            .map(|(i, _)| slots[i].unwrap_or(Ty::I64))
            .collect();
        let mut scope: HashMap<String, Ty> = params
            .iter()
            .cloned()
            .zip(params_ty.iter().copied())
            .collect();
        let ret = body_ret(body, &mut scope).unwrap_or(Ty::I64);
        sigs.insert(
            name.clone(),
            Sig {
                params: params_ty,
                ret,
            },
        );
    }

    if std::env::var("LINLUA_DEBUG").is_ok() {
        for (n, s) in &sigs {
            eprintln!("sig {n}: params={:?} ret={:?}", s.params, s.ret);
        }
    }
    let strings: HashMap<String, u32> = HashMap::new();
    let data_next: u32 = 0;
    let data_blob: Vec<(u32, Vec<u8>)> = Vec::new();

    // ---- module sections ----
    let mut types = TypeSection::new();
    let no_vals: &[ValType] = &[];
    types
        .ty()
        .function(no_vals.iter().copied(), no_vals.iter().copied()); // 0: () -> ()
    types.ty().function([ValType::I32], [ValType::I32]); // 1: $alloc
    types
        .ty()
        .function([ValType::F64, ValType::F64], [ValType::F64]); // 2: pow
    types.ty().function([ValType::I64], no_vals.iter().copied()); // 3: logi
    types.ty().function([ValType::F64], no_vals.iter().copied()); // 4: logf
    types.ty().function([ValType::I32], no_vals.iter().copied()); // 5: logb
    types.ty().function([ValType::I32], no_vals.iter().copied()); // 6: logstr
    types
        .ty()
        .function(no_vals.iter().copied(), no_vals.iter().copied()); // 7: logtab/logflush

    let mut imports = ImportSection::new();
    imports.import("env", "logi", wasm_encoder::EntityType::Function(3));
    imports.import("env", "logf", wasm_encoder::EntityType::Function(4));
    imports.import("env", "logb", wasm_encoder::EntityType::Function(5));
    imports.import("env", "logstr", wasm_encoder::EntityType::Function(6));
    imports.import("env", "logtab", wasm_encoder::EntityType::Function(7));
    imports.import("env", "logflush", wasm_encoder::EntityType::Function(7));
    imports.import("env", "pow", wasm_encoder::EntityType::Function(2));

    let mut functions = FunctionSection::new();
    functions.function(1); // $alloc (type 1)

    let mut user_index: HashMap<String, (u32, Sig)> = HashMap::new();
    let mut idx = FIRST_USER; // function index
    let mut type_idx: u32 = 8; // after the 8 static types
    for (name, _, _) in &fns {
        let sig = sigs[name].clone();
        let valtypes: Vec<ValType> = sig.params.iter().map(|t| t.val()).collect();
        types
            .ty()
            .function(valtypes.iter().copied(), [sig.ret.val()]);
        functions.function(type_idx);
        type_idx += 1;
        user_index.insert(name.clone(), (idx, sig));
        idx += 1;
    }
    let run_idx = idx;
    types
        .ty()
        .function(no_vals.iter().copied(), no_vals.iter().copied());
    functions.function(type_idx);

    let mut b = ModuleBuilder {
        fn_indices: user_index
            .iter()
            .map(|(n, (i, s))| (n.clone(), (*i, s.clone())))
            .collect(),
        sigs,
        strings,
        data_next,
        data_blob,
        alloc_idx: ALLOC_IDX,
        pow_idx: 6,
        log_i64_idx: 0,
        log_f64_idx: 1,
        log_bool_idx: 2,
        log_str_idx: 3,
        log_tab_idx: 4,
        log_flush_idx: 5,
    };

    // ---- codegen ----
    let mut codes = CodeSection::new();

    // $alloc: bump with a one-page growth step.
    {
        // 0 = n (param), 1 = old, 2 = end
        let mut f = Function::new(vec![(2, ValType::I32)]);
        for ins in [
            Ins::GlobalGet(0),
            Ins::LocalSet(1),
            Ins::LocalGet(1),
            Ins::LocalGet(0),
            Ins::I32Add,
            Ins::LocalSet(2),
            Ins::LocalGet(2),
            Ins::MemorySize(0),
            Ins::I32Const(16),
            Ins::I32Shl,
            Ins::I32GtU,
            Ins::If(BlockType::Empty),
            Ins::I32Const(1),
            Ins::MemoryGrow(0),
            Ins::Drop,
            Ins::End,
            Ins::LocalGet(2),
            Ins::GlobalSet(0),
            Ins::LocalGet(1),
            Ins::End,
        ] {
            f.instruction(&ins);
        }
        codes.function(&f);
    }

    // User functions.
    for (name, params, body) in &fns {
        let sig = b.sigs[name].clone();
        let mut locals_map: HashMap<String, (u32, Ty)> = HashMap::new();
        for (i, p) in params.iter().enumerate() {
            locals_map.insert(p.clone(), (i as u32, sig.params[i]));
        }
        let mut fc = FnCompiler {
            b: &mut b,
            locals: locals_map,
            n_locals: params.len() as u32,
            local_types: sig.params.iter().map(|t| t.val()).collect(),
            code: Vec::new(),
            depth: 0,
            loops: Vec::new(),
        };
        fc.compile_stmts(body)?;
        codes.function(&finish_fn(&fc, params.len()));
    }

    // $run.
    {
        let mut fc = FnCompiler {
            b: &mut b,
            locals: HashMap::new(),
            n_locals: 0,
            local_types: Vec::new(),
            code: Vec::new(),
            depth: 0,
            loops: Vec::new(),
        };
        fc.compile_stmts(&main)?;
        codes.function(&finish_fn(&fc, 0));
    }

    let mut memories = MemorySection::new();
    memories.memory(wasm_encoder::MemoryType {
        minimum: 1,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });

    let mut globals = wasm_encoder::GlobalSection::new();
    globals.global(
        wasm_encoder::GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(1024), // bump starts past the stack
    );

    let mut exports = ExportSection::new();
    exports.export("memory", wasm_encoder::ExportKind::Memory, 0);
    exports.export("run", wasm_encoder::ExportKind::Func, run_idx);

    let mut data = DataSection::new();
    if std::env::var("LINLUA_DEBUG").is_ok() {
        eprintln!(
            "data segments: {} strings: {:?}",
            b.data_blob.len(),
            b.strings.len()
        );
    }
    for (ptr, blob) in &b.data_blob {
        data.active(0, &ConstExpr::i32_const(*ptr as i32), blob.iter().copied());
    }

    let mut module = Module::new();
    module.section(&types);
    module.section(&imports);
    module.section(&functions);
    module.section(&memories);
    module.section(&globals);
    module.section(&exports);
    module.section(&codes);
    module.section(&data);

    let bytes = module.finish();
    if std::env::var("LINLUA_DEBUG").is_err() {
        wasm_validate(&bytes).map_err(err)?;
    }
    Ok(bytes)
}

/// Wraps a compiled body into a wasm-encoder Function: the locals
/// section groups consecutive same-type slots.
fn finish_fn(fc: &FnCompiler, n_params: usize) -> Function {
    let mut groups: Vec<(u32, ValType)> = Vec::new();
    for ty in fc.local_types.iter().skip(n_params).copied() {
        match groups.last_mut() {
            Some((n, t)) if *t == ty => *n += 1,
            _ => groups.push((1, ty)),
        }
    }
    let mut f = Function::new(groups);
    for ins in &fc.code {
        f.instruction(ins);
    }
    f.instruction(&Ins::End);
    f
}

/// Validates `.wasm` bytes with wasmparser.
fn wasm_validate(bytes: &[u8]) -> Result<(), String> {
    use wasmparser::Validator;
    // The dialect needs no special features beyond the defaults.
    let mut v = Validator::new();
    v.validate_all(bytes).map(|_| ()).map_err(|e| e.to_string())
}

/// Shallow argument typing for signature unification.
fn shallow_type(e: &Expr) -> Option<Ty> {
    match e {
        Expr::True | Expr::False => Some(Ty::Bool),
        Expr::Num(NumLit::Int(_)) => Some(Ty::I64),
        Expr::Num(NumLit::Float(_)) => Some(Ty::F64),
        Expr::Str(_) => Some(Ty::Str),
        Expr::Paren(inner) => shallow_type(inner),
        Expr::Table(fields) if !fields.is_empty() => {
            let first = fields.first()?;
            match first {
                TableField::Item(item) => match shallow_type(item)? {
                    Ty::I64 => Some(Ty::ArrI64),
                    Ty::F64 => Some(Ty::ArrF64),
                    _ => None,
                },
                _ => None,
            }
        }
        _ => None,
    }
}

/// Every direct call to a user function, in source order.
fn collect_calls(chunk: &[Stmt]) -> Vec<(String, Vec<Expr>)> {
    let mut out = Vec::new();
    for stmt in chunk {
        collect_calls_stmt(stmt, &mut out);
    }
    out
}

fn collect_calls_stmt(stmt: &Stmt, out: &mut Vec<(String, Vec<Expr>)>) {
    match stmt {
        Stmt::Local { inits, .. } => {
            for e in inits {
                collect_calls_expr(e, out);
            }
        }
        Stmt::Assign { target: _, value } => collect_calls_expr(value, out),
        Stmt::Expr(e) => collect_calls_expr(e, out),
        Stmt::If {
            branches,
            otherwise,
        } => {
            for (cond, body) in branches {
                collect_calls_expr(cond, out);
                for s in body {
                    collect_calls_stmt(s, out);
                }
            }
            if let Some(b) = otherwise {
                for s in b {
                    collect_calls_stmt(s, out);
                }
            }
        }
        Stmt::While { cond, body } => {
            collect_calls_expr(cond, out);
            for s in body {
                collect_calls_stmt(s, out);
            }
        }
        Stmt::Repeat { body, until } => {
            for s in body {
                collect_calls_stmt(s, out);
            }
            collect_calls_expr(until, out);
        }
        Stmt::ForNum {
            start,
            limit,
            step,
            body,
            ..
        } => {
            for e in [start, limit].into_iter().chain(step.iter()) {
                collect_calls_expr(e, out);
            }
            for s in body {
                collect_calls_stmt(s, out);
            }
        }
        Stmt::Return(Some(e)) => collect_calls_expr(e, out),
        Stmt::Do(body) => {
            for s in body {
                collect_calls_stmt(s, out);
            }
        }
        _ => {}
    }
}

fn collect_calls_expr(e: &Expr, out: &mut Vec<(String, Vec<Expr>)>) {
    match e {
        Expr::Call { callee, args } => {
            if let Expr::Ident(n) = &**callee {
                out.push((n.clone(), args.clone()));
            }
            for a in args {
                collect_calls_expr(a, out);
            }
        }
        Expr::Paren(inner) | Expr::Unary(_, inner) => collect_calls_expr(inner, out),
        Expr::Binary(_, l, r) => {
            collect_calls_expr(l, out);
            collect_calls_expr(r, out);
        }
        Expr::Index { obj, index } => {
            collect_calls_expr(obj, out);
            collect_calls_expr(index, out);
        }
        Expr::Table(fields) => {
            for f in fields {
                match f {
                    TableField::Item(item) => collect_calls_expr(item, out),
                    TableField::Keyed { key, value } => {
                        collect_calls_expr(key, out);
                        collect_calls_expr(value, out);
                    }
                }
            }
        }
        _ => {}
    }
}

/// The return type of a function body: the first return statement's
/// expression type, typed with a light scope map.
fn body_ret(stmts: &[Stmt], scope: &mut HashMap<String, Ty>) -> Option<Ty> {
    for stmt in stmts {
        match stmt {
            Stmt::Return(Some(e)) => return light_type(e, scope),
            // The first return may hide inside a control block (fib).
            Stmt::If { branches, .. } => {
                for (_, body) in branches {
                    if let Some(t) = body_ret(body, scope) {
                        return Some(t);
                    }
                }
            }
            Stmt::While { body, .. }
            | Stmt::Repeat { body, .. }
            | Stmt::Do(body)
            | Stmt::ForNum { body, .. } => {
                // A cloned scope: assignments inside the block must
                // not retype the outer locals mid-walk.
                let mut inner = scope.clone();
                if let Some(t) = body_ret(body, &mut inner) {
                    return Some(t);
                }
            }
            Stmt::Local { names, inits, .. } => {
                for ((n, _), init) in names.iter().zip(inits.iter()) {
                    let ty = light_type(init, scope).unwrap_or(Ty::I64);
                    scope.insert(n.clone(), ty);
                }
            }
            Stmt::Assign {
                target: Target::Name(n),
                value,
            } => {
                if let Some(ty) = light_type(value, scope) {
                    scope.insert(n.clone(), ty);
                }
            }
            _ => {}
        }
    }
    None
}

/// Light expression typing for return-type inference.
fn light_type(e: &Expr, scope: &mut HashMap<String, Ty>) -> Option<Ty> {
    match e {
        Expr::True | Expr::False => Some(Ty::Bool),
        Expr::Num(NumLit::Int(_)) => Some(Ty::I64),
        Expr::Num(NumLit::Float(_)) => Some(Ty::F64),
        Expr::Str(_) => Some(Ty::Str),
        Expr::Paren(inner) => light_type(inner, scope),
        Expr::Ident(n) => scope.get(n).copied(),
        Expr::Binary(op, l, r) => match op {
            BinOp::Concat => Some(Ty::Str),
            BinOp::Eq
            | BinOp::NotEq
            | BinOp::Lt
            | BinOp::Gt
            | BinOp::Le
            | BinOp::Ge
            | BinOp::And
            | BinOp::Or => Some(Ty::Bool),
            BinOp::Div | BinOp::Pow => Some(Ty::F64),
            _ => {
                let lt = light_type(l, scope);
                let rt = light_type(r, scope);
                match (lt, rt) {
                    (Some(Ty::I64), Some(Ty::I64)) => Some(Ty::I64),
                    _ => Some(Ty::F64),
                }
            }
        },
        Expr::Unary(UnOp::Len, _) => Some(Ty::I64),
        Expr::Unary(UnOp::Not, _) => Some(Ty::Bool),
        Expr::Unary(_, inner) => light_type(inner, scope),
        // Typed array indexing yields the element type.
        Expr::Index { obj, .. } => match scope.get(&match &**obj {
            Expr::Ident(n) => n.clone(),
            _ => String::new(),
        }) {
            Some(Ty::ArrI64) => Some(Ty::I64),
            Some(Ty::ArrF64) => Some(Ty::F64),
            _ => None,
        },
        _ => None,
    }
}
