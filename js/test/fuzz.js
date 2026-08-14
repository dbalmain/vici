// Differential fuzzing: WASM (Rust core) vs this JavaScript engine.
//
// Generates buffers and keystroke scripts, runs both engines, and diffs the
// rendered state blocks. A divergence is pasteable into `editor.vici`.
//
//     node test/fuzz.js [--cases 8000] [--seed 1] [--campaign soup]
//                       [--until] [--keep <dir>] [--oracle wasm|rust]
//
// The generator is seeded. Grapheme-cluster traps (flags, combining marks,
// ZWJ) and regex-shaped search patterns are out of scope — this hunts for
// differences in how editing commands are interpreted. Snapshot spelling of
// U+0000 (`\0` vs `\u{0}`) is normalised away for the same reason.

import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { keys as parseKeys, render as renderKeys } from '../src/keys.js';
import { cases as fixtureCases, runCase } from './oracle.js';

const FIXTURES = fixtureCases();
const HERE = dirname(fileURLToPath(import.meta.url));
const CRATE = join(HERE, '../..');
const DEFAULT_WASM = join(HERE, '../../../beetle/packages/vici-wasm/pkg/vici_wasm.cjs');

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

/** @param {() => number} next @param {readonly T[]} pool @returns {T} @template T */
function pick(next, pool) {
  return pool[Math.floor(next() * pool.length)];
}

/** @param {() => number} next @param {number} lo @param {number} hi */
function int(next, lo, hi) {
  return lo + Math.floor(next() * (hi - lo + 1));
}

// NFC scalars only. Flags, combining marks and ZWJ sequences are a grapheme
// non-goal; they stay out of the pool so a hit is about command meaning.
const FRAGMENTS = [
  'select', 'id,', 'name', 'from', 'users', 'where', 'x', 'ab', '  indented', '\t tabbed',
  '(a (b) c)', '{ body }', '[1, 2]', '"quoted"', "'single'", '`tick`', '<angle>', 'a.b.c',
  'foo_bar', 'CamelCase', 'UPPER', 'ß', 'café', '日本語', 'a\r', '',
  '   ', '-- comment', 'x = 1;', 'fn(a, b)', 'one two three', 'END',
  'straße', 'foo.bar', '99end', 'a_b', '()', '{}', '[]', '""', "''",
];

const BUFFERS = [
  '',
  'x',
  'xy',
  'a\n',
  'a\nb',
  'a\nb\n',
  'a\n\nb',
  'a\n\n',
  '\n',
  '\n\n',
  'aa\nbb\ncc',
  'aa\nbb\ncc\n',
  'select id, name\nfrom users\nwhere id = 1',
  '  indented\n\t tabbed\nend',
  'one two three four five',
  'foo_bar.baz 99end',
  '(a (b) c) x',
  'outer { mid { deep } here } end',
  '{\n  one: {\n    two: {\n      three: {\n      }\n    }\n  }\n}',
  'call(one, [two, three], "four")',
  'say "hello there" and \'goodbye\' and `tick`',
  'the quick brown fox jumps',
  'aa\n\nbb\n\ncc',
  '  foo  \n  bar  ',
  '\t\tfoo\nbar',
  'a\r\nb\r\nc',
  'straße café 日本語',
  '[x] item\n[ ] other',
  '()\n{}\n[]',
  'foo\n',
  ' \n \n ',
  'END',
  'a\u00a0b',
  'a—b',
  'a\u3000b',
  'a\u2028b',
  'it\u2019s',
  'a\u0000b',
];

const MOTIONS = [
  'h', 'l', 'j', 'k', '0', '^', '$', 'w', 'W', 'b', 'B', 'e', 'E', '{', '}',
  'G', 'gg', '%', 'H', 'M', 'L', ';', ',', 'n', 'N',
  'fa', 'Fa', 'ta', 'Ta', 'f,', 't"', 'f.', 't ', 'F ', 'T,',
  '`a', "'a", "''", '``', "'[", "']", "'^",
];

const OBJECTS = [
  'iw', 'aw', 'iW', 'aW', 'i(', 'a(', 'ib', 'i{', 'a{', 'i}', 'i"', 'a"',
  "i'", 'ip', 'ap', 'i[', 'a[', 'i<lt>', 'a<lt>', 'i`', 'a`', 'iB',
];

