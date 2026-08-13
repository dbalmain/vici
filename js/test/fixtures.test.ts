import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { parseCases, renderCase } from "../src/contract/index.js";
import type { Case } from "../src/contract/index.js";
import { createEngine } from "../src/index.js";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "../..");
const fixture = readFileSync(
  join(repoRoot, "crates/vici/tests/fixtures/editor.vici"),
  "utf8",
);
const snap = stripInsta(
  readFileSync(
    join(repoRoot, "crates/vici/tests/snapshots/editor_cases__editor_cases.snap"),
    "utf8",
  ),
);
const cases = parseCases(fixture);

function runCase(c: Case): string {
  const engine = createEngine(c.text);
  if (c.settings.viewport !== undefined) {
    engine.setViewport(c.settings.viewport);
  }
  if (c.settings.indent !== undefined) {
    engine.setIndent(c.settings.indent);
  }
  const effects = engine.typeKeys(c.keys);
  return renderCase(c.name, engine, effects);
}

describe("editor.vici", () => {
  it("parses the rust fixture set", () => {
    expect(cases.length).toBeGreaterThan(400);
  });

  for (let i = 0; i < cases.length; i++) {
    const c = cases[i];
    if (c === undefined) {
      break;
    }
    const last = i === cases.length - 1;
    it(c.name, () => {
      const rendered = runCase(c);
      const expected = sliceBlock(snap, c.name);
      const got = last ? rendered.replace(/\n+$/, "\n") : rendered;
      expect(got).toBe(expected);
    });
  }

  it("concatenated blocks match the rust insta snap", () => {
    const got = cases
      .map((c) => runCase(c))
      .join("")
      .replace(/\n+$/, "\n");
    expect(got).toBe(snap);
  });
});

describe("README smoke", () => {
  it("changes the word under the cursor", () => {
    const engine = createEngine("select id, name\nfrom users");
    engine.typeKeys("cwSELECT<Esc>");
    expect(engine.text()).toBe("SELECT id, name\nfrom users");
    expect(engine.mode()).toBe("Normal");
  });
});

function stripInsta(text: string): string {
  if (!text.startsWith("---\n")) {
    return text.endsWith("\n") ? text : `${text}\n`;
  }
  const end = text.indexOf("\n---\n");
  if (end < 0) {
    throw new Error("insta snap is missing its closing ---");
  }
  let body = text.slice(end + "\n---\n".length);
  if (!body.endsWith("\n")) {
    body += "\n";
  }
  return body.replace(/\n+$/, "\n");
}

function sliceBlock(text: string, name: string): string {
  const startToken = `== ${name} ==\n`;
  const start = text.indexOf(startToken);
  if (start < 0) {
    throw new Error(`missing snap block == ${name} ==`);
  }
  const next = text.indexOf("\n== ", start + startToken.length);
  if (next < 0) {
    return text.slice(start).replace(/\n+$/, "\n");
  }
  return text.slice(start, next + 1);
}
