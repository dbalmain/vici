// Benchmark inputs. Deterministic, so two runs on the same machine differ only
// by noise, and so another engine can be fed exactly the same work.
//
// The shapes mirror the beetle experiment's scoreboard (insert, word motion at
// three buffer sizes, bulk delete, undo, macro, search, whole-buffer operator,
// mixed session) so the two sets of numbers can be read side by side.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const WORDS = (
  'select from where group by having order limit offset join inner outer left right on as ' +
  'insert into values update set delete create table index view schema column primary key ' +
  'foreign unique default null true false and or not in like between exists case when then'
).split(' ');

/**
 * A seeded generator, so a 1 MiB buffer is the same 1 MiB every run.
 * @param {number} seed
 * @returns {() => number}
 */
function random(seed) {
  let state = seed >>> 0;
  return () => {
    state = (state * 1664525 + 1013904223) >>> 0;
    return state / 0x100000000;
  };
}

/**
 * Prose of roughly `bytes` bytes, in rows of eight to sixteen words.
 * @param {number} bytes
 * @param {number} [seed]
 * @returns {string}
 */
export function prose(bytes, seed = 7) {
  const next = random(seed);
  /** @type {string[]} */
  const rows = [];
  let size = 0;
  while (size < bytes) {
    const count = 8 + Math.floor(next() * 9);
    /** @type {string[]} */
    const row = [];
    for (let i = 0; i < count; i += 1) row.push(WORDS[Math.floor(next() * WORDS.length)]);
    const line = row.join(' ');
    rows.push(line);
    size += line.length + 1;
  }
  return rows.join('\n');
}

const TYPED = 'abcdefghijklmnopqrstuvwxyz0123456789 ';

/** @returns {string} */
function typedRun(length) {
  return TYPED.repeat(Math.ceil(length / TYPED.length)).slice(0, length);
}

/** A rare needle, planted once near the end and once in the middle. */
function withNeedle(text) {
  const half = Math.floor(text.length / 2);
  return `${text.slice(0, half)}\nneedle\n${text.slice(half)}\nneedle here\n`;
}

const FEATURES = fileURLToPath(new URL('../../FEATURES.txt', import.meta.url));

/** @typedef {{ name: string, text: string, script: string }} Workload */

/** @returns {Workload[]} */
export function workloads() {
  const small = prose(1024);
  const medium = prose(100 * 1024);
  const large = prose(1024 * 1024);
  return [
    { name: 'insert-1k', text: '', script: `i${typedRun(1005)}` },
    { name: 'words-small', text: small, script: '10w10b3dw' },
    { name: 'words-100k', text: medium, script: '50w50b' },
    { name: 'words-1m', text: large, script: '50w50b' },
    { name: 'delete-word', text: medium, script: `gg${'dw'.repeat(200)}` },
    { name: 'undo-storm', text: prose(10 * 1024), script: `${'ia<Esc>'.repeat(100)}${'u'.repeat(100)}` },
    { name: 'macro', text: 'one two three\nfour five six\nseven eight nine', script: 'qa~jq200@a' },
    { name: 'search', text: withNeedle(medium), script: '/needle<CR>nnn' },
    { name: 'operator-all', text: medium, script: 'ggdG' },
    { name: 'edit-session', text: readFileSync(FEATURES, 'utf8'), script: 'ggjwcwSELECT<Esc>viwywpu' },
  ];
}
