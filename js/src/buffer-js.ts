// JS-string buffer. Public offsets are still UTF-8 bytes so the Engine
// contract and editor.vici snapshots do not change.
//
// ASCII is the fast path: JS index === UTF-8 byte, `toString()` is free,
// insert at end is `s += ch` (V8 cons strings). Non-ASCII walks from the
// nearest row start. This is the shippable storage and the JS speed ceiling
// on the (ASCII) benches.

import type { Edit, Point } from "./contract/index.js";

import { advance, type Change } from "./edit.js";
import { isAscii, utf8Len } from "./utf8.js";
import type { ByteRange, TextBuffer } from "./text-buffer.js";

const encoder = new TextEncoder();

export class JsBuffer implements TextBuffer {
  #text: string;
  /** UTF-8 byte offset of each row start. Always at least `[0]`. */
  #rowUtf8: number[];
  /** JS string index of each row start. Parallel to `#rowUtf8`. */
  #rowJs: number[];
  #byteLen: number;
  #ascii: boolean;

  constructor(text = "") {
    this.#text = text;
    this.#ascii = isAscii(text);
    this.#byteLen = this.#ascii ? text.length : utf8Len(text);
    const rows = rowMaps(text, this.#ascii);
    this.#rowUtf8 = rows.utf8;
    this.#rowJs = rows.js;
  }

  static fromText(text: string): JsBuffer {
    return new JsBuffer(text);
  }

  toString(): string {
    return this.#text;
  }

  lenBytes(): number {
    return this.#byteLen;
  }

  isEmpty(): boolean {
    return this.#byteLen === 0;
  }

  lenRows(): number {
    return this.#rowUtf8.length;
  }

  byte(idx: number): number {
    if (!Number.isInteger(idx) || idx < 0 || idx >= this.#byteLen) {
      throw new RangeError(`byte index ${idx} out of range`);
    }
    if (this.#ascii) {
      return this.#text.charCodeAt(idx);
    }
    const js = this.#utf8ToJs(idx);
    const code = this.#text.charCodeAt(js);
    if (code < 0x80) {
      return code;
    }
    const cp = this.#text.codePointAt(js);
    if (cp === undefined) {
      throw new Error("string invariant broken");
    }
    const bytes = encoder.encode(String.fromCodePoint(cp));
    const charStart = this.#jsToUtf8(js);
    const at = idx - charStart;
    const value = bytes[at];
    if (value === undefined) {
      throw new Error("UTF-8 mapping invariant broken");
    }
    return value;
  }

  byteToPoint(byte: number): Point {
    if (!Number.isInteger(byte) || byte < 0 || byte > this.#byteLen) {
      throw new RangeError(`byte offset ${byte} out of range`);
    }
    const row = this.#rowAt(byte);
    return { row, col: byte - (this.#rowUtf8[row] ?? 0) };
  }

  pointToByte(point: Point): number {
    const last = this.#rowUtf8.length - 1;
    const row = clampInt(point.row, 0, last);
    const col = point.col < 0 ? 0 : point.col;
    const content = this.rowContentRange(row);
    return Math.min(content.start + col, content.end);
  }

  rowRange(row: number): ByteRange {
    const start = this.#rowUtf8[row];
    if (start === undefined) {
      throw new RangeError(`row ${row} out of range`);
    }
    const next = this.#rowUtf8[row + 1];
    return { start, end: next === undefined ? this.#byteLen : next };
  }

  rowContentRange(row: number): ByteRange {
    const full = this.rowRange(row);
    let end = full.end;
    if (end > full.start && this.byte(end - 1) === 0x0a) {
      end -= 1;
    }
    if (end > full.start && this.byte(end - 1) === 0x0d) {
      end -= 1;
    }
    return { start: full.start, end };
  }

  rowText(row: number): string {
    const range = this.rowContentRange(row);
    return this.textIn(range.start, range.end);
  }

  textIn(start: number, end: number): string {
    this.#checkRange(start, end);
    return this.#text.slice(this.#utf8ToJs(start), this.#utf8ToJs(end));
  }

  stageReplace(start: number, end: number, text: string): Change {
    this.#checkRange(start, end);
    const startPoint = this.byteToPoint(start);
    const edit: Edit = {
      startByte: start,
      oldEndByte: end,
      newEndByte: start + utf8Len(text),
      startPoint,
      oldEndPoint: this.byteToPoint(end),
      newEndPoint: advance(startPoint, text),
    };
    return {
      edit,
      removed: this.textIn(start, end),
      inserted: text,
    };
  }

  apply(change: Change): void {
    const { edit } = change;
    if (this.textIn(edit.startByte, edit.oldEndByte) !== change.removed) {
      throw new Error("buffer does not match the change being applied");
    }
    const jsStart = this.#utf8ToJs(edit.startByte);
    const jsEnd = this.#utf8ToJs(edit.oldEndByte);
    const inserted = change.inserted;
    this.#spliceRows(edit.startByte, edit.oldEndByte, inserted, jsStart);
    if (jsStart === this.#text.length) {
      this.#text += inserted;
    } else {
      this.#text =
        this.#text.slice(0, jsStart) + inserted + this.#text.slice(jsEnd);
    }
    this.#byteLen += utf8Len(inserted) - (edit.oldEndByte - edit.startByte);
    if (!isAscii(inserted)) {
      this.#ascii = false;
    } else if (!this.#ascii && isAscii(this.#text)) {
      this.#ascii = true;
    }
  }

  replace(start: number, end: number, text: string): Change {
    const change = this.stageReplace(start, end, text);
    this.apply(change);
    return change;
  }

  insert(at: number, text: string): Change {
    return this.replace(at, at, text);
  }

  delete(start: number, end: number): Change {
    return this.replace(start, end, "");
  }

  #checkRange(start: number, end: number): void {
    if (
      !Number.isInteger(start) ||
      !Number.isInteger(end) ||
      start < 0 ||
      end < start ||
      end > this.#byteLen
    ) {
      throw new RangeError(`byte range ${start}..${end} out of range`);
    }
  }

  #rowAt(byte: number): number {
    let lo = 0;
    let hi = this.#rowUtf8.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if ((this.#rowUtf8[mid] ?? 0) <= byte) {
        lo = mid;
      } else {
        hi = mid - 1;
      }
    }
    return lo;
  }

