// Resolving motions and text objects to buffer positions.
//
// Pure functions over a `TextBuffer`. No cursor state, no modes — the reducer
// owns those and passes in what it has.
//
// # Two granularities, deliberately
//
// - **Graphemes** for `h`, `l`, `x`, `r`, which are row-local in vi. An ASCII
//   row answers every grapheme question by arithmetic; anything else falls
//   back to a segmented row.
// - **Characters** for word motions and delimiter scanning, which cross rows.
//   These step UTF-8 lead bytes and never materialise a string.

import { BLANK, PUNCT, classOf, isSpace, isBlank, hasUppercase, graphemeStarts } from './unicode.js';

/** Motion kinds. Small integers so the reducer's switches stay dense. */
export const LEFT = 1;
export const RIGHT = 2;
export const DOWN = 3;
export const UP = 4;
export const FIRST_COLUMN = 5;
export const FIRST_NON_BLANK = 6;
export const LAST_COLUMN = 7;
export const WORD_FORWARD = 8;
export const WORD_BACKWARD = 9;
export const WORD_END = 10;
export const PARAGRAPH = 11;
export const FIND = 12;
export const REPEAT_FIND = 13;
export const SEARCH = 14;
export const REPEAT_SEARCH = 15;
export const GOTO_ROW = 16;
export const GOTO_FIRST_ROW = 17;
export const MATCH_PAIR = 18;
export const SCREEN_TOP = 19;
export const SCREEN_MIDDLE = 20;
export const SCREEN_BOTTOM = 21;
export const MARK = 22;
export const TO_OFFSET = 23;

/** Text object kinds. */
export const OBJ_WORD = 1;
export const OBJ_DELIMITED = 2;
export const OBJ_QUOTED = 3;
export const OBJ_PARAGRAPH = 4;

/** Where the cursor is allowed to rest. Normal mode sits *on* a character. */
export const ON_CHAR = 0;
export const PAST_END = 1;

/** Sentinel sticky column meaning "stay at the end of the row", as `$` does. */
export const STICKY_END = 0x7fffffff;

/**
 * @typedef {{ k: number, big?: boolean, backward?: boolean, till?: boolean, target?: string,
 *   reverse?: boolean, pattern?: string, name?: string, exact?: boolean, offset?: number,
 *   linewise?: boolean }} Motion
 */
/** @typedef {{ o: number, big?: boolean, open?: string, close?: string, quote?: string }} TextObject */
/** @typedef {{ lines: boolean, a: number, b: number }} Span */
/** @typedef {{ target: string, backward: boolean, till: boolean }} Find */
/** @typedef {import('./buffer.js').TextBuffer} TextBuffer */

/**
 * Whether an operator over this motion acts on whole rows.
 * @param {Motion} motion
 * @returns {boolean}
 */
export function isLinewise(motion) {
  switch (motion.k) {
    case TO_OFFSET:
      return Boolean(motion.linewise);
    case MARK:
      return !motion.exact;
    case DOWN:
    case UP:
    case GOTO_ROW:
    case GOTO_FIRST_ROW:
    case SCREEN_TOP:
    case SCREEN_MIDDLE:
    case SCREEN_BOTTOM:
      return true;
    default:
      return false;
  }
}

/**
 * Whether the character under the motion's destination is included.
 *
 * Forward `f` and `t` are both inclusive; `t` simply lands a character earlier.
 * Backward `F`/`T` are exclusive, leaving the character under the cursor alone.
 * @param {Motion} motion
 * @returns {boolean}
 */
export function isInclusive(motion) {
  switch (motion.k) {
    case WORD_END:
    case LAST_COLUMN:
    case MATCH_PAIR:
      return true;
    case FIND:
      return !motion.backward;
    default:
      return false;
  }
}

// ---------------------------------------------------------------------------
// grapheme stepping, row-local
// ---------------------------------------------------------------------------

/** @type {{ buf: TextBuffer | null, row: number, stamp: number, at: number[] }} */
const memo = { buf: null, row: -1, stamp: -1, at: [] };

/**
 * Absolute byte offsets of every grapheme boundary in `row`, including its end.
 * Only reached for rows that are not pure ASCII.
 * @param {TextBuffer} buf
 * @param {number} row
 * @returns {number[]}
 */
function boundaries(buf, row) {
  if (memo.buf === buf && memo.row === row && memo.stamp === buf.stamp) return memo.at;
  const start = buf.rowStart(row);
  const end = buf.rowContentEnd(row);
  const text = buf.textIn(start, end);
  /** @type {number[]} */
  const bytes = [];
  let byte = start;
  for (let i = 0; i < text.length; ) {
    const cp = /** @type {number} */ (text.codePointAt(i));
    bytes[i] = byte;
    byte += cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
    i += cp > 0xffff ? 2 : 1;
  }
  bytes[text.length] = end;
  const at = graphemeStarts(text).map((i) => bytes[i]);
  at.push(end);
  memo.buf = buf;
  memo.row = row;
  memo.stamp = buf.stamp;
  memo.at = at;
  return at;
}

