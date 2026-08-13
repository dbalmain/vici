// Key notation. A key *is* its canonical vi spelling: `"a"`, `"<C-r>"`, `"<Esc>"`.
//
// Interned strings make the keymap a plain object lookup, dot-repeat and macro
// storage a `join("")`, and equality a `===`. The alternative — a `{code, mods}`
// record — costs an allocation per keystroke and a structural comparison per
// keymap probe, for a distinction nothing above this layer needs.
//
// The spelling is Rust `Display for Key`, character for character, so scripts
// round-trip between the two engines.

/** @typedef {string} Key */

const NAMES = {
  Esc: '<Esc>',
  CR: '<CR>',
  Enter: '<CR>',
  Return: '<CR>',
  Tab: '<Tab>',
  BS: '<BS>',
  Del: '<Del>',
  Insert: '<Insert>',
  Space: ' ',
  lt: '<',
  gt: '>',
  bslash: '\\',
  Left: '<Left>',
  Right: '<Right>',
  Up: '<Up>',
  Down: '<Down>',
  Home: '<Home>',
  End: '<End>',
  PageUp: '<PageUp>',
  PageDown: '<PageDown>',
};

/** Keys whose bare spelling is bracketed, so a text key can never be mistaken for one. */
const BARE = { ' ': '<Space>', '<': '<lt>' };

export class KeyError extends Error {}

/**
 * Parse one `<...>` body into a canonical key.
 * @param {string} name
 * @returns {Key}
 */
function bracketed(name) {
  let ctrl = false;
  let alt = false;
  let shift = false;
  let rest = name;
  for (;;) {
    const flag = rest.slice(0, 2);
    const tail = rest.slice(2);
    // A trailing `-` is the key itself, as in `<C-->`.
    if (tail === '') break;
    if (flag === 'C-') ctrl = true;
    else if (flag === 'M-' || flag === 'A-') alt = true;
    else if (flag === 'S-') shift = true;
    else break;
    rest = tail;
  }

  let code = NAMES[rest];
  if (code === undefined) {
    const points = [...rest];
    if (points.length === 1) code = rest;
    else if (/^F\d+$/.test(rest) && Number(rest.slice(1)) < 256) code = `<${rest}>`;
    else throw new KeyError(`unknown key name \`<${name}>\``);
  }

  // A bracketed alias can resolve to a bare character (`<Space>`, `<lt>`).
  const special = code.length > 1 && code[0] === '<';
  let base = special ? code.slice(1, -1) : code;
  if (shift && !special) {
    // SHIFT is never combined with a character: the case already carries it,
    // so the flag is spent upcasing and then dropped — for every character,
    // not just letters, exactly as `char::to_ascii_uppercase` behaves.
    if (base >= 'a' && base <= 'z') base = base.toUpperCase();
    shift = false;
  }
  if (!ctrl && !alt && !shift) return special ? code : (BARE[base] ?? base);
  const named = special ? base : (base === ' ' ? 'Space' : base === '<' ? 'lt' : base);
  return `<${ctrl ? 'C-' : ''}${alt ? 'M-' : ''}${shift ? 'S-' : ''}${named}>`;
}

/**
 * Parse a key sequence in vi notation.
 * @param {string} spec
 * @returns {Key[]}
 */
export function keys(spec) {
  const out = [];
  let at = 0;
  while (at < spec.length) {
    const ch = String.fromCodePoint(/** @type {number} */ (spec.codePointAt(at)));
    at += ch.length;
    if (ch !== '<') {
      out.push(BARE[ch] ?? ch);
      continue;
    }
    const close = spec.indexOf('>', at);
    if (close < 0) throw new KeyError('unterminated `<` in key sequence');
    out.push(bracketed(spec.slice(at, close)));
    at = close + 1;
  }
  return out;
}

/**
 * Parse exactly one key.
 * @param {string} spec
 * @returns {Key}
 */
export function key(spec) {
  const parsed = keys(spec);
  if (parsed.length !== 1) throw new KeyError(`unknown key name \`<${spec}>\``);
  return parsed[0];
}

/**
 * Render a key sequence back into vi notation.
 * @param {readonly Key[]} sequence
 * @returns {string}
 */
export function render(sequence) {
  return sequence.join('');
}

/**
 * The character this key would insert as text, or `null` for anything modified.
 * @param {Key} k
 * @returns {string | null}
 */
export function keyText(k) {
  if (k.charCodeAt(0) !== 0x3c) return k;
  return k === '<Space>' ? ' ' : k === '<lt>' ? '<' : null;
}

/**
 * The unmodified ASCII digit this key represents, or `-1`.
 * @param {Key} k
 * @returns {number}
 */
export function keyDigit(k) {
  const code = k.charCodeAt(0);
  return k.length === 1 && code >= 0x30 && code <= 0x39 ? code - 0x30 : -1;
}