const OPERATORS = ['d', 'c', 'y', '>', '<lt>', 'gu', 'gU', 'g~'];
const SIMPLE = [
  'x', 'X', 'J', 'p', 'P', '~', 'u', '<C-r>', '.', 'ra', 'r ', 'r<lt>', 'rX',
  'ma', 'D', 'C', '<C-o>', '<C-i>', 'zz', 'zt', 'zb',
  '<C-d>', '<C-u>', '<C-f>', '<C-b>', ':',
];
const INSERTS = ['i', 'a', 'I', 'A', 'o', 'O', 'R'];
const TYPED = ['x', 'ab', ' ', '<CR>', '<BS>', '<C-w>', '<Tab>', 'é', '日', 'ß', 'X'];
const SURROUND = [
  'cs("', 'cs"(', 'ds(', 'ds"', "ds'", 'cs){', 'ds{', 'csbB', 'cs([',
  "cs'`", 'ds[', 'cs<lt>>', 'ds<lt>', 'cs`"',
];
const COUNTS = ['', '', '', '', '2', '3', '10'];

const VISUAL_ACTIONS = [
  'd', 'c<Esc>', 'y', '>', '<lt>', 'u', 'U', '~', 'S)', 'S(', 'S"', 'x', 's<Esc>',
  '<Esc>', 'p', 'P', 'J', 'ra', 'o<Esc>', 'O<Esc>', 'I<Esc>', 'A<Esc>', 'R<Esc>',
];

const PENDING_SCRIPTS = [
  'd<Esc>w', 'c<Esc>', 'y<Esc>', 'g<Esc>l', 'f<Esc>', 't<Esc>', 'm<Esc>',
  '/<Esc>', '/<BS>', '/a<BS><CR>', '/a<BS>b<CR>', '?<Esc>',
  '2<Esc>l', 'd2<Esc>w', 'cs<Esc>', 'ds<Esc>', 'csx<Esc>',
  'r<Esc>', 'q<Esc>', '@<Esc>', "'<Esc>", '`<Esc>',
  'ysw)', 'csxy', 'd<lt>', 's', 'S', 'Y', 'gv',
];

const EDGE_SCRIPTS = [
  'dd', 'ccx<Esc>', 'yy', 'Gdd', 'Gddp', 'ddGdd', 'dj', 'dk', 'dgg', 'dG',
  '2D', '3C', 'd0', 'c0', 'y0', 'd$', 'c$', 'y$', 'cw', 'cW', 'ce', 'cE',
  'dw', 'dW', 'de', 'd;', 'dn', 'd%', 'dip', 'dap', '2di{', '3diw', 'daw', 'caw',
  'gU$', 'gUw', 'guw', 'g~w', '>w', '>ip', '>i{', '3>>', '2<<',
  'v$d', 'V$d', 'vt.d', 'vwd', 'Vj>', 'v2i{d', 'viwd',
  '3ix<Esc>', '2o<Esc>', 'Rabc<Esc>', 'Rx<BS><Esc>',
  'o<Esc>u', 'O<Esc>u', 'i<C-w><Esc>', 'i<CR><BS><Esc>',
  'xuu<C-r>', 'J', '3J', '2p', '2P', '~', '3~', '3x',
  '1G', '5G', 'ggdG', 'Gdgg',
  '/x<CR>nN', '?x<CR>n', 'd/x<CR>', 'c?a<CR><Esc>',
  'fa;,,', '2fa', 't ;', 'd2t,',
  'majd\'a', 'majd`a', "G''", 'G``',
  'qa~q@a', 'qa<Esc>q@a', '@z', "'a", 'd\'\'',
  'qaix<Esc>q@a', 'ccx<Esc>.', 'dd.', 'vwcX<Esc>w.',
  '3.', 'Vj>.', ':',
];

const PREFIXES = ['', 'w', '$', 'G', 'e', '2w', 'ggj', 'fa'];

const EXTRAS = [
  'x', 'dw', 'cw<Esc>', 'dd', 'J', 'p', 'P', '~', '.', 'u', '3.',
  'vwd', 'Vj>', 'vo<Esc>', 'vra', 'vJx',
  'd0', '2D', 'd$', 'dgg', 'dG', 'd%', 'd}',
  'cs("', 'ds(', 'gUw', '>>', 'ma\'a', '/x<CR>n',
  'iX<Esc>', 'o<Esc>', 'R z'.replace(' ', '') + '<Esc>',
];