  #utf8ToJs(byte: number): number {
    if (this.#ascii) {
      return byte;
    }
    if (byte <= 0) {
      return 0;
    }
    if (byte >= this.#byteLen) {
      return this.#text.length;
    }
    const row = this.#rowAt(byte);
    let b = this.#rowUtf8[row] ?? 0;
    let js = this.#rowJs[row] ?? 0;
    const endJs = this.#rowJs[row + 1] ?? this.#text.length;
    while (js < endJs && b < byte) {
      const c = this.#text.charCodeAt(js);
      let width = 1;
      let units = 1;
      if (c < 0x80) {
        width = 1;
      } else if (c < 0x800) {
        width = 2;
      } else if (c >= 0xd800 && c <= 0xdbff) {
        width = 4;
        units = 2;
      } else {
        width = 3;
      }
      if (b + width > byte) {
        return js;
      }
      b += width;
      js += units;
    }
    return js;
  }

  #jsToUtf8(js: number): number {
    if (this.#ascii) {
      return js;
    }
    return jsToUtf8FromRow(this.#text, this.#rowUtf8, this.#rowJs, js);
  }

  #spliceRows(
    start: number,
    oldEnd: number,
    inserted: string,
    jsStart: number,
  ): void {
    const first = this.#rowAt(start);
    let i = first + 1;
    while (i < this.#rowUtf8.length && (this.#rowUtf8[i] ?? 0) <= oldEnd) {
      i += 1;
    }
    const addedUtf8: number[] = [];
    const addedJs: number[] = [];
    let utf8At = start;
    for (let j = 0; j < inserted.length; j++) {
      const c = inserted.charCodeAt(j);
      if (c < 0x80) {
        utf8At += 1;
      } else if (c < 0x800) {
        utf8At += 2;
      } else if (c >= 0xd800 && c <= 0xdbff) {
        utf8At += 4;
        j += 1;
      } else {
        utf8At += 3;
      }
      if (inserted[j] === "\n") {
        addedUtf8.push(utf8At);
        addedJs.push(jsStart + j + 1);
      }
    }
    const utf8Delta = utf8Len(inserted) - (oldEnd - start);
    const jsOldEnd = this.#utf8ToJs(oldEnd);
    const jsDelta = inserted.length - (jsOldEnd - jsStart);
    const tailUtf8: number[] = [];
    const tailJs: number[] = [];
    for (; i < this.#rowUtf8.length; i++) {
      tailUtf8.push((this.#rowUtf8[i] ?? 0) + utf8Delta);
      tailJs.push((this.#rowJs[i] ?? 0) + jsDelta);
    }
    this.#rowUtf8 = this.#rowUtf8.slice(0, first + 1).concat(addedUtf8, tailUtf8);
    this.#rowJs = this.#rowJs.slice(0, first + 1).concat(addedJs, tailJs);
  }
}

function jsToUtf8FromRow(
  text: string,
  rowUtf8: readonly number[],
  rowJs: readonly number[],
  js: number,
): number {
  let lo = 0;
  let hi = rowJs.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if ((rowJs[mid] ?? 0) <= js) {
      lo = mid;
    } else {
      hi = mid - 1;
    }
  }
  let b = rowUtf8[lo] ?? 0;
  let i = rowJs[lo] ?? 0;
  const end = rowJs[lo + 1] ?? text.length;
  const stop = js < end ? js : end;
  while (i < stop) {
    const c = text.charCodeAt(i);
    if (c < 0x80) {
      b += 1;
      i += 1;
    } else if (c < 0x800) {
      b += 2;
      i += 1;
    } else if (c >= 0xd800 && c <= 0xdbff) {
      b += 4;
      i += 2;
    } else {
      b += 3;
      i += 1;
    }
  }
  return b;
}

function rowMaps(
  text: string,
  ascii: boolean,
): { utf8: number[]; js: number[] } {
  const utf8 = [0];
  const js = [0];
  if (ascii) {
    for (let i = 0; i < text.length; i++) {
      if (text.charCodeAt(i) === 0x0a) {
        utf8.push(i + 1);
        js.push(i + 1);
      }
    }
    return { utf8, js };
  }
  let b = 0;
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    if (c < 0x80) {
      b += 1;
    } else if (c < 0x800) {
      b += 2;
    } else if (c >= 0xd800 && c <= 0xdbff) {
      b += 4;
      i += 1;
    } else {
      b += 3;
    }
    if (text.charCodeAt(i) === 0x0a) {
      utf8.push(b);
      js.push(i + 1);
    }
  }
  return { utf8, js };
}

function clampInt(value: number, min: number, max: number): number {
  if (value < min) {
    return min;
  }
  if (value > max) {
    return max;
  }
  return value;
}
