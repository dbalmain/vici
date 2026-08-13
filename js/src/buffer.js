// The text buffer. Knows nothing about modes, keys, history or rendering.
//
// # Why bytes
//
// Every index in the public API is a UTF-8 byte offset, and every point column
// is a byte offset within its row — the coordinate system `tree_sitter::Point`
// and the Rust core both use. Storing UTF-8 directly is what makes that free:
// an engine over a JavaScript string pays a scan to convert every offset it
// hands out, on every keystroke, forever. Here the offsets *are* the storage.
//
// Rows are counted by LF only, matching the parser. A `\r\n` is one row break
// with the `\r` left as an ordinary content byte at the end of the row.

import { graphemeStarts } from './unicode.js';

const ENCODER = new TextEncoder();
const DECODER = new TextDecoder();
const LF = 0x0a;
const CR = 0x0d;

/** @typedef {{ row: number, col: number }} Point */
/**
 * @typedef {{
 *   startByte: number, oldEndByte: number, newEndByte: number,
 *   startPoint: Point, oldEndPoint: Point, newEndPoint: Point,
 * }} Edit
 */
/**
 * @typedef {{
 *   edit: Edit, removed: string, inserted: string,
 *   removedBytes: Uint8Array, insertedBytes: Uint8Array, insertedRows: number[],
 * }} Change
 */

/**
 * Shift an offset from an edit's pre-image into its post-image.
 *
 * The one gravity rule for every remembered position: callers do not choose an
 * affinity. An offset inside removed text collapses to the edit's start.
 * @param {Edit} edit
 * @param {number} offset
 * @returns {number}
 */
export function shift(edit, offset) {
  if (offset <= edit.startByte) return offset;
  if (offset >= edit.oldEndByte) return offset - edit.oldEndByte + edit.newEndByte;
  return edit.startByte;
}

/**
 * The geometry of the change that would put this one back.
 * @param {Edit} edit
 * @returns {Edit}
 */
export function invertEdit(edit) {
  return {
    startByte: edit.startByte,
    oldEndByte: edit.newEndByte,
    newEndByte: edit.oldEndByte,
    startPoint: edit.startPoint,
    oldEndPoint: edit.newEndPoint,
    newEndPoint: edit.oldEndPoint,
  };
}

/**
 * The change that undoes this one.
 * @param {Change} change
 * @returns {Change}
 */
export function invertChange(change) {
  return {
    edit: invertEdit(change.edit),
    removed: change.inserted,
    inserted: change.removed,
    removedBytes: change.insertedBytes,
    insertedBytes: change.removedBytes,
    insertedRows: rowOffsets(change.removedBytes),
  };
}

/**
 * Offsets of every LF in `bytes`, relative to its start.
 * @param {Uint8Array} bytes
 * @returns {number[]}
 */
function rowOffsets(bytes) {
  const out = [];
  for (let i = bytes.indexOf(LF); i >= 0; i = bytes.indexOf(LF, i + 1)) out.push(i);
  return out;
}

/** A UTF-8 text buffer addressed entirely in byte offsets. */
export class TextBuffer {
  /** @param {string} [text] */
  constructor(text = '') {
    const bytes = ENCODER.encode(text);
    /** Gap buffer: bytes live in `[0, gap)` and `[gapEnd, data.length)`. */
    this.data = new Uint8Array(Math.max(bytes.length + 64, 256));
    this.data.set(bytes);
    this.gap = bytes.length;
    this.gapEnd = this.data.length;
    this.size = bytes.length;
    /** Start offset of every row. Rows are counted by LF only. */
    this.rows = [0];
    /** Non-ASCII byte count. Zero unlocks the arithmetic fast paths. */
    this.wide = 0;
    this.#scan(bytes);
    /** @type {string | null} */
    this.cache = null;
    /** Bumped by every applied change, so derived caches can tell they are stale. */
    this.stamp = 0;
  }