/**
 * The highest grapheme column the cursor may occupy on `row`.
 * @param {number} count number of graphemes on the row
 * @param {number} bound
 * @returns {number}
 */
function maxCol(count, bound) {
  return bound === PAST_END ? count : Math.max(count - 1, 0);
}

/**
 * The grapheme column of `byte` within its row.
 * @param {TextBuffer} buf
 * @param {number} byte
 * @returns {number}
 */
export function graphemeCol(buf, byte) {
  const row = buf.rowOf(byte);
  if (buf.asciiRow(row)) {
    const start = buf.rowStart(row);
    return Math.max(Math.min(byte - start, buf.rowContentEnd(row) - start), 0);
  }
  const at = boundaries(buf, row);
  let col = 0;
  while (col + 1 < at.length && at[col + 1] <= byte) col += 1;
  return col;
}

/**
 * The byte offset of grapheme column `col` on `row`, clamped.
 * @param {TextBuffer} buf
 * @param {number} row
 * @param {number} col
 * @param {number} bound
 * @returns {number}
 */
function byteAtCol(buf, row, col, bound) {
  if (buf.asciiRow(row)) {
    const start = buf.rowStart(row);
    return start + Math.min(col, maxCol(buf.rowContentEnd(row) - start, bound));
  }
  const at = boundaries(buf, row);
  return at[Math.min(col, maxCol(at.length - 1, bound))];
}

/**
 * Pull `byte` back to a legal cursor position, snapping it to a grapheme
 * boundary — the single guarantee that callers computing offsets in bytes
 * cannot leave the cursor mid-character.
 * @param {TextBuffer} buf
 * @param {number} byte
 * @param {number} bound
 * @returns {number}
 */
export function clamp(buf, byte, bound) {
  const at = Math.min(byte, buf.length);
  const row = buf.rowOf(at);
  const start = buf.rowStart(row);
  if (buf.asciiRow(row)) {
    return start + Math.min(at - start, maxCol(buf.rowContentEnd(row) - start, bound));
  }
  const bounds = boundaries(buf, row);
  const limit = maxCol(bounds.length - 1, bound);
  let col = 0;
  while (col < limit && bounds[col + 1] <= at) col += 1;
  return bounds[col];
}

/**
 * @param {TextBuffer} buf
 * @param {number} byte
 * @param {number} bound
 * @returns {number}
 */
function nextGrapheme(buf, byte, bound) {
  const row = buf.rowOf(byte);
  if (buf.asciiRow(row)) {
    const start = buf.rowStart(row);
    const col = Math.max(Math.min(byte - start, buf.rowContentEnd(row) - start), 0);
    return start + Math.min(col + 1, maxCol(buf.rowContentEnd(row) - start, bound));
  }
  return byteAtCol(buf, row, graphemeCol(buf, byte) + 1, bound);
}

/**
 * @param {TextBuffer} buf
 * @param {number} byte
 * @param {number} bound
 * @returns {number}
 */
function prevGrapheme(buf, byte, bound) {
  const row = buf.rowOf(byte);
  return byteAtCol(buf, row, Math.max(graphemeCol(buf, byte) - 1, 0), bound);
}

/**
 * @param {TextBuffer} buf
 * @param {number} row
 * @returns {number}
 */
function firstNonBlank(buf, row) {
  const end = buf.rowContentEnd(row);
  let at = buf.rowStart(row);
  while (at < end && isSpace(buf.charAt(at))) at = buf.nextChar(at);
  return at === end ? buf.rowStart(row) : at;
}

/**
 * @param {TextBuffer} buf
 * @param {number} row
 * @returns {boolean}
 */
function blankRow(buf, row) {
  const start = buf.rowStart(row);
  const end = buf.rowContentEnd(row);
  if (buf.wide === 0) {
    for (let at = start; at < end; at += 1) if (!isSpace(buf.byteAt(at))) return false;
    return true;
  }
  return isBlank(buf.textIn(start, end));
}

// ---------------------------------------------------------------------------
// word classes
// ---------------------------------------------------------------------------

/**
 * @param {TextBuffer} buf
 * @param {number} at
 * @param {boolean} big
 * @returns {number} class, or -1 past the end
 */
function classAt(buf, at, big) {
  const cp = buf.charAt(at);
  return cp < 0 ? -1 : classOf(cp, big);
}

/**
 * `w` / `W`: the start of the next word.
 * @param {TextBuffer} buf
 * @param {number} from
 * @param {boolean} big
 * @returns {number}
 */
function wordForward(buf, from, big) {
  let pos = from;
  const start = classAt(buf, pos, big);
  if (start !== -1 && start !== BLANK) {
    while (classAt(buf, pos, big) === start) pos = buf.nextChar(pos);
  }
  while (classAt(buf, pos, big) === BLANK) pos = buf.nextChar(pos);
  return pos;
}

/**
 * `b` / `B`: the start of this word, or of the previous one.
 * @param {TextBuffer} buf
 * @param {number} from
 * @param {boolean} big
 * @returns {number}
 */
