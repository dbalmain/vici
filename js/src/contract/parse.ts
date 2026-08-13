// Port of `/home/dave/w/vici/crates/vici/tests/editor_cases.rs`
// (`parse_cases`, `valid_name`, `unescape`, `parse_settings`). Errors throw
// with the same messages as the Rust asserts.

import type { Case, Indent, Settings, Viewport } from "./types.js";

export function parseCases(fixture: string): Case[] {
  const cases: Case[] = [];
  const names = new Set<string>();
  for (const chunk of fixture.split("\n---\n")) {
    const lines = chunk
      .split("\n")
      .filter((line) => line.length > 0 && !line.startsWith("#"));
    if (lines.length === 0) {
      continue;
    }
    const header = lines[0];
    const name =
      header !== undefined && header.startsWith("case ")
        ? header.slice("case ".length)
        : undefined;
    if (name === undefined || !validName(name)) {
      throw new Error("<unknown>: case must be first and kebab-case");
    }
    let text: string | undefined;
    let keys: string | undefined;
    let settings: Settings | undefined;
    for (const line of lines.slice(1)) {
      if (line.startsWith("text")) {
        const value = line.slice("text".length);
        if (value.length > 0 && !value.startsWith(" ")) {
          throw new Error(`${name}: malformed text`);
        }
        if (text !== undefined) {
          throw new Error(`${name}: duplicate text`);
        }
        text = unescape(value.startsWith(" ") ? value.slice(1) : "", name);
      } else if (line.startsWith("keys ")) {
        if (keys !== undefined) {
          throw new Error(`${name}: duplicate keys`);
        }
        keys = line.slice("keys ".length);
      } else if (line.startsWith("with ")) {
        if (settings !== undefined) {
          throw new Error(`${name}: duplicate with`);
        }
        settings = parseSettings(line.slice("with ".length), name);
      } else {
        throw new Error(`${name}: unknown fixture prefix: ${line}`);
      }
    }
    if (text === undefined) {
      throw new Error(`${name}: missing text`);
    }
    if (keys === undefined) {
      throw new Error(`${name}: missing keys`);
    }
    if (names.has(name)) {
      throw new Error(`${name}: duplicate case name`);
    }
    names.add(name);
    cases.push({
      name,
      text,
      keys,
      settings: settings ?? {},
    });
  }
  if (cases.length === 0) {
    throw new Error("<unknown>: fixture has no cases");
  }
  return cases;
}

export function validName(name: string): boolean {
  if (name.length === 0 || name.startsWith("-") || name.endsWith("-")) {
    return false;
  }
  for (let i = 0; i < name.length; i++) {
    const code = name.charCodeAt(i);
    const isLower = code >= 97 && code <= 122;
    const isDigit = code >= 48 && code <= 57;
    if (!isLower && !isDigit && code !== 45) {
      return false;
    }
  }
  return true;
}

export function unescape(value: string, name: string): string {
  let out = "";
  for (let i = 0; i < value.length; i++) {
    const ch = value[i];
    if (ch !== "\\") {
      out += ch;
      continue;
    }
    const next = value[++i];
    if (next === undefined) {
      throw new Error(`${name}: unsupported trailing backslash`);
    }
    switch (next) {
      case "n":
        out += "\n";
        break;
      case "r":
        out += "\r";
        break;
      case "t":
        out += "\t";
        break;
      case "\\":
        out += "\\";
        break;
      default:
        throw new Error(`${name}: unsupported escape \\${next}`);
    }
  }
  return out;
}

/** Settings are space-separated, their values comma-separated. */
export function parseSettings(value: string, name: string): Settings {
  const settings: Settings = {};
  for (const setting of value.split(/\s+/).filter((part) => part.length > 0)) {
    const eq = setting.indexOf("=");
    if (eq < 0) {
      throw new Error(`${name}: setting needs a value: ${setting}`);
    }
    const key = setting.slice(0, eq);
    const values = setting.slice(eq + 1).split(",");
    if (key === "viewport" && values.length === 2) {
      if (settings.viewport !== undefined) {
        throw new Error(`${name}: duplicate viewport`);
      }
      const topRow = values[0];
      const height = values[1];
      if (topRow === undefined || height === undefined) {
        throw new Error(`${name}: unsupported setting ${setting}`);
      }
      const viewport: Viewport = {
        topRow: number(topRow, name),
        height: number(height, name),
      };
      settings.viewport = viewport;
    } else if (key === "indent" && values.length === 3) {
      if (settings.indent !== undefined) {
        throw new Error(`${name}: duplicate indent`);
      }
      const shiftWidth = values[0];
      const tabWidth = values[1];
      const kind = values[2];
      if (
        shiftWidth === undefined ||
        tabWidth === undefined ||
        kind === undefined
      ) {
        throw new Error(`${name}: unsupported setting ${setting}`);
      }
      let useTabs: boolean;
      if (kind === "tabs") {
        useTabs = true;
      } else if (kind === "spaces") {
        useTabs = false;
      } else {
        throw new Error(`${name}: indent wants tabs or spaces, got ${kind}`);
      }
      const indent: Indent = {
        shiftWidth: number(shiftWidth, name),
        tabWidth: number(tabWidth, name),
        useTabs,
      };
      settings.indent = indent;
    } else {
      throw new Error(`${name}: unsupported setting ${setting}`);
    }
  }
  return settings;
}

function number(value: string, name: string): number {
  if (!/^\d+$/.test(value)) {
    throw new Error(`${name}: invalid setting number ${value}`);
  }
  return Number(value);
}
