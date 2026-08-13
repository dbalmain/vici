// Motions and text objects. Outcomes match vici `motion.rs`.
// Graphemes (`h l x r`) use Intl.Segmenter. Word / find / search walk
// Unicode scalar values; search match starts sit on grapheme boundaries.

import type { Viewport } from "./contract/index.js";

import type { TextBuffer as Buffer } from "./text-buffer.js";
import { isAscii } from "./utf8.js";

export type Bound = "OnChar" | "PastEnd";

/** Sentinel sticky column meaning "stay at the end of the row", as `$` does. */
export const STICKY_END = Number.POSITIVE_INFINITY;

const utf8 = new TextEncoder();
const segmenter = new Intl.Segmenter(undefined, { granularity: "grapheme" });

const WHITE_SPACE = /^\p{White_Space}$/u;
const ALPHABETIC = /^\p{Alphabetic}$/u;
const NUMBER = /^\p{N}$/u;

export type Find = {
  target: string;
  backward: boolean;
  till: boolean;
};

export type LastSearch = {
  pattern: string;
  backward: boolean;
};

export type Motion =
  | "Left"
  | "Right"
  | "Down"
  | "Up"
  | "FirstColumn"
  | "FirstNonBlank"
  | "LastColumn"
  | "WordForward"
  | "BigWordForward"
  | "WordBackward"
  | "BigWordBackward"
  | "WordEnd"
  | "BigWordEnd"
  | "ParagraphForward"
  | "ParagraphBackward"
  | "GotoRow"
  | "GotoFirstRow"
  | "MatchPair"
  | "ScreenTop"
  | "ScreenMiddle"
  | "ScreenBottom"
  | "RepeatFind"
  | "RepeatFindReverse"
  | "RepeatSearch"
  | "RepeatSearchReverse"
  | { type: "Find"; target: string; backward: boolean; till: boolean }
  | { type: "Search"; pattern: string; backward: boolean }
  | { type: "Mark"; name: string; exact: boolean }
  | { type: "ToOffset"; offset: number; linewise: boolean };

export type ObjectScope = "Inner" | "Around";

export type TextObject =
  | { type: "Word"; big: boolean }
  | { type: "Delimited"; open: string; close: string }
  | { type: "Quoted"; quote: string }
  | { type: "Paragraph" };

export type Span =
  | { kind: "chars"; start: number; end: number }
  | { kind: "lines"; first: number; last: number };

/** Whole-row operator target: `j`/`k`/`G`/`gg`/`H`/`M`/`L`. */
export function isLinewise(motion: Motion): boolean {
  if (typeof motion === "object") {
    if (motion.type === "ToOffset") {
      return motion.linewise;
    }
    if (motion.type === "Mark") {
      return !motion.exact;
    }
    return false;
  }
  return (
    motion === "Down" ||
    motion === "Up" ||
    motion === "GotoRow" ||
    motion === "GotoFirstRow" ||
    motion === "ScreenTop" ||
    motion === "ScreenMiddle" ||
    motion === "ScreenBottom"
  );
}

/** Destination character is included. `;`/`,` inherit this from last find. */
export function isInclusive(motion: Motion): boolean {
  if (typeof motion === "object") {
    return motion.type === "Find" && !motion.backward;
  }
  return (
    motion === "LastColumn" ||
    motion === "WordEnd" ||
    motion === "BigWordEnd" ||
    motion === "MatchPair"
  );
}

export function findOf(motion: Motion): Find | undefined {
  if (typeof motion === "object" && motion.type === "Find") {
    return {
      target: motion.target,
      backward: motion.backward,
      till: motion.till,
    };
  }
  return undefined;
}

export function searchOf(
  motion: Motion,
): { pattern: string; backward: boolean } | undefined {
  if (typeof motion === "object" && motion.type === "Search") {
    return { pattern: motion.pattern, backward: motion.backward };
  }
  return undefined;
}

export function spanIsLinewise(span: Span): boolean {
  return span.kind === "lines";
}

/** In-place rewrite: linewise stops before the last row's terminator. */
export function spanContentRange(
  buffer: Buffer,
  span: Span,
): { start: number; end: number } {
  if (span.kind === "chars") {
    return { start: span.start, end: span.end };
  }
  return {
    start: buffer.rowRange(span.first).start,
    end: buffer.rowContentRange(span.last).end,
  };
}