function wordBackward(buf, from, big) {
  let pos = buf.prevChar(from);
  while (pos > 0 && classAt(buf, pos, big) === BLANK) pos = buf.prevChar(pos);
  const current = classAt(buf, pos, big);
  if (current === -1 || current === BLANK) return pos;
  while (pos > 0) {
    const prev = buf.prevChar(pos);
    if (classAt(buf, prev, big) !== current) break;
    pos = prev;
  }
  return pos;
}

/**
 * `e` / `E`: the last character of this word, or of the next one.
 * @param {TextBuffer} buf
 * @param {number} from
 * @param {boolean} big
 * @returns {number}
 */
function wordEnd(buf, from, big) {
  let pos = buf.nextChar(from);
  while (classAt(buf, pos, big) === BLANK) pos = buf.nextChar(pos);
  const current = classAt(buf, pos, big);
  if (current === -1) return buf.prevChar(pos);
  for (;;) {
    const next = buf.nextChar(pos);
    if (classAt(buf, next, big) !== current) return pos;
    pos = next;
  }
}

/**
 * @param {TextBuffer} buf
 * @param {number} from
 * @param {boolean} backward
 * @param {number} count
 * @param {number} bound
 * @returns {number}
 */
function paragraph(buf, from, backward, count, bound) {
  let row = buf.rowOf(from);
  const lastRow = buf.rowCount - 1;
  const edge = () => (backward ? 0 : clamp(buf, buf.length, bound));
  for (let step = 0; step < count; step += 1) {
    if (backward ? row === 0 : row >= lastRow) return edge();
    row += backward ? -1 : 1;
    while (row !== 0 && row !== lastRow && !blankRow(buf, row)) row += backward ? -1 : 1;
    if (!blankRow(buf, row)) return edge();
  }
  return buf.rowStart(row);
}

/**
 * The run of same-class characters containing `at`.
 * @param {TextBuffer} buf
 * @param {number} at
 * @param {boolean} big
 * @returns {[number, number]}
 */
function wordRun(buf, at, big) {
  const current = classAt(buf, at, big);
  if (current === -1) return [at, at];
  let start = at;
  while (start > 0) {
    const prev = buf.prevChar(start);
    if (classAt(buf, prev, big) !== current) break;
    start = prev;
  }
  let end = at;
  while (classAt(buf, end, big) === current) end = buf.nextChar(end);
  return [start, end];
}

/**
 * End of the whitespace run at `at`, stopping at the row's newline: a word
 * object that swallowed it would join two rows.
 * @param {TextBuffer} buf
 * @param {number} at
 * @param {boolean} big
 * @returns {number}
 */
function blankRunEnd(buf, at, big) {
  let end = at;
  while (classAt(buf, end, big) === BLANK && buf.byteAt(end) !== 0x0a) end = buf.nextChar(end);
  return end;
}

/**
 * @param {TextBuffer} buf
 * @param {number} at
 * @param {boolean} big
 * @returns {number}
 */
function blankRunStart(buf, at, big) {
  let start = at;
  while (start > 0) {
    const prev = buf.prevChar(start);
    if (classAt(buf, prev, big) !== BLANK || buf.byteAt(prev) === 0x0a) break;
    start = prev;
  }
  return start;
}

// ---------------------------------------------------------------------------
// find, row-local
// ---------------------------------------------------------------------------

/**
 * Find `find.target` within `from`'s row.
 *
 * `skipAdjacent` is for `;` and `,`: a `t` parks one character short of its
 * target, so repeating it would resolve to where the cursor already is.
 * Stepping the origin one character along excludes exactly that target.
 * @param {TextBuffer} buf
 * @param {number} from
 * @param {Find} find
 * @param {number} count
 * @param {boolean} skipAdjacent
 * @returns {number | null}
 */
function findInRow(buf, from, find, count, skipAdjacent) {
  const row = buf.rowOf(from);
  const start = buf.rowStart(row);
  const end = buf.rowContentEnd(row);
  const target = /** @type {number} */ (find.target.codePointAt(0));
  let origin = from;
  if (skipAdjacent && find.till) {
    origin = find.backward ? Math.max(buf.prevChar(from), start) : Math.min(buf.nextChar(from), end);
  }

  let hit = -1;
  if (find.backward) {
    // The count-th match before the origin, so the row is walked forwards and
    // counted from the far end.
    hits.length = 0;
    for (let at = start; at < origin; at = buf.nextChar(at)) {
      if (buf.charAt(at) === target) hits.push(at);
    }
    if (hits.length >= count) hit = hits[hits.length - count];
  } else {
    let seen = 0;
    for (let at = buf.nextChar(origin); at < end; at = buf.nextChar(at)) {
      if (buf.charAt(at) === target && (seen += 1) === count) {
        hit = at;
        break;
      }
    }
  }
  if (hit < 0) return null;
  if (!find.till) return hit;
  // `t`/`T` stop one character short of the target.
  return find.backward ? buf.nextChar(hit) : buf.prevChar(hit);
}

/** Scratch for the backward find, which has to walk the row forwards. */
/** @type {number[]} */
const hits = [];

// ---------------------------------------------------------------------------
// pairs
// ---------------------------------------------------------------------------

