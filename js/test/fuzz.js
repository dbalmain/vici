// Differential fuzzing against the Rust core.
//
// Generates random buffers and random keystroke scripts, runs both engines
// over them, and diffs the rendered state blocks. Cases are written in
// `editor.vici` format and replayed by `cargo run --example replay`, which
// means a divergence is not just a failure message: paste the case into
// `crates/vici/tests/fixtures/editor.vici` and it is a permanent regression
// test for both engines at once.
//
//     node test/fuzz.js [--cases 2000] [--seed 1] [--keep <dir>]
//
// The generator is seeded, so a reported seed reproduces the run exactly.

import { execFileSync } from 'node:child_process';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { runCase } from './oracle.js';

const CRATE = fileURLToPath(new URL('../../', import.meta.url));

/**
 * @param {number} seed
 * @returns {() => number}
 */
function random(seed) {
  let state = (seed >>> 0) || 1;
  return () => {
    state ^= state << 13;
    state >>>= 0;
    state ^= state >>> 17;
    state ^= state << 5;
    state >>>= 0;
    return state / 0x100000000;
  };
}

/** Fragments chosen to land on the seams: quotes, nesting, wide text, blanks. */
const FRAGMENTS = [
  'select', 'id,', 'name', 'from', 'users', 'where', 'x', 'ab', '  indented', '\t tabbed',
  '(a (b) c)', '{ body }', '[1, 2]', '"quoted"', "'single'", '`tick`', '<angle>', 'a.b.c',
  'foo_bar', 'CamelCase', 'UPPER', 'ß', 'café', '日本語', '🇦🇺', 'é', 'a\r', '',
  '   ', '-- comment', 'x = 1;', 'fn(a, b)', 'one two three', 'END',
];

const MOTIONS = [
  'h', 'l', 'j', 'k', '0', '^', '$', 'w', 'W', 'b', 'B', 'e', 'E', '{', '}', 'G', 'gg', '%',
  'H', 'M', 'L', ';', ',', 'n', 'N', 'fa', 'Fa', 'ta', 'Ta', 'f,', 't"', '`a', "'a",
];
const OBJECTS = ['iw', 'aw', 'iW', 'aW', 'i(', 'a(', 'ib', 'i{', 'a{', 'i"', 'a"', "i'", 'ip', 'ap', 'i[', 'a<lt>'];
const OPERATORS = ['d', 'c', 'y', '>', '<lt>', 'gu', 'gU', 'g~'];
const SIMPLE = [
  'x', 'X', 'J', 'p', 'P', '~', 'u', '<C-r>', '.', 'ra', 'r ', 'ma', 'D', 'C',
  '<C-o>', '<C-i>', 'zz', '<C-d>', '<C-u>', '<C-f>', '<C-b>', ':',
];
const INSERTS = ['i', 'a', 'I', 'A', 'o', 'O', 'R'];
const TYPED = ['x', 'ab', ' ', '<CR>', '<BS>', '<C-w>', '<Tab>', 'é', '日'];
const SURROUND = ['cs("', 'cs"(', 'ds(', 'ds"', 'cs){', 'ds{'];
const COUNTS = ['', '', '', '', '2', '3', '10'];

/**
 * @param {() => number} next
 * @param {readonly string[]} pool
 * @returns {string}
 */
function pick(next, pool) {
  return pool[Math.floor(next() * pool.length)];
}

/**
 * @param {() => number} next
 * @returns {string}
 */
function text(next) {
  const rows = 1 + Math.floor(next() * 5);
  /** @type {string[]} */
  const out = [];
  for (let i = 0; i < rows; i += 1) {
    const parts = Math.floor(next() * 3);
    /** @type {string[]} */
    const row = [];
    for (let p = 0; p <= parts; p += 1) row.push(pick(next, FRAGMENTS));
    out.push(row.join(' '));
  }
  return out.join('\n') + (next() < 0.3 ? '\n' : '');
}

/**
 * One step of a keystroke script: a movement, an operator over something, an
 * insert session, or a standalone command.
 * @param {() => number} next
 * @returns {string}
 */
