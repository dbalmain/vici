// Buffer + history with the stage → record → apply ordering.

import type { Edit } from "./contract/index.js";

import { JsBuffer } from "./buffer-js.js";
import { History, type Step } from "./history.js";
import type { BufferFactory, TextBuffer } from "./text-buffer.js";

const defaultBuffer: BufferFactory = (text = "") => JsBuffer.fromText(text);

export class Document {
  readonly buffer: TextBuffer;
  readonly history: History;

  constructor(text = "", makeBuffer: BufferFactory = defaultBuffer) {
    this.buffer = makeBuffer(text);
    this.history = new History();
  }

  static fromText(text: string): Document {
    return new Document(text);
  }

  toString(): string {
    return this.buffer.toString();
  }

  replace(start: number, end: number, text: string): Edit {
    const change = this.buffer.stageReplace(start, end, text);
    this.history.record(change);
    this.buffer.apply(change);
    return change.edit;
  }

  insert(at: number, text: string): Edit {
    return this.replace(at, at, text);
  }

  delete(start: number, end: number): Edit {
    return this.replace(start, end, "");
  }

  grouped<T>(edits: (doc: Document) => T): T {
    this.history.beginGroup();
    const out = edits(this);
    this.history.endGroup();
    return out;
  }

  undo(): Step {
    const step = this.history.undo();
    for (const change of step.changes) {
      this.buffer.apply(change);
    }
    return step;
  }

  redo(): Step {
    const step = this.history.redo();
    for (const change of step.changes) {
      this.buffer.apply(change);
    }
    return step;
  }

  undoDepth(): number {
    return this.history.undoDepth();
  }

  redoDepth(): number {
    return this.history.redoDepth();
  }
}
