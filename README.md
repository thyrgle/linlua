# linlua

A Luau-flavored Lua subset with **gradual types** and **gradual linear
memory** — the linjs architecture applied to Lua.

Same thesis as [linjs](https://github.com/thyrgle/linjs): dynamic code
runs as-is; annotations opt hot paths into native-speed execution.

```lua
-- @own
local a = {1, 2, 3, 4}
local total: number = 0
for i = 0, #a - 1 do
  total += a[i + 1]  -- 1-based, like Lua
end
print(total, #a)
```

- **Luau-style types** — `local x: number = 0`, `{number}` arrays,
  `any` — checked statically, erased at runtime.
- **`-- @own` / `-- @ref` comments** — arena semantics in linear
  memory: ownership inferred for the rest.
- **Semantics are Lua 5.4's** — integer/float subtyping, floored
  division, single `nil`, truthiness where only `nil` and `false` are
  falsy. Every fixture is differential-tested against a real
  `lua5.4`.

## Roadmap

1. **Lua front-end** — lexer, parser, interpreter; `lua5.4` as the
   semantics oracle. **Done.**
2. **Memory + types** — `-- @own`/`-- @ref` arenas with enforced
   moves, borrows, and ownership inference, plus Luau-style type
   checking (`linlua check`). **Done.**
3. **WASM backend** — the strict dialect in linear memory,
   checksum-gated against the interpreter and `lua5.4`. **Done.**
4. **QBE** — the strict dialect IR compiled to native assembly via
   [QBE](https://c9x.me/compile/); the wasm module and the native
   binary are two thin backends over one IR. **Done** — the compiled
   WASM module lowers to QBE IL, assembles, and links against a small
   C runtime: fib(30) runs ~130× faster than the interpreter.
5. **Python kernels** — a third front-end on the same engine.

## Native via QBE

The compiled module IS the portable IR. `qbe::wasm_to_qbe` lowers it
to [QBE](https://c9x.me/compile/) IL; `qbe` assembles; `cc` links
against ~30 lines of C runtime. One program, two engines:

```sh
cargo run --release --example qbedbg   # or: linlua::run_native(src)
```

```text
compile+cc: 102ms
native fib(30) x5: 55ms total      (~11ms per run)
interp fib(30) x5: 7.13s total     (~1.43s per run)
```

Lowering rules worth knowing: every wasm i32 promotes to QBE `l`
(addresses/lengths are small positives, so 32-bit wraparound never
matters); comparison results stay word-sized; memarg offsets become
explicit address adds; structured control flow becomes labeled blocks
with jumps; `if`-with-result assigns one temporary in both arms and
lets QBE's SSA construction insert the phi; linear memory is a static
16 MiB image with the data segments baked in and `pow`/`floor` lower
to libm calls. The C runtime formats floats with `printf("%.14g")` —
the very same call Lua itself makes, so the ties round identically.

The native output is checksum-gated against the interpreter AND
`lua5.4` for every fixture, same as the WASM path.

## The WASM strict dialect

`linlua::compile_to_wasm` compiles a kernel-shaped subset to a real
module: numbers keep Lua's integer/float subtyping (i64/f64 with the
interpreter's arithmetic propagation), sequences live in linear
memory (length header + 1-based elements), and `print` arrives
through typed host imports — `logi` for integers, `logf` for floats,
with the host formatting floats as `%.14g`, ties rounded half-to-even,
matching C and Lua exactly.

Not in the dialect (rejected at compile time): closures, first-class
functions, varargs, `for..in`, globals as variables, string
concatenation, keyed tables, nil. Sequences have a fixed length —
stores beyond it trap (the one documented divergence from Lua's
growing tables).

The proof: every fixture runs on the interpreter AND as a compiled
module in Node — byte-identical — and the unannotated core runs
against a real `lua5.4` too. Three engines, one checksum.

## The type layer

Luau-style annotations, checked statically, erased at runtime:

```lua
local count: number = 0
local names: {string} = {"a", "b"}
local xs: {{number}} = {{1}, {2}}

local function total(xs: {number}): number
  local sum = 0
  for i = 1, #xs do
    sum = sum + xs[i]
  end
  return sum
end
```

- The type language: `nil`, `number`, `string`, `boolean`, `any`,
  and `{T}` arrays. Unannotated code is inferred where the
  initializer is obvious and treated as `any` elsewhere — `any` is
  compatible with everything, so plain Lua never errors.
- `linlua check file.lua` renders the diagnostics and exits 1 on
  anything wrong.
- Types never change what a program does; the test suite pins
  erasure (typed and untyped forms run identically).
- One honest trade: annotated syntax is Luau-flavored, so annotated
  programs are outside the `lua5.4` differential's scope (real Lua
  cannot parse `:` annotations). The dynamic core keeps full
  differential coverage; the checker and erasure tests pin the rest.

## The memory layer

A comment opts a declaration into linear memory:

```lua
-- @own
local a = {10, 20, 30}   -- an arena-owned sequence
-- @ref
local r = a              -- a read-only view; the owner stays bound
```

The rules, enforced at runtime:

- **Moves**: aliasing an owned value (`local b = a`) moves it — the
  source becomes a tombstone, and using it is an error.
- **Borrows**: every write through a `-- @ref` fails, even after the
  reference is copied; calls borrow their owned arguments, so a callee
  that writes through its parameter fails loudly.
- **Escapes**: an owned value cannot be returned, stored in a
  container, or nested inside another owned value.
- **Inference**: unannotated sequence tables that provably never
  escape allocate into the arena anyway — `linlua run` infers by
  default (`--no-infer` to disable). Inference is output-invisible,
  and the differential suite pins that against `lua5.4`. One
  documented limit: programs that print table *addresses* are outside
  the contract (GC and arena storage use different, equally arbitrary
  address formats).

## Running it

```sh
cargo test   # the test suite is the demo
cargo run -- program.lua
```

The differential tests shell out to `lua5.4`; they skip gracefully
when it is absent.

License: MIT OR Apache-2.0.