/**
 * Byte offsets of the delimiters enclosing `at`, counting nesting. A cursor
 * sitting *on* either delimiter counts as inside, which is what makes `ci(`
 * work with the cursor on the paren itself.
 * @param {TextBuffer} buf
 * @param {number} at
 * @param {number} open
 * @param {number} close
 * @returns {[number, number] | null}
 */
function enclosingPair(buf, at, open, close) {
  let start;
  if (buf.charAt(at) === open) {
    start = at;
  } else {
    let depth = 0;
    let pos = at;
    for (;;) {
      if (pos === 0) return null;
      pos = buf.prevChar(pos);
      const ch = buf.charAt(pos);
      if (ch === close) depth += 1;
      else if (ch === open) {
        if (depth === 0) break;
        depth -= 1;
      }
    }
    start = pos;
  }

  let depth = 0;
  let pos = buf.nextChar(start);
  for (;;) {
    const ch = buf.charAt(pos);
    if (ch < 0) return null;
    if (ch === open) depth += 1;
    else if (ch === close) {
      if (depth === 0) return [start, pos];
      depth -= 1;
    }
    pos = buf.nextChar(pos);
  }
}

/**
 * Byte offsets of the quote pair around `at`, searched within the row.
 *
 * Quotes do not nest, so pairs are taken in order and the one containing the
 * cursor wins; failing that, the next pair after it does.
 * @param {TextBuffer} buf
 * @param {number} at
 * @param {number} quote
 * @returns {[number, number] | null}
 */
function enclosingQuotes(buf, at, quote) {
  const row = buf.rowOf(at);
  const end = buf.rowContentEnd(row);
  let open = -1;
  for (let pos = buf.rowStart(row); pos < end; pos = buf.nextChar(pos)) {
    if (buf.charAt(pos) !== quote) continue;
    if (open < 0) open = pos;
    else {
      if (at <= pos) return [open, pos];
      open = -1;
    }
  }
  return null;
}

/**
 * `%` — the match of the first delimiter on the row at or after the cursor.
 *
 * Brackets are looked for first, so `%` does what vi does wherever vi does
 * anything at all. Quotes are ours — vi rings on them — and are only consulted
 * when the row holds no bracket, so no vi behaviour is displaced.
 * @param {TextBuffer} buf
 * @param {number} at
 * @returns {number | null}
 */
function matchPair(buf, at) {
  const end = buf.rowContentEnd(buf.rowOf(at));
  let quote = -1;
  for (let pos = at; pos < end; pos = buf.nextChar(pos)) {
    const ch = buf.charAt(pos);
    if (ch < 0) return null;
    for (let i = 0; i < PAIRS.length; i += 2) {
      if (ch !== PAIRS[i] && ch !== PAIRS[i + 1]) continue;
      const found = enclosingPair(buf, pos, PAIRS[i], PAIRS[i + 1]);
      if (!found) return null;
      return pos === found[0] ? found[1] : found[0];
    }
    if (quote < 0 && QUOTES.includes(ch)) quote = pos;
  }
  if (quote < 0) return null;
  const found = enclosingQuotes(buf, quote, buf.charAt(quote));
  return found ? (quote === found[0] ? found[1] : found[0]) : null;
}

const PAIRS = [0x28, 0x29, 0x5b, 0x5d, 0x7b, 0x7d];
const QUOTES = [0x22, 0x27, 0x60];

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

/** @type {{ buf: TextBuffer | null, stamp: number, pattern: string, at: number[] }} */
const found = { buf: null, stamp: -1, pattern: '', at: [] };

/**
 * Every literal match of `pattern`, in order.
 *
 * Smartcase: an all-lowercase pattern matches case-insensitively, while any
 * uppercase letter makes it case-sensitive. Matches must begin at a grapheme
 * boundary, so a pattern cannot land inside a cluster.
 * @param {TextBuffer} buf
 * @param {string} pattern
 * @returns {number[]}
 */
function matches(buf, pattern) {
  if (found.buf === buf && found.stamp === buf.stamp && found.pattern === pattern) return found.at;
  const sensitive = hasUppercase(pattern);
  const ascii = !/[^\x00-\x7f]/.test(pattern);
  const at =
    ascii && (sensitive || buf.wide === 0)
      ? scanBytes(buf, pattern, sensitive)
      : scanText(buf, pattern, sensitive);
  found.buf = buf;
  found.stamp = buf.stamp;
  found.pattern = pattern;
  found.at = at;
  return at;
}

/**
 * @param {TextBuffer} buf
 * @param {string} pattern
 * @param {boolean} sensitive
 * @returns {number[]}
 */
