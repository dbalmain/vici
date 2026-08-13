// Change geometry. Field names match the Engine contract (`Edit`, `Point`);
// behaviour matches `crates/vici/src/edit.rs`.

import type { Edit, Point } from "./contract/index.js";

import { utf8Len } from "./utf8.js";

export type { Edit, Point } from "./contract/index.js";

const utf8 = new TextEncoder();

/** A change complete enough to apply or reverse without consulting a buffer. */
export type Change = {
  edit: Edit;
  removed: string;
  inserted: string;
};

/**
 * Shift `offset` from this edit's pre-image into its post-image.
 *
 * - `offset <= startByte` stays unchanged.
 * - `offset >= oldEndByte` becomes `(offset - oldEndByte) + newEndByte`.
 * - Otherwise the offset was in removed text and collapses to `startByte`.
 */
export function shift(edit: Edit, offset: number): number {
  if (offset <= edit.startByte) {
    return offset;
  }
  if (offset >= edit.oldEndByte) {
    return offset - edit.oldEndByte + edit.newEndByte;
  }
  return edit.startByte;
}

export function invertEdit(edit: Edit): Edit {
  return {
    startByte: edit.startByte,
    oldEndByte: edit.newEndByte,
    newEndByte: edit.oldEndByte,
    startPoint: edit.startPoint,
    oldEndPoint: edit.newEndPoint,
    newEndPoint: edit.oldEndPoint,
  };
}

export function invertChange(change: Change): Change {
  return {
    edit: invertEdit(change.edit),
    removed: change.inserted,
    inserted: change.removed,
  };
}

export function isNoopChange(change: Change): boolean {
  return change.removed === change.inserted;
}

/**
 * Where `start` ends up after `text` is inserted there.
 *
 * When `text` contains a newline, the column is measured from the last `\n`,
 * not `start.col + len`. Lengths are UTF-8 bytes.
 */
export function advance(start: Point, text: string): Point {
  if (!text.includes("\n")) {
    return { row: start.row, col: start.col + utf8Len(text) };
  }
  return advanceBytes(start, utf8.encode(text));
}

export function advanceBytes(start: Point, text: Uint8Array): Point {
  let lastNl = -1;
  let nlCount = 0;
  for (let i = 0; i < text.length; i++) {
    if (text[i] === 0x0a) {
      lastNl = i;
      nlCount += 1;
    }
  }
  if (lastNl === -1) {
    return { row: start.row, col: start.col + text.length };
  }
  return { row: start.row + nlCount, col: text.length - lastNl - 1 };
}
