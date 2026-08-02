# vici — roadmap

Written 2026-08-02, at `c27f95e`. Safe to read cold: it says where things stand,
what was decided and why, and what to build next in enough detail to start.

## Where things stand

226 tests. `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test --workspace` all clean. `origin/main` is at `19588e6`; `c27f95e` is
local.

Since `ceaa3fb`:

| commit    | what                                                        |
| --------- | ----------------------------------------------------------- |
| `17a6a99` | insert-mode `<C-o>`, counted text objects, `gu`/`gU`/`g~`   |
| `4a155e1` | `>>` and `<<`, with a host-supplied `Indent`                |
| `ae1a34f` | host-reported `Viewport`; paging carries the caret; `H M L` |
| `9b9bbb7` | text objects seek forward when the cursor is outside a pair |
| `2105069` | fix: linewise operators that reach the last row             |
| `4430c97` | inner block shrinks when its delimiters own their rows      |
| `3813303` | `%`                                                         |
| `19588e6` | `.` repeats a visual operator                               |

`stash@{0}` holds a reverted experiment: per-row `U` snapshots, so `U` could
reach a row you had wandered off and come back to. It worked and cost about
twenty lines, but was dropped as not worth it. `U` itself has since gone the
same way, so the stash is now of historical interest only.

## Decisions already taken — don't relitigate these

- **ropey stays on the latest release**, currently `2.0.0-beta.1`. Not a blocker
  for publishing 0.1.0.
- **A trailing newline yields a phantom final row.** `"aa\nbb\ncc\n"` is four
  rows, the last empty, so `G` lands below the last line of the file and `G>>`,
  `Gx`, and `GgUU` do nothing there. Rows are newline-_separated_ here, where vi
  treats a newline as a terminator. Known, measured, accepted.
- **`.` after a characterwise visual selection re-aims the motion** rather than
  repeating a character count, so `vwd` then `.` deletes to the next word
  boundary where vi deletes the same number of characters. Matching vi means
  giving visual mode a second, geometry-based repeat path alongside key replay —
  about fifty lines and a hole in the "everything is key replay" story.
- **A count on `i"` is ignored**, where vi treats any count above one as
  "include the quotes". Documented in FEATURES §8; not worth the code.
- **`3.` replays the whole command three times** rather than substituting the
  count. Differs from vi, documented in FEATURES §11.
- **`s`/`S` remain unbound** deliberately, as do named registers — one unnamed
  register is the design.
- **FEATURES.txt ticks are Dave's**, recorded by walking the harness. An agent
  must not tick them: a tick asserts a walk that did not happen. Add entries
  unticked. (`1f4d2f3` isolates a batch that arrived pre-ticked, if the
  checklist ever needs resetting: `git revert 1f4d2f3`.)

## Position store and jump list

Remembered positions that survive edits are editor navigation state, so the
store lives in `Editor`, not `Document`. `Editor::edit` and `Editor::revert`
shift entries through each applied `Edit`; `set_text` clears them because it
replaces the whole buffer. Normal-mode `<C-o>`/`<C-i>` navigate the jump list,
and `Editor::jump_to(offset)` is the public host landing move.

Marks are deliberately a separate slice. They share the position arithmetic but
should not make the document layer own upper-layer navigation state.

## Then: search

Search is not implemented. Build it as a core-resolved motion rather than a
host-resolved prompt façade.

Make search **core-resolved, not host-resolved**:

- `Motion::Search`, not a `Command`.
- The prompt becomes an await-a-line state in `Pending`, exactly as `f` awaits
  one char.
- Matching goes behind a `Matcher` trait with a literal + smartcase default, so
  the crate still does not depend on `regex`.

Then `d/foo<CR>`, `3n`, `.` and macros all work for free, because the pattern is
just more keys and all three of those are key replay. The previous shape
dead-ended precisely because it tried to round-trip through the host
mid-command. `n`/`N` push to the jump list from the previous step.

## Cheap, whenever

- **`{` and `}`** paragraph motions. The blank-row machinery already exists for
  `ip`/`ap`, so this is one match arm in `motion::resolve` and two bindings.

## Bigger, later

- **Visual block.** The expensive one: it touches every operator.
- **surround.vim.** Cheaper than it looks — `motion::object_span` already
  returns both the inner and around spans, and the delimiters are the set
  difference. Two non-contiguous edits in one undo step is already solved, since
  `Editor::run` groups every command; apply the closing delimiter first so the
  opening insert does not shift it. The real cost is that **`ys` cannot be
  bound**: `y` resolves to an operator at depth one, so the keymap never looks
  up `ys` as a two-key sequence. Either surround takes a different prefix, or
  `Pending` learns that an operator can also be a prefix — which is what vim
  does, with a timeout. `ys` must have `yanks() == false` despite the `y`. About
  a day; `t` for HTML tags wants a real scan and can wait.
- **leap.vim.** Needs the viewport (done), the jump list, and the matcher from
  search. Operator-pending works via the trick `Motion::RepeatFind` already
  uses: let `Pending` consume the label keypress and stash the resolved offset.
  Its dot-repeat is the first genuine break in "everything is key replay" — vim
  re-searches the two-char pattern rather than replaying the label. **Build it
  as a separate `vici-leap` crate against the public API.** If that turns out to
  be impossible, the missing seam is a more interesting finding than another
  built-in, and a better README argument than the one currently made in prose.

## How to check vi fidelity

Worth keeping: every behaviour question in this stretch was settled by running
real vim rather than reasoning about it, and it repeatedly overturned confident
guesses.

```fish
# one shot, cursor forced to line 1
printf 'x (a (b) c) y\n' > in.txt
vim -u NONE -N -es -c 'set sw=4 ts=8 expandtab' -c 'call cursor(1,1)' \
    -c 'normal! d%' -c 'wq! out.txt' in.txt
```

Three traps, all of which bit:

- **`vim -es` starts the cursor on the last line.** Always
  `-c 'call cursor(1,1)'` first, or multi-row probes silently test something
  else.
- **`-c "normal! <C-d>"` sends the literal text**, not the key. Use
  `-c 'exe "normal! \<C-d>"'`.
- **`vim -es` has no window**, so `H`/`M`/`L` and paging need `nvim --headless`
  instead, which has a real 80×24 screen. `line("w0")` and `line("w$")` report
  the visible range.

To read what an object actually _covers_, don't infer it from a delete — select
it and read the marks, which also tells you whether vi made it linewise:

```fish
nvim --headless -u NONE -n -c 'call cursor(1,1)' -c 'exe "normal! vi{\<Esc>"' \
  -c 'call writefile([visualmode()." ".string(getpos("''<")[1:2])], "out")' -c 'q!' in.txt
```

Diff a table of cases against the same table run through `Editor::type_keys` in
a throwaway `crates/vici/tests/probe.rs`, then delete it.

## Notes for whoever picks this up

- Dave's shell is **fish**. Shell snippets for him need fish syntax.
- Commit when the work is done; **do not push** — he pushes when ready.
- The harness is the acceptance test: drive it over a pty, then `:w` and read
  the file from disk. Screen-scraping is unreliable because ratatui only
  repaints changed cells.
- Session memory is keyed to the project's directory path, so nothing remembered
  under `~/w/headless-vi` follows the rename to `~/w/vici`. That is why this is
  a file in the repo.