function scanBytes(buf, pattern, sensitive) {
  const bytes = buf.contiguous();
  const needle = new Uint8Array(pattern.length);
  for (let i = 0; i < pattern.length; i += 1) {
    const code = pattern.charCodeAt(i);
    needle[i] = sensitive ? code : code | (isAlpha(code) ? 0x20 : 0);
  }
  const first = needle[0];
  // Candidates come from the engine's own `indexOf`, which is vectorised;
  // testing every byte here costs several times as much. A folded search has
  // two possible first bytes, so it runs two scans and takes the nearer hit.
  const other = !sensitive && isAlpha(first) ? first & ~0x20 : -1;
  const limit = bytes.length - needle.length;
  /** @type {number[]} */
  const out = [];
  // Each candidate keeps its own cursor. Re-asking for one that has already
  // reported "no more" would rescan the whole tail per hit, which is how a
  // linear search turns quadratic on a buffer that holds only one of the two
  // cases — the common one, since prose is mostly lowercase.
  let lower = bytes.indexOf(first, 0);
  let upper = other >= 0 ? bytes.indexOf(other, 0) : -1;
  for (;;) {
    const hit = lower < 0 ? upper : upper < 0 ? lower : Math.min(lower, upper);
    if (hit < 0 || hit > limit) break;
    let i = 1;
    for (; i < needle.length; i += 1) {
      const byte = bytes[hit + i];
      if (byte !== needle[i] && (sensitive || (byte | (isAlpha(byte) ? 0x20 : 0)) !== needle[i])) break;
    }
    // A `\r\n` is one grapheme, so a match cannot start on its newline.
    if (i === needle.length && !(first === 0x0a && hit > 0 && bytes[hit - 1] === 0x0d)) out.push(hit);
    const next = hit + 1;
    if (lower >= 0 && lower < next) lower = bytes.indexOf(first, next);
    if (upper >= 0 && upper < next) upper = bytes.indexOf(other, next);
  }
  return out;
}

/**
 * @param {number} code
 * @returns {boolean}
 */
function isAlpha(code) {
  return (code >= 0x41 && code <= 0x5a) || (code >= 0x61 && code <= 0x7a);
}

/**
 * The general path: grapheme starts of the whole buffer, tested with the same
 * progressive case folding Rust uses, so `ß` cannot half-match.
 * @param {TextBuffer} buf
 * @param {string} pattern
 * @param {boolean} sensitive
 * @returns {number[]}
 */
function scanText(buf, pattern, sensitive) {
  const text = buf.toString();
  const folded = sensitive ? null : pattern.toLowerCase();
  /** @type {number[]} */
  const out = [];
  let byte = 0;
  let next = 0;
  const starts = graphemeStarts(text);
  for (let s = 0; s < starts.length; s += 1) {
    while (next < starts[s]) {
      const cp = /** @type {number} */ (text.codePointAt(next));
      byte += cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
      next += cp > 0xffff ? 2 : 1;
    }
    if (literalPrefix(text, starts[s], pattern, folded)) out.push(byte);
  }
  return out;
}

/**
 * @param {string} text
 * @param {number} at
 * @param {string} pattern
 * @param {string | null} folded
 * @returns {boolean}
 */
function literalPrefix(text, at, pattern, folded) {
  if (folded === null) return text.startsWith(pattern, at);
  let candidate = '';
  for (let i = at; i < text.length; ) {
    const cp = /** @type {number} */ (text.codePointAt(i));
    i += cp > 0xffff ? 2 : 1;
    candidate += String.fromCodePoint(cp).toLowerCase();
    if (candidate === folded) return true;
    if (!folded.startsWith(candidate)) return false;
  }
  return false;
}

/**
 * Find the counted literal match in `direction`, wrapping at either end.
 * @param {TextBuffer} buf
 * @param {number} from
 * @param {string} pattern
 * @param {boolean} backward
 * @param {number} repeat
 * @returns {number | null}
 */
function search(buf, from, pattern, backward, repeat) {
  if (pattern === '') return null;
  const at = matches(buf, pattern);
  if (at.length === 0) return null;
  let landed = from;
  for (let step = 0; step < repeat; step += 1) {
    // First index at or past the pivot: `landed` itself going backwards, one
    // beyond it going forwards. Either way the current position is excluded,
    // and running off the end wraps.
    const pivot = backward ? landed : landed + 1;
    let low = 0;
    let high = at.length;
    while (low < high) {
      const mid = (low + high) >> 1;
      if (at[mid] >= pivot) high = mid;
      else low = mid + 1;
    }
    landed = backward
      ? low > 0
        ? at[low - 1]
        : at[at.length - 1]
      : low < at.length
        ? at[low]
        : at[0];
  }
  return landed;
}

// ---------------------------------------------------------------------------
// motion resolution
// ---------------------------------------------------------------------------

/**
 * Byte range covering rows `first..=last`, including the final row's newline
 * when there is one. This is what a linewise operator deletes.
 * @param {TextBuffer} buf
 * @param {number} first
 * @param {number} last
 * @returns {[number, number]}
 */
export function rowSpan(buf, first, last) {
  const bottom = Math.min(last, buf.rowCount - 1);
  const end = buf.rowEnd(bottom);
  // No trailing newline on the final row: take the preceding one instead, so
  // `dd` on the last row does not leave a blank.
  if (end === buf.length && first > 0) return [buf.rowEnd(first - 1) - 1, end];
  return [buf.rowStart(first), end];
}

/**
 * @typedef {{ sticky: number, lastFind: Find | null, lastSearch: [string, boolean] | null,
 *   viewport: { topRow: number, height: number }, bound: number }} Context
 */

