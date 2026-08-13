// Speed benchmarks.
//
// Protocol, kept deliberately close to the beetle experiment's so the two
// scoreboards can be read together:
//
// - The engine is rebuilt from the workload's text before every sample, and
//   that rebuild is *not* timed. Only the key script is.
// - `text()` is never called inside the timed region.
// - Each workload is run in bulk (`typeKeys(script)`) and per key
//   (`handleKey` for each parsed key), because a host feeds one key at a time.
// - Cold start is measured in a fresh process, not mixed in here.
//
// `node bench/run.js [--vs <path-to-other-engine>] [--json <file>]`
//
// The comparison engine only has to expose `createEngine(text)` (or a default
// export, or an `Editor` class) with `typeKeys` / `handleKey` / `setText`.

import { execFileSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { cpus, totalmem } from 'node:os';
import { fileURLToPath } from 'node:url';

import { Editor, vim, keys } from '../src/index.js';
import { workloads } from './workloads.js';

const SAMPLES = 40;
const WARMUP = 8;
const MIN_MILLIS = 300;

/**
 * Time `body` alone. `prepare` rebuilds the engine between samples and is
 * deliberately outside the clock: loading a megabyte of text is not what any
 * of these workloads is measuring.
 * @param {() => any} prepare
 * @param {(state: any) => void} body
 * @returns {{ p50: number, p95: number, mean: number, iters: number }}
 */
function measure(prepare, body) {
  for (let i = 0; i < WARMUP; i += 1) body(prepare());
  /** @type {number[]} */
  const samples = [];
  const deadline = performance.now() + MIN_MILLIS;
  while (samples.length < SAMPLES || (performance.now() < deadline && samples.length < 400)) {
    const state = prepare();
    const start = process.hrtime.bigint();
    body(state);
    samples.push(Number(process.hrtime.bigint() - start) / 1e6);
  }
  samples.sort((a, b) => a - b);
  return {
    p50: samples[samples.length >> 1],
    p95: samples[Math.min(Math.floor(samples.length * 0.95), samples.length - 1)],
    mean: samples.reduce((sum, value) => sum + value, 0) / samples.length,
    iters: samples.length,
  };
}

/**
 * @param {number} millis
 * @returns {string}
 */
function human(millis) {
  if (millis >= 1) return `${millis.toFixed(millis >= 10 ? 0 : 2)} ms`;
  if (millis >= 0.001) return `${(millis * 1000).toFixed(millis >= 0.01 ? 0 : 1)} µs`;
  return `${(millis * 1e6).toFixed(0)} ns`;
}

/** The engine under test, wrapped so a foreign engine can be measured the same way. */
const local = {
  name: 'vici.js',
  /** @param {string} text */
  make(text) {
    return new Editor(text, KEYMAP);
  },
  /** @param {Editor} engine @param {string} script */
  bulk(engine, script) {
    engine.typeKeys(script);
  },
  /** @param {Editor} engine @param {readonly string[]} script */
  perKey(engine, script) {
    for (const key of script) engine.handleKey(key);
  },
  parse: keys,
};
const KEYMAP = vim();

/**
 * @param {string} path
 * @returns {Promise<any>}
 */
async function foreign(path) {
  const module = await import(path);
  const create = module.createEngine ?? module.createEditor ?? module.default;
  const Klass = module.Editor ?? module.Engine;
  const parse = module.keys ?? keys;
  return {
    name: path.replace(/.*\/(packages|src)\//, ''),
    /** @param {string} text */
    make(text) {
      return typeof create === 'function' ? create(text) : new Klass(text);
    },
    /** @param {any} engine @param {string} script */
    bulk(engine, script) {
      engine.typeKeys(script);
    },
    /** @param {any} engine @param {readonly any[]} script */
    perKey(engine, script) {
      for (const key of script) engine.handleKey(key);
    },
    parse,
  };
}

/** @returns {{ p50: number, mean: number }} */
function coldStart() {
  const entry = fileURLToPath(new URL('../src/index.js', import.meta.url));
  /** @type {number[]} */
  const samples = [];
  for (let i = 0; i < 10; i += 1) {
    const out = execFileSync(
      process.execPath,
      [
        '-e',
        `const t=performance.now();import(${JSON.stringify(entry)}).then(m=>{new m.Editor('x');console.log(performance.now()-t)})`,
      ],
      { encoding: 'utf8' },
    );
    samples.push(Number(out.trim()));
  }
  samples.sort((a, b) => a - b);
  return { p50: samples[samples.length >> 1], mean: samples.reduce((a, b) => a + b, 0) / samples.length };
}

async function main() {
  const args = process.argv.slice(2);
  const versus = args.indexOf('--vs');
  const jsonAt = args.indexOf('--json');
  const engines = [local];
  if (versus >= 0) engines.push(await foreign(args[versus + 1]));

  const cold = coldStart();
  const rows = [];
  for (const workload of workloads()) {
    const parsed = local.parse(workload.script);
    for (const engine of engines) {
      const script = engine === local ? parsed : engine.parse(workload.script);
      const prepare = () => engine.make(workload.text);
      for (const [mode, body] of [
        ['bulk', (/** @type {any} */ state) => engine.bulk(state, workload.script)],
        ['per-key', (/** @type {any} */ state) => engine.perKey(state, script)],
      ]) {
        const stats = measure(prepare, /** @type {(state: any) => void} */ (body));
        rows.push({ workload: workload.name, engine: engine.name, mode, ...stats });
      }
    }
  }

  const cpu = cpus()[0]?.model ?? 'unknown';
  const lines = [
    '# vici.js speed',
    '',
    `Generated: ${new Date().toISOString()}`,
    '',
    `- Node ${process.version} on ${process.platform}/${process.arch}`,
    `- ${cpu}, ${(totalmem() / 2 ** 30).toFixed(0)} GiB`,
    `- Engine rebuild is untimed; \`text()\` is never called in the timed region.`,
    '',
    '## Cold start',
    '',
    `A fresh process importing the module and constructing one editor: **${human(cold.p50)}** p50 (${human(cold.mean)} mean over 10 processes).`,
    '',
    '## Hot',
    '',
    '| Workload | Engine | Mode | p50 | p95 | mean | iters |',
    '| --- | --- | --- | ---: | ---: | ---: | ---: |',
  ];
  for (const row of rows) {
    lines.push(
      `| \`${row.workload}\` | ${row.engine} | ${row.mode} | ${human(row.p50)} | ${human(row.p95)} | ${human(row.mean)} | ${row.iters} |`,
    );
  }
  const report = `${lines.join('\n')}\n`;
  process.stdout.write(report);
  if (jsonAt >= 0) {
    writeFileSync(args[jsonAt + 1], `${JSON.stringify({ cold, rows, node: process.version, cpu }, null, 2)}\n`);
  }
}

await main();