const KEY_SOUP = [
  'h', 'j', 'k', 'l', 'd', 'c', 'y', 'g', 'u', 'U', 'i', 'a', 'o', 'O', 'v', 'V',
  'x', 'X', 'w', 'W', 'b', 'B', 'e', 'E', '0', '$', '^', 'G', '%', '{', '}',
  'f', 't', 'F', 'T', 'r', 'm', 'q', '@', '/', '?', 'n', 'N', 's', 'S',
  'p', 'P', 'J', '.', '~', '>', '<lt>', '1', '2', '3', ':', 'z', 'H', 'M', 'L',
  'D', 'C', 'R', 'I', 'A', ' ', '"', "'", '`', '(', ')', '[', ']',
  '<Esc>', '<CR>', '<BS>', '<Tab>', '<C-r>', '<C-o>', '<C-i>', '<Space>',
  '<C-d>', '<C-u>', '<Del>', '<Left>', '<Right>', '<Home>', '<End>',
];

const CORPUS = [
  ['', 'dd'],
  ['x', 'x'],
  ['a\nb', 'J'],
  ['a\n\nb', 'J'],
  ['aa\nbb\ncc', 'Gdd'],
  ['aa\nbb\ncc\n', 'Gdd'],
  ['aa\nbb', 'yyGp'],
  ['foo bar', '$bcwX<Esc>'],
  ['foo', 'cwX<Esc>'],
  ['one two three four five six', '2d3w'],
  ['a\nb\nc\nd\ne', '2d3j'],
  ['outer { mid { deep } here }', 'd2i{'],
  ['outer { mid { deep } here }', '2di{'],
  ['abc', '<S-Space>'],
  ['abc', 'i<S-Space><Esc>'],
  ['abc def', 'd<S-Space>'],
  ['ab\ncd', 'vo<Esc>'],
  ['ab\ncd', 'ywvp'],
  ['abc', 'vra'],
  ['aa\nbb\ncc', 'vJJ'],
  ['abc', '3iZ<Esc>'],
  ['a\nb', '2oX<Esc>'],
  ['(foo)', 'csbB'],
  ['(foo)', 'dsb'],
  ['one two one', 'd/two<CR>'],
  ['id ID Id', '/id<CR>n'],
  ['id ID Id', '/ID<CR>'],
  ['aaa', 'qa@aq@a'],
  ['one two three', 'vwcX<Esc>w.'],
  ['abc def ghi', 'dw3.'],
  ['abc', 'd<Esc>w'],
  ['word', 'ysiw)'],
  ['abc', 's'],
  ['abc', 'Y'],
  ['foo bar', 'gUU'],
  ['FOO BAR', 'guu'],
  ['straße', 'gUiw'],
  ['straße', 'g~w'],
  ['a\u00a0b', 'w'],
  ['a—b', 'dw'],
  ['aKb', '/k<CR>'],
  ['iİıI', 'gU$'],
  ['[x] item', '%'],
  ['"hello"', '%'],
  ['(a(b)c)', 'l%'],
  ['abcdef', 'Rxy<BS>z<Esc>'],
  ['ab', 'Rxyz<Esc>'],
  ['ab\ncd', 'ji<BS><Esc>'],
  ['a\nb\nc\nd', 'V3>'],
  ['a\nb\nc\nd', '3V>'],
  ['  abc', '<End><Home>'],
  ['abc', '<Space><Space>'],
  ['abc', 'i<C-c>'],
  ['abc', 'i<Right><Left>X<Esc>'],
];

const CAMPAIGNS = [
  'corpus',
  'grid',
  'soup',
  'mixed',
  'edges',
  'visual',
  'objects',
  'repeat',
  'search',
  'surround',
  'marks',
  'indent',
  'pending',
  'replace',
  'unicode',
  'fixture-mut',
];

/**
 * Systematic coverage: interesting buffers × (positioning prefix + edge script),
 * then each fixture case plus one extra command. These are the combinations
 * random walks rarely land on.
 * @returns {Case[]}
 */
function gridCases() {
  /** @type {Case[]} */
  const out = [];
  let n = 0;
  const name = () => `fuzz-grid-${String((n += 1)).padStart(5, '0')}`;
  const viewports = [null, { topRow: 0, height: 1 }, { topRow: 1, height: 3 }, { topRow: 0, height: 0 }];
  const indents = [null, { shiftWidth: 2, tabWidth: 4, useTabs: false }, { shiftWidth: 1, tabWidth: 8, useTabs: true }];

  for (const text of BUFFERS) {
    for (const prefix of PREFIXES) {
      for (const script of EDGE_SCRIPTS) {
        out.push({
          name: name(),
          text,
          keys: prefix + script,
          viewport: viewports[out.length % viewports.length],
          indent: indents[out.length % indents.length],
        });
      }
    }
  }

  for (const base of FIXTURES) {
    for (const extra of EXTRAS) {
      out.push({
        name: name(),
        text: base.text,
        keys: base.keys + extra,
        viewport: base.viewport,
        indent: base.indent,
      });
    }
  }
  return out;
}

