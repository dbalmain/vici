// The oracle harness: run the Rust crate's own fixture cases through this
// engine and render each one in the Rust snapshot's exact format.
//
// `crates/vici/tests/fixtures/editor.vici` and its insta snapshot are the
// contract. Rendering the same state block character for character means a
// divergence in *any* observable — text, cursor, register, marks, jumps,
// pending keys, effects — fails the suite, not just the ones a hand-written
// assertion happened to check.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { Editor } from '../src/index.js';
import * as C from '../src/command.js';
import { render } from '../src/keys.js';

const ROOT = fileURLToPath(new URL('../../crates/vici/tests/', import.meta.url));

export const FIXTURE = `${ROOT}fixtures/editor.vici`;
export const SNAPSHOT = `${ROOT}snapshots/editor_cases__editor_cases.snap`;

/** @typedef {{ name: string, text: string, keys: string, viewport: any, indent: any }} Case */

/**
 * @param {string} value
 * @param {string} name
 * @returns {string}
 */
function unescape(value, name) {
  return value.replace(/\\(.)/g, (_, ch) => {
    const mapped = { n: '\n', r: '\r', t: '\t', '\\': '\\' }[ch];
    if (mapped === undefined) throw new Error(`${name}: unsupported escape \\${ch}`);
    return mapped;
  });
}

/**
 * Settings are space-separated, their values comma-separated:
 * `with viewport=0,6 indent=4,8,spaces`.
 * @param {string} value
 * @param {Case} into
 * @param {string} name
 */
function parseSettings(value, into, name) {
  for (const setting of value.split(/\s+/).filter(Boolean)) {
    const [key, values] = setting.split('=');
    const parts = (values ?? '').split(',');
    if (key === 'viewport' && parts.length === 2) {
      into.viewport = { topRow: Number(parts[0]), height: Number(parts[1]) };
    } else if (key === 'indent' && parts.length === 3) {
      into.indent = {
        shiftWidth: Number(parts[0]),
        tabWidth: Number(parts[1]),
        useTabs: parts[2] === 'tabs',
      };
    } else {
      throw new Error(`${name}: unsupported setting ${setting}`);
    }
  }
}

/**
 * @param {string} fixture
 * @returns {Case[]}
 */
export function parseCases(fixture) {
  /** @type {Case[]} */
  const cases = [];
  const seen = new Set();
  for (const chunk of fixture.split('\n---\n')) {
    const lines = chunk.split('\n').filter((line) => line !== '' && !line.startsWith('#'));
    if (lines.length === 0) continue;
    const name = lines[0].startsWith('case ') ? lines[0].slice(5) : null;
    if (name === null || !/^[a-z0-9]+(-[a-z0-9]+)*$/.test(name)) {
      throw new Error('<unknown>: case must be first and kebab-case');
    }
    if (seen.has(name)) throw new Error(`${name}: duplicate case name`);
    seen.add(name);
    /** @type {Case} */
    const entry = { name, text: '', keys: '', viewport: null, indent: null };
    let text = false;
    let keys = false;
    for (const line of lines.slice(1)) {
      if (line === 'text' || line.startsWith('text ')) {
        if (text) throw new Error(`${name}: duplicate text`);
        text = true;
        entry.text = unescape(line.slice(5), name);
      } else if (line.startsWith('keys ')) {
        if (keys) throw new Error(`${name}: duplicate keys`);
        keys = true;
        entry.keys = line.slice(5);
      } else if (line.startsWith('with ')) {
        parseSettings(line.slice(5), entry, name);
      } else {
        throw new Error(`${name}: unknown fixture prefix: ${line}`);
      }
    }
    if (!text) throw new Error(`${name}: missing text`);
    if (!keys) throw new Error(`${name}: missing keys`);
    cases.push(entry);
  }
  if (cases.length === 0) throw new Error('<unknown>: fixture has no cases');
  return cases;
}

// Rust's `Debug for str`: escape the four short forms, then anything
// unprintable or grapheme-extended as `\u{...}`.
const UNPRINTABLE = /[\p{Cc}\p{Cf}\p{Cs}\p{Co}\p{Zl}\p{Zp}\p{Zs}]|\P{Assigned}/u;
const EXTENDED = /\p{Grapheme_Extend}/u;
const SHORT = { '\t': '\\t', '\r': '\\r', '\n': '\\n', '\\': '\\\\', '"': '\\"' };