  /**
   * @param {Uint8Array} bytes
   */
  #scan(bytes) {
    for (let i = 0; i < bytes.length; i += 1) {
      const byte = bytes[i];
      if (byte === LF) this.rows.push(i + 1);
      else if (byte >= 0x80) this.wide += 1;
    }
  }

  /** @returns {number} */
  get length() {
    return this.size;
  }

  /** Number of rows. An empty buffer has one row. @returns {number} */
  get rowCount() {
    return this.rows.length;
  }

  /**
   * The byte at `at`, or `-1` past the end.
   * @param {number} at
   * @returns {number}
   */
  byteAt(at) {
    if (at >= this.size || at < 0) return -1;
    return this.data[at < this.gap ? at : at + (this.gapEnd - this.gap)];
  }

  /**
   * The row containing `at`.
   * @param {number} at
   * @returns {number}
   */
  rowOf(at) {
    const rows = this.rows;
    let low = 0;
    let high = rows.length - 1;
    while (low < high) {
      const mid = (low + high + 1) >> 1;
      if (rows[mid] <= at) low = mid;
      else high = mid - 1;
    }
    return low;
  }

  /**
   * Start of `row`.
   * @param {number} row
   * @returns {number}
   */
  rowStart(row) {
    return this.rows[Math.min(row, this.rows.length - 1)];
  }

  /**
   * End of `row`, including its line terminator.
   * @param {number} row
   * @returns {number}
   */
  rowEnd(row) {
    return row + 1 < this.rows.length ? this.rows[row + 1] : this.size;
  }

  /**
   * End of `row`, excluding its line terminator. The range a row-scoped
   * operation targets, so restoring content never disturbs row structure.
   * @param {number} row
   * @returns {number}
   */
  rowContentEnd(row) {
    const start = this.rowStart(row);
    let end = this.rowEnd(row);
    if (end > start && this.byteAt(end - 1) === LF) end -= 1;
    if (end > start && this.byteAt(end - 1) === CR) end -= 1;
    return end;
  }

  /**
   * @param {number} at
   * @returns {Point}
   */
  pointAt(at) {
    const row = this.rowOf(at);
    return { row, col: at - this.rows[row] };
  }

  /**
   * Clamps into the buffer rather than throwing, so a stale point converts.
   * @param {Point} point
   * @returns {number}
   */
  byteOfPoint(point) {
    const row = Math.min(point.row, this.rows.length - 1);
    return Math.min(this.rowStart(row) + point.col, this.rowContentEnd(row));
  }

  /**
   * Whether `row` is pure ASCII, so grapheme columns are byte arithmetic.
   * @param {number} row
   * @returns {boolean}
   */
  asciiRow(row) {
    if (this.wide === 0) return true;
    const end = this.rowContentEnd(row);
    for (let at = this.rowStart(row); at < end; at += 1) {
      if (this.byteAt(at) >= 0x80) return false;
    }
    return true;
  }

  /**
   * The code point starting at `at`, or `-1` past the end.
   * @param {number} at
   * @returns {number}
   */
  charAt(at) {
    const lead = this.byteAt(at);
    if (lead < 0x80) return lead;
    if (lead < 0xe0) return ((lead & 0x1f) << 6) | (this.byteAt(at + 1) & 0x3f);
    if (lead < 0xf0) {
      return ((lead & 0x0f) << 12) | ((this.byteAt(at + 1) & 0x3f) << 6) | (this.byteAt(at + 2) & 0x3f);
    }
    return (
      ((lead & 0x07) << 18) |
      ((this.byteAt(at + 1) & 0x3f) << 12) |
      ((this.byteAt(at + 2) & 0x3f) << 6) |
      (this.byteAt(at + 3) & 0x3f)
    );
  }

  /**
   * The next UTF-8 character boundary, saturating at the end.
   * @param {number} at
   * @returns {number}
   */
  nextChar(at) {
    const lead = this.byteAt(at);
    if (lead < 0) return at;
    return at + (lead < 0x80 ? 1 : lead < 0xe0 ? 2 : lead < 0xf0 ? 3 : 4);
  }

  /**
   * The previous UTF-8 character boundary, saturating at zero.
   * @param {number} at
   * @returns {number}
   */
  prevChar(at) {
    let pos = at - 1;
    while (pos > 0 && (this.byteAt(pos) & 0xc0) === 0x80) pos -= 1;
    return Math.max(pos, 0);
  }

  /**
   * @param {number} start
   * @param {number} end
   * @returns {string}
   */
  textIn(start, end) {
    if (end <= start) return '';
    if (this.wide === 0 && end - start <= 256) {
      // Short ASCII runs — every row of source code — beat `TextDecoder`'s
      // fixed per-call cost by a wide margin.
      let out = '';
      for (let at = start; at < end; at += 1) out += String.fromCharCode(this.byteAt(at));
      return out;
    }
    return DECODER.decode(this.slice(start, end));
  }

  /**
   * The bytes in `[start, end)`, copied only when the range spans the gap.
   * @param {number} start
   * @param {number} end
   * @returns {Uint8Array}
   */
  slice(start, end) {
    const skip = this.gapEnd - this.gap;
    if (end <= this.gap) return this.data.subarray(start, end);
    if (start >= this.gap) return this.data.subarray(start + skip, end + skip);
    const out = new Uint8Array(end - start);
    out.set(this.data.subarray(start, this.gap));
    out.set(this.data.subarray(this.gapEnd, end + skip), this.gap - start);
    return out;
  }

  /** @returns {string} */
  toString() {
    this.cache ??= DECODER.decode(this.slice(0, this.size));
    return this.cache;
  }

  /**
   * The whole buffer as one contiguous array, for scanning.
   * @returns {Uint8Array}
   */
  contiguous() {
    this.#moveGap(this.size);
    return this.data.subarray(0, this.size);
  }

  /**
   * Compute the change that replacing `[start, end)` with `text` would produce,
   * without mutating anything — so a history can observe the pre-image.
   * @param {number} start
   * @param {number} end
   * @param {string} text
   * @returns {Change}
   */
  stage(start, end, text) {
    const inserted = ENCODER.encode(text);
    const startPoint = this.pointAt(start);
    const breaks = rowOffsets(inserted);
    const last = breaks.length > 0 ? breaks[breaks.length - 1] : -1;
    return {
      edit: {
        startByte: start,
        oldEndByte: end,
        newEndByte: start + inserted.length,
        startPoint,
        oldEndPoint: this.pointAt(end),
        // A newline in the inserted text measures the new column from the
        // start of its *final* row, not from where the insertion began.
        newEndPoint:
          last < 0
            ? { row: startPoint.row, col: startPoint.col + inserted.length }
            : { row: startPoint.row + breaks.length, col: inserted.length - last - 1 },
      },
      removed: this.textIn(start, end),
      inserted: text,
      removedBytes: end > start ? this.slice(start, end).slice() : EMPTY,
      insertedBytes: inserted,
      insertedRows: breaks,
    };
  }

  /**
   * Apply a previously staged change.
   * @param {Change} change
   */
  apply(change) {
    const { startByte: start, oldEndByte: end } = change.edit;
    const insert = change.insertedBytes;
    this.#moveGap(start);
    // Removed text is swallowed by widening the gap; inserted text fills it.
    this.gapEnd += end - start;
    this.#reserve(insert.length);
    this.data.set(insert, this.gap);
    this.gap += insert.length;
    this.size += insert.length - (end - start);
    this.cache = null;
    this.stamp += 1;

    for (let i = 0; i < change.removedBytes.length; i += 1) {
      if (change.removedBytes[i] >= 0x80) this.wide -= 1;
    }
    for (let i = 0; i < insert.length; i += 1) {
      if (insert[i] >= 0x80) this.wide += 1;
    }
    this.#reroll(start, end, insert.length, change.insertedRows);
  }

  /**
   * Rebuild the row index across an applied edit.
   * @param {number} start
   * @param {number} end
   * @param {number} length
   * @param {number[]} breaks
   */
  #reroll(start, end, length, breaks) {
    const rows = this.rows;
    const first = this.rowOf(start) + 1;
    let past = first;
    // A row start at `p` exists because of the LF at `p - 1`, which this edit
    // removed exactly when `start < p <= end`.
    while (past < rows.length && rows[past] <= end) past += 1;
    const added = breaks.map((offset) => start + offset + 1);
    const delta = length - (end - start);

    if (added.length > 4096) {
      this.rows = rows.slice(0, first).concat(added, rows.slice(past));
      for (let i = first + added.length; i < this.rows.length; i += 1) this.rows[i] += delta;
      return;
    }
    rows.splice(first, past - first, ...added);
    if (delta !== 0) {
      for (let i = first + added.length; i < rows.length; i += 1) rows[i] += delta;
    }
  }

  /**
   * @param {number} to
   */
  #moveGap(to) {
    const skip = this.gapEnd - this.gap;
    if (to === this.gap || skip === 0) {
      this.gap = to;
      this.gapEnd = to + skip;
      return;
    }
    if (to < this.gap) this.data.copyWithin(to + skip, to, this.gap);
    else this.data.copyWithin(this.gap, this.gapEnd, to + skip);
    this.gap = to;
    this.gapEnd = to + skip;
  }

  /**
   * @param {number} want
   */
  #reserve(want) {
    if (this.gapEnd - this.gap >= want) return;
    const capacity = Math.max(this.size * 2, this.size + want + 1024);
    const grown = new Uint8Array(capacity);
    grown.set(this.data.subarray(0, this.gap));
    const tail = this.data.length - this.gapEnd;
    grown.set(this.data.subarray(this.gapEnd), capacity - tail);
    this.data = grown;
    this.gapEnd = capacity - tail;
  }

  /**
   * UTF-16 offsets of grapheme starts within a row's text, for the rare row
   * that cannot answer arithmetically.
   * @param {number} row
   * @returns {number[]}
   */
  rowGraphemes(row) {
    return graphemeStarts(this.textIn(this.rowStart(row), this.rowContentEnd(row)));
  }
}

const EMPTY = new Uint8Array(0);
