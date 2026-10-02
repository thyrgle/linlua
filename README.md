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
   semantics oracle.
2. **Memory + types** — `-- @own`/`-- @ref` arenas and Luau-style type
   checking, both erased at runtime.
3. **WASM backend** — the strict dialect in linear memory,
   checksum-gated against the interpreter and `lua5.4`.
4. **QBE** — the strict dialect IR compiled to native assembly via
   [QBE](https://c9x.me/compile/); the wasm module and the native
   binary are two thin backends over one IR.
5. **Python kernels** — a third front-end on the same engine.

## Running it

```sh
cargo test   # the test suite is the demo
cargo run -- program.lua
```

The differential tests shell out to `lua5.4`; they skip gracefully
when it is absent.

License: MIT OR Apache-2.0.
