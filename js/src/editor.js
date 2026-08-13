// The reducer: `(state, key) -> (state, effects)`.
//
// This is the only stateful type here. It owns the cursor, the mode, the
// pending parser, the register and the jump list, and it drives the document
// and the motion layer.
//
// # Why the reducer shape earns its keep
//
// Because a keystroke is the unit of input and every resolution hands back the
// keys it consumed, three features collapse into one mechanism: dot-repeat
// stores the keys of the last change and re-feeds them, macros store the keys
// between `q{reg}` and `q` and re-feed them, and tests are keystroke scripts.
// Replaying keys rather than re-executing commands is what makes `.` correct
// for `ciwfoo<Esc>` without any special handling of the typed text.

import * as C from './command.js';
import * as M from './motion.js';
import { Document } from './document.js';
import { shift, Register } from './buffer.js';
import { keys as parseKeys } from './keys.js';
import { vim } from './keymap.js';
import { PENDING, COMMAND, REJECTED, Pending } from './pending.js';
import { recase, isSpace } from './unicode.js';

/** How deep `.` and `@` may nest before the editor refuses, so a self-playing macro terminates. */
const MAX_REPLAY_DEPTH = 64;
/** Oldest jump entries are discarded once the list reaches this size. */
const MAX_JUMPS = 100;
/** Automatic marks occupy the slots after the lowercase user marks. */
const AUTO_MARKS = '<>[]^';

/** @typedef {import('./keys.js').Key} Key */
/** @typedef {import('./buffer.js').Edit} Edit */
/** @typedef {import('./motion.js').Span} Span */
/**
 * @typedef {{ type: 'edit', edit: Edit } | { type: 'mode', mode: number } |
 *   { type: 'scroll', scroll: number } | { type: 'prompt' } | { type: 'bell' } |
 *   { type: 'recordingStarted', register: string } |
 *   { type: 'recordingStopped', register: string }} Effect
 */

/** @type {Effect} */
const BELL = { type: 'bell' };

/**
 * @param {string} name
 * @returns {number} slot, or -1
 */
function markIndex(name) {
  if (name >= 'a' && name <= 'z') return name.charCodeAt(0) - 97;
  const auto = AUTO_MARKS.indexOf(name);
  return auto < 0 ? -1 : 26 + auto;
}

/**
 * The delimiters and padding convention selected by a surround key.
 * @param {string} delimiter
 * @returns {[string, string, boolean] | null}
 */
function surroundPair(delimiter) {
  switch (delimiter) {
    case '(':
      return ['(', ')', true];
    case '[':
      return ['[', ']', true];
    case '{':
      return ['{', '}', true];
    case '<':
      return ['<', '>', true];
    case ')':
      return ['(', ')', false];
    case ']':
      return ['[', ']', false];
    case '}':
      return ['{', '}', false];
    case '>':
      return ['<', '>', false];
    case '"':
    case "'":
    case '`':
      return [delimiter, delimiter, false];
    default:
      return null;
  }
}

/** Motions that cross enough of the buffer to deserve a return point. */
function pushesJump(motion) {
  switch (motion.k) {
    case M.GOTO_ROW:
    case M.GOTO_FIRST_ROW:
    case M.PARAGRAPH:
    case M.MATCH_PAIR:
    case M.SCREEN_TOP:
    case M.SCREEN_MIDDLE:
    case M.SCREEN_BOTTOM:
    case M.TO_OFFSET:
    case M.SEARCH:
    case M.REPEAT_SEARCH:
      return true;
    default:
      return false;
  }
}

/** A vi-like editor over a document. */
export class Editor {
  /**
   * @param {string} [text]
   * @param {import('./keymap.js').Keymap} [keymap]
   */
  constructor(text = '', keymap = vim()) {
    this.doc = new Document(text);
    this.keymap = keymap;
    /** Host-supplied indentation policy. Shift width, tab width, and whether to render tabs. */
    this.indent = { shiftWidth: 4, tabWidth: 8, useTabs: false };
    /** Host-supplied viewport facts. Height zero means no host has reported one. */
    this.viewport = { topRow: 0, height: 0 };
    this.pending = new Pending();
    this.mode = C.NORMAL;
    this.cursor = 0;
    /** Remembered column for `j`/`k`. `STICKY_END` means "row end". */
    this.sticky = 0;
    /** @type {number | null} Visual-mode anchor; the other end of the selection. */
    this.anchor = null;
    /** The unnamed register. */
    this.register = new Register(EMPTY_BYTES, false);
    /** @type {number[]} Places left by far motions and host-side jumps. */
    this.jumps = [];
    /** An entry being visited, or `jumps.length` when the caret is at the present. */
    this.jumpAt = 0;
    /** @type {(number | null)[]} Named and automatic positions, shifted through every edit. */
    this.marks = new Array(31).fill(null);
    /** @type {import('./motion.js').Find | null} */
    this.lastFind = null;
    /** @type {[string, boolean] | null} */
    this.lastSearch = null;
    /** @type {Key[]} Keys of the last buffer-changing command, for `.`. */
    this.lastChange = [];
    /** @type {Key[] | null} Keys accumulated while an insert session is open. */
    this.changeKeys = null;
    /** @type {Key[]} Keys that have shaped the current visual selection, for `.` to replay. */
    this.visualKeys = [];
    /** @type {{ register: string, script: Key[] } | null} */
    this.recording = null;
    /** @type {Map<string, Key[]>} */
    this.macros = new Map();
    this.replayDepth = 0;
    /** True while an insert session's undo group is open. */
    this.insertGroup = false;
    /**
     * Reused motion context: `resolve` never retains it.
     * @type {import('./motion.js').Context}
     */
    this.ctx = { sticky: 0, lastFind: null, lastSearch: null, viewport: this.viewport, bound: M.ON_CHAR };
  }

  // -- queries ---------------------------------------------------------

  /** @returns {import('./buffer.js').TextBuffer} */
  get buffer() {
    return this.doc.buffer;
  }

  /** @returns {string} */
  text() {
    return this.doc.buffer.toString();
  }

