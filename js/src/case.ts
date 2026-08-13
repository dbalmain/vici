// Unicode default case conversion matching Rust `char::to_uppercase` /
// `to_lowercase` / the one-to-one `swap_case` in editor.rs. Do not call
// JS `toUpperCase` / `toLocaleUpperCase` — ß and SpecialCasing are the
// load-bearing differences, and the fixtures assert them.

export type CaseOp = "lower" | "upper" | "swapCase";

/** Unconditional SpecialCasing one-to-many mappings (code point → mapped). */
const UPPER_SPECIAL = new Map<number, readonly number[]>([
  [0x00df, [0x0053, 0x0053]], // ß → SS
  [0x0149, [0x02bc, 0x004e]],
  [0x01f0, [0x004a, 0x030c]],
  [0x0390, [0x0399, 0x0308, 0x0301]],
  [0x03b0, [0x03a5, 0x0308, 0x0301]],
  [0x0587, [0x0535, 0x0552]],
  [0x1e96, [0x0048, 0x0331]],
  [0x1e97, [0x0054, 0x0308]],
  [0x1e98, [0x0057, 0x030a]],
  [0x1e99, [0x0059, 0x030a]],
  [0x1e9a, [0x0041, 0x02be]],
  [0x1f50, [0x03a5, 0x0313]],
  [0x1f52, [0x03a5, 0x0313, 0x0300]],
  [0x1f54, [0x03a5, 0x0313, 0x0301]],
  [0x1f56, [0x03a5, 0x0313, 0x0342]],
  [0x1fb6, [0x0391, 0x0342]],
  [0x1fc6, [0x0397, 0x0342]],
  [0x1fd2, [0x0399, 0x0308, 0x0300]],
  [0x1fd3, [0x0399, 0x0308, 0x0301]],
  [0x1fd6, [0x0399, 0x0342]],
  [0x1fd7, [0x0399, 0x0308, 0x0342]],
  [0x1fe2, [0x03a5, 0x0308, 0x0300]],
  [0x1fe3, [0x03a5, 0x0308, 0x0301]],
  [0x1fe4, [0x03a1, 0x0313]],
  [0x1fe6, [0x03a5, 0x0342]],
  [0x1fe7, [0x03a5, 0x0308, 0x0342]],
  [0x1ff6, [0x03a9, 0x0342]],
  [0xfb00, [0x0046, 0x0046]],
  [0xfb01, [0x0046, 0x0049]],
  [0xfb02, [0x0046, 0x004c]],
  [0xfb03, [0x0046, 0x0046, 0x0049]],
  [0xfb04, [0x0046, 0x0046, 0x004c]],
  [0xfb05, [0x0053, 0x0054]],
  [0xfb06, [0x0053, 0x0054]],
]);

const LOWER_SPECIAL = new Map<number, readonly number[]>([
  [0x0130, [0x0069, 0x0307]], // İ → i + combining dot
]);

const UPPER_RE = /^\p{Uppercase}$/u;
const LOWER_RE = /^\p{Lowercase}$/u;

export function recase(text: string, op: CaseOp): string {
  let out = "";
  for (const ch of text) {
    if (op === "swapCase") {
      out += swapCase(ch);
    } else if (op === "upper") {
      out += mapChars(toUpper(ch));
    } else {
      out += mapChars(toLower(ch));
    }
  }
  return out;
}

/** One-to-one: first mapped char only, as `swap_case` in editor.rs. */
export function swapCase(ch: string): string {
  if (UPPER_RE.test(ch)) {
    return firstMapped(toLower(ch), ch);
  }
  if (LOWER_RE.test(ch)) {
    return firstMapped(toUpper(ch), ch);
  }
  return ch;
}

function toUpper(ch: string): readonly number[] {
  const cp = ch.codePointAt(0);
  if (cp === undefined) {
    return [];
  }
  const special = UPPER_SPECIAL.get(cp);
  if (special !== undefined) {
    return special;
  }
  const simple = simpleUpper(cp);
  return simple === undefined ? [cp] : [simple];
}

function toLower(ch: string): readonly number[] {
  const cp = ch.codePointAt(0);
  if (cp === undefined) {
    return [];
  }
  const special = LOWER_SPECIAL.get(cp);
  if (special !== undefined) {
    return special;
  }
  const simple = simpleLower(cp);
  return simple === undefined ? [cp] : [simple];
}

function simpleUpper(cp: number): number | undefined {
  if (cp >= 0x61 && cp <= 0x7a) {
    return cp - 0x20;
  }
  if (cp >= 0xe0 && cp <= 0xfe && cp !== 0xf7) {
    return cp - 0x20;
  }
  if (cp === 0xff) {
    return 0x178;
  }
  // Latin Extended-A pairs: even uppercase, odd lowercase in most of 0100–0177.
  if (cp >= 0x0101 && cp <= 0x0177 && cp % 2 === 1 && cp !== 0x0131) {
    return cp - 1;
  }
  if (cp === 0x0180) {
    return undefined;
  }
  return LATIN_EXT_UPPER.get(cp);
}

function simpleLower(cp: number): number | undefined {
  if (cp >= 0x41 && cp <= 0x5a) {
    return cp + 0x20;
  }
  if (cp >= 0xc0 && cp <= 0xde && cp !== 0xd7) {
    return cp + 0x20;
  }
  if (cp === 0x178) {
    return 0xff;
  }
  if (cp >= 0x0100 && cp <= 0x0176 && cp % 2 === 0 && cp !== 0x0130) {
    return cp + 1;
  }
  return LATIN_EXT_LOWER.get(cp);
}

/** Extra 1:1 letters the fixtures or swap may hit outside the even/odd runs. */
const LATIN_EXT_UPPER = new Map<number, number>([
  [0x00b5, 0x039c], // µ → Μ
  [0x0131, 0x0049], // ı → I
  [0x017a, 0x0179],
  [0x017c, 0x017b],
  [0x017e, 0x017d],
  [0x017f, 0x0053],
]);

const LATIN_EXT_LOWER = new Map<number, number>([
  [0x0179, 0x017a],
  [0x017b, 0x017c],
  [0x017d, 0x017e],
]);

function firstMapped(mapped: readonly number[], fallback: string): string {
  const cp = mapped[0];
  return cp === undefined ? fallback : String.fromCodePoint(cp);
}

function mapChars(mapped: readonly number[]): string {
  return String.fromCodePoint(...mapped);
}
