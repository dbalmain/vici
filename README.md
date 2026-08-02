# vici

A headless vi editing core in Rust. Modes, motions, operators, counts, text
objects, visual mode, undo, dot-repeat and macros — with no view attached.

[![crates.io](https://img.shields.io/crates/v/vici.svg)](https://crates.io/crates/vici)

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
| `History`  | changes                  | ropes, keys, rendering      |
| `Document` | both of the above        | modes, keys, rendering      |
| `Keymap`   | key sequences → bindings | buffers, cursors            |
| `Pending`  | vi's command grammar     | buffers, cursors            |
| `Editor`   | all of the above         | rendering, terminals, files |

Undo is a trait, not a policy. `LinearHistory` is the default; `NoHistory`
discards; an undo tree is yours to write. Changes are self-inverting, so a
history policy can hand back the changes needed to undo or redo a step.

Keymaps are four layers mirroring vi's `nmap`/`omap`/`vmap`/`imap`, with
fallback, which is how `i` can mean _insert_ in normal mode and _inner_ after an
operator. Bindings are plain data — no closures — so a keymap can be
deserialised from config without redesigning anything.

## What's implemented

Normal, insert, replace and visual (character and line) modes. Motions
`h j k l 0 ^ $ w W b B e E f F t T ; , G gg H M L %`. Operators `d c y > <` over
motions, doubled (`dd`, `>>`, `<<`), and text objects `iw aw i( a( i" a" ip` and
friends. Counts, including the multiplication in `2d3w`. `x X r ~ J p P`, all
six insert entries, undo, redo, dot-repeat and macros.

Shifting has to know what an indent is worth, so the host supplies an `Indent`:
shift width, tab width, and whether to render tabs. That is the one place
display width reaches the core, and it arrives as a parameter rather than as
ownership of layout. A host that expands tabs on screen must hand over the same
tab width it renders with, or `<<` removes something other than what you can
see.

The host also reports a `Viewport`: its first displayed buffer row and height.
That is a fact about the host's window, not layout the core owns. Supplying it
whenever the host renders or resizes lets page moves carry the caret and makes
`H`/`M`/`L` meaningful; stale viewport facts make those motions answer for the
wrong screen.

Dot-repeat and macros are both implemented as key replay, so they cost almost
nothing and behave correctly for free — including `.` after an insert session.

## What isn't

- **Search.** `/ ? n N` are not implemented. `:` opens a host prompt for ex
  command execution.
- **Registers and marks.** One unnamed register, deliberately. This is a core
  for self-contained single-buffer editing, not a vi clone.
- **Visual block** and the `s S` shortcuts.
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

## Trying it

```sh
cargo run -p vici-harness
```

A terminal harness that opens `FEATURES.txt` — a checklist of every bound key
with sample text to try it on. The right pane logs every effect as it is
emitted, which is the quickest way to see that typing produces one `Edit` per
keystroke while undo still treats the whole insert session as one step. `F10`
quits, `F2` hides the log.

The harness is deliberately in its own unpublished crate, so `vici` never needs
a UI dependency. It is also the honest demonstration of the layering: owning the
viewport, expanding tabs, measuring display width, answering `:` prompts and
touching the filesystem are all _its_ code, not the core's.

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