  /** Cursor position as a row and byte column. @returns {import('./buffer.js').Point} */
  cursorPoint() {
    return this.buffer.pointAt(this.cursor);
  }

  /**
   * The visual selection as a byte range, inclusive of the character under the
   * cursor — which is what makes `vd` delete what you can see.
   * @returns {[number, number] | null}
   */
  selection() {
    if (this.anchor === null) return null;
    const buf = this.buffer;
    const anchor = this.anchor;
    if (this.mode === C.VISUAL) {
      const start = Math.min(anchor, this.cursor);
      const end = Math.max(anchor, this.cursor);
      return [start, M.resolve(buf, end, RIGHT, null, this.#ctx(M.PAST_END)) ?? end];
    }
    if (this.mode === C.VISUAL_LINE) {
      return M.rowSpan(buf, buf.rowOf(Math.min(anchor, this.cursor)), buf.rowOf(Math.max(anchor, this.cursor)));
    }
    return null;
  }

  /**
   * The offset remembered under a named or automatic mark.
   * @param {string} name
   * @returns {number | null}
   */
  mark(name) {
    const index = markIndex(name);
    return index < 0 ? null : this.marks[index];
  }

  /** Keys of a partially-typed command, for a `showcmd` indicator. @returns {Key[]} */
  pendingKeys() {
    return this.pending.keys;
  }

  /**
   * Move to a host-supplied location, remembering the position left behind.
   * @param {number} offset
   */
  jumpTo(offset) {
    this.#pushJump();
    this.#place(offset);
  }

  /**
   * Replace the whole buffer, resetting cursor and history-independent state.
   * @param {string} text
   * @returns {Edit}
   */
  setText(text) {
    const edit = this.doc.replace(0, this.buffer.length, text);
    this.cursor = 0;
    this.sticky = 0;
    this.anchor = null;
    this.pending.reset();
    this.mode = C.NORMAL;
    this.jumps = [];
    this.jumpAt = 0;
    this.marks.fill(null);
    return edit;
  }

  /** @param {{ topRow: number, height: number }} viewport */
  setViewport(viewport) {
    this.viewport = viewport;
    this.ctx.viewport = viewport;
  }

  /** @param {{ shiftWidth: number, tabWidth: number, useTabs: boolean }} indent */
  setIndent(indent) {
    this.indent = indent;
  }

  // -- input -----------------------------------------------------------

  /**
   * Feed one key.
   * @param {Key} key
   * @returns {Effect[]}
   */
  handleKey(key) {
    // A bare `q` stops recording, so it must be caught before the parser can
    // treat it as "await a register". Recording is editor state, which is why
    // the parser cannot decide this.
    if (
      this.replayDepth === 0 &&
      this.mode === C.NORMAL &&
      this.recording !== null &&
      key === 'q' &&
      this.pending.idle
    ) {
      const { register, script } = this.recording;
      this.recording = null;
      this.macros.set(register, script);
      return [/** @type {Effect} */ ({ type: 'recordingStopped', register })];
    }

    // Record raw keys, not resolved commands. Replayed keys are excluded, so
    // `@a` inside a recording stores `@a` rather than its expansion.
    if (this.replayDepth === 0 && this.recording !== null) this.recording.script.push(key);
    if (this.changeKeys !== null) this.changeKeys.push(key);

    const resolution = this.pending.feed(key, this.mode, this.keymap);
    if (resolution.r === PENDING) return [];
    if (resolution.r === REJECTED) return [BELL];
    if (resolution.r !== COMMAND) return [];

    const command = resolution.command;
    const consumed = /** @type {Key[]} */ (resolution.keys);
    const wasVisual = C.isVisual(this.mode);
    const effects = this.#run(command, /** @type {number | null} */ (resolution.count));
    // Everything typed since the selection opened, so that `.` can replay the
    // shape and not just the operator. The operator's own key is not among
    // them: by the time it runs, visual mode is over.
    if (C.isVisual(this.mode)) {
      if (!wasVisual) this.visualKeys.length = 0;
      for (const k of consumed) this.visualKeys.push(k);
    }
    this.#noteChange(command, consumed);
    return effects;
  }

  /**
   * @param {readonly Key[]} sequence
   * @returns {Effect[]}
   */
  handleKeys(sequence) {
    /** @type {Effect[]} */
    const out = [];
    for (const key of sequence) {
      const effects = this.handleKey(key);
      for (const effect of effects) out.push(effect);
    }
    return out;
  }

  /**
   * Feed a key sequence written in vi notation.
   * @param {string} spec
   * @returns {Effect[]}
   */
  typeKeys(spec) {
    return this.handleKeys(parseKeys(spec));
  }

  // -- execution -------------------------------------------------------

  /**
   * @param {number} bound
   * @returns {import('./motion.js').Context}
   */
  #ctx(bound) {
    const ctx = this.ctx;
    ctx.sticky = this.sticky;
    ctx.lastFind = this.lastFind;
    ctx.lastSearch = this.lastSearch;
    ctx.bound = bound;
    return ctx;
  }

  /** @returns {number} */
  #bound() {
    return this.mode === C.INSERT || this.mode === C.REPLACE ? M.PAST_END : M.ON_CHAR;
  }

  /**
   * Apply `motion` `times` from `at`, under `bound`.
   * @param {number} at
   * @param {import('./motion.js').Motion} motion
   * @param {number} times
   * @param {number} bound
   * @returns {number}
   */
  #step(at, motion, times, bound) {
    return M.resolve(this.buffer, at, motion, times, this.#ctx(bound)) ?? at;
  }

  /**
   * @param {number} mode
   * @param {Effect[]} effects
   */
  #setMode(mode, effects) {
    if (this.mode === mode) return;
    this.mode = mode;
    this.cursor = M.clamp(this.buffer, this.cursor, this.#bound());
    effects.push({ type: 'mode', mode });
  }

