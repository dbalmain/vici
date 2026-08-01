# vici

A headless vi editing core in Rust. Modes, motions, operators, counts, text
objects, visual mode, undo, dot-repeat and macros — with no view attached.

[![crates.io](https://img.shields.io/crates/v/vici.svg)](https://crates.io/crates/vici)
[![docs.rs](https://docs.rs/vici/badge.svg)](https://docs.rs/vici)

```rust
use vici::Editor;

let mut ed = Editor::from_text("select id, name\nfrom users");
ed.type_keys("cwSELECT<Esc>").unwrap();
assert_eq!(ed.buffer().to_string(), "SELECT id, name\nfrom users");
```

## Why another one

Every other Rust vi implementation I looked at owns something it shouldn't: a
widget, a viewport, or a whole application framework. That is fine until you
want vi keybindings _plus_ syntax highlighting, linting and completions in the
same pane — at which point the editor and the language tooling are fighting over
who owns the buffer.

`vici` owns the buffer and nothing else. It has no rendering, no terminal, no
filesystem, no async, and no opinion about how text is displayed. The entire
interface is one function:

```rust
fn handle_key(&mut self, key: Key) -> Vec<Effect>
```

`Effect::Edit` carries a change in exactly the shape tree-sitter and LSP want,
so incremental reparsing is a `From` impl in your code rather than a dependency
in this crate's tree:

```rust
impl From<vici::Edit> for tree_sitter::InputEdit {
    fn from(e: vici::Edit) -> Self { /* field for field */ }
}
```

Cursor position, mode and selection are all queryable, so they aren't duplicated
into effects. Effects are only the things the core genuinely cannot do itself.

## Layers

Each is ignorant of the ones above it, and each is usable on its own.

| Layer      | Knows about              | Does not know about         |
| ---------- | ------------------------ | --------------------------- |
| `Buffer`   | bytes, rows, ropes       | modes, keys, undo           |
| `History`  | changes and their rows   | ropes, keys, rendering      |
| `Document` | both of the above        | modes, keys, rendering      |
| `Keymap`   | key sequences → bindings | buffers, cursors            |
| `Pending`  | vi's command grammar     | buffers, cursors            |
| `Editor`   | all of the above         | rendering, terminals, files |

Undo is a trait, not a policy. `LinearHistory` is the default; `NoHistory`
discards; an undo tree is yours to write. Because a change is self-inverting and
records its own row, `U` (undo all changes on the current line) works — which it
does not in CodeMirror, and therefore not in Obsidian.

Keymaps are four layers mirroring vi's `nmap`/`omap`/`vmap`/`imap`, with
fallback, which is how `i` can mean _insert_ in normal mode and _inner_ after an
operator. Bindings are plain data — no closures — so a keymap can be
deserialised from config without redesigning anything.

## What's implemented

Normal, insert, replace and visual (character and line) modes. Motions
`h j k l 0 ^ $ w W b B e E f F t T ; , G gg`. Operators `d c y` over motions,
doubled (`dd`), and text objects `iw aw i( a( i" a" ip` and friends. Counts,
including the multiplication in `2d3w`. `x X r ~ J p P`, all six insert entries,
undo, redo, `U`, dot-repeat and macros.

Dot-repeat and macros are both implemented as key replay, so they cost almost
nothing and behave correctly for free — including `.` after an insert session.

## What isn't

- **Search and `:` execution.** `/ ? n N :` resolve and emit effects, but there
  is not yet an API for the host to hand a match back. Next on the list.
- **Registers and marks.** One unnamed register, deliberately. This is a core
  for self-contained single-buffer editing, not a vi clone.
- **Visual block**, `%`, `H`/`M`/`L`, and the `D C s S` shortcuts.
- **Display width.** The sticky column for `j`/`k` counts graphemes, not
  terminal cells, so it diverges from vi on tabs and CJK. Width is the view's
  knowledge; `motion.rs` documents where a layout trait would plug in.

## Coordinates

Everything public in the buffer layer is a **byte** offset, and every `Point`
column is a byte offset within its row. That is not a convenience — it is what
makes an `Edit` convert field-for-field into `tree_sitter::InputEdit`.

Grapheme space (so `l` and `x` step over combining marks and emoji as one unit)
belongs to the motion layer. Display-width space belongs to the view. Holding
this crate to one coordinate system is what stops those from leaking into each
other.

Rows are counted by LF only, matching tree-sitter. Line endings are preserved
byte-for-byte; a `\r` is ordinary content at the end of a row.

## Status

Early but tested — the test suite reads as keystroke scripts, which is the main
argument for the headless shape:

```rust
assert_eq!(typed("one two", "ciwX<Esc>w."), "X X");
assert_eq!(typed("aa\nbb\ncc", "qa~jq@a@a"), "Aa\nBb\nCc");
```

The API will change. A `vici-ratatui` widget crate is planned.

## License

MIT OR Apache-2.0, at your option.