function step(next) {
  const roll = next();
  const count = pick(next, COUNTS);
  if (roll < 0.3) return count + pick(next, MOTIONS);
  if (roll < 0.5) {
    const operator = pick(next, OPERATORS);
    return count + operator + (next() < 0.4 ? pick(next, OBJECTS) : pick(next, MOTIONS));
  }
  if (roll < 0.58) return count + pick(next, OPERATORS).repeat(2);
  if (roll < 0.72) return count + pick(next, SIMPLE);
  if (roll < 0.82) {
    let session = pick(next, INSERTS);
    const typed = 1 + Math.floor(next() * 3);
    for (let i = 0; i < typed; i += 1) session += pick(next, TYPED);
    return `${session}<Esc>`;
  }
  if (roll < 0.88) {
    // Visual mode, shaped by a motion or an object, then acted on.
    const kind = next() < 0.5 ? 'v' : 'V';
    const shape = next() < 0.5 ? pick(next, MOTIONS) : pick(next, OBJECTS);
    return kind + shape + pick(next, ['d', 'c<Esc>', 'y', '>', 'u', 'U', '~', 'S)', 'x', '<Esc>']);
  }
  if (roll < 0.93) return pick(next, SURROUND);
  if (roll < 0.97) return `/${pick(next, ['a', 'b', 'x', 'END', 'zz'])}<CR>`;
  return `qa${pick(next, SIMPLE)}q2@a`;
}

/**
 * @param {() => number} next
 * @returns {string}
 */
function script(next) {
  const steps = 1 + Math.floor(next() * 4);
  let out = '';
  for (let i = 0; i < steps; i += 1) out += step(next);
  return out;
}

/**
 * @param {string} value
 * @returns {string}
 */
function escape(value) {
  return value.replace(/\\/g, '\\\\').replace(/\n/g, '\\n').replace(/\r/g, '\\r').replace(/\t/g, '\\t');
}

/**
 * @param {string} blob
 * @returns {Map<string, string>}
 */
function blocks(blob) {
  /** @type {Map<string, string>} */
  const out = new Map();
  for (const part of blob.split('\n== ')) {
    const body = part.startsWith('== ') ? part : `== ${part}`;
    const end = body.indexOf(' ==', 3);
    if (end < 0) continue;
    out.set(body.slice(3, end), body.trimEnd());
  }
  return out;
}

function main() {
  const args = process.argv.slice(2);
  const flag = (name, fallback) => {
    const at = args.indexOf(name);
    return at < 0 ? fallback : args[at + 1];
  };
  const total = Number(flag('--cases', '2000'));
  const seed = Number(flag('--seed', '1'));
  const next = random(seed);

  /** @type {{ name: string, text: string, keys: string, viewport: any, indent: any }[]} */
  const cases = [];
  const lines = [];
  for (let i = 0; i < total; i += 1) {
    const entry = {
      name: `fuzz-${String(i).padStart(5, '0')}`,
      text: text(next),
      keys: script(next),
      viewport: next() < 0.25 ? { topRow: Math.floor(next() * 3), height: 1 + Math.floor(next() * 6) } : null,
      indent: next() < 0.2 ? { shiftWidth: 1 + Math.floor(next() * 4), tabWidth: 4, useTabs: next() < 0.5 } : null,
    };
    cases.push(entry);
    let block = `case ${entry.name}\ntext ${escape(entry.text)}\nkeys ${entry.keys}`;
    if (entry.viewport) block += `\nwith viewport=${entry.viewport.topRow},${entry.viewport.height}`;
    if (entry.indent) {
      block += `${entry.viewport ? ' ' : '\nwith '}indent=${entry.indent.shiftWidth},${entry.indent.tabWidth},${entry.indent.useTabs ? 'tabs' : 'spaces'}`;
    }
    lines.push(block);
  }

  const dir = flag('--keep', null) ?? mkdtempSync(join(tmpdir(), 'vici-fuzz-'));
  const fixture = join(dir, 'cases.vici');
  writeFileSync(fixture, `${lines.join('\n---\n')}\n`);

  const rust = execFileSync('cargo', ['run', '-q', '--example', 'replay', '--', fixture], {
    cwd: CRATE,
    encoding: 'utf8',
    maxBuffer: 256 * 1024 * 1024,
  });
  const oracle = blocks(rust);

  /** @type {string[]} */
  const diverged = [];
  for (const entry of cases) {
    let mine;
    try {
      mine = runCase(entry).trimEnd();
    } catch (error) {
      mine = `== ${entry.name} ==\nthrew: ${error instanceof Error ? error.message : String(error)}`;
    }
    const theirs = oracle.get(entry.name);
    if (theirs === undefined) {
      diverged.push(`${entry.name}: the Rust replay produced no block`);
      continue;
    }
    if (mine !== theirs) {
      diverged.push(
        `case ${entry.name}\ntext ${escape(entry.text)}\nkeys ${entry.keys}\n` +
          `--- rust ---\n${theirs}\n--- js ---\n${mine}`,
      );
    }
  }

  const report = `${total} cases, seed ${seed}: ${total - diverged.length} agree, ${diverged.length} diverge`;
  if (diverged.length === 0) {
    console.log(`${report}\nfixture: ${fixture}`);
    return;
  }
  console.error(`${report}\nfixture: ${fixture}\n`);
  for (const failure of diverged.slice(0, 8)) console.error(`${failure}\n`);
  process.exitCode = 1;
}

main();
