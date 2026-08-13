import { describe, expect, it } from "vitest";

import { Buffer, advance, invertChange, shift } from "../src/index.js";

describe("rows are counted by LF only", () => {
  it("treats \\r as ordinary content", () => {
    const buffer = Buffer.fromText("a\rb\nc");
    expect(buffer.lenRows()).toBe(2);
    expect(buffer.rowText(0)).toBe("a\rb");
    expect(buffer.byteToPoint(4)).toEqual({ row: 1, col: 0 });
  });

  it("adds a phantom empty row after a trailing newline", () => {
    const buffer = Buffer.fromText("hello\n");
    expect(buffer.lenRows()).toBe(2);
    expect(buffer.rowText(1)).toBe("");
    expect(buffer.byteToPoint(6)).toEqual({ row: 1, col: 0 });
  });
});

describe("byte / point mapping", () => {
  it("counts café's é as two UTF-8 bytes", () => {
    const buffer = Buffer.fromText("-- café");
    expect(buffer.lenBytes()).toBe(8);
    expect(buffer.byteToPoint(8)).toEqual({ row: 0, col: 8 });
    expect(buffer.byte(6)).toBe(0xc3);
    expect(buffer.byte(7)).toBe(0xa9);
  });
});

describe("insert and invert", () => {
  it("restores bytes and points", () => {
    const original = "hello\nwoéld";
    const buffer = Buffer.fromText(original);
    const change = buffer.replace(1, 5, "i\n");
    buffer.apply(invertChange(change));
    expect(buffer.toString()).toBe(original);
    expect(buffer.lenBytes()).toBe(new TextEncoder().encode(original).length);
  });
});

describe("advance / shift", () => {
  it("adds byte length when there is no newline", () => {
    expect(advance({ row: 1, col: 3 }, "é")).toEqual({ row: 1, col: 5 });
  });

  it("applies mark gravity", () => {
    const buffer = Buffer.fromText("abcdef");
    const change = buffer.replace(2, 4, "XYZ");
    expect(shift(change.edit, 3)).toBe(2);
    expect(shift(change.edit, 4)).toBe(5);
  });
});
