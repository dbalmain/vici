// Vi key notation. Behaviour matches `crates/vici/src/key.rs`.
// Types come from the Engine contract; this module is parse / render / construct.

import type { Key, KeyCode } from "./contract/index.js";
import { Mods } from "./contract/index.js";

export class KeyParseError extends Error {
  readonly kind: "unterminated" | "unknown";
  readonly keyName?: string;

  constructor(kind: "unterminated" | "unknown", keyName?: string) {
    super(
      kind === "unterminated"
        ? "unterminated `<` in key sequence"
        : `unknown key name \`<${keyName}>\``,
    );
    this.name = "KeyParseError";
    this.kind = kind;
    if (keyName !== undefined) {
      this.keyName = keyName;
    }
  }
}

/** SHIFT+char uses ASCII uppercase and drops the SHIFT bit. */
export function makeKey(code: KeyCode, mods: Mods = Mods.NONE): Key {
  if (code.type === "Char" && (mods & Mods.SHIFT) !== 0) {
    return {
      code: { type: "Char", char: asciiUppercase(code.char) },
      mods: mods & ~Mods.SHIFT,
    };
  }
  return { code, mods };
}

export function charKey(ch: string): Key {
  return makeKey({ type: "Char", char: ch }, Mods.NONE);
}

export function ctrlKey(ch: string): Key {
  return makeKey({ type: "Char", char: ch }, Mods.CTRL);
}

export function codeKey(code: KeyCode): Key {
  return makeKey(code, Mods.NONE);
}

export function asText(key: Key): string | undefined {
  if (key.code.type === "Char" && key.mods === Mods.NONE) {
    return key.code.char;
  }
  return undefined;
}

export function asDigit(key: Key): number | undefined {
  const ch = asText(key);
  if (ch === undefined || ch.length !== 1) {
    return undefined;
  }
  const code = ch.codePointAt(0);
  if (code === undefined || code < 48 || code > 57) {
    return undefined;
  }
  return code - 48;
}

export function keys(spec: string): Key[] {
  const out: Key[] = [];
  const chars = [...spec];
  for (let i = 0; i < chars.length; i++) {
    const ch = chars[i]!;
    if (ch !== "<") {
      out.push(charKey(ch));
      continue;
    }
    let name = "";
    let closed = false;
    for (i += 1; i < chars.length; i++) {
      const inner = chars[i]!;
      if (inner === ">") {
        closed = true;
        break;
      }
      name += inner;
    }
    if (!closed) {
      throw new KeyParseError("unterminated");
    }
    out.push(parseBracketed(name));
  }
  return out;
}

export function key(spec: string): Key {
  const parsed = keys(spec);
  if (parsed.length !== 1) {
    throw new KeyParseError("unknown", spec);
  }
  return parsed[0]!;
}

export function renderKey(keyValue: Key): string {
  const { code, mods } = keyValue;
  if (code.type === "Char" && mods === Mods.NONE && code.char !== " " && code.char !== "<") {
    return code.char;
  }
  const name = keyName(code);
  let out = "<";
  if ((mods & Mods.CTRL) !== 0) {
    out += "C-";
  }
  if ((mods & Mods.ALT) !== 0) {
    out += "M-";
  }
  if ((mods & Mods.SHIFT) !== 0) {
    out += "S-";
  }
  return `${out}${name}>`;
}

export function render(sequence: readonly Key[]): string {
  return sequence.map(renderKey).join("");
}

function keyName(code: KeyCode): string {
  switch (code.type) {
    case "Char":
      if (code.char === " ") {
        return "Space";
      }
      if (code.char === "<") {
        return "lt";
      }
      return code.char;
    case "Esc":
      return "Esc";
    case "Enter":
      return "CR";
    case "Tab":
      return "Tab";
    case "Backspace":
      return "BS";
    case "Delete":
      return "Del";
    case "Insert":
      return "Insert";
    case "Left":
      return "Left";
    case "Right":
      return "Right";
    case "Up":
      return "Up";
    case "Down":
      return "Down";
    case "Home":
      return "Home";
    case "End":
      return "End";
    case "PageUp":
      return "PageUp";
    case "PageDown":
      return "PageDown";
    case "F":
      return `F${code.n}`;
    default: {
      const _never: never = code;
      return _never;
    }
  }
}

function parseBracketed(name: string): Key {
  let mods = Mods.NONE;
  let rest = name;
  for (;;) {
    if (rest.length < 2) {
      break;
    }
    const prefix = rest.slice(0, 2);
    const tail = rest.slice(2);
    let flag: Mods | undefined;
    if (prefix === "C-") {
      flag = Mods.CTRL;
    } else if (prefix === "M-" || prefix === "A-") {
      flag = Mods.ALT;
    } else if (prefix === "S-") {
      flag = Mods.SHIFT;
    } else {
      break;
    }
    // A trailing `-` is the key itself, as in `<C-->`.
    if (tail.length === 0) {
      break;
    }
    mods |= flag;
    rest = tail;
  }

  const named = NAMED[rest];
  if (named !== undefined) {
    return makeKey(named, mods);
  }

  const scalars = [...rest];
  if (scalars.length === 1) {
    return makeKey({ type: "Char", char: scalars[0]! }, mods);
  }
  if (scalars[0] === "F" && scalars.length > 1) {
    const n = parseU8(rest.slice(1));
    if (n !== undefined) {
      return makeKey({ type: "F", n }, mods);
    }
  }
  throw new KeyParseError("unknown", name);
}

const NAMED: Record<string, KeyCode> = {
  Esc: { type: "Esc" },
  CR: { type: "Enter" },
  Enter: { type: "Enter" },
  Return: { type: "Enter" },
  Tab: { type: "Tab" },
  BS: { type: "Backspace" },
  Del: { type: "Delete" },
  Insert: { type: "Insert" },
  Space: { type: "Char", char: " " },
  lt: { type: "Char", char: "<" },
  gt: { type: "Char", char: ">" },
  bslash: { type: "Char", char: "\\" },
  Left: { type: "Left" },
  Right: { type: "Right" },
  Up: { type: "Up" },
  Down: { type: "Down" },
  Home: { type: "Home" },
  End: { type: "End" },
  PageUp: { type: "PageUp" },
  PageDown: { type: "PageDown" },
};

function asciiUppercase(ch: string): string {
  const cp = ch.codePointAt(0);
  if (cp === undefined) {
    return ch;
  }
  if (cp >= 0x61 && cp <= 0x7a) {
    return String.fromCodePoint(cp - 0x20);
  }
  return String.fromCodePoint(cp);
}

function parseU8(digits: string): number | undefined {
  if (!/^\d+$/.test(digits)) {
    return undefined;
  }
  const n = Number(digits);
  if (!Number.isInteger(n) || n < 0 || n > 255) {
    return undefined;
  }
  return n;
}