/** @returns {Case[]} */
function corpusCases() {
  return CORPUS.map(([text, keys], index) => ({
    name: `fuzz-corpus-${String(index).padStart(5, '0')}`,
    text,
    keys,
    viewport: null,
    indent: null,
  }));
}

/**
 * @param {() => number} next
 * @returns {string}
 */
function randomText(next) {
  if (next() < 0.45) return pick(next, BUFFERS);
  const rows = 1 + Math.floor(next() * 6);
  /** @type {string[]} */
  const out = [];
  for (let i = 0; i < rows; i += 1) {
    const parts = Math.floor(next() * 4);
    /** @type {string[]} */
    const row = [];
    for (let p = 0; p <= parts; p += 1) row.push(pick(next, FRAGMENTS));
    out.push(row.join(' '));
  }
  return out.join('\n') + (next() < 0.35 ? '\n' : '');
}

/**
 * @param {() => number} next
 * @returns {string}
 */
function count(next) {
  return pick(next, COUNTS);
}

/**
 * @param {() => number} next
 * @returns {string}
 */
function insertSession(next) {
  let session = pick(next, INSERTS);
  const typed = 1 + Math.floor(next() * 4);
  for (let i = 0; i < typed; i += 1) session += pick(next, TYPED);
  if (next() < 0.15) session += '<Esc>' + pick(next, ['u', '.', 'h', 'x']);
  return `${session}<Esc>`;
}

/**
 * @param {() => number} next
 * @returns {string}
 */
function visualStep(next) {
  const kind = next() < 0.45 ? 'v' : next() < 0.8 ? 'V' : next() < 0.5 ? 'vV' : 'Vv';
  const shape = next() < 0.55 ? pick(next, MOTIONS) : pick(next, OBJECTS);
  return count(next) + kind + shape + pick(next, VISUAL_ACTIONS);
}

/**
 * @param {() => number} next
 * @returns {string}
 */
function operatorStep(next) {
  const operator = pick(next, OPERATORS);
  const target = next() < 0.45 ? pick(next, OBJECTS) : pick(next, MOTIONS);
  if (next() < 0.2) return count(next) + operator + operator;
  return count(next) + operator + (next() < 0.35 ? count(next) : '') + target;
}

/**
 * @param {() => number} next
 * @returns {string}
 */
function step(next) {
  const roll = next();
  if (roll < 0.22) return count(next) + pick(next, MOTIONS);
  if (roll < 0.44) return operatorStep(next);
  if (roll < 0.56) return count(next) + pick(next, SIMPLE);
  if (roll < 0.68) return insertSession(next);
  if (roll < 0.8) return visualStep(next);
  if (roll < 0.87) return pick(next, SURROUND);
  if (roll < 0.92) return pick(next, PENDING_SCRIPTS);
  if (roll < 0.96) {
    const needle = pick(next, ['a', 'b', 'x', 'END', 'zz', 'ID', 'Foo', ' ']);
    const cmd = next() < 0.7 ? '/' : '?';
    return `${cmd}${needle}<CR>${next() < 0.5 ? 'n' : 'N'}`;
  }
  return `qa${pick(next, SIMPLE)}q${count(next) || '2'}@a`;
}

/**
 * @param {() => number} next
 * @param {number} [extra]
 * @returns {string}
 */
function mixedScript(next, extra = 0) {
  const steps = 1 + Math.floor(next() * 5) + extra;
  let out = '';
  for (let i = 0; i < steps; i += 1) out += step(next);
  return out;
}

/**
 * @param {() => number} next
 * @returns {{ topRow: number, height: number } | null}
 */
function maybeViewport(next) {
  if (next() >= 0.35) return null;
  return { topRow: int(next, 0, 4), height: int(next, 0, 8) };
}

/**
 * @param {() => number} next
 * @returns {{ shiftWidth: number, tabWidth: number, useTabs: boolean } | null}
 */