/** Delete range: linewise takes a row break with it (`rowSpan`). */
export function spanDeleteRange(
  buffer: Buffer,
  span: Span,
): { start: number; end: number } {
  if (span.kind === "chars") {
    return { start: span.start, end: span.end };
  }
  return rowSpan(buffer, span.first, span.last);
}

export function spanHome(buffer: Buffer, span: Span): number {
  if (span.kind === "chars") {
    return span.start;
  }
  return buffer.rowContentRange(span.first).start;
}

/** Byte offsets of every grapheme boundary in `row`, including the row end. */
export function graphemeBoundaries(buffer: Buffer, row: number): number[] {
  const range = buffer.rowContentRange(row);
  const text = buffer.textIn(range.start, range.end);
  if (isAscii(text)) {
    const out = new Array<number>(text.length + 1);
    for (let i = 0; i <= text.length; i++) {
      out[i] = range.start + i;
    }
    return out;
  }
  const out: number[] = [];
  let byte = range.start;
  for (const { segment } of segmenter.segment(text)) {
    out.push(byte);
    byte += utf8.encode(segment).length;
  }
  out.push(range.end);
  return out;
}

function maxCol(boundaries: readonly number[], bound: Bound): number {
  const last = boundaries.length - 1;
  return bound === "PastEnd" ? last : Math.max(0, last - 1);
}

export function graphemeCol(buffer: Buffer, byte: number): number {
  const row = buffer.byteToPoint(byte).row;
  const boundaries = graphemeBoundaries(buffer, row);
  for (let i = boundaries.length - 1; i >= 0; i--) {
    if ((boundaries[i] ?? 0) <= byte) {
      return i;
    }
  }
  return 0;
}

function byteAtCol(
  buffer: Buffer,
  row: number,
  col: number,
  bound: Bound,
): number {
  const boundaries = graphemeBoundaries(buffer, row);
  const index = Math.min(col, maxCol(boundaries, bound));
  return boundaries[index] ?? boundaries[0] ?? 0;
}

export function clamp(buffer: Buffer, byte: number, bound: Bound): number {
  const limited = Math.min(Math.max(0, byte), buffer.lenBytes());
  const row = buffer.byteToPoint(limited).row;
  const boundaries = graphemeBoundaries(buffer, row);
  const allowed = boundaries.slice(0, maxCol(boundaries, bound) + 1);
  let i = 0;
  while (i < allowed.length && (allowed[i] ?? 0) <= limited) {
    i += 1;
  }
  return allowed[Math.max(0, i - 1)] ?? 0;
}

function nextGrapheme(buffer: Buffer, byte: number, bound: Bound): number {
  const row = buffer.byteToPoint(byte).row;
  return byteAtCol(buffer, row, graphemeCol(buffer, byte) + 1, bound);
}

function prevGrapheme(buffer: Buffer, byte: number, bound: Bound): number {
  const row = buffer.byteToPoint(byte).row;
  return byteAtCol(
    buffer,
    row,
    Math.max(0, graphemeCol(buffer, byte) - 1),
    bound,
  );
}

export function firstNonBlank(buffer: Buffer, row: number): number {
  const range = buffer.rowContentRange(row);
  const text = buffer.textIn(range.start, range.end);
  let offset = 0;
  for (const ch of text) {
    if (!WHITE_SPACE.test(ch)) {
      return range.start + offset;
    }
    offset += utf8.encode(ch).length;
  }
  return range.start;
}

function blankRow(buffer: Buffer, row: number): boolean {
  return buffer.rowText(row).trim() === "";
}

function utf8CharLen(first: number): number {
  if (first < 0x80) {
    return 1;
  }
  if (first < 0xe0) {
    return 2;
  }
  if (first < 0xf0) {
    return 3;
  }
  return 4;
}

function charAt(buffer: Buffer, byte: number): string | undefined {
  if (byte < 0 || byte >= buffer.lenBytes()) {
    return undefined;
  }
  const first = buffer.byte(byte);
  if (first < 0x80) {
    return String.fromCharCode(first);
  }
  const len = utf8CharLen(first);
  return buffer.textIn(byte, byte + len);
}

