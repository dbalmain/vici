import { strict as assert } from 'node:assert';
import { test } from 'node:test';

import { cases, runCase, snapshot } from './oracle.js';

const all = cases();
const expected = snapshot();

test('the fixture file still holds every case', () => {
  assert.equal(all.length, 411);
});

test('every case matches the Rust snapshot', () => {
  const blocks = expected.split('\n== ');
  /** @type {Map<string, string>} */
  const want = new Map();
  blocks.forEach((block, index) => {
    const body = index === 0 ? block : `== ${block}`;
    const name = body.slice(3, body.indexOf(' ==', 3));
    want.set(name, body);
  });

  /** @type {string[]} */
  const failures = [];
  for (const entry of all) {
    const actual = runCase(entry).trimEnd();
    const oracle = (want.get(entry.name) ?? '<missing>').trimEnd();
    if (actual !== oracle) failures.push(`--- want ---\n${oracle}\n--- got ---\n${actual}`);
  }
  assert.equal(
    failures.length,
    0,
    `${failures.length}/${all.length} cases diverge:\n\n${failures.slice(0, 6).join('\n\n')}`,
  );
});

test('the whole snapshot matches, block for block', () => {
  const actual = all.map((entry) => runCase(entry)).join('');
  assert.equal(actual.trimEnd(), expected.trimEnd());
});