function maybeIndent(next) {
  if (next() >= 0.3) return null;
  return {
    shiftWidth: pick(next, [1, 2, 3, 4, 8]),
    tabWidth: pick(next, [2, 4, 8]),
    useTabs: next() < 0.5,
  };
}

/**
 * @typedef {{ name: string, text: string, keys: string, viewport: any, indent: any }} Case
 */

/**
 * @param {() => number} next
 * @param {string} campaign
 * @param {number} index
 * @returns {Case}
 */
function generateCase(next, campaign, index) {
  /** @type {Case} */
  const entry = {
    name: `fuzz-${campaign}-${String(index).padStart(5, '0')}`,
    text: randomText(next),
    keys: '',
    viewport: maybeViewport(next),
    indent: maybeIndent(next),
  };

  switch (campaign) {
    case 'edges':
      entry.text = pick(next, BUFFERS);
      entry.keys = pick(next, EDGE_SCRIPTS) + (next() < 0.4 ? step(next) : '');
      break;
    case 'visual':
      entry.keys = visualStep(next) + (next() < 0.6 ? visualStep(next) : '') + (next() < 0.4 ? pick(next, ['.', 'u', 'p', 'gv']) : '');
      break;
    case 'objects':
      entry.text = pick(next, [
        'outer { mid { deep } here } end',
        '{\n  one: {\n    two: {\n      three: {\n      }\n    }\n  }\n}',
        'call(one, [two, three], "four") and more',
        '(a (b) c) x [1, 2] { body }',
        'say "hello there" and \'goodbye\' and `tick`',
        'the quick brown fox jumps over',
        'aa\n\nbb\n\ncc\n\n',
        pick(next, BUFFERS),
      ]);
      entry.keys = count(next) + pick(next, OPERATORS) + pick(next, OBJECTS);
      if (next() < 0.5) entry.keys += step(next);
      break;
    case 'repeat':
      entry.keys =
        pick(next, [
          insertSession(next) + 'w.',
          operatorStep(next) + '.',
          visualStep(next) + 'w.',
          'ccX<Esc>j.',
          'vwcX<Esc>w.',
          'Vj>jj.',
          '3ix<Esc>w.',
          `qa${step(next)}q${count(next) || '2'}@a`,
          'qa~jq@a@a',
          operatorStep(next) + 'u<C-r>',
        ]) + (next() < 0.4 ? pick(next, ['.', 'u', '3.']) : '');
      break;
    case 'search': {
      const needle = pick(next, ['a', 'b', 'x', 'e', 'END', 'id', 'ID', 'Foo', 'ß', ' ']);
      const cmd = next() < 0.6 ? '/' : '?';
      const op = pick(next, ['', 'd', 'c', 'y', 'gU']);
      entry.keys = `${op}${cmd}${needle}<CR>`;
      if (next() < 0.5) entry.keys += pick(next, ['n', 'N', 'nN', 'd n'.replace(' ', ''), '3n']);
      if (next() < 0.3) entry.keys += pick(next, ['.', 'u', 'n']);
      break;
    }
    case 'surround':
      entry.text = pick(next, [
        '(a (b) c) x',
        'say "hello" and \'goodbye\'',
        '{ body } and (tail)',
        '[1, 2] <T>',
        pick(next, BUFFERS),
      ]);
      entry.keys = pick(next, SURROUND);
      if (next() < 0.5) entry.keys = visualStep(next) + pick(next, ['S)', 'S(', 'S"', "S'", 'S{']);
      if (next() < 0.3) entry.keys += pick(next, ['.', 'u', 'ds(', 'cs)[']);
      break;
    case 'marks':
      entry.keys =
        pick(next, ['ma', 'mb', 'm z'.replace(' ', '')]) +
        pick(next, ['j', 'w', 'G', '/x<CR>', '}']) +
        pick(next, ["'a", '`a', "''", '``', "'[", "']", '<C-o>', '<C-i>']);
      if (next() < 0.5) entry.keys += pick(next, ['d\'a', 'd`a', "d''", 'y`a', 'u']);
      break;
    case 'indent':
      entry.indent = {
        shiftWidth: pick(next, [1, 2, 3, 4, 8]),
        tabWidth: pick(next, [2, 4, 8]),
        useTabs: next() < 0.5,
      };
      entry.text = pick(next, [
        'one\ntwo\nthree',
        '\tfoo\n  bar\n',
        '  indented\n\t tabbed\n',
        ' \nfoo\n',
        pick(next, BUFFERS),
      ]);
      entry.keys = pick(next, ['>>', '<<', '3>>', '>j', '>ip', '>i{', 'Vj>', '>w', '>>u', '>>.']);
      break;
    case 'pending':
      entry.keys = pick(next, PENDING_SCRIPTS) + (next() < 0.5 ? step(next) : '');
      break;
    case 'replace':
      entry.keys =
        'R' +
        Array.from({ length: 1 + int(next, 0, 4) }, () => pick(next, TYPED.concat(['<Left>', '<Right>']))).join('') +
        '<Esc>';
      if (next() < 0.4) entry.keys += pick(next, ['u', '.', 'R x'.replace(' ', '') + '<Esc>']);
      break;
    case 'corpus': {
      const [text, keys] = pick(next, CORPUS);
      entry.text = text;
      entry.keys = keys + (next() < 0.5 ? step(next) : '');
      break;
    }
    case 'unicode': {
      const ch = pick(next, [
        '\u00a0', '—', '\u2019', '\u3000', '\u2028', '\u2029',
        'K', 'İ', 'ı', 'ǅ', 'Ⅰ', 'ª', '²', '·', 'ａ', 'Ａ', '\u00ad',
      ]);
      entry.text = `a${ch}b ${ch} end`;
      entry.keys = pick(next, ['w', 'dw', 'ldiw', 'l~', 'gUiw', 'guw', '/a<CR>n', 'e', 'b', 'diW', 'daW']);
      break;
    }
    case 'soup': {
      const n = 4 + int(next, 0, 16);
      let keys = '';
      for (let i = 0; i < n; i += 1) keys += pick(next, KEY_SOUP);
      entry.text = pick(next, BUFFERS);
      entry.keys = keys;
      break;
    }
    case 'fixture-mut': {
      const base = pick(next, FIXTURES);
      entry.text = base.text;
      entry.viewport = base.viewport;
      entry.indent = base.indent;
      entry.keys = base.keys + mixedScript(next, 1);
      break;
    }
    default:
      entry.keys = mixedScript(next);
      break;
  }

  return entry;
}

