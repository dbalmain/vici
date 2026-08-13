// What a host actually ships.
//
// `esbuild --bundle --minify --format=esm` of the public entry point, weighed
// raw, gzipped and brotli'd. Brotli is the number that matters: it is what a
// browser downloads.
//
// `node bench/size.js [--vs <entry>]` also weighs another engine's entry point
// through the identical pipeline.

import { execFileSync } from 'node:child_process';
import { gzipSync, brotliCompressSync, constants } from 'node:zlib';
import { fileURLToPath } from 'node:url';

const ESBUILD = fileURLToPath(new URL('../node_modules/.bin/esbuild', import.meta.url));

/**
 * @param {string} entry
 * @returns {Buffer}
 */
function bundle(entry) {
  return execFileSync(ESBUILD, ['--bundle', '--minify', '--format=esm', '--target=es2022', entry], {
    maxBuffer: 64 * 1024 * 1024,
  });
}

/**
 * @param {number} bytes
 * @returns {string}
 */
function kib(bytes) {
  return `${(bytes / 1024).toFixed(1)} KiB (${bytes})`;
}

/**
 * @param {string} label
 * @param {string} entry
 * @returns {{ label: string, raw: number, gzip: number, brotli: number }}
 */
function weigh(label, entry) {
  const code = bundle(entry);
  return {
    label,
    raw: code.length,
    gzip: gzipSync(code, { level: 9 }).length,
    brotli: brotliCompressSync(code, {
      params: { [constants.BROTLI_PARAM_QUALITY]: 11, [constants.BROTLI_PARAM_SIZE_HINT]: code.length },
    }).length,
  };
}

const args = process.argv.slice(2);
const rows = [weigh('vici-js', fileURLToPath(new URL('../src/index.js', import.meta.url)))];
const versus = args.indexOf('--vs');
if (versus >= 0) rows.push(weigh(args[versus + 1].replace(/.*\/packages\//, ''), args[versus + 1]));

const lines = [
  '# vici-js size',
  '',
  `Generated: ${new Date().toISOString()}`,
  '',
  '`esbuild --bundle --minify --format=esm --target=es2022`, then gzip -9 and brotli -11.',
  '',
  '| Artifact | raw | gzip | brotli |',
  '| --- | ---: | ---: | ---: |',
  ...rows.map((row) => `| ${row.label} | ${kib(row.raw)} | ${kib(row.gzip)} | ${kib(row.brotli)} |`),
  '',
];
process.stdout.write(`${lines.join('\n')}\n`);
