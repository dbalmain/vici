# vici (JavaScript)

A headless vi editing core for JavaScript. Modes, motions, operators, counts,
text objects, visual mode, undo, dot-repeat, macros, marks and surround — with
no view attached.

```js
import { Editor } from "vici";

const editor = new Editor("select id, name\nfrom users");
editor.typeKeys("cwSELECT<Esc>");
editor.text(); // 'SELECT id, name\nfrom users'
```

This is a second implementation of the [Rust core](../README.md) in the same
repository, not a binding to it. The Rust crate is the behavioural oracle: this
engine passes all **411** of its fixture cases, rendering the same state block
character for character, and a differential fuzzer keeps it that way.

## Why not WASM

A WASM build of the Rust core is a fine thing to ship, and this package is not
an argument against it. It is an argument that a host which already runs
JavaScript should not pay to cross a boundary for an editing core this small.
The comparison below is against the sibling [beetle](https://github.com/)
experiment's TypeScript engine, which implements the same contract; its WASM
column is in that project's own report.

## Design

Three decisions account for most of the difference from a straightforward port.

**The buffer stores UTF-8 bytes.** The contract is byte offsets — every cursor,
edit, mark and register position is a UTF-8 byte offset, because that is the
shape `tree_sitter::InputEdit` wants. An engine that stores a JavaScript string
must convert on every offset it hands out, on every keystroke, forever. Storing
UTF-8 in a `Uint8Array` gap buffer makes the offsets _be_ the storage, and turns
the row index, word motions and search into byte arithmetic. The cost lands on
`text()`, which decodes — so hosts should read it when they need to render, not
in a loop.

**ASCII is a fast path, not an assumption.** The buffer keeps a count of
non-ASCII bytes. While it is zero, grapheme columns are subtraction; when it is
not, only the rows that actually contain wide text pay for segmentation. The
engine stays correct for `🇦🇺` and combining marks either way — those cases are
in the fixture suite.

**`Intl.Segmenter` is built on first need.** Constructing one costs tens of
milliseconds of ICU startup, which for most editors is the largest single item
in their time-to-first-keystroke. An all-ASCII buffer never constructs one.

Two smaller ones: a key _is_ its canonical vi spelling (`"a"`, `"<C-r>"`,
`"<Esc>"`), so the keymap is a `Map` probe and dot-repeat storage is a
`join('')`; and the row index carries a pending shift from a pivot, so an edit
does not rewrite every row after it.

## Results

Node v24.18.1, AMD Ryzen 9 9955HX, linux/x64. Both engines in one process, same
workloads, same harness; p50 of bulk `typeKeys`. Full tables, including per-key
dispatch and p95, are in [reports/bench.md](reports/bench.md).

| Workload                              |    vici-js |    beetle vici-js |      |
| ------------------------------------- | ---------: | ----------------: | ---- |
| cold start (fresh process)            |  **10 ms** | 92 ms<sup>†</sup> | 9×   |
| `insert-1k` — type 1 000 characters   | **737 µs** |           8.41 ms | 11×  |
| `words-small` — `10w10b3dw`, 1 KiB    |  **14 µs** |             25 µs | 1.8× |
| `words-100k` — `50w50b`, 100 KiB      | **9.9 µs** |             33 µs | 3.3× |
| `words-1m` — `50w50b`, 1 MiB          |  **14 µs** |             35 µs | 2.5× |
| `delete-word` — 200 × `dw`, 100 KiB   | **428 µs** |           4.03 ms | 9.4× |
| `undo-storm` — 100 inserts, 100 undos | **305 µs** |           1.66 ms | 5.4× |
| `macro` — `qa~jq200@a`                | **391 µs** |            815 µs | 2.1× |
| `search` — `/needle<CR>nnn`, 100 KiB  | **382 µs** |           1.15 ms | 3.0× |
| `operator-all` — `ggdG`, 100 KiB      |      28 µs |        **7.7 µs** | 0.3× |
| `edit-session` — mixed script, 19 KiB | **112 µs** |           1.85 ms | 17×  |

<sup>†</sup> the sibling project's published cold-start figure, measured the
same way on the same machine; the others are measured here, side by side.

`operator-all` is the honest loss, and it is the trade-off working as designed:
deleting a whole buffer is where a JavaScript string wins, because V8 makes
`slice` and substring nearly free, while a byte buffer has to copy what it
displaced. Everything that touches offsets per keystroke goes the other way.

For scale, the same project measured its Rust-in-WASM build at 244 µs on
`edit-session` and 27 ms on `search`; this engine is faster than that WASM build
on both.

### Size

`esbuild --bundle --minify --format=esm`, then brotli — what a browser
downloads. Same pipeline for both. See [reports/size.md](reports/size.md).

| Artifact       |          raw |         gzip |       brotli |
| -------------- | -----------: | -----------: | -----------: |
| vici-js        | **40.9 KiB** | **13.2 KiB** | **12.0 KiB** |
| beetle vici-js |     47.9 KiB |     14.3 KiB |     12.9 KiB |

## Staying in step with the Rust core

Two mechanisms, both cheap to run.

**The fixture oracle.** `crates/vici/tests/fixtures/editor.vici` holds 411 cases
and `tests/snapshots/` holds the state each one produces. `npm test` runs every
case through this engine and compares the rendered block — text, cursor, mode,
selection, register, history depth, jumps, marks, pending keys, last change,
recording, and the full effect stream. Nothing is skipped and there is no
allowlist, so a divergence in any observable fails the suite.

**The differential fuzzer.** `npm run fuzz` generates random buffers and random
keystroke scripts, replays them through the Rust core
(`cargo run --example replay`) and diffs the same blocks:

```sh
npm run fuzz -- --cases 5000 --seed 7
```

Because the generated cases are written in `editor.vici` format, a divergence is
directly pasteable into the fixture file, where it becomes a permanent
regression test for _both_ engines. The generator is seeded, so a reported seed
reproduces a run exactly.

## Running it

```sh
npm install
npm test           # 411 oracle cases + layer tests, no dependencies at runtime
npm run check      # typecheck the JSDoc types
npm run types      # emit types/*.d.ts
npm run bench      # speed, to stdout
npm run size       # bundle size, to stdout
npm run fuzz       # differential fuzz against the Rust core (needs cargo)
```

`npm run bench` and `npm run size` accept `--vs <entry>` to measure another
engine that exposes `createEngine(text)` or an `Editor` class through the same
`typeKeys` / `handleKey` interface.

## API

```js
const editor = new Editor(text); // or createEditor(text)
editor.handleKey("<C-r>"); // → Effect[]
editor.handleKeys(["d", "w"]);
editor.typeKeys("2d3w"); // vi notation

editor.text(); // decode the buffer
editor.cursor; // byte offset
editor.cursorPoint(); // { row, col }, col in bytes
editor.mode; // NORMAL | INSERT | REPLACE | VISUAL | VISUAL_LINE
editor.selection(); // [start, end] | null
editor.register.text; // decoded on demand
editor.mark("a"); // byte offset | null
editor.setViewport({ topRow, height }); // whenever the host renders or resizes
editor.setIndent({ shiftWidth, tabWidth, useTabs });
editor.setText(text);
editor.jumpTo(offset); // host-side navigation, clamped
```

Effects are the things the core cannot do itself. Everything else — cursor,
mode, selection — is queryable, so it is not duplicated into the stream:

```js
{
  type: "edit", edit;
} // startByte/oldEndByte/newEndByte + points
{
  type: "mode", mode;
}
{
  type: "scroll", scroll;
}
{
  type: "prompt";
} // `:`
{
  type: "bell";
}
{
  type: "recordingStarted", register;
}
{
  type: "recordingStopped", register;
}
```

`edit` converts field for field into `tree_sitter.InputEdit`, which is the whole
reason the coordinates are bytes.

## Known divergences

None are reachable from the fixture suite or from 5 000 fuzzed cases, but they
are where to look first if one turns up.

- **Unicode tables.** Segmentation uses `Intl.Segmenter` where Rust uses
  `unicode-segmentation`; character classes use Unicode property escapes
  (`\p{Alphabetic}`, `\p{White_Space}`, `\p{Uppercase}`) where Rust uses its own
  tables. A Unicode version skew between the JavaScript engine and the Rust
  crate could show up on code points neither suite exercises.
- **Case mapping.** `recase` maps one code point at a time with
  `toUpperCase`/`toLowerCase`, which matches Rust's `char::to_uppercase` /
  `to_lowercase` including the one-to-many forms (`ß` → `SS`). Applying them to
  a single code point also avoids JavaScript's context-sensitive final-sigma
  rule, which Rust's per-character mapping does not have.
- **Search.** The smartcase policy is identical and matching is literal, as in
  the Rust core. An ASCII pattern over a buffer that also contains non-ASCII
  takes a byte path that folds ASCII case only, so a pattern of `k` will not
  match a Kelvin sign the way the general path would.

## Next

- A WASM build of the Rust core behind this same interface, so the fuzzer can
  run in a browser as well as against a native binary.
- Publishing both engines to npm under one name, with the WASM build as an
  optional entry point.
- `crates/vici/examples/replay.rs` duplicates the fixture parser and renderer
  from `tests/editor_cases.rs`. Those want to be one module in the crate, so
  both the snapshot test and the differential harness share it.

## License

MIT OR Apache-2.0, at your option.
