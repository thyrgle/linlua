//! The QBE backend: the compiled WASM module IS the portable IR.
//!
//! `wasm_to_qbe` lowers a strict-dialect module to QBE IL text; `qbe`
//! turns that into assembly and `cc` links it against a small C
//! runtime (where `printf("%.14g")` gives Lua's float formatting by
//! construction — it is the very same C library call Lua makes).
//!
//! Lowering rules:
//! - Every wasm i32 promotes to QBE `l`: addresses and lengths are
//!   small positives, so 32-bit wraparound never matters. `i32.store`
//!   becomes `storel` (8 bytes) — layout-compatible because both the
//!   string blob and the sequence header carry 4 bytes of padding.
//! - Comparison results stay `w` on the operand stack (QBE compares
//!   produce words); `jnz` consumes them directly, and boolean locals
//!   zero-extend to `l`.
//! - Structured control flow (block/loop/if + br depths) lowers to
//!   labeled blocks with explicit jumps; `if` with a result assigns
//!   one temporary in both arms and lets QBE's SSA construction phi
//!   them.
//! - Linear memory is a static 16 MiB image (`$mem`) with the data
//!   segments baked in; the bump pointer is `$bump`. Growth
//!   disappears (the image is the document size).
//! - `print` lands in the C runtime; `math.huge` etc. are constants;
//!   `^` lowers to a call to libm's pow.

use std::collections::HashMap;
use std::fmt::Write as _;

use wasmparser::{Parser, Payload, ValType as WValType};

/// The C runtime: print with Lua's exact formatting, pow from libm.
pub const RT_C: &str = r#"#include <stdio.h>
#include <string.h>
#include <math.h>

static int first = 1;

static void sep(void) { if (!first) printf("\t"); first = 0; }

void rt_logi(long long v) {
    sep();
    printf("%lld", v);
}

void rt_logf(double v) {
    char buf[64];
    snprintf(buf, sizeof buf, "%.14g", v);
    if (!strpbrk(buf, ".eEn")) strcat(buf, ".0");
    sep();
    printf("%s", buf);
}

void rt_logb(long long v) {
    sep();
    printf(v ? "true" : "false");
}

void rt_logstr(char *p) {
    int len = *(int *)p;
    sep();
    fwrite(p + 8, 1, len, stdout);
}

void rt_logtab(void) {}
void rt_logflush(void) { printf("\n"); first = 1; }

double rt_pow(double a, double b) { return pow(a, b); }

double rt_floor(double x) { return floor(x); }
"#;

const MEM_BYTES: usize = 16 * 1024 * 1024;
const MEM_PAGES: u32 = (MEM_BYTES / 65536) as u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Qt {
    L,
    W,
    D,
}

impl Qt {
    fn letter(self) -> char {
        match self {
            Qt::L => 'l',
            Qt::W => 'w',
            Qt::D => 'd',
        }
    }
}

#[derive(Clone)]
struct FuncInfo {
    params: Vec<WValType>,
    results: Vec<WValType>,
    qname: String,
}

struct DataSeg {
    offset: usize,
    bytes: Vec<u8>,
}