function advanceChar(buffer: Buffer, byte: number): number {
  if (byte < 0 || byte >= buffer.lenBytes()) {
    return byte;
  }
  const first = buffer.byte(byte);
  if (first < 0x80) {
    return byte + 1;
  }
  return byte + utf8CharLen(first);
}

function retreatChar(buffer: Buffer, byte: number): number {
  if (byte <= 0) {
    return 0;
  }
  let i = byte - 1;
  while (i > 0 && (buffer.byte(i) & 0xc0) === 0x80) {
    i -= 1;
  }
  return i;
}

type Class = "Blank" | "Word" | "Punct";

function classify(ch: string, big: boolean): Class {
  if (WHITE_SPACE.test(ch)) {
    return "Blank";
  }
  if (big || ch === "_" || ALPHABETIC.test(ch) || NUMBER.test(ch)) {
    return "Word";
  }
  return "Punct";
}

function classAt(buffer: Buffer, byte: number, big: boolean): Class | undefined {
  const ch = charAt(buffer, byte);
  return ch === undefined ? undefined : classify(ch, big);
}

function wordForward(buffer: Buffer, from: number, big: boolean): number {
  let pos = from;
  const start = classAt(buffer, pos, big);
  if (start !== undefined && start !== "Blank") {
    while (classAt(buffer, pos, big) === start) {
      pos = advanceChar(buffer, pos);
    }
  }
  while (classAt(buffer, pos, big) === "Blank") {
    pos = advanceChar(buffer, pos);
  }
  return pos;
}

/** `b` / `B`: the start of this word, or of the previous one. */
export function wordBackward(
  buffer: Buffer,
  from: number,
  big = false,
): number {
  let pos = retreatChar(buffer, from);
  while (pos > 0 && classAt(buffer, pos, big) === "Blank") {
    pos = retreatChar(buffer, pos);
  }
  const current = classAt(buffer, pos, big);
  if (current === undefined || current === "Blank") {
    return pos;
  }
  while (pos > 0) {
    const prev = retreatChar(buffer, pos);
    if (classAt(buffer, prev, big) === current) {
      pos = prev;
    } else {
      break;
    }
  }
  return pos;
}

function wordEnd(buffer: Buffer, from: number, big: boolean): number {
  let pos = advanceChar(buffer, from);
  while (classAt(buffer, pos, big) === "Blank") {
    pos = advanceChar(buffer, pos);
  }
  const current = classAt(buffer, pos, big);
  if (current === undefined) {
    return retreatChar(buffer, pos);
  }
  for (;;) {
    const next = advanceChar(buffer, pos);
    if (classAt(buffer, next, big) === current) {
      pos = next;
    } else {
      return pos;
    }
  }
}

function wordRun(
  buffer: Buffer,
  at: number,
  big: boolean,
): { start: number; end: number } {
  const current = classAt(buffer, at, big);
  if (current === undefined) {
    return { start: at, end: at };
  }
  let start = at;
  while (start > 0) {
    const prev = retreatChar(buffer, start);
    if (classAt(buffer, prev, big) === current) {
      start = prev;
    } else {
      break;
    }
  }
  let end = at;
  while (classAt(buffer, end, big) === current) {
    end = advanceChar(buffer, end);
  }
  return { start, end };
}

function blankRunEnd(buffer: Buffer, at: number, big: boolean): number {
  let end = at;
  while (classAt(buffer, end, big) === "Blank" && charAt(buffer, end) !== "\n") {
    end = advanceChar(buffer, end);
  }
  return end;
}

function blankRunStart(buffer: Buffer, at: number, big: boolean): number {
  let start = at;
  while (start > 0) {
    const prev = retreatChar(buffer, start);
    if (classAt(buffer, prev, big) === "Blank" && charAt(buffer, prev) !== "\n") {
      start = prev;
    } else {
      break;
    }
  }
  return start;
}

function paragraph(
  buffer: Buffer,
  from: number,
  backward: boolean,
  count: number,
  bound: Bound,
): number {
  let row = buffer.byteToPoint(from).row;
  const lastRow = buffer.lenRows() - 1;
  for (let i = 0; i < count; i++) {
    const next = backward ? (row > 0 ? row - 1 : undefined) : row < lastRow ? row + 1 : undefined;
    if (next === undefined) {
      return backward ? 0 : clamp(buffer, buffer.lenBytes(), bound);
    }
    row = next;
    while (row !== 0 && row !== lastRow && !blankRow(buffer, row)) {
      row = backward ? row - 1 : row + 1;
    }
    if (!blankRow(buffer, row)) {
      return backward ? 0 : clamp(buffer, buffer.lenBytes(), bound);
    }
  }
  return buffer.rowContentRange(row).start;
}