/**
 * Where `motion` lands, starting from `from`. `null` when it cannot be
 * performed at all.
 * @param {TextBuffer} buf
 * @param {number} from
 * @param {Motion} motion
 * @param {number | null} count
 * @param {Context} ctx
 * @returns {number | null}
 */
export function resolve(buf, from, motion, count, ctx) {
  const repeat = count ?? 1;
  const row = buf.rowOf(from);
  const rows = buf.rowCount;
  const bound = ctx.bound;
  let target;

  switch (motion.k) {
    case LEFT: {
      let pos = from;
      for (let i = 0; i < repeat; i += 1) pos = prevGrapheme(buf, pos, bound);
      target = pos;
      break;
    }
    case RIGHT: {
      let pos = from;
      for (let i = 0; i < repeat; i += 1) pos = nextGrapheme(buf, pos, bound);
      target = pos;
      break;
    }
    case DOWN:
      target = byteAtCol(buf, Math.min(row + repeat, rows - 1), ctx.sticky, bound);
      break;
    case UP:
      target = byteAtCol(buf, Math.max(row - repeat, 0), ctx.sticky, bound);
      break;
    case FIRST_COLUMN:
      target = buf.rowStart(row);
      break;
    case FIRST_NON_BLANK:
      target = firstNonBlank(buf, row);
      break;
    case LAST_COLUMN:
      target = byteAtCol(buf, Math.min(row + repeat - 1, rows - 1), STICKY_END, bound);
      break;
    case WORD_FORWARD: {
      let pos = from;
      for (let i = 0; i < repeat; i += 1) pos = wordForward(buf, pos, Boolean(motion.big));
      target = pos;
      break;
    }
    case WORD_BACKWARD: {
      let pos = from;
      for (let i = 0; i < repeat; i += 1) pos = wordBackward(buf, pos, Boolean(motion.big));
      target = pos;
      break;
    }
    case WORD_END: {
      let pos = from;
      for (let i = 0; i < repeat; i += 1) pos = wordEnd(buf, pos, Boolean(motion.big));
      target = pos;
      break;
    }
    case PARAGRAPH:
      target = paragraph(buf, from, Boolean(motion.backward), repeat, bound);
      break;
    case FIND: {
      const hit = findInRow(buf, from, /** @type {Find} */ (motion), repeat, false);
      if (hit === null) return null;
      target = hit;
      break;
    }
    case REPEAT_FIND: {
      const last = ctx.lastFind;
      if (!last) return null;
      const find = motion.reverse ? { ...last, backward: !last.backward } : last;
      const hit = findInRow(buf, from, find, repeat, true);
      if (hit === null) return null;
      target = hit;
      break;
    }
    case SEARCH: {
      const hit = search(buf, from, /** @type {string} */ (motion.pattern), Boolean(motion.backward), repeat);
      if (hit === null) return null;
      target = hit;
      break;
    }
    case REPEAT_SEARCH: {
      if (!ctx.lastSearch) return null;
      const [pattern, backward] = ctx.lastSearch;
      const hit = search(buf, from, pattern, backward !== Boolean(motion.reverse), repeat);
      if (hit === null) return null;
      target = hit;
      break;
    }
    // `G` and `gg` take the count as an absolute row, 1-based.
    case GOTO_ROW:
      target = firstNonBlank(buf, count === null ? rows - 1 : Math.min(Math.max(count - 1, 0), rows - 1));
      break;
    case GOTO_FIRST_ROW:
      target = firstNonBlank(buf, count === null ? 0 : Math.min(Math.max(count - 1, 0), rows - 1));
      break;
    case MATCH_PAIR: {
      const hit = matchPair(buf, from);
      if (hit === null) return null;
      target = hit;
      break;
    }
    case SCREEN_TOP:
    case SCREEN_MIDDLE:
    case SCREEN_BOTTOM: {
      const hit = screenMotion(buf, motion.k, repeat, ctx.viewport);
      if (hit === null) return null;
      target = hit;
      break;
    }
    // Marks belong to the editor's navigation state. It turns them into the
    // concrete `ToOffset` vocabulary before this pure resolver is called.
    case MARK:
      return null;
    default: {
      const at = clamp(buf, /** @type {number} */ (motion.offset), bound);
      target = motion.linewise ? firstNonBlank(buf, buf.rowOf(at)) : at;
      break;
    }
  }
  return clamp(buf, target, bound);
}

/**
 * @param {TextBuffer} buf
 * @param {number} kind
 * @param {number} repeat
 * @param {{ topRow: number, height: number }} viewport
 * @returns {number | null}
 */
function screenMotion(buf, kind, repeat, viewport) {
  // A zero height means no host has reported a screen, so screen-relative
  // motions have no meaningful target rather than pretending row zero is it.
  if (viewport.height === 0) return null;
  const last = buf.rowCount - 1;
  const top = Math.min(viewport.topRow, last);
  const bottom = Math.min(viewport.topRow + Math.max(viewport.height - 1, 0), last);
  const row =
    kind === SCREEN_TOP
      ? Math.min(viewport.topRow + repeat - 1, last)
      : kind === SCREEN_MIDDLE
        ? top + ((bottom - top) >> 1)
        : Math.max(bottom - (repeat - 1), 0);
  return firstNonBlank(buf, row);
}

