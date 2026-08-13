import type { Point } from "./contract/index.js";

import type { Change } from "./edit.js";

export type ByteRange = {
  start: number;
  end: number;
};

/**
 * Storage the engine and motions walk. Public offsets are UTF-8 bytes.
 * Two implementations: a JS string (`JsBuffer`) and a UTF-8 piece table
 * (`Utf8Buffer`).
 */
export type TextBuffer = {
  toString(): string;
  lenBytes(): number;
  isEmpty(): boolean;
  lenRows(): number;
  byte(idx: number): number;
  byteToPoint(byte: number): Point;
  pointToByte(point: Point): number;
  rowRange(row: number): ByteRange;
  rowContentRange(row: number): ByteRange;
  rowText(row: number): string;
  textIn(start: number, end: number): string;
  stageReplace(start: number, end: number, text: string): Change;
  apply(change: Change): void;
  replace(start: number, end: number, text: string): Change;
  insert(at: number, text: string): Change;
  delete(start: number, end: number): Change;
};

export type BufferFactory = (text?: string) => TextBuffer;
