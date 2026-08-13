import { describe, expect, it } from "vitest";

import {
  KeyParseError,
  Mods,
  asDigit,
  asText,
  charKey,
  codeKey,
  ctrlKey,
  key,
  keys,
  makeKey,
  render,
} from "../src/index.js";

describe("aliases parse to their canonical keys", () => {
  it.each([
    ["<Esc>", codeKey({ type: "Esc" })],
    ["<CR>", codeKey({ type: "Enter" })],
    ["<Enter>", codeKey({ type: "Enter" })],
    ["<Return>", codeKey({ type: "Enter" })],
    ["<Space>", charKey(" ")],
    ["<lt>", charKey("<")],
    ["<gt>", charKey(">")],
    ["<bslash>", charKey("\\")],
    ["<BS>", codeKey({ type: "Backspace" })],
    ["<Del>", codeKey({ type: "Delete" })],
    ["<F5>", codeKey({ type: "F", n: 5 })],
    ["<F12>", codeKey({ type: "F", n: 12 })],
    ["<C-->", ctrlKey("-")],
    ["<C-d>", ctrlKey("d")],
    ["<M-x>", makeKey({ type: "Char", char: "x" }, Mods.ALT)],
    ["<A-x>", makeKey({ type: "Char", char: "x" }, Mods.ALT)],
  ] as const)("%s", (spec, expected) => {
    expect(key(spec)).toEqual(expected);
  });
});

describe("SHIFT normalisation", () => {
  it("uppercases characters with ASCII rules and drops SHIFT", () => {
    expect(key("<S-a>")).toEqual(charKey("A"));
    expect(key("A")).toEqual(charKey("A"));
    expect(makeKey({ type: "Char", char: "a" }, Mods.SHIFT)).toEqual(
      charKey("A"),
    );
    // Unicode case mapping (ß → SS) is not this slice.
    expect(makeKey({ type: "Char", char: "ß" }, Mods.SHIFT)).toEqual(
      charKey("ß"),
    );
  });

  it("keeps SHIFT on non-char keys", () => {
    expect(key("<S-Tab>")).toEqual(makeKey({ type: "Tab" }, Mods.SHIFT));
    expect(key("<S-Esc>")).toEqual(makeKey({ type: "Esc" }, Mods.SHIFT));
  });
});

describe("malformed notation", () => {
  it("reports unterminated `<`", () => {
    expect(() => keys("<C-d")).toThrow(KeyParseError);
    expect(() => keys("<C-d")).toThrow("unterminated `<` in key sequence");
    expect(() => keys("<")).toThrow("unterminated `<` in key sequence");
  });

  it("reports unknown key names", () => {
    expect(() => keys("<Nope>")).toThrow("unknown key name `<Nope>`");
    expect(() => key("ab")).toThrow("unknown key name `<ab>`");
    expect(() => key("")).toThrow("unknown key name `<>`");
  });
});

describe("parse/render round-trip", () => {
  const specs = [
    "2dw<Esc><C-r>",
    "<C-d>",
    "<C-->",
    "<F12>",
    "<M-x>",
    "<A-x>",
    "<S-Tab>",
    "<Esc>",
    "<CR>",
    "<Enter>",
    "<Return>",
    "<Space>",
    "<lt>",
    "<gt>",
    "<bslash>",
    "<BS>",
    "<Del>",
    "<F5>",
    "A",
    "<S-a>",
    "hello",
    "<C-Space>",
    "<S-Esc>",
    "<C-lt>",
  ];

  it("parse(render(keys(spec))) equals keys(spec)", () => {
    for (const spec of specs) {
      const parsed = keys(spec);
      expect(keys(render(parsed))).toEqual(parsed);
    }
  });

  it("renders aliases to their canonical form", () => {
    expect(render(keys("A"))).toBe("A");
    expect(render(keys("<S-a>"))).toBe("A");
    expect(render(keys("<Enter>"))).toBe("<CR>");
    expect(render(keys("<Return>"))).toBe("<CR>");
    expect(render(keys("<A-x>"))).toBe("<M-x>");
    expect(render(keys("<Space>"))).toBe("<Space>");
    expect(render(keys("<lt>"))).toBe("<lt>");
    expect(render(keys("<gt>"))).toBe(">");
    expect(render(keys("<bslash>"))).toBe("\\");
    expect(render(keys("<C-d>"))).toBe("<C-d>");
    expect(render(keys("<C-->"))).toBe("<C-->");
    expect(render(keys("<S-Tab>"))).toBe("<S-Tab>");
    expect(render(keys("2dw<Esc><C-r>"))).toBe("2dw<Esc><C-r>");
    // vici renders modified `>` as `<C->>`, which cannot be parsed back
    // (the first `>` closes the bracket). Write `<C-gt>` in scripts.
    expect(render(keys("<C-gt>"))).toBe("<C->>");
  });
});

describe("Key helpers", () => {
  it("asText is only an unmodified character", () => {
    expect(asText(charKey("w"))).toBe("w");
    expect(asText(ctrlKey("w"))).toBeUndefined();
    expect(asText(codeKey({ type: "Esc" }))).toBeUndefined();
  });

  it("asDigit is an unmodified ASCII digit", () => {
    expect(asDigit(charKey("3"))).toBe(3);
    expect(asDigit(charKey("a"))).toBeUndefined();
    expect(asDigit(ctrlKey("3"))).toBeUndefined();
  });
});
