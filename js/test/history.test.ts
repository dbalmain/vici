import { describe, expect, it } from "vitest";

import { Document, invertChange } from "../src/index.js";

describe("grouped undo/redo", () => {
  it("treats nested groups as one undo step", () => {
    const document = Document.fromText("");
    document.grouped((document) => {
      document.grouped((document) => {
        document.insert(0, "a");
      });
      document.insert(1, "b");
    });
    expect(document.undoDepth()).toBe(1);
    document.undo();
    expect(document.toString()).toBe("");
  });

  it("undoes a group by applying inverted changes in reverse", () => {
    const document = Document.fromText("xyz");
    document.grouped((document) => {
      document.replace(0, 1, "A");
      document.replace(2, 3, "C");
    });
    expect(document.toString()).toBe("AyC");
    const step = document.undo();
    expect(step.changes).toHaveLength(2);
    expect(document.toString()).toBe("xyz");
    document.redo();
    expect(document.toString()).toBe("AyC");
  });

  it("truncates the redo tail on a new change", () => {
    const document = Document.fromText("a");
    document.insert(1, "b");
    document.undo();
    expect(document.redoDepth()).toBe(1);
    document.insert(1, "c");
    expect(document.redoDepth()).toBe(0);
    expect(document.toString()).toBe("ac");
  });

  it("does not record noop changes", () => {
    const document = Document.fromText("abc");
    document.replace(0, 1, "a");
    expect(document.undoDepth()).toBe(0);
    expect(document.toString()).toBe("abc");
  });

  it("discards the oldest steps when a limit is set", () => {
    const document = Document.fromText("");
    document.history.setLimit(2);
    for (const [at, text] of [
      [0, "a"],
      [1, "b"],
      [2, "c"],
    ] as const) {
      document.insert(at, text);
    }
    expect(document.undoDepth()).toBe(2);
    document.undo();
    document.undo();
    expect(document.toString()).toBe("a");
    expect(document.undo().changes).toEqual([]);
  });

  it("records the pre-image before applying a change", () => {
    const document = Document.fromText("abc");
    document.replace(0, 1, "X");
    expect(document.toString()).toBe("Xbc");
    document.undo();
    expect(document.toString()).toBe("abc");
    document.redo();
    expect(document.toString()).toBe("Xbc");
  });

  it("leaves redo intact when a noop is recorded after undo", () => {
    const document = Document.fromText("a");
    document.insert(1, "b");
    document.undo();
    document.replace(0, 1, "a");
    expect(document.redoDepth()).toBe(1);
    document.redo();
    expect(document.toString()).toBe("ab");
  });
});

describe("Change invert", () => {
  it("is self-inverse on a document edit", () => {
    const document = Document.fromText("ab\ncd");
    const before = document.toString();
    const change = document.buffer.stageReplace(1, 4, "X\nY");
    document.buffer.apply(change);
    document.buffer.apply(invertChange(change));
    expect(document.toString()).toBe(before);
    document.buffer.apply(invertChange(invertChange(change)));
    expect(document.toString()).toBe("aX\nYd");
  });
});