function nextCharOffset(text: string, at: number): number {
  const rest = text.slice(byteToJs(text, at));
  const ch = [...rest][0];
  return ch === undefined ? at : at + utf8.encode(ch).length;
}

function prevCharOffset(text: string, at: number): number {
  const head = text.slice(0, byteToJs(text, at));
  const chars = [...head];
  const ch = chars[chars.length - 1];
  return ch === undefined ? at : at - utf8.encode(ch).length;
}

/** JS string index of a UTF-8 byte offset in `text`. */
function byteToJs(text: string, byte: number): number {
  let seen = 0;
  let js = 0;
  for (const ch of text) {
    if (seen >= byte) {
      return js;
    }
    seen += utf8.encode(ch).length;
    js += ch.length;
  }
  return text.length;
}

function charOffsets(text: string): { offset: number; ch: string }[] {
  const out: { offset: number; ch: string }[] = [];
  let offset = 0;
  for (const ch of text) {
    out.push({ offset, ch });
    offset += utf8.encode(ch).length;
  }
  return out;
}

function findInRow(
  buffer: Buffer,
  from: number,
  find: Find,
  count: number,
  skipAdjacent: boolean,
): number | undefined {
  const row = buffer.byteToPoint(from).row;
  const range = buffer.rowContentRange(row);
  const text = buffer.textIn(range.start, range.end);
  const cursor = from - range.start;
  const origin =
    skipAdjacent && find.till
      ? find.backward
        ? prevCharOffset(text, cursor)
        : nextCharOffset(text, cursor)
      : cursor;

  let hit: number | undefined;
  const chars = charOffsets(text);
  if (find.backward) {
    let seen = 0;
    for (let i = chars.length - 1; i >= 0; i--) {
      const item = chars[i]!;
      if (item.offset >= origin) {
        continue;
      }
      if (item.ch === find.target) {
        seen += 1;
        if (seen === count) {
          hit = range.start + item.offset;
          break;
        }
      }
    }
  } else {
    let seen = 0;
    for (const item of chars) {
      if (item.offset <= origin) {
        continue;
      }
      if (item.ch === find.target) {
        seen += 1;
        if (seen === count) {
          hit = range.start + item.offset;
          break;
        }
      }
    }
  }
  if (hit === undefined) {
    return undefined;
  }
  if (!find.till) {
    return hit;
  }
  return find.backward ? advanceChar(buffer, hit) : retreatChar(buffer, hit);
}

const PAIRS: [string, string][] = [
  ["(", ")"],
  ["[", "]"],
  ["{", "}"],
];
const QUOTES = ['"', "'", "`"];

function matchPair(buffer: Buffer, at: number): number | undefined {
  const end = buffer.rowContentRange(buffer.byteToPoint(at).row).end;
  let pos = at;
  let quote: { pos: number; ch: string } | undefined;
  while (pos < end) {
    const ch = charAt(buffer, pos);
    if (ch === undefined) {
      return undefined;
    }
    const pair = PAIRS.find(([open, close]) => ch === open || ch === close);
    if (pair !== undefined) {
      const found = enclosingPair(buffer, pos, pair[0], pair[1]);
      if (found === undefined) {
        return undefined;
      }
      return pos === found.start ? found.end : found.start;
    }
    if (quote === undefined && QUOTES.includes(ch)) {
      quote = { pos, ch };
    }
    pos = advanceChar(buffer, pos);
  }
  if (quote === undefined) {
    return undefined;
  }
  const found = enclosingQuotes(buffer, quote.pos, quote.ch);
  if (found === undefined) {
    return undefined;
  }
  return quote.pos === found.start ? found.end : found.start;
}

