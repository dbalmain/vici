// Character classification and case mapping.
//
// The Rust core leans on `char::is_whitespace`, `is_alphanumeric`, `is_upper`
// and the one-to-many case mappings. Each has an exact JavaScript counterpart
// in a Unicode property escape or in `String.prototype.toUpperCase` applied one
// code point at a time — no vendored tables, and no version skew of our own
// making. ASCII, which is every hot path, never reaches the regexes.

const WHITE_SPACE = /\p{White_Space}/u;
const ALPHANUMERIC = /[\p{Alphabetic}\p{Nd}\p{Nl}\p{No}]/u;
const UPPERCASE = /\p{Uppercase}/u;
const LOWERCASE = /\p{Lowercase}/u;

/** vi's three character classes. `Blank` sorts first so `class || 0` is falsy-safe. */
export const BLANK = 0;
export const WORD = 1;
export const PUNCT = 2;

/** ASCII class table, indexed by code point. */
const ASCII_CLASS = new Uint8Array(128).fill(PUNCT);
for (let i = 0; i < 128; i += 1) {
  const ch = String.fromCharCode(i);
  if (i === 0x20 || (i >= 0x09 && i <= 0x0d)) ASCII_CLASS[i] = BLANK;
  else if (ch === '_' || (ch >= '0' && ch <= '9') || /[a-zA-Z]/.test(ch)) ASCII_CLASS[i] = WORD;
}

/**
 * vi's character class. A `big` word (`W`, `B`, `E`) collapses word and
 * punctuation into one, so `foo.bar` is one WORD but three words.
 * @param {number} cp code point
 * @param {boolean} big
 * @returns {number}
 */
export function classOf(cp, big) {
  const cls = cp < 128 ? ASCII_CLASS[cp] : classOfSlow(cp);
  return big && cls === PUNCT ? WORD : cls;
}

/**
 * @param {number} cp
 * @returns {number}
 */
function classOfSlow(cp) {
  const ch = String.fromCodePoint(cp);
  if (WHITE_SPACE.test(ch)) return BLANK;
  return ALPHANUMERIC.test(ch) ? WORD : PUNCT;
}

/**
 * @param {number} cp
 * @returns {boolean}
 */
export function isSpace(cp) {
  return cp < 128 ? ASCII_CLASS[cp] === BLANK : WHITE_SPACE.test(String.fromCodePoint(cp));
}

/**
 * `str::trim().is_empty()`.
 * @param {string} text
 * @returns {boolean}
 */
export function isBlank(text) {
  return text.trim() === '';
}

/**
 * Rust's `char::to_uppercase`/`to_lowercase` are one-to-many — `ß` uppercases
 * to `SS` — so the result can be longer than the input. Swapping stays
 * one-to-one, since there is no sensible reverse of that.
 * @param {string} text
 * @param {number} how -1 lower, 1 upper, 0 swap
 * @returns {string}
 */
export function recase(text, how) {
  let out = '';
  for (const ch of text) {
    if (how < 0) out += ch.toLowerCase();
    else if (how > 0) out += ch.toUpperCase();
    else out += swapChar(ch);
  }
  return out;
}

/**
 * @param {string} ch one code point
 * @returns {string}
 */
function swapChar(ch) {
  if (UPPERCASE.test(ch)) return firstPoint(ch.toLowerCase());
  if (LOWERCASE.test(ch)) return firstPoint(ch.toUpperCase());
  return ch;
}

/**
 * @param {string} text
 * @returns {string}
 */
function firstPoint(text) {
  return String.fromCodePoint(/** @type {number} */ (text.codePointAt(0)));
}

/** Whether a pattern's case makes a search case-sensitive. */
export function hasUppercase(text) {
  for (const ch of text) if (UPPERCASE.test(ch)) return true;
  return false;
}

/**
 * Grapheme boundaries, built on demand.
 *
 * Constructing an `Intl.Segmenter` costs tens of milliseconds of ICU startup,
 * which is most of what a cold JavaScript editor would otherwise pay. Every
 * ASCII buffer answers its boundary questions arithmetically and never touches
 * this, so the cost is only borne by text that genuinely needs it.
 * @type {Intl.Segmenter | null}
 */
let segmenter = null;

/**
 * UTF-16 offsets of every grapheme start in `text`.
 * @param {string} text
 * @returns {number[]}
 */
export function graphemeStarts(text) {
  segmenter ??= new Intl.Segmenter();
  const out = [];
  for (const { index } of segmenter.segment(text)) out.push(index);
  return out;
}