  /**
   * @param {number} start
   * @param {number} end
   * @param {string} text
   * @param {Effect[]} effects
   */
  #edit(start, end, text, effects) {
    if (start === end && text === '') return;
    const edit = this.doc.replace(start, end, text);
    this.#shiftPositions(edit);
    effects.push({ type: 'edit', edit });
  }

  /**
   * Every command runs inside an undo group, bracketed by the caret on either
   * side so undo and redo can put it back where the user was. Groups nest, so
   * this is safe while an insert session's group is already open. An empty
   * group is never pushed, so non-editing commands cost nothing.
   * @param {any} command
   * @param {number | null} count
   * @returns {Effect[]}
   */
  #run(command, count) {
    this.doc.beginGroup(this.cursor);
    const effects = this.#dispatch(command, count);
    this.#rememberChange(effects);
    this.doc.endGroup(this.cursor);
    return effects;
  }

  /**
   * @param {any} command
   * @param {number | null} count
   * @returns {Effect[]}
   */
  #dispatch(command, count) {
    /** @type {Effect[]} */
    const effects = [];
    const repeat = count ?? 1;

    switch (command.c) {
      case C.MOVE: {
        const target = this.#resolveMark(command.motion);
        if (target === null) {
          effects.push(BELL);
          break;
        }
        // A submitted pattern becomes the last search even when it has no
        // match; `n` then repeats that same failed search, as vi does.
        this.#rememberSearch(target);
        const landed = M.resolve(this.buffer, this.cursor, target, count, this.#ctx(this.#bound()));
        if (landed === null) {
          effects.push(BELL);
          break;
        }
        if (landed !== this.cursor && pushesJump(target)) this.#pushJump();
        this.cursor = landed;
        if (target.k === M.FIND) this.lastFind = /** @type {import('./motion.js').Find} */ (target);
        // Vertical movement consumes the sticky column without changing it;
        // `$` sticks to row ends, so a subsequent `j` stays at the end.
        if (target.k === M.LAST_COLUMN) this.sticky = M.STICKY_END;
        else if (target.k !== M.UP && target.k !== M.DOWN) {
          this.sticky = M.graphemeCol(this.buffer, this.cursor);
        }
        break;
      }

      case C.OPERATE: {
        const target = command.target;
        if (target.t === C.T_MOTION) {
          if (target.motion.k === M.FIND) this.lastFind = target.motion;
          this.#rememberSearch(target.motion);
        }
        const span = this.#spanOf(command.operator, target, count);
        if (span === null) {
          effects.push(BELL);
          break;
        }
        this.#operate(command.operator, span, C.isVisual(this.mode) ? repeat : 1, effects);
        break;
      }

      case C.SELECT_OBJECT: {
        const span = M.objectSpan(this.buffer, this.cursor, command.around, command.object, repeat);
        if (span === null) {
          effects.push(BELL);
          break;
        }
        const start = span.lines ? this.buffer.rowStart(span.a) : span.a;
        const end = span.lines ? this.buffer.rowEnd(span.b) : span.b;
        this.anchor = start;
        this.#place(M.clamp(this.buffer, Math.max(end - 1, 0), M.ON_CHAR));
        break;
      }

      case C.ENTER_INSERT:
        this.#enterInsert(command.at, effects);
        break;

      case C.ENTER_REPLACE:
        this.#openInsertGroup();
        this.#setMode(C.REPLACE, effects);
        break;

      case C.ENTER_VISUAL:
        if (this.mode === command.kind) {
          this.#leaveVisual(true, effects);
        } else {
          this.anchor = this.cursor;
          this.#setMode(command.kind, effects);
        }
        break;

      case C.ENTER_NORMAL: {
        const leavingInsert = this.mode === C.INSERT || this.mode === C.REPLACE;
        this.#closeInsertGroup();
        if (leavingInsert) {
          // vi's insert cursor sits *between* characters, so leaving puts it on
          // the character to the left. This has to happen before the mode
          // switch: `#setMode` clamps to `ON_CHAR`, and doing both would move
          // the cursor twice.
          this.cursor = this.#step(this.cursor, LEFT, 1, M.PAST_END);
          this.#setMark('^', this.cursor);
        }
        if (C.isVisual(this.mode)) {
          this.#leaveVisual(true, effects);
        } else {
          this.anchor = null;
          this.#setMode(C.NORMAL, effects);
        }
        // The cursor moved, so the column `j`/`k` aim for has to follow it.
        if (leavingInsert) this.sticky = M.graphemeCol(this.buffer, this.cursor);
        break;
      }

      case C.DELETE_CHAR: {
        const start = command.before ? this.#step(this.cursor, LEFT, repeat, M.ON_CHAR) : this.cursor;
        const end = command.before ? this.cursor : this.#step(this.cursor, RIGHT, repeat, M.PAST_END);
        if (start === end) {
          effects.push(BELL);
          break;
        }
        this.#yank({ lines: false, a: start, b: end });
        this.#edit(start, end, '', effects);
        this.#place(start);
        break;
      }

      case C.REPLACE_CHAR: {
        const end = this.#step(this.cursor, RIGHT, repeat, M.PAST_END);
        if (end === this.cursor) effects.push(BELL);
        else this.#edit(this.cursor, end, command.text.repeat(repeat), effects);
        break;
      }

      case C.CHANGE_SURROUND:
        this.#changeSurround(command.from, command.to, effects);
        break;

      case C.DELETE_SURROUND:
        this.#deleteSurround(command.target, effects);
        break;

      case C.SURROUND_SELECTION:
        this.#surroundSelection(command.delimiter, effects);
        break;

      case C.SWAP_CASE: {
        const end = this.#step(this.cursor, RIGHT, repeat, M.PAST_END);
        if (end === this.cursor) {
          effects.push(BELL);
          break;
        }
        this.#edit(this.cursor, end, recase(this.buffer.textIn(this.cursor, end), 0), effects);
        this.#place(end);
        break;
      }

      case C.JOIN_ROWS:
        this.#joinRows(Math.max(repeat, 2), effects);
        break;

      case C.PUT:
        this.#put(command.before, repeat, effects);
        break;

      case C.UNDO:
        this.#revert(this.doc.undo(), effects);
        break;

      case C.REDO:
        this.#revert(this.doc.redo(), effects);
        break;

      case C.REPEAT: {
        const script = this.lastChange;
        if (script.length === 0) effects.push(BELL);
        else for (const effect of this.#replay(script, repeat)) effects.push(effect);
        break;
      }

      case C.RECORD_MACRO:
        this.recording = { register: command.register, script: [] };
        effects.push(/** @type {Effect} */ ({ type: 'recordingStarted', register: command.register }));
        break;

      case C.PLAY_MACRO: {
        const script = this.macros.get(command.register);
        if (script === undefined) effects.push(BELL);
        else for (const effect of this.#replay(script, repeat)) effects.push(effect);
        break;
      }

      case C.SET_MARK: {
        const index = markIndex(command.name);
        if (index < 0) effects.push(BELL);
        else this.marks[index] = this.cursor;
        break;
      }

      case C.JUMP_BACK:
        this.#jumpBack(effects);
        break;

      case C.JUMP_FORWARD:
        this.#jumpForward(effects);
        break;

      case C.SCROLL: {
        const scroll = command.scroll;
        if (this.viewport.height !== 0 && scroll <= C.PAGE_UP) {
          const height = this.viewport.height;
          const down = scroll === C.HALF_PAGE_DOWN || scroll === C.PAGE_DOWN;
          // vi preserves two rows of overlap between full pages.
          const rows =
            scroll <= C.HALF_PAGE_UP ? Math.max(height >> 1, 1) : Math.max(height - 2, 1);
          const landed = this.#step(this.cursor, down ? DOWN : UP, rows, this.#bound());
          if (landed !== this.cursor) {
            this.#pushJump();
            this.cursor = landed;
          }
        }
        effects.push({ type: 'scroll', scroll });
        break;
      }

      case C.COMMAND_PROMPT:
        effects.push({ type: 'prompt' });
        break;

      case C.INSERT_TEXT: {
        const text = command.text;
        const end =
          this.mode === C.REPLACE ? this.#step(this.cursor, RIGHT, 1, M.PAST_END) : this.cursor;
        this.#edit(this.cursor, end, text, effects);
        this.cursor += byteLength(text);
        this.sticky = M.graphemeCol(this.buffer, this.cursor);
        break;
      }

      case C.INSERT_NEWLINE:
        this.#edit(this.cursor, this.cursor, '\n', effects);
        this.cursor += 1;
        this.sticky = 0;
        break;

      case C.DELETE_BACK: {
        const start = this.#prevPosition();
        if (start === this.cursor) {
          effects.push(BELL);
          break;
        }
        this.#edit(start, this.cursor, '', effects);
        this.cursor = start;
        this.sticky = M.graphemeCol(this.buffer, this.cursor);
        break;
      }

      default: {
        const start = M.resolve(this.buffer, this.cursor, WORD_BACK, null, this.#ctx(M.PAST_END)) ?? this.cursor;
        if (start >= this.cursor) {
          effects.push(BELL);
          break;
        }
        this.#edit(start, this.cursor, '', effects);
        this.cursor = start;
        this.sticky = M.graphemeCol(this.buffer, this.cursor);
        break;
      }
    }

    return effects;
  }

  // -- helpers ---------------------------------------------------------

  /**
   * One position back, crossing a row boundary if need be — which plain `h`
   * deliberately will not do.
   * @returns {number}
   */
  #prevPosition() {
    const point = this.cursorPoint();
    if (point.col > 0) return this.#step(this.cursor, LEFT, 1, M.PAST_END);
    if (point.row === 0) return this.cursor;
    return this.buffer.rowContentEnd(point.row - 1);
  }

  /**
   * The grapheme immediately before an exclusive selection or edit endpoint.
   * `h` stops at a row boundary, but a linewise selection's end can be the
   * start of the following row.
   * @param {number} byte
   * @returns {number}
   */
  #previousGrapheme(byte) {
    const at = Math.min(byte, this.buffer.length);
    const point = this.buffer.pointAt(at);
    // The row above ends at its content end, not at `byte - 1`: a `\r\n`
    // terminator is two bytes and one grapheme.
    if (at > 0 && point.col === 0) return this.buffer.rowContentEnd(point.row - 1);
    return this.#step(at, LEFT, 1, M.PAST_END);
  }

  /**
   * @param {string} name
   * @param {number} offset
   */
  #setMark(name, offset) {
    const index = markIndex(name);
    if (index >= 0) this.marks[index] = offset;
  }

  /**
   * Remember the outer extent of every edit emitted by one command.
   * @param {Effect[]} effects
   */
  #rememberChange(effects) {
    /** @type {Edit[]} */
    const edits = [];
    for (const effect of effects) if (effect.type === 'edit') edits.push(effect.edit);
    if (edits.length === 0) return;
    // Shifting edits run from bottom to top, and each edit's coordinates are in
    // the buffer as it stood when that one happened, so carry its endpoints
    // through later edits before comparing the command's final extent.
    let start = Infinity;
    let end = 0;
    for (let i = 0; i < edits.length; i += 1) {
      let from = edits[i].startByte;
      let to = edits[i].newEndByte;
      for (let later = i + 1; later < edits.length; later += 1) {
        from = shift(edits[later], from);
        to = shift(edits[later], to);
      }
      start = Math.min(start, from);
      end = Math.max(end, to);
    }
    this.#setMark('[', start);
    this.#setMark(']', this.#previousGrapheme(end));
  }

  /** Capture visual marks before an operator changes the selection's buffer. */
  #rememberVisualSelection() {
    const selection = this.selection();
    if (selection === null) return;
    this.#setMark('<', selection[0]);
    this.#setMark('>', this.#previousGrapheme(selection[1]));
  }

  /**
   * The operator semantics `;` or `,` inherits from the remembered find.
   * Without this, an inclusive check has no direction to answer from and falls
   * back to exclusive, which silently costs `d;` a character.
   * @param {import('./motion.js').Motion} motion
   * @returns {[boolean, boolean]}
   */
  #semantics(motion) {
    if (motion.k === M.REPEAT_FIND && this.lastFind !== null) {
      const effective = {
        k: M.FIND,
        backward: this.lastFind.backward !== Boolean(motion.reverse),
        till: this.lastFind.till,
        target: this.lastFind.target,
      };
      return [M.isLinewise(effective), M.isInclusive(effective)];
    }
    return [M.isLinewise(motion), M.isInclusive(motion)];
  }

  /**
   * Turn an editor-owned mark into the pure resolver's concrete vocabulary.
   * @param {import('./motion.js').Motion} motion
   * @returns {import('./motion.js').Motion | null}
   */
  #resolveMark(motion) {
    if (motion.k !== M.MARK) return motion;
    // `''` and ``` `` ``` return to where the latest jump started, which is the
    // newest ring entry. Going there pushes the position being left in turn, so
    // repeating the key toggles between the two rather than walking further
    // back as `<C-o>` does.
    const name = /** @type {string} */ (motion.name);
    const offset =
      name === "'" || name === '`' ? (this.jumps.length > 0 ? this.jumps[this.jumps.length - 1] : null) : this.mark(name);
    if (offset === null || offset === undefined) return null;
    return { k: M.TO_OFFSET, offset, linewise: !motion.exact };
  }

  /**
   * @param {import('./motion.js').Motion} motion
   */
  #rememberSearch(motion) {
    if (motion.k === M.SEARCH) {
      this.lastSearch = [/** @type {string} */ (motion.pattern), Boolean(motion.backward)];
    }
  }

  /**
   * Reposition the cursor and refresh the remembered column with it. Anything
   * that moves the cursor *other than a motion* comes through here: leaving
   * `sticky` behind makes the next `j`/`k` aim at where the cursor used to be.
   * @param {number} byte
   */
  #place(byte) {
    this.cursor = M.clamp(this.buffer, byte, this.#bound());
    this.sticky = M.graphemeCol(this.buffer, this.cursor);
  }

  /** Remember the current position and discard destinations beyond it. */
  #pushJump() {
    this.jumps.length = this.jumpAt;
    this.jumps.push(this.cursor);
    if (this.jumps.length > MAX_JUMPS) this.jumps.shift();
    this.jumpAt = this.jumps.length;
  }

  /**
   * @param {Effect[]} effects
   */
  #jumpBack(effects) {
    if (this.jumps.length === 0) {
      effects.push(BELL);
      return;
    }
    if (this.jumpAt === this.jumps.length) {
      // Add the present so `<C-i>` can return here, then skip it to reach the
      // position before the jump that started this traversal.
      this.#pushJump();
      this.jumpAt = this.jumps.length - 2;
    } else if (this.jumpAt === 0) {
      effects.push(BELL);
      return;
    } else {
      this.jumpAt -= 1;
    }
    this.#place(this.jumps[this.jumpAt]);
  }

  /**
   * Return towards the present, ringing at the newest position as `#jumpBack`
   * does at the oldest.
   * @param {Effect[]} effects
   */
  #jumpForward(effects) {
    const next = this.jumpAt + 1;
    if (next >= this.jumps.length) {
      effects.push(BELL);
      return;
    }
    this.#place(this.jumps[next]);
    // Landing on the last entry means we are back at the present.
    this.jumpAt = next + 1 === this.jumps.length ? this.jumps.length : next;
  }

  /**
   * Shift every editor-owned remembered position through an applied edit.
   * @param {Edit} edit
   */
  #shiftPositions(edit) {
    for (let i = 0; i < this.jumps.length; i += 1) this.jumps[i] = shift(edit, this.jumps[i]);
    for (let i = 0; i < this.marks.length; i += 1) {
      const mark = this.marks[i];
      if (mark !== null) this.marks[i] = shift(edit, mark);
    }
  }

  /**
   * Apply the outcome of an undo or redo. The caret goes back to where the
   * history says it was; failing that, to the last edit's site.
   * @param {import('./document.js').Step} step
   * @param {Effect[]} effects
   */
  #revert(step, effects) {
    if (step.changes.length === 0) {
      effects.push(BELL);
      return;
    }
    for (const change of step.changes) {
      this.#shiftPositions(change.edit);
      effects.push({ type: 'edit', edit: change.edit });
    }
    this.#place(step.cursor ?? step.changes[step.changes.length - 1].edit.startByte);
  }

  /**
   * @param {Span} span
   */
  #yank(span) {
    const buf = this.buffer;
    const start = span.lines ? buf.rowStart(span.a) : span.a;
    const end = span.lines ? buf.rowEnd(span.b) : span.b;
    let bytes = buf.slice(start, end).slice();
    // A linewise yank always ends in a row break, even when taken from a file
    // whose final row has none.
    if (span.lines && (bytes.length === 0 || bytes[bytes.length - 1] !== 0x0a)) {
      const terminated = new Uint8Array(bytes.length + 1);
      terminated.set(bytes);
      terminated[bytes.length] = 0x0a;
      bytes = terminated;
    }
    this.register = new Register(bytes, span.lines);
    this.#setMark('[', start);
    this.#setMark(']', this.#previousGrapheme(end));
  }

  // -- operators -------------------------------------------------------

  /**
   * Resolve an operator's target to a span.
   * @param {number} operator
   * @param {any} target
   * @param {number | null} count
   * @returns {Span | null}
   */
  #spanOf(operator, target, count) {
    const buf = this.buffer;
    /** @type {Span | null} */
    let span = null;

    if (target.t === C.T_MOTION) {
      let motion = this.#resolveMark(target.motion);
      if (motion === null) return null;
      // vi's one famous irregularity: `cw` behaves like `ce`, so that changing
      // a word does not swallow the space after it.
      if (operator === C.CHANGE && motion.k === M.WORD_FORWARD) {
        motion = { k: M.WORD_END, big: motion.big };
      }
      // Resolution keeps the repeat-find form, because that is what tells the
      // resolver to skip the target a `t` is already parked on. Operator
      // semantics have to come from the concrete find it stands for.
      const [linewise, inclusive] = this.#semantics(motion);
      // An exclusive motion's landing place is the span's *end boundary*, not
      // somewhere the cursor has to be able to sit, so it may be one past the
      // last character — otherwise `dw` on the last word of the file leaves its
      // final character behind.
      const landed = M.resolve(buf, this.cursor, motion, count, this.#ctx(inclusive ? M.ON_CHAR : M.PAST_END));
      if (landed === null) return null;
      if (linewise) {
        span = {
          lines: true,
          a: buf.rowOf(Math.min(this.cursor, landed)),
          b: buf.rowOf(Math.max(this.cursor, landed)),
        };
      } else {
        let end = Math.max(this.cursor, landed);
        if (inclusive) end = M.resolve(buf, end, RIGHT, null, this.#ctx(M.PAST_END)) ?? end;
        span = { lines: false, a: Math.min(this.cursor, landed), b: end };
      }
    } else if (target.t === C.T_CURRENT_ROW) {
      const first = this.cursorPoint().row;
      span = { lines: true, a: first, b: Math.min(first + (count ?? 1) - 1, buf.rowCount - 1) };
    } else if (target.t === C.T_OBJECT) {
      span = M.objectSpan(buf, this.cursor, target.around, target.object, count ?? 1);
    } else if (this.mode === C.VISUAL) {
      const selection = this.selection();
      if (selection === null) return null;
      span = { lines: false, a: selection[0], b: selection[1] };
    } else if (this.mode === C.VISUAL_LINE) {
      const anchor = /** @type {number} */ (this.anchor);
      span = {
        lines: true,
        a: buf.rowOf(Math.min(anchor, this.cursor)),
        b: buf.rowOf(Math.max(anchor, this.cursor)),
      };
    }

    if (span === null) return null;
    if (C.forcesLinewise(operator) && !span.lines) {
      return { lines: true, a: buf.rowOf(span.a), b: buf.rowOf(Math.max(span.b - 1, span.a)) };
    }
    return span;
  }

  /**
   * @param {number} operator
   * @param {Span} span
   * @param {number} amount
   * @param {Effect[]} effects
   */
  #operate(operator, span, amount, effects) {
    const wasVisual = C.isVisual(this.mode);
    if (wasVisual) this.#rememberVisualSelection();
    const empty = span.lines ? this.buffer.length === 0 : span.a === span.b;
    if (empty && operator !== C.CHANGE && !C.forcesLinewise(operator)) {
      effects.push(BELL);
      // Still drop the selection: a no-op operator must not strand the editor
      // in visual mode, or the next keystroke is interpreted against a
      // selection the user thinks they have dismissed.
      if (C.isVisual(this.mode)) this.#leaveVisual(false, effects);
      return;
    }
    if (C.yanks(operator)) this.#yank(span);

    switch (operator) {
      case C.SHIFT_RIGHT:
      case C.SHIFT_LEFT: {
        this.#shiftRows(span.a, span.b, operator === C.SHIFT_RIGHT, amount, effects);
        this.cursor = this.#step(this.buffer.rowStart(span.a), FIRST_NON_BLANK, 1, M.ON_CHAR);
        break;
      }
      case C.LOWER:
      case C.UPPER:
      case C.SWAP: {
        const [start, end] = M.contentRange(this.buffer, span);
        const home = M.spanHome(this.buffer, span);
        const how = operator === C.LOWER ? -1 : operator === C.UPPER ? 1 : 0;
        this.#edit(start, end, recase(this.buffer.textIn(start, end), how), effects);
        this.cursor = M.clamp(this.buffer, home, this.#bound());
        if (span.lines) this.cursor = this.#step(this.cursor, FIRST_NON_BLANK, 1, M.ON_CHAR);
        break;
      }
      case C.YANK:
        this.cursor = M.clamp(this.buffer, M.spanHome(this.buffer, span), this.#bound());
        break;
      case C.DELETE: {
        const [start, end] = M.deleteRange(this.buffer, span);
        const home = M.spanHome(this.buffer, span);
        this.#edit(start, end, '', effects);
        this.cursor = M.clamp(this.buffer, home, this.#bound());
        if (span.lines) this.cursor = this.#step(this.cursor, FIRST_NON_BLANK, 1, M.ON_CHAR);
        break;
      }
      default: {
        // Linewise change empties the rows but keeps one, so insert begins on a
        // blank row rather than joining the next one up. That is exactly the
        // content range: it stops short of the terminator.
        const [start, end] = M.contentRange(this.buffer, span);
        this.#edit(start, end, '', effects);
        this.cursor = start;
        this.#openInsertGroup();
        this.#setMode(C.INSERT, effects);
        break;
      }
    }

    if (wasVisual && C.isVisual(this.mode)) this.#leaveVisual(false, effects);
    this.sticky = M.graphemeCol(this.buffer, this.cursor);
  }

  /**
   * Shift rows from bottom to top so replacing one indent cannot invalidate an
   * offset still needed by an earlier row. Empty rows stay untouched, while a
   * whitespace-only row is deliberately still an indent worth changing.
   * @param {number} first
   * @param {number} last
   * @param {boolean} right
   * @param {number} amount
   * @param {Effect[]} effects
   */
  #shiftRows(first, last, right, amount, effects) {
    const columns = this.indent.shiftWidth * amount;
    const tabWidth = Math.max(this.indent.tabWidth, 1);
    for (let row = last; row >= first; row -= 1) {
      const start = this.buffer.rowStart(row);
      const contentEnd = this.buffer.rowContentEnd(row);
      if (start === contentEnd) continue;
      let indentEnd = start;
      let old = 0;
      for (; indentEnd < contentEnd; indentEnd += 1) {
        const byte = this.buffer.byteAt(indentEnd);
        if (byte === 0x20) old += 1;
        else if (byte === 0x09) old += tabWidth - (old % tabWidth);
        else break;
      }
      const width = right ? old + columns : Math.max(old - columns, 0);
      const rendered = this.indent.useTabs
        ? '\t'.repeat(Math.floor(width / tabWidth)) + ' '.repeat(width % tabWidth)
        : ' '.repeat(width);
      if (rendered !== this.buffer.textIn(start, indentEnd)) {
        this.#edit(start, indentEnd, rendered, effects);
      }
    }
  }

  /**
   * @param {boolean} remember
   * @param {Effect[]} effects
   */
  #leaveVisual(remember, effects) {
    if (remember) this.#rememberVisualSelection();
    this.anchor = null;
    this.#setMode(C.NORMAL, effects);
  }

  // -- insert ----------------------------------------------------------

  /**
   * @param {number} at
   * @param {Effect[]} effects
   */
  #enterInsert(at, effects) {
    this.#openInsertGroup();
    switch (at) {
      case C.AT_AFTER:
        this.cursor = this.#step(this.cursor, RIGHT, 1, M.PAST_END);
        break;
      case C.AT_FIRST_NON_BLANK:
        this.cursor = this.#step(this.cursor, FIRST_NON_BLANK, 1, M.ON_CHAR);
        break;
      case C.AT_END_OF_ROW:
        this.cursor = this.buffer.rowContentEnd(this.cursorPoint().row);
        break;
      case C.AT_ROW_BELOW: {
        const end = this.buffer.rowContentEnd(this.cursorPoint().row);
        this.#edit(end, end, '\n', effects);
        this.cursor = end + 1;
        break;
      }
      case C.AT_ROW_ABOVE: {
        const start = this.buffer.rowStart(this.cursorPoint().row);
        this.#edit(start, start, '\n', effects);
        this.cursor = start;
        break;
      }
      default:
        break;
    }
    this.#setMode(C.INSERT, effects);
    this.sticky = M.graphemeCol(this.buffer, this.cursor);
  }

  /**
   * Open the undo group that spans a whole insert session. This is the coarser
   * of the two granularities: the edit stream still reports one edit per
   * keystroke so highlighting keeps up, but the user gets one `u`.
   */
  #openInsertGroup() {
    if (this.insertGroup) return;
    this.doc.beginGroup(this.cursor);
    this.insertGroup = true;
  }

  #closeInsertGroup() {
    if (!this.insertGroup) return;
    this.doc.endGroup(this.cursor);
    this.insertGroup = false;
  }

  // -- edits that need more than a span --------------------------------

  /**
   * @param {string} target
   * @returns {[number, number, number, number] | null}
   */
  #surroundOffsets(target) {
    const object = this.keymap.object(target === '<' ? '<lt>' : target);
    if (object === undefined) return null;
    let openWidth;
    let closeWidth;
    if (object.o === M.OBJ_DELIMITED) {
      openWidth = byteLength(/** @type {string} */ (object.open));
      closeWidth = byteLength(/** @type {string} */ (object.close));
    } else if (object.o === M.OBJ_QUOTED) {
      openWidth = byteLength(/** @type {string} */ (object.quote));
      closeWidth = openWidth;
    } else {
      return null;
    }
    const pair = M.delimiters(this.buffer, this.cursor, object);
    return pair === null ? null : [pair[0], pair[1], openWidth, closeWidth];
  }

  /**
   * Remove one space from each edge only when both exist. Requiring two
   * distinct bytes avoids treating the sole payload of `( )` twice.
   * @param {number} open
   * @param {number} close
   * @param {number} openWidth
   * @returns {boolean}
   */
  #surroundPadding(open, close, openWidth) {
    const innerStart = open + openWidth;
    return (
      innerStart < close - 1 && this.buffer.byteAt(innerStart) === 0x20 && this.buffer.byteAt(close - 1) === 0x20
    );
  }

  /**
   * @param {string} from
   * @param {string} to
   * @param {Effect[]} effects
   */
  #changeSurround(from, to, effects) {
    const replacement = surroundPair(to);
    const offsets = this.#surroundOffsets(from);
    if (replacement === null || offsets === null) {
      effects.push(BELL);
      return;
    }
    const [newOpen, newClose, newPadding] = replacement;
    const [open, close, openWidth, closeWidth] = offsets;
    const oldPadding = this.#surroundPadding(open, close, openWidth);
    // Offsets belong to the original buffer. Editing the later delimiter first
    // keeps the opening edit from shifting the closing one.
    this.#edit(oldPadding ? close - 1 : close, close + closeWidth, newPadding ? ` ${newClose}` : newClose, effects);
    this.#edit(open, open + openWidth + (oldPadding ? 1 : 0), newPadding ? `${newOpen} ` : newOpen, effects);
    this.#place(open);
  }

  /**
   * @param {string} target
   * @param {Effect[]} effects
   */
  #deleteSurround(target, effects) {
    const offsets = this.#surroundOffsets(target);
    if (offsets === null) {
      effects.push(BELL);
      return;
    }
    // Open and close target spellings deliberately behave alike. Padding is a
    // property of the existing pair, not of the key used to name it.
    const [open, close, openWidth, closeWidth] = offsets;
    const padding = this.#surroundPadding(open, close, openWidth);
    this.#edit(padding ? close - 1 : close, close + closeWidth, '', effects);
    this.#edit(open, open + openWidth + (padding ? 1 : 0), '', effects);
    this.#place(open);
  }

  /**
   * @param {string} delimiter
   * @param {Effect[]} effects
   */
  #surroundSelection(delimiter, effects) {
    const pair = surroundPair(delimiter);
    const selection = this.selection();
    if (pair === null || selection === null) {
      effects.push(BELL);
      return;
    }
    const [open, close, padding] = pair;
    this.#rememberVisualSelection();
    let home;
    if (this.mode === C.VISUAL_LINE) {
      // A linewise selection puts its delimiters on rows of their own, as
      // surround.vim does. Wrapping the raw span instead would leave the
      // closing delimiter prefixed to the row *after* the selection. Padding is
      // meaningless once each delimiter owns a row, so `S(` and `S)` agree.
      const buf = this.buffer;
      const anchor = this.anchor ?? this.cursor;
      const start = buf.rowStart(buf.rowOf(Math.min(anchor, this.cursor)));
      const end = buf.rowContentEnd(buf.rowOf(Math.max(anchor, this.cursor)));
      this.#edit(end, end, `\n${close}`, effects);
      this.#edit(start, start, `${open}\n`, effects);
      home = start;
    } else {
      this.#edit(selection[1], selection[1], padding ? ` ${close}` : close, effects);
      this.#edit(selection[0], selection[0], padding ? `${open} ` : open, effects);
      home = selection[0];
    }
    this.#leaveVisual(false, effects);
    this.#place(home);
  }

  /**
   * @param {number} rows
   * @param {Effect[]} effects
   */
  #joinRows(rows, effects) {
    for (let i = 1; i < rows; i += 1) {
      const row = this.cursorPoint().row;
      if (row + 1 >= this.buffer.rowCount) {
        effects.push(BELL);
        return;
      }
      const end = this.buffer.rowContentEnd(row);
      const nextStart = this.buffer.rowEnd(row);
      const nextEnd = this.buffer.rowEnd(row + 1);
      let leading = nextStart;
      while (leading < nextEnd && isSpace(this.buffer.charAt(leading))) leading = this.buffer.nextChar(leading);
      // A single space replaces the newline and the next row's indent.
      const separator = leading === nextEnd || end === this.buffer.rowStart(row) ? '' : ' ';
      this.#edit(end, leading, separator, effects);
      this.cursor = end;
    }
    this.sticky = M.graphemeCol(this.buffer, this.cursor);
  }

  /**
   * @param {boolean} before
   * @param {number} repeat
   * @param {Effect[]} effects
   */
  #put(before, repeat, effects) {
    if (this.register.isEmpty) {
      effects.push(BELL);
      return;
    }
    let text = this.register.text.repeat(repeat);
    if (this.register.linewise) {
      const row = this.cursorPoint().row;
      const rowStart = this.buffer.rowStart(row);
      const rowEnd = this.buffer.rowEnd(row);
      // Ensure the pasted block is newline-terminated so rows stay whole.
      if (!text.endsWith('\n')) text += '\n';
      // A file that does not end in a newline has no row break to paste after,
      // so putting below its final row has to supply one — and give up its own
      // trailing newline in exchange, so the file ends as it began.
      const last = this.buffer.length;
      const breakFirst =
        !before && rowEnd === last && last > 0 && this.buffer.byteAt(last - 1) !== 0x0a;
      const at = before ? rowStart : rowEnd;
      if (breakFirst) text = `\n${text.slice(0, -1)}`;
      this.#edit(at, at, text, effects);
      // The pasted rows start after the break this had to open.
      this.cursor = this.#step(breakFirst ? at + 1 : at, FIRST_NON_BLANK, 1, M.ON_CHAR);
    } else {
      const at = before ? this.cursor : this.#step(this.cursor, RIGHT, 1, M.PAST_END);
      this.#edit(at, at, text, effects);
      this.cursor = M.clamp(this.buffer, at + byteLength(text) - 1, M.ON_CHAR);
    }
    this.sticky = M.graphemeCol(this.buffer, this.cursor);
  }

  // -- replay ----------------------------------------------------------

  /**
   * @param {readonly Key[]} script
   * @param {number} times
   * @returns {Effect[]}
   */
  #replay(script, times) {
    if (this.replayDepth >= MAX_REPLAY_DEPTH) return [BELL];
    this.replayDepth += 1;
    /** @type {Effect[]} */
    const effects = [];
    for (let i = 0; i < times; i += 1) {
      for (const key of script) {
        for (const effect of this.handleKey(key)) effects.push(effect);
      }
    }
    this.replayDepth -= 1;
    return effects;
  }

  /**
   * Track what `.` should replay. Commands that enter insert mode open a
   * *session*: keys accumulate until the mode ends, so `.` after `ciwfoo<Esc>`
   * replays the typed text too. One-shot changes record immediately.
   * @param {any} command
   * @param {readonly Key[]} consumed
   */
  #noteChange(command, consumed) {
    // A visual operator is only half of what happened: replaying a bare `>`
    // would leave an operator pending for whatever gets typed next. The keys
    // that opened and shaped the selection go in front of it, so `Vj>` repeats
    // as `Vj>` — the same two rows, from wherever the caret now is.
    const visual =
      (command.c === C.OPERATE && command.target.t === C.T_SELECTION) ||
      command.c === C.SURROUND_SELECTION;
    /** @type {Key[]} */
    let script = [];
    if (visual) {
      script = this.visualKeys;
      this.visualKeys = [];
    }
    for (const key of consumed) script.push(key);

    const c = command.c;
    if (c === C.ENTER_INSERT || c === C.ENTER_REPLACE || (c === C.OPERATE && command.operator === C.CHANGE)) {
      this.changeKeys = script;
      return;
    }
    if (c === C.ENTER_NORMAL) {
      if (this.changeKeys !== null) {
        this.lastChange = this.changeKeys;
        this.changeKeys = null;
      }
      return;
    }
    // One-shot changes record immediately — but only outside a session, whose
    // keys are already accumulating.
    if (this.changeKeys !== null) return;
    const oneShot =
      (c === C.OPERATE && command.operator !== C.YANK) ||
      c === C.DELETE_CHAR ||
      c === C.REPLACE_CHAR ||
      c === C.CHANGE_SURROUND ||
      c === C.DELETE_SURROUND ||
      c === C.SURROUND_SELECTION ||
      c === C.JOIN_ROWS ||
      c === C.PUT ||
      c === C.SWAP_CASE;
    if (oneShot) this.lastChange = script;
  }
}

const EMPTY_BYTES = new Uint8Array(0);

/** Frequently resolved motions, allocated once. */
const LEFT = { k: M.LEFT };
const RIGHT = { k: M.RIGHT };
const DOWN = { k: M.DOWN };
const UP = { k: M.UP };
const FIRST_NON_BLANK = { k: M.FIRST_NON_BLANK };
const WORD_BACK = { k: M.WORD_BACKWARD, big: false };

/**
 * UTF-8 length of a string.
 * @param {string} text
 * @returns {number}
 */
function byteLength(text) {
  let size = 0;
  for (let i = 0; i < text.length; i += 1) {
    const code = text.charCodeAt(i);
    if (code < 0x80) size += 1;
    else if (code < 0x800) size += 2;
    else if (code >= 0xd800 && code < 0xdc00) {
      size += 4;
      i += 1;
    } else size += 3;
  }
  return size;
}