/// Lowers a compiled strict-dialect module to QBE IL.
pub fn wasm_to_qbe(wasm: &[u8]) -> Result<String, String> {
    // ---- pass 1: module shape ----
    let mut funcs: Vec<FuncInfo> = Vec::new(); // imports first, then defined
    let mut types: Vec<(Vec<WValType>, Vec<WValType>)> = Vec::new();
    let mut defined: Vec<(u64, usize)> = Vec::new(); // (func idx, type idx)
    let mut data_segs: Vec<DataSeg> = Vec::new();
    let mut run_idx: Option<u64> = None;
    let mut bump_init: i64 = 1024;

    for payload in Parser::new(0).parse_all(wasm) {
        match payload.map_err(|e| e.to_string())? {
            Payload::TypeSection(t) => {
                for ty in t {
                    let rec = ty.map_err(|e| e.to_string())?;
                    // One plain func per rec group in this dialect.
                    for subty in rec.types() {
                        if let wasmparser::CompositeInnerType::Func(f) = &subty.composite_type.inner
                        {
                            types.push((f.params().to_vec(), f.results().to_vec()));
                        }
                    }
                }
            }
            Payload::ImportSection(s) => {
                for imports in s {
                    let imports = imports.map_err(|e| e.to_string())?;
                    for imp in imports {
                        let (_, i) = imp.map_err(|e| e.to_string())?;
                        if let wasmparser::TypeRef::Func(t) = &i.ty {
                            let qname = match (i.module, i.name) {
                                ("env", "logi") => "$rt_logi",
                                ("env", "logf") => "$rt_logf",
                                ("env", "logb") => "$rt_logb",
                                ("env", "logstr") => "$rt_logstr",
                                ("env", "logtab") => "$rt_logtab",
                                ("env", "logflush") => "$rt_logflush",
                                ("env", "pow") => "$rt_pow",
                                (m, n) => return Err(format!("unknown import {m}.{n}")),
                            }
                            .to_string();
                            funcs.push(FuncInfo {
                                params: types[*t as usize].0.clone(),
                                results: types[*t as usize].1.clone(),
                                qname,
                            });
                        }
                    }
                }
            }
            Payload::FunctionSection(s) => {
                for t in s {
                    let t = t.map_err(|e| e.to_string())?;
                    let idx = funcs.len() as u32;
                    defined.push((idx as u64, t as usize));
                    funcs.push(FuncInfo {
                        params: types[t as usize].0.clone(),
                        results: types[t as usize].1.clone(),
                        qname: format!("$f{idx}"),
                    });
                }
            }
            Payload::GlobalSection(s) => {
                for g in s {
                    let g = g.map_err(|e| e.to_string())?;
                    let mut rdr = g.init_expr.get_binary_reader();
                    let n = rdr.bytes_remaining();
                    if let Ok(raw) = rdr.read_bytes(n) {
                        if raw.len() >= 2 && raw[0] == 0x41 {
                            let (v, _) = read_i64_leb(&raw[1..]);
                            bump_init = v;
                        }
                    }
                }
            }
            Payload::ExportSection(s) => {
                for e in s {
                    let e = e.map_err(|e| e.to_string())?;
                    if e.name == "run" && matches!(e.kind, wasmparser::ExternalKind::Func) {
                        run_idx = Some(e.index as u64);
                    }
                }
            }
            Payload::DataSection(s) => {
                for d in s {
                    let d = d.map_err(|e| e.to_string())?;
                    if let wasmparser::DataKind::Active { offset_expr, .. } = d.kind {
                        let mut rdr = offset_expr.get_binary_reader();
                        let n = rdr.bytes_remaining();
                        if let Ok(raw) = rdr.read_bytes(n) {
                            if raw.len() >= 2 && raw[0] == 0x41 {
                                let (off, _) = read_i64_leb(&raw[1..]);
                                data_segs.push(DataSeg {
                                    offset: off as usize,
                                    bytes: d.data.to_vec(),
                                });
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let run_export_idx = run_idx.ok_or("no run export")?;

    // ---- the memory image and globals ----
    let mut out = String::new();
    let mut image = vec![0u8; MEM_BYTES];
    for seg in &data_segs {
        let end = seg.offset + seg.bytes.len();
        if end > MEM_BYTES {
            return Err("data segment exceeds the memory image".into());
        }
        image[seg.offset..end].copy_from_slice(&seg.bytes);
    }
    write!(out, "data $mem = {{ ").unwrap();
    emit_blob(&mut out, &image);
    writeln!(out, " }}").unwrap();
    writeln!(out, "data $bump = {{ l {bump_init} }}").unwrap();

    // ---- pass 2: lower each defined function ----
    let mut ncode = 0usize;
    for payload in Parser::new(0).parse_all(wasm) {
        if let Payload::CodeSectionEntry(body) = payload.map_err(|e| e.to_string())? {
            let (idx, type_idx) = defined[ncode];
            let sig = &types[type_idx];
            let info = &funcs[idx as usize];
            let exported = run_export_idx == idx;
            let ret = match sig.1.first() {
                Some(WValType::F64) => "d ",
                Some(_) => "l ",
                None => "",
            };
            let params: Vec<String> = sig
                .0
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let letter = if *t == WValType::F64 { 'd' } else { 'l' };
                    format!("{letter} %v{i}")
                })
                .collect();
            let kw = if exported {
                "export function"
            } else {
                "function"
            };
            writeln!(out, "{kw} {ret}{}({}) {{", info.qname, params.join(", ")).unwrap();

            lower_body(&mut out, &body, sig, &funcs);

            writeln!(out, "}}").unwrap();
            ncode += 1;
        }
    }

    // ---- main: the entry point calls $run ----
    writeln!(out, "export function w $main() {{").unwrap();
    writeln!(out, "@start").unwrap();
    writeln!(out, "\tcall {}()", funcs[run_export_idx as usize].qname).unwrap();
    writeln!(out, "\tret 0").unwrap();
    writeln!(out, "}}").unwrap();

    Ok(out)
}

/// Reads a signed LEB128 (wasm const exprs).
fn read_i64_leb(bytes: &[u8]) -> (i64, usize) {
    let mut v: i64 = 0;
    let mut shift = 0;
    let mut i = 0;
    loop {
        let byte = bytes[i];
        i += 1;
        v |= ((byte & 0x7f) as i64) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            if shift < 64 && byte & 0x40 != 0 {
                v |= -1i64 << shift;
            }
            break;
        }
    }
    (v, i)
}

/// The image as alternating zero runs and byte strings.
fn emit_blob(out: &mut String, image: &[u8]) {
    let mut i = 0;
    let mut first = true;
    while i < image.len() {
        if image[i] == 0 {
            let start = i;
            while i < image.len() && image[i] == 0 {
                i += 1;
            }
            if !first {
                out.push_str(", ");
            }
            first = false;
            write!(out, "z {}", i - start).unwrap();
        } else {
            let start = i;
            while i < image.len() && image[i] != 0 {
                i += 1;
            }
            if !first {
                out.push_str(", ");
            }
            first = false;
            write!(out, "b \"").unwrap();
            for byte in &image[start..i] {
                if (0x20..0x7f).contains(byte) && *byte != b'"' && *byte != b'\\' {
                    out.push(*byte as char);
                } else {
                    write!(out, "\\{:03o}", byte).unwrap();
                }
            }
            out.push('"');
        }
    }
    if first {
        out.push_str("z 1");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Block,
    Loop,
    If,
}

struct Ctl {
    kind: Kind,
    /// `br` target: loop top / block-or-if end.
    br_label: String,
    /// Label after the construct (None for loops).
    end_label: Option<String>,
}

struct IfState {
    end: String,
    else_label: String,
    res: Option<(String, Qt)>,
    else_seen: bool,
}

struct Lower<'o> {
    out: &'o mut String,
    tmp: u32,
    lbl: u32,
    dead_pending: bool,
    locals: HashMap<u32, (String, Qt)>,
    stack: Vec<(String, Qt)>,
    ctl: Vec<Ctl>,
    if_stack: Vec<IfState>,
    funcs: Vec<FuncInfo>,
    results: Vec<WValType>,
}

impl<'o> Lower<'o> {
    fn fresh_tmp(&mut self) -> String {
        self.tmp += 1;
        format!("t{}", self.tmp)
    }

    fn fresh_label(&mut self, prefix: &str) -> String {
        self.lbl += 1;
        format!("@{prefix}{}", self.lbl)
    }

    fn emit(&mut self, line: String) {
        if self.dead_pending {
            let l = self.fresh_label("dead");
            writeln!(self.out, "{l}").unwrap();
            self.dead_pending = false;
        }
        writeln!(self.out, "\t{line}").unwrap();
    }

    fn emit_label(&mut self, label: &str) {
        writeln!(self.out, "{label}").unwrap();
        self.dead_pending = false;
    }

    fn jump_only(&mut self, line: String) {
        if self.dead_pending {
            let l = self.fresh_label("dead");
            writeln!(self.out, "{l}").unwrap();
        }
        writeln!(self.out, "\t{line}").unwrap();
        self.dead_pending = true;
    }

    fn push_new(&mut self, ty: Qt) -> String {
        let t = self.fresh_tmp();
        self.stack.push((t.clone(), ty));
        t
    }

    fn pop(&mut self) -> (String, Qt) {
        self.stack.pop().expect("wasm stack underflow")
    }

    fn assign(&mut self, dst: String, dqt: Qt, src: String, sqt: Qt) {
        if dst == src {
            return;
        }
        match (dqt, sqt) {
            (Qt::L, Qt::W) => self.emit(format!("%{dst} =l extuw %{src}")),
            (Qt::D, Qt::L) => self.emit(format!("%{dst} =d sltof %{src}")),
            (Qt::L, Qt::D) => self.emit(format!("%{dst} =l dtosl %{src}")),
            _ => self.emit(format!("%{dst} ={} copy %{src}", dqt.letter())),
        }
    }

    fn br_target(&self, depth: u32) -> Result<String, String> {
        let n = self.ctl.len();
        if depth as usize >= n {
            return Err("br depth out of range".into());
        }
        Ok(self.ctl[n - 1 - depth as usize].br_label.clone())
    }

    fn op(&mut self, op: &wasmparser::Operator) -> Result<(), String> {
        use wasmparser::Operator as O;
        match op {
            O::I32Const { value } => {
                let t = self.fresh_tmp();
                self.emit(format!("%{t} =l copy {value}"));
                self.stack.push((t, Qt::L));
            }
            O::I64Const { value } => {
                let t = self.fresh_tmp();
                self.emit(format!("%{t} =l copy {value}"));
                self.stack.push((t, Qt::L));
            }
            O::F64Const { value } => {
                let t = self.fresh_tmp();
                let text = float_lit(f64::from_bits(value.bits()));
                self.emit(format!("%{t} =d copy d_{text}"));
                self.stack.push((t, Qt::D));
            }
            O::LocalGet { local_index } => {
                let (name, qt) = self.locals[local_index].clone();
                self.stack.push((name, qt));
            }
            O::LocalSet { local_index } => {
                let (v, vqt) = self.pop();
                let (lname, lqt) = self.locals[local_index].clone();
                self.assign(lname, lqt, v, vqt);
            }
            O::LocalTee { local_index } => {
                let (v, vqt) = self.pop();
                let (lname, lqt) = self.locals[local_index].clone();
                self.assign(lname.clone(), lqt, v, vqt);
                self.stack.push((lname, lqt));
            }
            O::GlobalGet { global_index: 0 } => {
                let a = self.fresh_tmp();
                self.emit(format!("%{a} =l copy $bump"));
                let t = self.push_new(Qt::L);
                self.emit(format!("%{t} =l loadl %{a}"));
            }
            O::GlobalSet { global_index: 0 } => {
                let (v, _) = self.pop();
                let a = self.fresh_tmp();
                self.emit(format!("%{a} =l copy $bump"));
                self.emit(format!("storel %{v}, %{a}"));
            }
            O::I32Add | O::I64Add => self.binop("add"),
            O::I32Or => self.binop("or"),
            O::I32Mul | O::I64Mul => self.binop("mul"),
            O::I64Sub => self.binop("sub"),
            O::I32Shl | O::I64Shl => self.binop("shl"),
            O::I64ShrS => self.binop("sar"),
            O::I64And => self.binop("and"),
            O::I64Or => self.binop("or"),
            O::I64Xor => self.binop("xor"),
            O::I64DivS => self.binop("div"),
            O::I64RemS => self.binop("rem"),
            O::I32GtU => self.cmpop("cugtl"),
            O::I64LtS => self.cmpop("csltl"),
            O::I64GtS => self.cmpop("csgtl"),
            O::I64LeS => self.cmpop("cslel"),
            O::I64GeS => self.cmpop("csgel"),
            O::I64Eq | O::I32Eq => self.cmpop("ceql"),
            O::I64Eqz | O::I32Eqz => {
                let (a, qt) = self.pop();
                let t = self.push_new(Qt::W);
                let c = if qt == Qt::W { "ceqw" } else { "ceql" };
                self.emit(format!("%{t} =w {c} %{a}, 0"));
            }
            O::F64Add => self.binop("add"),
            O::F64Sub => self.binop("sub"),
            O::F64Mul => self.binop("mul"),
            O::F64Div => self.binop("div"),
            O::F64Eq => self.cmpop("ceqd"),
            O::F64Lt => self.cmpop("cltd"),
            O::F64Gt => self.cmpop("cgtd"),
            O::F64Le => self.cmpop("cled"),
            O::F64Ge => self.cmpop("cged"),
            O::F64Floor => {
                let (a, _) = self.pop();
                let t = self.push_new(Qt::D);
                self.emit(format!("%{t} =d call $rt_floor(d %{a})"));
            }
            O::F64ConvertI64S => {
                let (a, _) = self.pop();
                let t = self.push_new(Qt::D);
                self.emit(format!("%{t} =d sltof %{a}"));
            }
            O::I32WrapI64 | O::I64ExtendI32U => {
                let (a, qt) = self.pop();
                if qt == Qt::W {
                    let t = self.push_new(Qt::L);
                    self.emit(format!("%{t} =l extuw %{a}"));
                } else {
                    self.stack.push((a, Qt::L));
                }
            }
            O::I32Load { memarg } | O::I64Load { memarg } => {
                let (a, _) = self.pop();
                let base = self.fresh_tmp();
                if memarg.offset == 0 {
                    self.emit(format!("%{base} =l add $mem, %{a}"));
                } else {
                    let off = self.fresh_tmp();
                    self.emit(format!("%{off} =l add $mem, {}", memarg.offset));
                    self.emit(format!("%{base} =l add %{off}, %{a}"));
                }
                let t = self.push_new(Qt::L);
                self.emit(format!("%{t} =l loadl %{base}"));
            }
            O::F64Load { memarg } => {
                let (a, _) = self.pop();
                let base = self.fresh_tmp();
                if memarg.offset == 0 {
                    self.emit(format!("%{base} =l add $mem, %{a}"));
                } else {
                    let off = self.fresh_tmp();
                    self.emit(format!("%{off} =l add $mem, {}", memarg.offset));
                    self.emit(format!("%{base} =l add %{off}, %{a}"));
                }
                let t = self.push_new(Qt::D);
                self.emit(format!("%{t} =d loadd %{base}"));
            }
            O::I32Store { memarg } | O::I64Store { memarg } => {
                let (v, _) = self.pop();
                let (a, _) = self.pop();
                let base = self.fresh_tmp();
                if memarg.offset == 0 {
                    self.emit(format!("%{base} =l add $mem, %{a}"));
                } else {
                    let off = self.fresh_tmp();
                    self.emit(format!("%{off} =l add $mem, {}", memarg.offset));
                    self.emit(format!("%{base} =l add %{off}, %{a}"));
                }
                self.emit(format!("storel %{v}, %{base}"));
            }
            O::F64Store { memarg } => {
                let (v, _) = self.pop();
                let (a, _) = self.pop();
                let base = self.fresh_tmp();
                if memarg.offset == 0 {
                    self.emit(format!("%{base} =l add $mem, %{a}"));
                } else {
                    let off = self.fresh_tmp();
                    self.emit(format!("%{off} =l add $mem, {}", memarg.offset));
                    self.emit(format!("%{base} =l add %{off}, %{a}"));
                }
                self.emit(format!("stored %{v}, %{base}"));
            }
            O::MemorySize { .. } => {
                let t = self.push_new(Qt::L);
                self.emit(format!("%{t} =l copy {MEM_PAGES}"));
            }
            O::MemoryGrow { .. } => {
                self.pop();
                let t = self.push_new(Qt::L);
                self.emit(format!("%{t} =l copy 0"));
            }
            O::Call { function_index } => {
                let info = self.funcs[*function_index as usize].clone();
                let n = info.params.len();
                if self.stack.len() < n {
                    return Err("call underflow".into());
                }
                let args: Vec<(String, Qt)> = self.stack.split_off(self.stack.len() - n);
                // logstr takes a raw memory offset; the host needs a
                // real pointer.
                let is_logstr = info.qname == "$rt_logstr";
                let list: Vec<String> = args
                    .iter()
                    .enumerate()
                    .map(|(i, (v, t))| {
                        if is_logstr && i == 0 {
                            let p = self.fresh_tmp();
                            self.emit(format!("%{p} =l add $mem, %{v}"));
                            format!("l %{p}")
                        } else {
                            format!("{} %{v}", t.letter())
                        }
                    })
                    .collect();
                match info.results.first() {
                    Some(WValType::F64) => {
                        let t = self.push_new(Qt::D);
                        self.emit(format!("%{t} =d call {}({})", info.qname, list.join(", ")));
                    }
                    Some(_) => {
                        let t = self.push_new(Qt::L);
                        self.emit(format!("%{t} =l call {}({})", info.qname, list.join(", ")));
                    }
                    None => self.emit(format!("call {}({})", info.qname, list.join(", "))),
                }
            }
            O::Return => {
                if self.results.is_empty() {
                    self.jump_only("ret".into());
                } else {
                    let (v, _) = self.pop();
                    self.jump_only(format!("ret %{v}"));
                }
            }
            O::Unreachable => self.jump_only("hlt".into()),
            O::Drop => {
                self.pop();
            }
            O::Block { .. } => {
                let end = self.fresh_label("b");
                self.ctl.push(Ctl {
                    kind: Kind::Block,
                    br_label: end.clone(),
                    end_label: Some(end),
                });
            }
            O::Loop { .. } => {
                let top = self.fresh_label("l");
                self.emit_label(&top);
                self.ctl.push(Ctl {
                    kind: Kind::Loop,
                    br_label: top,
                    end_label: None,
                });
            }
            O::If { blockty } => {
                let (c, _) = self.pop();
                let then = self.fresh_label("then");
                let els = self.fresh_label("else");
                let end = self.fresh_label("end");
                self.jump_only(format!("jnz %{c}, {then}, {els}"));
                self.emit_label(&then);
                let else_label = els.clone();
                // A result type: one temp assigned in both arms —
                // QBE's SSA construction inserts the phi.
                let res = match blockty {
                    wasmparser::BlockType::Type(WValType::F64) => {
                        let r = self.fresh_tmp();
                        Some((r, Qt::D))
                    }
                    wasmparser::BlockType::Type(_) => {
                        let r = self.fresh_tmp();
                        Some((r, Qt::L))
                    }
                    _ => None,
                };
                self.ctl.push(Ctl {
                    kind: Kind::If,
                    br_label: end.clone(),
                    end_label: Some(end.clone()),
                });
                self.if_stack.push(IfState {
                    end,
                    else_label,
                    res,
                    else_seen: false,
                });
            }
            O::Else => {
                let mut st = self.if_stack.pop().expect("else without if");
                if let Some((r, qt)) = &st.res {
                    let (v, vqt) = self.pop();
                    self.assign(r.clone(), *qt, v, vqt);
                }
                self.jump_only(format!("jmp {}", st.end));
                self.emit_label(&st.else_label);
                st.else_seen = true;
                self.if_stack.push(st);
                return Ok(());
            }
            O::End => {
                // An If's end carries the else label + phi; blocks and
                // loops close plainly.
                // The function body's own final End has no control
                // entry — it just terminates the function.
                if self.ctl.is_empty() {
                    return Ok(());
                }
                let is_if = matches!(self.ctl.last().map(|c| c.kind), Some(Kind::If));
                if is_if && !self.if_stack.is_empty() {
                    let st = self.if_stack.pop().unwrap();
                    self.ctl.pop();
                    if let Some((r, qt)) = &st.res {
                        let (v, vqt) = self.pop();
                        self.assign(r.clone(), *qt, v, vqt);
                        self.stack.push((r.clone(), *qt));
                    }
                    if !st.else_seen {
                        // The then arm needs its exit jump, and the
                        // else label must exist (jnz targets it).
                        self.jump_only(format!("jmp {}", st.end));
                        self.emit_label(&st.else_label);
                    }
                    self.emit_label(&st.end);
                    return Ok(());
                }
                let ctl = self.ctl.pop().expect("end without block");
                if let Some(end) = ctl.end_label {
                    self.emit_label(&end);
                }
            }
            O::Br { relative_depth } => {
                let label = self.br_target(*relative_depth)?;
                self.jump_only(format!("jmp {label}"));
            }
            O::BrIf { relative_depth } => {
                let label = self.br_target(*relative_depth)?;
                let (c, _) = self.pop();
                let fall = self.fresh_label("fall");
                self.jump_only(format!("jnz %{c}, {label}, {fall}"));
                self.emit_label(&fall);
            }
            other => return Err(format!("unlowered operator: {other:?}")),
        }
        Ok(())
    }

    fn binop(&mut self, qop: &str) {
        let (b, bt) = self.pop();
        let (a, at) = self.pop();
        // Word operands (comparison results) stay word-sized; the
        // promoted dialect otherwise lives in longs/doubles.
        let ty = if at == Qt::D || bt == Qt::D {
            Qt::D
        } else if at == Qt::W || bt == Qt::W {
            Qt::W
        } else {
            Qt::L
        };
        let t = self.push_new(ty);
        self.emit(format!("%{t} ={} {qop} %{a}, %{b}", ty.letter()));
    }

    fn cmpop(&mut self, qop: &str) {
        let (b, _) = self.pop();
        let (a, _) = self.pop();
        let t = self.push_new(Qt::W);
        self.emit(format!("%{t} =w {qop} %{a}, %{b}"));
    }
}

fn lower_body(
    out: &mut String,
    body: &wasmparser::FunctionBody,
    sig: &(Vec<WValType>, Vec<WValType>),
    funcs: &[FuncInfo],
) {
    let mut locals = HashMap::new();
    for (i, t) in sig.0.iter().enumerate() {
        locals.insert(i as u32, (format!("v{i}"), wasm_qt(*t)));
    }
    let mut next = sig.0.len() as u32;
    let mut lr = body.get_locals_reader().expect("locals");
    let groups = lr.get_count();
    for _ in 0..groups {
        let (n, t) = lr.read().expect("local group");
        for _ in 0..n {
            locals.insert(next, (format!("v{next}"), wasm_qt(t)));
            next += 1;
        }
    }
    writeln!(out, "@start").unwrap();
    let mut lw = Lower {
        out,
        tmp: 0,
        lbl: 0,
        dead_pending: false,
        locals,
        stack: Vec::new(),
        ctl: Vec::new(),
        if_stack: Vec::new(),
        funcs: funcs.to_vec(),
        results: sig.1.clone(),
    };
    let mut ops = body.get_operators_reader().expect("ops");
    while !ops.eof() {
        let off = ops.original_position();
        match ops.read() {
            Ok(op) => {
                if let Err(e) = lw.op(&op) {
                    panic!("qbe lower at 0x{off:03x}: {e}");
                }
            }
            Err(e) => panic!("qbe lower read: {e}"),
        }
    }
    // The function end: the wasm stack top is the return value.
    if !lw.dead_pending {
        if !sig.1.is_empty() {
            let (v, _) = lw.pop();
            lw.jump_only(format!("ret %{v}"));
        } else {
            lw.jump_only("ret".into());
        }
    }
}

fn wasm_qt(t: WValType) -> Qt {
    match t {
        WValType::F64 => Qt::D,
        _ => Qt::L,
    }
}

/// QBE float constants use the `d_` prefix and a decimal form.
fn float_lit(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let s = format!("{x}");
    if s.contains(['.', 'e']) {
        s
    } else {
        format!("{s}.0")
    }
}

/// Assembles the native binary: wasm -> QBE IL -> assembly -> cc.
pub fn compile_native(source: &str) -> Result<Vec<u8>, String> {
    let wasm = crate::compile_to_wasm(source).map_err(|e| e.message)?;
    let il = wasm_to_qbe(&wasm)?;
    let qbe = std::env::var("LINLUA_QBE").unwrap_or_else(|_| "qbe".into());
    let cc = std::env::var("LINLUA_CC").unwrap_or_else(|_| "cc".into());

    let dir = std::env::temp_dir().join("linlua_native");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let base = dir.join(format!("linlua_{id}"));
    std::fs::write(format!("{}.ssa", base.display()), &il).map_err(|e| e.to_string())?;
    std::fs::write(format!("{}_rt.c", base.display()), RT_C).map_err(|e| e.to_string())?;

    let asm = format!("{}.s", base.display());
    let st = std::process::Command::new(&qbe)
        .arg(format!("{}.ssa", base.display()))
        .arg("-o")
        .arg(&asm)
        .output()
        .map_err(|e| format!("cannot run {qbe}: {e}"))?;
    if !st.status.success() {
        return Err(format!(
            "qbe failed: {}",
            String::from_utf8_lossy(&st.stderr)
        ));
    }

    let bin = base.display().to_string();
    let st = std::process::Command::new(&cc)
        .args([&asm, &format!("{}_rt.c", base.display()), "-o", &bin, "-lm"])
        .output()
        .map_err(|e| format!("cannot run {cc}: {e}"))?;
    if !st.status.success() {
        return Err(format!(
            "cc failed: {}",
            String::from_utf8_lossy(&st.stderr)
        ));
    }
    std::fs::read(&bin).map_err(|e| e.to_string())
}

/// Runs a compiled native binary, returning its stdout.
pub fn run_native(source: &str) -> Result<String, String> {
    let bytes = compile_native(source)?;
    let dir = std::env::temp_dir().join("linlua_native");
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let bin = dir.join(format!("run_{id}"));
    std::fs::write(&bin, &bytes).map_err(|e| e.to_string())?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())?;
    let out = std::process::Command::new(&bin)
        .output()
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&bin);
    if !out.status.success() {
        return Err(format!(
            "native binary failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}