// ---------------------------------------------------------------------------
// text objects
// ---------------------------------------------------------------------------

/**
 * The span a text object covers with the cursor at `at`.
 *
 * `count` means something different for each kind, following vi: nesting levels
 * for a delimited pair, runs of text for a word, paragraphs for a paragraph.
 * Quotes do not nest and have nothing to count, so they ignore it.
 * @param {TextBuffer} buf
 * @param {number} at
 * @param {boolean} around
 * @param {TextObject} object
 * @param {number} count
 * @returns {Span | null}
 */
export function objectSpan(buf, at, around, object, count) {
  const repeat = Math.max(count, 1);
  switch (object.o) {
    case OBJ_WORD:
      return wordObject(buf, at, around, Boolean(object.big), repeat);
    case OBJ_DELIMITED: {
      const open = /** @type {number} */ (/** @type {string} */ (object.open).codePointAt(0));
      const close = /** @type {number} */ (/** @type {string} */ (object.close).codePointAt(0));
      const pair = pairAtLevel(buf, at, open, close, repeat);
      return pair && pairSpan(buf, pair[0], pair[1], around);
    }
    case OBJ_QUOTED: {
      const quote = /** @type {number} */ (/** @type {string} */ (object.quote).codePointAt(0));
      const pair = enclosingQuotes(buf, at, quote);
      return pair && pairSpan(buf, pair[0], pair[1], around);
    }
    default:
      return paragraphObject(buf, at, around, repeat);
  }
}

/**
 * Byte offsets of the delimiters enclosing `at`, for surround. Word and
 * paragraph objects have none.
 * @param {TextBuffer} buf
 * @param {number} at
 * @param {TextObject} object
 * @returns {[number, number] | null}
 */
export function delimiters(buf, at, object) {
  if (object.o === OBJ_DELIMITED) {
    return enclosingPair(
      buf,
      at,
      /** @type {number} */ (/** @type {string} */ (object.open).codePointAt(0)),
      /** @type {number} */ (/** @type {string} */ (object.close).codePointAt(0)),
    );
  }
  if (object.o === OBJ_QUOTED) {
    return enclosingQuotes(buf, at, /** @type {number} */ (/** @type {string} */ (object.quote).codePointAt(0)));
  }
  return null;
}

/**
 * `iw` / `aw`, with a count taking in further runs of text. A stretch of
 * whitespace is a run of its own, so `3iw` is word, space, word, while `3aw` is
 * three words each with the space that follows it.
 * @param {TextBuffer} buf
 * @param {number} at
 * @param {boolean} around
 * @param {boolean} big
 * @param {number} count
 * @returns {Span | null}
 */
function wordObject(buf, at, around, big, count) {
  const [runStart, runEnd] = wordRun(buf, at, big);
  if (runStart === runEnd) return null;
  // A word object never joins rows, so the newline ending this one is where a
  // count runs out.
  const spent = (end) => {
    const byte = buf.byteAt(end);
    return byte === -1 || byte === 0x0a;
  };
  let start = runStart;
  let end = runEnd;
  if (!around) {
    for (let i = 1; i < count; i += 1) {
      if (spent(end)) break;
      end = classAt(buf, end, big) === BLANK ? blankRunEnd(buf, end, big) : wordRun(buf, end, big)[1];
    }
  } else {
    // `aw` takes the trailing whitespace too, or the leading run when there is
    // none after.
    end = blankRunEnd(buf, runEnd, big);
    const trailing = end > runEnd;
    for (let i = 1; i < count; i += 1) {
      if (spent(end)) break;
      end = blankRunEnd(buf, wordRun(buf, end, big)[1], big);
    }
    if (!trailing) start = blankRunStart(buf, runStart, big);
  }
  return { lines: false, a: start, b: end };
}

/**
 * The delimiter pair `count` nesting levels from the cursor.
 *
 * Inside a pair the count climbs *outward*; inside none, vi seeks forward to
 * the next pair and the count descends *inward* from it.
 * @param {TextBuffer} buf
 * @param {number} at
 * @param {number} open
 * @param {number} close
 * @param {number} count
 * @returns {[number, number] | null}
 */
function pairAtLevel(buf, at, open, close, count) {
  const inside = enclosingPair(buf, at, open, close);
  if (inside) {
    let [start, end] = inside;
    for (let i = 1; i < count; i += 1) {
      if (start === 0) return null;
      const outer = enclosingPair(buf, buf.prevChar(start), open, close);
      // Searching from just before this pair finds an adjacent sibling as
      // readily as an enclosing one — `(a)(b)` has no second level — so only a
      // pair that genuinely contains this one counts as a level.
      if (!outer || outer[0] >= start || outer[1] <= end) return null;
      [start, end] = outer;
    }
    return [start, end];
  }
  // vi does not seek backwards, so a pair the cursor has already passed is out
  // of reach.
  const ahead = nextOpen(buf, at, buf.length, open, close);
  if (ahead === null) return null;
  const seeded = enclosingPair(buf, ahead, open, close);
  if (!seeded) return null;
  let [start, end] = seeded;
  for (let i = 1; i < count; i += 1) {
    // The *first* pair nested inside. Siblings are not levels: with nothing
    // nested inside, the object fails rather than settling for what we have.
    const inner = nextOpen(buf, buf.nextChar(start), end, open, close);
    if (inner === null) return null;
    const deeper = enclosingPair(buf, inner, open, close);
    if (!deeper) return null;
    [start, end] = deeper;
  }
  return [start, end];
}

