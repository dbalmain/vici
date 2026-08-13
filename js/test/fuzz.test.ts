// Alignment fuzzer. Rust `vici-oracle` is the source of truth; JS must match
// the snapshot block. A miss prints a pasteable editor.vici case.
//
// Default `npm test` runs a short smoke. `npm run fuzz` (or FUZZ_CASES=N)
// is the long blast.

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { renderCase } from "../src/contract/index.js";
import { createEngine } from "../src/index.js";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "../..");
const oracleBin = join(repoRoot, "target/debug/vici-oracle");

const ATOMS = [
  "h",
  "j",
  "k",
  "l",
  "w",
  "b",
  "e",
  "W",
  "B",
  "E",
  "0",
  "^",
  "$",
  "G",
  "gg",
  "%",
  "H",
  "M",
  "L",
  "<C-d>",
  "<C-u>",
  "<C-o>",
  "ma",
  "`a",
  "'a",
  "dw",
  "ciw<Esc>",
  "yi(",
  ">>",
  "gUiw",
  "x",
  "X",
  "ré",
  "~",
  "J",
  "p",
  "P",
  "ié<Esc>",
  "i日本<Esc>",
  "vwd",
  "v~",
  "u",
  "<C-r>",
  ".",
  "dd",
  "yy",
  "p",
  "aw",
  "i\"",
  "/a<CR>",
  "n",
  "f,",
  ";",
];

const CHUNKS = [
  "",
  "a",
  "  ",
  "\t",
  "\n",
  "\r\n",
  "()[]{}<>",
  "!?,.;_",
  "café",
  "日本語",
  "word",
  "the quick brown",
];

const smoke = process.env.FUZZ_CASES === undefined;
const caseCount = Number(process.env.FUZZ_CASES ?? 24);
const seed = Number(process.env.FUZZ_SEED ?? 1);

describe("rust/js alignment", () => {
  it(`agrees on ${caseCount} random scripts (seed ${seed})`, () => {
    ensureOracle();
    const rng = mulberry32(seed >>> 0);
    const misses: string[] = [];
    for (let i = 0; i < caseCount; i++) {
      const text = randomText(rng);
      const keys = randomScript(rng);
      const name = `fuzz-${seed}-${i}`;
      const rust = oracle(name, text, keys);
      const js = jsSnap(name, text, keys);
      if (rust !== js) {
        misses.push(formatMiss(name, text, keys, rust, js));
      }
    }
    expect(misses, misses.join("\n---\n")).toEqual([]);
  });

  it("agrees on a known mixed script", () => {
    ensureOracle();
    const text = "select id, name\nfrom users";
    const keys = "wciwX<Esc>jdw";
    expect(jsSnap("mixed", text, keys)).toBe(oracle("mixed", text, keys));
  });
});

function jsSnap(name: string, text: string, keys: string): string {
  const engine = createEngine(text);
  engine.setViewport({ topRow: 0, height: 6 });
  const effects = engine.typeKeys(keys);
  return renderCase(name, engine, effects);
}

function oracle(name: string, text: string, keys: string): string {
  const result = spawnSync(
    oracleBin,
    ["--name", name, "--text", text, "--keys", keys, "--with", "viewport=0,6"],
    { encoding: "utf8", cwd: repoRoot },
  );
  if (result.status !== 0) {
    throw new Error(
      `vici-oracle failed (${result.status}): ${result.stderr || result.stdout}`,
    );
  }
  return result.stdout;
}

function formatMiss(
  name: string,
  text: string,
  keys: string,
  rust: string,
  js: string,
): string {
  const dumped = spawnSync(
    oracleBin,
    ["--fixture", "--name", name, "--text", text, "--keys", keys],
    { encoding: "utf8", cwd: repoRoot },
  );
  const fixture =
    dumped.status === 0
      ? dumped.stdout
      : `case ${name}\ntext ${JSON.stringify(text)}\nkeys ${keys}\n`;
  return [
    "alignment miss — paste this into editor.vici:",
    fixture.trimEnd(),
    "",
    "--- rust ---",
    rust.trimEnd(),
    "--- js ---",
    js.trimEnd(),
  ].join("\n");
}

function ensureOracle(): void {
  if (existsSync(oracleBin)) {
    return;
  }
  const result = spawnSync("cargo", ["build", "-p", "vici-oracle"], {
    cwd: repoRoot,
    encoding: "utf8",
    stdio: "pipe",
  });
  if (result.status !== 0) {
    throw new Error(`cargo build -p vici-oracle failed:\n${result.stderr}`);
  }
}

function randomText(rng: () => number): string {
  const n = 1 + Math.floor(rng() * 5);
  let out = "";
  for (let i = 0; i < n; i++) {
    out += CHUNKS[Math.floor(rng() * CHUNKS.length)] ?? "";
  }
  return out.length === 0 ? "x" : out;
}

function randomScript(rng: () => number): string {
  const n = 1 + Math.floor(rng() * 6);
  let out = "";
  for (let i = 0; i < n; i++) {
    out += ATOMS[Math.floor(rng() * ATOMS.length)] ?? "l";
  }
  return out;
}

function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a += 0x6d2b79f5;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

void smoke;