/**
 * @param {string} text
 * @returns {string}
 */
export function debugString(text) {
  let out = '"';
  for (const ch of text) {
    const short = SHORT[ch];
    if (short !== undefined) out += short;
    else if (ch === ' ' || (!UNPRINTABLE.test(ch) && !EXTENDED.test(ch))) out += ch;
    else out += `\\u{${/** @type {number} */ (ch.codePointAt(0)).toString(16)}}`;
  }
  return `${out}"`;
}

const MODES = ['Normal', 'Insert', 'Replace', 'Visual(Char)', 'Visual(Line)'];
const SCROLLS = ['HalfPageDown', 'HalfPageUp', 'PageDown', 'PageUp', 'Center', 'Top', 'Bottom'];

/**
 * @param {import('../src/editor.js').Effect} effect
 * @returns {string}
 */
function renderEffect(effect) {
  switch (effect.type) {
    case 'edit': {
      const e = effect.edit;
      return (
        `edit ${e.startByte}..${e.oldEndByte} -> ${e.newEndByte}; ` +
        `(${e.startPoint.row},${e.startPoint.col})..(${e.oldEndPoint.row},${e.oldEndPoint.col})` +
        ` -> (${e.newEndPoint.row},${e.newEndPoint.col})`
      );
    }
    case 'mode':
      return `mode ${MODES[effect.mode]}`;
    case 'scroll':
      return `scroll ${SCROLLS[effect.scroll]}`;
    case 'prompt':
      return 'command prompt :';
    case 'recordingStarted':
      return `recording @${effect.register}`;
    case 'recordingStopped':
      return `recorded @${effect.register}`;
    default:
      return 'bell';
  }
}

const MARK_NAMES = [...'abcdefghijklmnopqrstuvwxyz<>[]^'];

/**
 * @param {string} name
 * @param {Editor} editor
 * @param {import('../src/editor.js').Effect[]} effects
 * @returns {string}
 */
export function renderCase(name, editor, effects) {
  const selection = editor.selection();
  const point = editor.cursorPoint();
  // The automatic marks are as much state as the named ones; leaving them out
  // would mean a regression in `'[`/`']` never showed up in a case block.
  const marks = MARK_NAMES.map((mark) => [mark, editor.mark(mark)])
    .filter(([, offset]) => offset !== null)
    .map(([mark, offset]) => `${mark}:${offset}`);
  const lines = [
    `== ${name} ==`,
    `text: ${debugString(editor.text())}`,
    `cursor: ${editor.cursor} @ ${point.row}:${point.col}`,
    `mode: ${MODES[editor.mode]}; selection: ${selection === null ? '-' : `${selection[0]}..${selection[1]}`}`,
    `register: ${editor.register.linewise ? 'line' : 'char'} ${debugString(editor.register.text)}`,
    `history: undo=${editor.doc.undoDepth} redo=${editor.doc.redoDepth}`,
    `jumps: [${editor.jumps.join(', ')}]`,
    `marks: [${marks.join(', ')}]`,
    `pending: ${debugString(render(editor.pendingKeys()))}; last-change: ${debugString(
      render(editor.lastChange),
    )}; recording: ${editor.recording === null ? '-' : editor.recording.register}`,
    'effects:',
  ];
  for (const effect of effects) lines.push(`  ${renderEffect(effect)}`);
  return `${lines.join('\n')}\n\n`;
}

/**
 * @param {Case} entry
 * @returns {string}
 */
export function runCase(entry) {
  const editor = new Editor(entry.text);
  if (entry.viewport) editor.setViewport(entry.viewport);
  if (entry.indent) editor.setIndent(entry.indent);
  return renderCase(entry.name, editor, editor.typeKeys(entry.keys));
}

/** @returns {Case[]} */
export function cases() {
  return parseCases(readFileSync(FIXTURE, 'utf8'));
}

/** The insta snapshot with its YAML header stripped. @returns {string} */
export function snapshot() {
  const raw = readFileSync(SNAPSHOT, 'utf8');
  return raw.slice(raw.indexOf('\n---\n') + 5);
}
