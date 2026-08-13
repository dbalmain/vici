import { describe, expect, it } from "vitest";

import { isAscii, jsToUtf8, utf8Len, utf8ToJs } from "../src/utf8.js";

describe("utf8 helpers", () => {
  it("treats ASCII as identity", () => {
    expect(isAscii("abc")).toBe(true);
    expect(utf8Len("abc")).toBe(3);
    expect(utf8ToJs("abc", 2)).toBe(2);
    expect(jsToUtf8("abc", 2)).toBe(2);
  });

  it("counts é as two UTF-8 bytes", () => {
    expect(isAscii("café")).toBe(false);
    expect(utf8Len("café")).toBe(5);
    expect(utf8ToJs("café", 3)).toBe(3);
    expect(utf8ToJs("café", 5)).toBe(4);
    expect(jsToUtf8("café", 4)).toBe(5);
  });
});