/**
 * @param {string} value
 * @returns {string}
 */
function escape(value) {
  return value.replace(/\\/g, '\\\\').replace(/\n/g, '\\n').replace(/\r/g, '\\r').replace(/\t/g, '\\t');
}

/**
 * @param {Case} entry
 * @returns {string}
 */
function formatCase(entry) {
  let block = `case ${entry.name}\ntext ${escape(entry.text)}\nkeys ${entry.keys}`;
  if (entry.viewport) block += `\nwith viewport=${entry.viewport.topRow},${entry.viewport.height}`;
  if (entry.indent) {
    block += `${entry.viewport ? ' ' : '\nwith '}indent=${entry.indent.shiftWidth},${entry.indent.tabWidth},${entry.indent.useTabs ? 'tabs' : 'spaces'}`;
  }
  return block;
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

/**
 * @param {string} wasmPath
 */
function loadWasm(wasmPath) {
  const require = createRequire(import.meta.url);
  try {
    return require(wasmPath);
  } catch (error) {
    throw new Error(
      `WASM oracle not found at ${wasmPath}. Build beetle's wasm first (cd ../beetle && npm run build:wasm).`,
      { cause: error },
    );
  }
}

/**
 * @param {any} wasm
 * @param {Case} entry
 * @returns {string}
 */
function runWasm(wasm, entry) {
  const viewport = entry.viewport ? `${entry.viewport.topRow},${entry.viewport.height}` : undefined;
  const indent = entry.indent
    ? `${entry.indent.shiftWidth},${entry.indent.tabWidth},${entry.indent.useTabs ? 'tabs' : 'spaces'}`
    : undefined;
  return String(wasm.run_case(entry.name, entry.text, entry.keys, viewport, indent)).trimEnd();
}

/**
 * @param {Case} entry
 * @returns {string}
 */
function runJs(entry) {
  return runCase(entry).trimEnd();
}

/**
 * @param {Case} entry
 * @param {string} theirs
 * @param {string} mine
 * @returns {string}
 */
/** @param {string} block */
function normalizeSnapshot(block) {
  return block.replaceAll('\\0', '\\u{0}');
}

function describe(entry, theirs, mine) {
  return `${formatCase(entry)}\n--- rust ---\n${theirs}\n--- js ---\n${mine}`;
}

/**
 * Shortest key prefix that still diverges. Command interpretation bugs usually
 * show up on the first disagreeing keystroke, not at the end of a long script.
 * @param {any} wasm
 * @param {Case} entry
 * @returns {Case}
 */
/**
 * First key prefix whose final state disagrees. Used to catch scripts that
 * diverge and later reconverge.
 * @param {any} wasm
 * @param {Case} entry
 * @returns {Case | null}
 */
function firstDisagreeingPrefix(wasm, entry) {
  let parsed;
  try {
    parsed = parseKeys(entry.keys);
  } catch {
    return null;
  }
  for (let i = 1; i < parsed.length; i += 1) {
    const next = { ...entry, keys: renderKeys(parsed.slice(0, i)) };
    try {
      if (runJs(next) !== runWasm(wasm, next)) return next;
    } catch {
      return next;
    }
  }
  return null;
}

function shrinkKeys(wasm, entry) {
  let parsed;
  try {
    parsed = parseKeys(entry.keys);
  } catch {
    return entry;
  }
  if (parsed.length <= 1) return entry;

  const diverges = (keys) => {
    const next = { ...entry, keys };
    try {
      return runJs(next) !== runWasm(wasm, next);
    } catch {
      return true;
    }
  };

  let lo = 1;
  let hi = parsed.length;
  while (lo < hi) {
    const mid = Math.floor((lo + hi) / 2);
    if (diverges(renderKeys(parsed.slice(0, mid)))) hi = mid;
    else lo = mid + 1;
  }
  return { ...entry, keys: renderKeys(parsed.slice(0, lo)) };
}

/**
 * @param {any} wasm
 * @param {Case} entry
 * @returns {Case}
 */
function shrink(wasm, entry) {
  let best = shrinkKeys(wasm, entry);
  const candidates = [
    { ...best, viewport: null },
    { ...best, indent: null },
    { ...best, viewport: null, indent: null },
  ];
  if (best.text.includes('\n')) candidates.push({ ...best, text: best.text.split('\n')[0] });
  if (best.text.length > 1) candidates.push({ ...best, text: best.text.slice(0, 1) });
  for (const candidate of candidates) {
    try {
      if (runJs(candidate) !== runWasm(wasm, candidate)) {
        const smaller = candidate.text.length + candidate.keys.length < best.text.length + best.keys.length;
        const fewerSettings = !candidate.viewport && !candidate.indent && (best.viewport || best.indent);
        if (smaller || fewerSettings) best = candidate;
      }
    } catch {
      // keep best
    }
  }
  return best;
}

function main() {
  const args = process.argv.slice(2);
  const flag = (name, fallback) => {
    const at = args.indexOf(name);
    return at < 0 ? fallback : args[at + 1];
  };
  const total = Number(flag('--cases', '8000'));
  const seed = Number(flag('--seed', '1'));
  const campaignArg = flag('--campaign', 'all');
  const oracle = flag('--oracle', 'wasm');
  const until = args.includes('--until');
  const wasmPath = flag('--wasm', DEFAULT_WASM);
  const campaigns = campaignArg === 'all' ? CAMPAIGNS : [campaignArg];
  for (const name of campaigns) {
    if (!CAMPAIGNS.includes(name)) {
      console.error(`unknown campaign ${name}; choose from ${CAMPAIGNS.join(', ')}`);
      process.exitCode = 2;
      return;
    }
  }

  const wasm = oracle === 'wasm' ? loadWasm(wasmPath) : null;
  let rustOracle = null;
  const wantGrid = campaigns.includes('grid');
  const wantCorpus = campaigns.includes('corpus');
  const randomCampaigns = campaigns.filter((name) => name !== 'grid' && name !== 'corpus');

  /** @type {Case[]} */
  const generated = [];
  const lines = [];
  let next = random(seed);
  let produced = 0;
  let attempt = 0;
  const limit = until ? Number.POSITIVE_INFINITY : total;

  const runOne = (entry) => {
    let mine;
    try {
      mine = runJs(entry);
    } catch (error) {
      mine = `== ${entry.name} ==\nthrew: ${error instanceof Error ? error.message : String(error)}`;
    }
    let theirs;
    try {
      theirs = wasm ? runWasm(wasm, entry) : rustOracle?.get(entry.name);
    } catch (error) {
      theirs = `== ${entry.name} ==\nthrew: ${error instanceof Error ? error.message : String(error)}`;
    }
    if (theirs === undefined) return `${entry.name}: the Rust replay produced no block`;
    // Rust's `Debug for str` spells U+0000 as `\0`; the JS renderer uses `\u{0}`.
    // That is a snapshot-spelling gap, not a command-interpretation one.
    if (normalizeSnapshot(mine) !== normalizeSnapshot(theirs)) return describe(entry, theirs, mine);
    return null;
  };

  if (oracle === 'rust') {
    next = random(seed);
    for (let i = 0; i < total; i += 1) {
      const entry = generateCase(next, campaigns[i % campaigns.length], i);
      generated.push(entry);
      lines.push(formatCase(entry));
    }
    const dir = flag('--keep', null) ?? mkdtempSync(join(tmpdir(), 'vici-fuzz-'));
    const fixture = join(dir, 'cases.vici');
    writeFileSync(fixture, `${lines.join('\n---\n')}\n`);
    const rust = execFileSync('cargo', ['run', '-q', '-p', 'vici-oracle', '--', fixture], {
      cwd: CRATE,
      encoding: 'utf8',
      maxBuffer: 256 * 1024 * 1024,
    });
    rustOracle = blocks(rust);
    /** @type {string[]} */
    const diverged = [];
    for (const entry of generated) {
      const failure = runOne(entry);
      if (failure) diverged.push(failure);
    }
    report(total, seed, campaigns, fixture, diverged, wasm);
    return;
  }

  /** @type {string[]} */
  const diverged = [];
  const dir = flag('--keep', null) ?? mkdtempSync(join(tmpdir(), 'vici-fuzz-'));
  const fixture = join(dir, 'cases.vici');

  const queued = [
    ...(wantCorpus ? corpusCases() : []),
    ...(wantGrid ? gridCases() : []),
  ];
  let queueAt = 0;
  while (produced < limit) {
    const entry =
      queueAt < queued.length
        ? queued[queueAt++]
        : randomCampaigns.length === 0
          ? null
          : generateCase(next, randomCampaigns[attempt % randomCampaigns.length], produced);
    if (entry === null) break;
    generated.push(entry);
    lines.push(formatCase(entry));
    let failure = runOne(entry);
    // Even when the final states match, a later undo/`ggdG` can hide an
    // earlier disagreement. Walk prefixes of soup/mixed scripts.
    if (!failure && (entry.name.includes('soup') || entry.name.includes('mixed'))) {
      const mid = firstDisagreeingPrefix(wasm, entry);
      if (mid) failure = runOne(mid);
    }
    // Sticky column, last-find, last-search and macros are not in the
    // snapshot. A trailing observer motion makes them observable.
    if (!failure) {
      for (const probe of ['j', ';', 'n', '@a']) {
        const observed = { ...entry, name: `${entry.name}-obs`, keys: entry.keys + probe };
        const hit = runOne(observed);
        if (hit) {
          failure = hit;
          break;
        }
      }
    }
    produced += 1;
    attempt += 1;
    if (failure) {
      const shrunk = shrink(wasm, entry);
      const again = runOne(shrunk) ?? failure;
      diverged.push(again);
      if (until || diverged.length >= 8) break;
    }
    if (until && produced % 2000 === 0) {
      console.error(`… ${produced} cases, seed ${seed}, still agreeing`);
    }
  }

  writeFileSync(fixture, `${lines.join('\n---\n')}\n`);
  report(produced, seed, campaigns, fixture, diverged, wasm);
}

/**
 * @param {number} total
 * @param {number} seed
 * @param {string[]} campaigns
 * @param {string} fixture
 * @param {string[]} diverged
 * @param {any} wasm
 */
function report(total, seed, campaigns, fixture, diverged, wasm) {
  const reportLine = `${total} cases, seed ${seed}, campaigns ${campaigns.join(',')}: ${total - diverged.length} agree, ${diverged.length} diverge`;
  if (diverged.length === 0) {
    console.log(`${reportLine}\nfixture: ${fixture}`);
    return;
  }
  console.error(`${reportLine}\nfixture: ${fixture}\n`);
  for (const failure of diverged.slice(0, 8)) console.error(`${failure}\n`);
  if (wasm && diverged.length > 0) {
    const keep = join(dirname(fixture), 'minimized.vici');
    writeFileSync(keep, `${diverged.map((block) => block.split('\n--- rust ---')[0].trimEnd()).join('\n---\n')}\n`);
    console.error(`minimized: ${keep}`);
  }
  process.exitCode = 1;
}

main();