/**
 * The first `open` in `[from, limit)`, or `null` if a `close` turns up first.
 * @param {TextBuffer} buf
 * @param {number} from
 * @param {number} limit
 * @param {number} open
 * @param {number} close
 * @returns {number | null}
 */
function nextOpen(buf, from, limit, open, close) {
  for (let pos = from; pos < limit; pos = buf.nextChar(pos)) {
    const ch = buf.charAt(pos);
    if (ch === open) return pos;
    if (ch === close) return null;
  }
  return null;
}

/**
 * @param {TextBuffer} buf
 * @param {number} start
 * @param {number} end
 * @param {boolean} around
 * @returns {Span}
 */
function pairSpan(buf, start, end, around) {
  if (around) return { lines: false, a: start, b: buf.nextChar(end) };
  return innerSpan(buf, start, end);
}

/**
 * The inside of a pair, following vi's rule for delimiters that own their rows.
 *
 * When the opening delimiter is the last thing on its row the inside starts at
 * the row below; when the closing delimiter has nothing but indent before it,
 * the inside ends where the row above ends. So `di{` on a function body takes
 * the body's rows and leaves the braces where they were. vi shrinks the span
 * here rather than promoting the object to linewise.
 * @param {TextBuffer} buf
 * @param {number} open
 * @param {number} close
 * @returns {Span}
 */
function innerSpan(buf, open, close) {
  const afterOpen = buf.nextChar(open);
  const openRow = buf.rowOf(open);
  const startsBelow = afterOpen === buf.rowContentEnd(openRow) && openRow + 1 < buf.rowCount;
  const start = startsBelow ? buf.rowStart(openRow + 1) : afterOpen;

  const closeRow = buf.rowOf(close);
  // Only indent between the row's start and the delimiter. On a single-row
  // pair this cannot hold, since the opening delimiter is itself in the way.
  const endsAbove = isBlank(buf.textIn(buf.rowStart(closeRow), close));
  let end = close;
  if (endsAbove) {
    const above = buf.rowContentEnd(closeRow - 1);
    // The row break above the delimiter is the object's to take only when the
    // span already begins at a row boundary. Otherwise the front of that row
    // survives and needs its newline.
    end = startsBelow ? above + 1 : above;
  }
  return { lines: false, a: start, b: Math.max(end, start) };
}

/**
 * `ip` / `ap`, blank-row delimited and always linewise.
 * @param {TextBuffer} buf
 * @param {number} at
 * @param {boolean} around
 * @param {number} count
 * @returns {Span}
 */
function paragraphObject(buf, at, around, count) {
  const rows = buf.rowCount;
  const row = buf.rowOf(at);
  let first = row;
  while (first > 0 && !blankRow(buf, first - 1)) first -= 1;
  let last = row;
  while (last + 1 < rows && !blankRow(buf, last + 1)) last += 1;
  // Counts work as they do for words, with a run of blank rows standing in for
  // a run of whitespace.
  if (!around) {
    for (let i = 1; i < count; i += 1) {
      if (last + 1 >= rows) break;
      const want = blankRow(buf, last + 1);
      last += 1;
      while (last + 1 < rows && blankRow(buf, last + 1) === want) last += 1;
    }
  } else {
    for (let step = 0; step < count; step += 1) {
      if (step > 0) {
        if (last + 1 >= rows || blankRow(buf, last + 1)) break;
        last += 1;
        while (last + 1 < rows && !blankRow(buf, last + 1)) last += 1;
      }
      while (last + 1 < rows && blankRow(buf, last + 1)) last += 1;
    }
  }
  return { lines: true, a: first, b: last };
}

/**
 * The bytes an operator rewrites in place, leaving row structure alone.
 * @param {TextBuffer} buf
 * @param {Span} span
 * @returns {[number, number]}
 */
export function contentRange(buf, span) {
  if (!span.lines) return [span.a, span.b];
  return [buf.rowStart(span.a), buf.rowContentEnd(span.b)];
}

/**
 * The bytes a delete removes, including a row terminator so `dd` takes the row
 * break with it.
 * @param {TextBuffer} buf
 * @param {Span} span
 * @returns {[number, number]}
 */
export function deleteRange(buf, span) {
  return span.lines ? rowSpan(buf, span.a, span.b) : [span.a, span.b];
}

/**
 * Where the caret belongs once the operator is done.
 * @param {TextBuffer} buf
 * @param {Span} span
 * @returns {number}
 */
export function spanHome(buf, span) {
  return span.lines ? buf.rowStart(span.a) : span.a;
}