function screenMotion(
  buffer: Buffer,
  motion: "ScreenTop" | "ScreenMiddle" | "ScreenBottom",
  repeat: number,
  viewport: Viewport,
): number | undefined {
  if (viewport.height === 0) {
    return undefined;
  }
  const last = buffer.lenRows() - 1;
  const top = Math.min(viewport.topRow, last);
  const bottom = Math.min(viewport.topRow + Math.max(0, viewport.height - 1), last);
  let row: number;
  switch (motion) {
    case "ScreenTop":
      row = Math.min(viewport.topRow + Math.max(0, repeat - 1), last);
      break;
    case "ScreenMiddle":
      row = top + Math.floor((bottom - top) / 2);
      break;
    case "ScreenBottom":
      row = Math.max(0, bottom - Math.max(0, repeat - 1));
      break;
  }
  return firstNonBlank(buffer, row);
}

function isUppercase(ch: string): boolean {
  return ch !== ch.toLowerCase();
}

function foldLower(text: string): string {
  let out = "";
  for (const ch of text) {
    out += ch.toLowerCase();
  }
  return out;
}

function literalPrefixAt(
  text: string,
  jsIndex: number,
  pattern: string,
  foldedPattern: string | undefined,
): boolean {
  if (foldedPattern === undefined) {
    return text.startsWith(pattern, jsIndex);
  }
  let candidate = "";
  let i = jsIndex;
  while (i < text.length) {
    const code = text.codePointAt(i);
    if (code === undefined) {
      return false;
    }
    const ch = String.fromCodePoint(code);
    i += ch.length;
    candidate += ch.toLowerCase();
    if (candidate === foldedPattern) {
      return true;
    }
    if (!foldedPattern.startsWith(candidate)) {
      return false;
    }
  }
  return false;
}

function search(
  buffer: Buffer,
  from: number,
  pattern: string,
  backward: boolean,
  repeat: number,
): number | undefined {
  if (pattern === "") {
    return undefined;
  }
  const text = buffer.toString();
  const sensitive = [...pattern].some(isUppercase);
  const folded = sensitive ? undefined : foldLower(pattern);
  const matches: number[] = [];
  if (isAscii(text) && isAscii(pattern)) {
    // Native indexOf. ASCII grapheme starts are every offset.
    const hay = folded === undefined ? text : text.toLowerCase();
    const needle = folded ?? pattern;
    let fromJs = 0;
    for (;;) {
      const at = hay.indexOf(needle, fromJs);
      if (at < 0) {
        break;
      }
      matches.push(at);
      fromJs = at + 1;
    }
  } else {
    // One pass over grapheme starts. Per-offset `byteToJs` + `slice(tail)`
    // was O(n²) and made the 100 KiB search bench unusable.
    let byte = 0;
    let js = 0;
    for (const { segment } of segmenter.segment(text)) {
      if (literalPrefixAt(text, js, pattern, folded)) {
        matches.push(byte);
      }
      byte += utf8.encode(segment).length;
      js += segment.length;
    }
  }
  if (matches.length === 0) {
    return undefined;
  }
  let landed = from;
  for (let i = 0; i < repeat; i++) {
    if (backward) {
      let hit: number | undefined;
      for (let j = matches.length - 1; j >= 0; j--) {
        const offset = matches[j]!;
        if (offset < landed) {
          hit = offset;
          break;
        }
      }
      landed = hit ?? matches[matches.length - 1]!;
    } else {
      landed = matches.find((offset) => offset > landed) ?? matches[0]!;
    }
  }
  return landed;
}

