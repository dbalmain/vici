// Port of `/home/dave/w/vici/crates/vici/tests/editor_cases.rs`
// (`render_case`, `render_effect`). Snapshot strings must match Rust `Debug`.

import type { Effect, Engine } from "./types.js";

/** a–z then the automatic marks, matching `editor_cases.rs`. */
const MARK_ORDER = [
  ..."abcdefghijklmnopqrstuvwxyz",
  "<",
  ">",
  "[",
  "]",
  "^",
] as const;

export function renderCase(
  name: string,
  engine: Engine,
  effects: readonly Effect[],
): string {
  const selection = engine.selection();
  const selectionText =
    selection === null ? "-" : `${selection.start}..${selection.end}`;
  const register = engine.register();
  const registerKind = register.linewise ? "line" : "char";
  const recording = engine.recording() ?? "-";
  const point = engine.cursorPoint();
  const marks = formatMarks(engine.marks());
  const jumps = formatJumps(engine.jumps());
  let snapshot =
    `== ${name} ==\n` +
    `text: ${rustDebugString(engine.text())}\n` +
    `cursor: ${engine.cursor()} @ ${point.row}:${point.col}\n` +
    `mode: ${engine.mode()}; selection: ${selectionText}\n` +
    `register: ${registerKind} ${rustDebugString(register.text)}\n` +
    `history: undo=${engine.undoDepth()} redo=${engine.redoDepth()}\n` +
    `jumps: ${jumps}\n` +
    `marks: ${marks}\n` +
    `pending: ${rustDebugString(engine.pending())}; last-change: ${rustDebugString(engine.lastChange())}; recording: ${recording}\n` +
    `effects:\n`;
  for (const effect of effects) {
    snapshot += `  ${renderEffect(effect)}\n`;
  }
  snapshot += "\n";
  return snapshot;
}

export function renderEffect(effect: Effect): string {
  switch (effect.type) {
    case "Edit": {
      const e = effect.edit;
      return (
        `edit ${e.startByte}..${e.oldEndByte} -> ${e.newEndByte}; ` +
        `(${e.startPoint.row},${e.startPoint.col})..` +
        `(${e.oldEndPoint.row},${e.oldEndPoint.col}) -> ` +
        `(${e.newEndPoint.row},${e.newEndPoint.col})`
      );
    }
    case "ModeChanged":
      return `mode ${effect.mode}`;
    case "Scroll":
      return `scroll ${effect.scroll}`;
    case "CommandPrompt":
      return "command prompt :";
    case "Bell":
      return "bell";
    case "RecordingStarted":
      return `recording @${effect.register}`;
    case "RecordingStopped":
      return `recorded @${effect.register}`;
  }
}

/**
 * Rust `{:?}` for `&str` / `String`. Special-cases `"`, `\`, `\n`, `\r`, `\t`,
 * `\0`; printable ASCII and most assigned Unicode stay literal; combining
 * marks, controls, private-use, extra whitespace, default-ignorables, format
 * controls, and unassigned code points become `\u{hex}` (lowercase, no pad).
 */
export function rustDebugString(value: string): string {
  let out = '"';
  for (const ch of value) {
    out += rustDebugChar(ch);
  }
  return `${out}"`;
}

const WHITE_SPACE = /^\p{White_Space}$/u;
const GRAPHEME_EXTEND = /^\p{Grapheme_Extend}$/u;
const DEFAULT_IGNORABLE = /^\p{Default_Ignorable_Code_Point}$/u;
const FORMAT = /^\p{gc=Cf}$/u;
const UNASSIGNED = /^\p{gc=Cn}$/u;

function rustDebugChar(ch: string): string {
  switch (ch) {
    case '"':
      return '\\"';
    case "\\":
      return "\\\\";
    case "\n":
      return "\\n";
    case "\t":
      return "\\t";
    case "\r":
      return "\\r";
    case "\0":
      return "\\0";
    default:
      break;
  }
  const cp = ch.codePointAt(0);
  if (cp === undefined) {
    return ch;
  }
  // ASCII printable, plus two halfwidth katakana voice marks Rust leaves raw.
  if ((cp >= 0x20 && cp <= 0x7e) || cp === 0xff9e || cp === 0xff9f) {
    return ch;
  }
  if (shouldEscape(ch, cp)) {
    return `\\u{${cp.toString(16)}}`;
  }
  return ch;
}

function shouldEscape(ch: string, cp: number): boolean {
  if (cp <= 0x1f || (cp >= 0x7f && cp <= 0x9f)) {
    return true;
  }
  if (
    (cp >= 0xe000 && cp <= 0xf8ff) ||
    (cp >= 0xf0000 && cp <= 0xffffd) ||
    (cp >= 0x100000 && cp <= 0x10fffd)
  ) {
    return true;
  }
  if (cp >= 0xd800 && cp <= 0xdfff) {
    return true;
  }
  return (
    WHITE_SPACE.test(ch) ||
    (cp > 0x02ff && GRAPHEME_EXTEND.test(ch)) ||
    DEFAULT_IGNORABLE.test(ch) ||
    FORMAT.test(ch) ||
    UNASSIGNED.test(ch)
  );
}

function formatMarks(
  marks: readonly { name: string; offset: number }[],
): string {
  const byName = new Map(marks.map((mark) => [mark.name, mark.offset]));
  const parts: string[] = [];
  for (const name of MARK_ORDER) {
    const offset = byName.get(name);
    if (offset !== undefined) {
      parts.push(`${name}:${offset}`);
    }
  }
  return parts.length === 0 ? "[]" : `[${parts.join(", ")}]`;
}

function formatJumps(jumps: readonly number[]): string {
  return jumps.length === 0 ? "[]" : `[${jumps.join(", ")}]`;
}