export function resolve(
  buffer: Buffer,
  from: number,
  motion: Motion,
  count: number | undefined,
  sticky: number,
  bound: Bound,
  lastFind?: Find,
  lastSearch?: LastSearch,
  viewport: Viewport = { topRow: 0, height: 0 },
): number | undefined {
  const repeat = count === undefined ? 1 : Math.max(1, count);
  const point = buffer.byteToPoint(from);
  const rows = buffer.lenRows();
  let target: number | undefined;
  if (typeof motion === "object") {
    if (motion.type === "Find") {
      target = findInRow(
        buffer,
        from,
        {
          target: motion.target,
          backward: motion.backward,
          till: motion.till,
        },
        repeat,
        false,
      );
    } else if (motion.type === "Search") {
      target = search(buffer, from, motion.pattern, motion.backward, repeat);
    } else if (motion.type === "ToOffset") {
      const offset = clamp(buffer, motion.offset, bound);
      target = motion.linewise
        ? firstNonBlank(buffer, buffer.byteToPoint(offset).row)
        : offset;
    } else {
      // Mark — Editor turns this into ToOffset before calling resolve.
      return undefined;
    }
  } else {
    switch (motion) {
      case "Left": {
        let pos = from;
        for (let i = 0; i < repeat; i++) {
          pos = prevGrapheme(buffer, pos, bound);
        }
        target = pos;
        break;
      }
      case "Right": {
        let pos = from;
        for (let i = 0; i < repeat; i++) {
          pos = nextGrapheme(buffer, pos, bound);
        }
        target = pos;
        break;
      }
      case "Down":
        target = byteAtCol(
          buffer,
          Math.min(point.row + repeat, rows - 1),
          sticky,
          bound,
        );
        break;
      case "Up":
        target = byteAtCol(
          buffer,
          Math.max(0, point.row - repeat),
          sticky,
          bound,
        );
        break;
      case "FirstColumn":
        target = buffer.rowContentRange(point.row).start;
        break;
      case "FirstNonBlank":
        target = firstNonBlank(buffer, point.row);
        break;
      case "LastColumn":
        target = byteAtCol(
          buffer,
          Math.min(point.row + repeat - 1, rows - 1),
          STICKY_END,
          bound,
        );
        break;
      case "WordForward":
      case "BigWordForward": {
        const big = motion === "BigWordForward";
        let pos = from;
        for (let i = 0; i < repeat; i++) {
          pos = wordForward(buffer, pos, big);
        }
        target = pos;
        break;
      }
      case "WordBackward":
      case "BigWordBackward": {
        const big = motion === "BigWordBackward";
        let pos = from;
        for (let i = 0; i < repeat; i++) {
          pos = wordBackward(buffer, pos, big);
        }
        target = pos;
        break;
      }
      case "WordEnd":
      case "BigWordEnd": {
        const big = motion === "BigWordEnd";
        let pos = from;
        for (let i = 0; i < repeat; i++) {
          pos = wordEnd(buffer, pos, big);
        }
        target = pos;
        break;
      }
      case "ParagraphForward":
        target = paragraph(buffer, from, false, repeat, bound);
        break;
      case "ParagraphBackward":
        target = paragraph(buffer, from, true, repeat, bound);
        break;
      case "GotoRow": {
        const targetRow =
          count === undefined
            ? rows - 1
            : Math.min(Math.max(0, count - 1), rows - 1);
        target = firstNonBlank(buffer, targetRow);
        break;
      }
      case "GotoFirstRow": {
        const targetRow =
          count === undefined ? 0 : Math.min(Math.max(0, count - 1), rows - 1);
        target = firstNonBlank(buffer, targetRow);
        break;
      }
      case "MatchPair":
        target = matchPair(buffer, from);
        break;
      case "ScreenTop":
      case "ScreenMiddle":
      case "ScreenBottom":
        target = screenMotion(buffer, motion, repeat, viewport);
        break;
      case "RepeatFind":
      case "RepeatFindReverse": {
        if (lastFind === undefined) {
          return undefined;
        }
        const find = {
          ...lastFind,
          backward:
            motion === "RepeatFindReverse"
              ? !lastFind.backward
              : lastFind.backward,
        };
        target = findInRow(buffer, from, find, repeat, true);
        break;
      }
      case "RepeatSearch":
      case "RepeatSearchReverse": {
        if (lastSearch === undefined) {
          return undefined;
        }
        const backward =
          motion === "RepeatSearchReverse"
            ? !lastSearch.backward
            : lastSearch.backward;
        target = search(buffer, from, lastSearch.pattern, backward, repeat);
        break;
      }
    }
  }
  if (target === undefined) {
    return undefined;
  }
  return clamp(buffer, target, bound);
}

export function rowSpan(
  buffer: Buffer,
  first: number,
  last: number,
): { start: number; end: number } {
  const lastRow = Math.min(last, buffer.lenRows() - 1);
  const start = buffer.rowRange(first).start;
  const end = buffer.rowRange(lastRow).end;
  if (end === buffer.lenBytes() && first > 0) {
    return { start: buffer.rowRange(first - 1).end - 1, end };
  }
  return { start, end };
}

function wordObject(
  buffer: Buffer,
  at: number,
  scope: ObjectScope,
  big: boolean,
  count: number,
): Span | undefined {
  const run = wordRun(buffer, at, big);
  if (run.start === run.end) {
    return undefined;
  }
  const spent = (end: number): boolean => {
    const ch = charAt(buffer, end);
    return ch === undefined || ch === "\n";
  };
  if (scope === "Inner") {
    let end = run.end;
    for (let i = 1; i < count; i++) {
      if (spent(end)) {
        break;
      }
      end =
        classAt(buffer, end, big) === "Blank"
          ? blankRunEnd(buffer, end, big)
          : wordRun(buffer, end, big).end;
    }
    return { kind: "chars", start: run.start, end };
  }
  let end = blankRunEnd(buffer, run.end, big);
  const trailing = end > run.end;
  for (let i = 1; i < count; i++) {
    if (spent(end)) {
      break;
    }
    end = blankRunEnd(buffer, wordRun(buffer, end, big).end, big);
  }
  const start = trailing ? run.start : blankRunStart(buffer, run.start, big);
  return { kind: "chars", start, end };
}

function enclosingPair(
  buffer: Buffer,
  at: number,
  open: string,
  close: string,
): { start: number; end: number } | undefined {
  let start: number;
  if (charAt(buffer, at) === open) {
    start = at;
  } else {
    let depth = 0;
    let pos = at;
    for (;;) {
      if (pos === 0) {
        return undefined;
      }
      pos = retreatChar(buffer, pos);
      const ch = charAt(buffer, pos);
      if (ch === close) {
        depth += 1;
      } else if (ch === open) {
        if (depth === 0) {
          start = pos;
          break;
        }
        depth -= 1;
      }
    }
  }

  let depth = 0;
  let pos = advanceChar(buffer, start);
  for (;;) {
    const ch = charAt(buffer, pos);
    if (ch === undefined) {
      return undefined;
    }
    if (ch === open) {
      depth += 1;
    } else if (ch === close) {
      if (depth === 0) {
        return { start, end: pos };
      }
      depth -= 1;
    }
    pos = advanceChar(buffer, pos);
  }
}

function enclosingQuotes(
  buffer: Buffer,
  at: number,
  quote: string,
): { start: number; end: number } | undefined {
  const row = buffer.byteToPoint(at).row;
  const range = buffer.rowContentRange(row);
  const text = buffer.textIn(range.start, range.end);
  const positions: number[] = [];
  for (const item of charOffsets(text)) {
    if (item.ch === quote) {
      positions.push(range.start + item.offset);
    }
  }
  for (let i = 0; i + 1 < positions.length; i += 2) {
    const open = positions[i]!;
    const close = positions[i + 1]!;
    if (at <= close) {
      return { start: open, end: close };
    }
  }
  return undefined;
}

function nextOpen(
  buffer: Buffer,
  from: number,
  limit: number,
  open: string,
  close: string,
): number | undefined {
  let pos = from;
  while (pos < limit) {
    const ch = charAt(buffer, pos);
    if (ch === open) {
      return pos;
    }
    if (ch === close) {
      return undefined;
    }
    pos = advanceChar(buffer, pos);
  }
  return undefined;
}

function seekPair(
  buffer: Buffer,
  at: number,
  open: string,
  close: string,
): { start: number; end: number } | undefined {
  const start = nextOpen(buffer, at, buffer.lenBytes(), open, close);
  if (start === undefined) {
    return undefined;
  }
  return enclosingPair(buffer, start, open, close);
}

function climbOut(
  buffer: Buffer,
  pair: { start: number; end: number },
  open: string,
  close: string,
  count: number,
): { start: number; end: number } | undefined {
  let { start, end } = pair;
  for (let i = 1; i < count; i++) {
    if (start === 0) {
      return undefined;
    }
    const outer = enclosingPair(
      buffer,
      retreatChar(buffer, start),
      open,
      close,
    );
    if (outer === undefined || outer.start >= start || outer.end <= end) {
      return undefined;
    }
    start = outer.start;
    end = outer.end;
  }
  return { start, end };
}

function descendInto(
  buffer: Buffer,
  pair: { start: number; end: number },
  open: string,
  close: string,
  count: number,
): { start: number; end: number } | undefined {
  let { start, end } = pair;
  for (let i = 1; i < count; i++) {
    const inner = nextOpen(
      buffer,
      advanceChar(buffer, start),
      end,
      open,
      close,
    );
    if (inner === undefined) {
      return undefined;
    }
    const found = enclosingPair(buffer, inner, open, close);
    if (found === undefined) {
      return undefined;
    }
    start = found.start;
    end = found.end;
  }
  return { start, end };
}

function pairAtLevel(
  buffer: Buffer,
  at: number,
  open: string,
  close: string,
  count: number,
): { start: number; end: number } | undefined {
  const enclosing = enclosingPair(buffer, at, open, close);
  if (enclosing !== undefined) {
    return climbOut(buffer, enclosing, open, close, count);
  }
  const sought = seekPair(buffer, at, open, close);
  if (sought === undefined) {
    return undefined;
  }
  return descendInto(buffer, sought, open, close, count);
}

function innerSpan(
  buffer: Buffer,
  open: number,
  close: number,
): { start: number; end: number } {
  const afterOpen = advanceChar(buffer, open);
  const openRow = buffer.byteToPoint(open).row;
  const startsBelow =
    afterOpen === buffer.rowContentRange(openRow).end &&
    openRow + 1 < buffer.lenRows();
  const start = startsBelow
    ? buffer.rowRange(openRow + 1).start
    : afterOpen;

  const closeRow = buffer.byteToPoint(close).row;
  const endsAbove =
    buffer.textIn(buffer.rowRange(closeRow).start, close).trim() === "";
  let end: number;
  if (endsAbove) {
    const above = buffer.rowContentRange(closeRow - 1).end;
    end = startsBelow ? above + 1 : above;
  } else {
    end = close;
  }
  return { start, end: Math.max(end, start) };
}

function pairSpan(
  buffer: Buffer,
  start: number,
  end: number,
  scope: ObjectScope,
): Span {
  if (scope === "Inner") {
    const inner = innerSpan(buffer, start, end);
    return { kind: "chars", start: inner.start, end: inner.end };
  }
  return { kind: "chars", start, end: advanceChar(buffer, end) };
}

function paragraphObject(
  buffer: Buffer,
  at: number,
  scope: ObjectScope,
  count: number,
): Span {
  const rows = buffer.lenRows();
  const row = buffer.byteToPoint(at).row;
  let first = row;
  while (first > 0 && !blankRow(buffer, first - 1)) {
    first -= 1;
  }
  let last = row;
  while (last + 1 < rows && !blankRow(buffer, last + 1)) {
    last += 1;
  }
  if (scope === "Inner") {
    for (let i = 1; i < count; i++) {
      if (last + 1 >= rows) {
        break;
      }
      const want = blankRow(buffer, last + 1);
      last += 1;
      while (last + 1 < rows && blankRow(buffer, last + 1) === want) {
        last += 1;
      }
    }
  } else {
    for (let step = 0; step < count; step++) {
      if (step > 0) {
        if (last + 1 >= rows || blankRow(buffer, last + 1)) {
          break;
        }
        last += 1;
        while (last + 1 < rows && !blankRow(buffer, last + 1)) {
          last += 1;
        }
      }
      while (last + 1 < rows && blankRow(buffer, last + 1)) {
        last += 1;
      }
    }
  }
  return { kind: "lines", first, last };
}

/** Byte offsets of the delimiters enclosing `at`. Word / paragraph: none. */
export function delimiters(
  buffer: Buffer,
  at: number,
  object: TextObject,
): { start: number; end: number } | undefined {
  switch (object.type) {
    case "Delimited":
      return enclosingPair(buffer, at, object.open, object.close);
    case "Quoted":
      return enclosingQuotes(buffer, at, object.quote);
    case "Word":
    case "Paragraph":
      return undefined;
  }
}

export function objectSpan(
  buffer: Buffer,
  at: number,
  scope: ObjectScope,
  object: TextObject,
  count: number,
): Span | undefined {
  const times = Math.max(1, count);
  switch (object.type) {
    case "Word":
      return wordObject(buffer, at, scope, object.big, times);
    case "Delimited": {
      const pair = pairAtLevel(buffer, at, object.open, object.close, times);
      if (pair === undefined) {
        return undefined;
      }
      return pairSpan(buffer, pair.start, pair.end, scope);
    }
    case "Quoted": {
      const pair = enclosingQuotes(buffer, at, object.quote);
      if (pair === undefined) {
        return undefined;
      }
      return pairSpan(buffer, pair.start, pair.end, scope);
    }
    case "Paragraph":
      return paragraphObject(buffer, at, scope, times);
  }
}
