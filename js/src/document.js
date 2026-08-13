// A linear undo stack over a stream of self-inverting changes, and the minimal
// composition of it with a buffer.
//
// Nothing here knows what the buffer's storage is. The only currency is a
// `Change`, which carries the text it displaced and is therefore self-
// inverting: undoing one is applying its inverse. No document snapshots.

import { TextBuffer, invertChange } from './buffer.js';

/** @typedef {import('./buffer.js').Change} Change */
/** @typedef {import('./buffer.js').Edit} Edit */
/** @typedef {{ changes: Change[], cursor: number | null }} Step */

/**
 * One undo step: the changes it applied, bracketed by the caret on either side.
 * @typedef {{ changes: Change[], before: number | null, after: number | null }} Group
 */

/** A buffer paired with a linear undo history. */
export class Document {
  /** @param {string} [text] */
  constructor(text = '') {
    this.buffer = new TextBuffer(text);
    /** @type {Group[]} `groups[..cursor]` are applied; `groups[cursor..]` are the redo tail. */
    this.groups = [];
    this.at = 0;
    this.depth = 0;
    /** @type {Change[]} */
    this.open = [];
    /** @type {number | null} Caret at the moment the outermost open group began. */
    this.openFrom = null;
    /** @type {number | null} */
    this.limit = null;
  }

  /** Number of steps `undo` can take. @returns {number} */
  get undoDepth() {
    return this.at;
  }

  /** Number of steps `redo` can take. @returns {number} */
  get redoDepth() {
    return this.groups.length - this.at;
  }

  /**
   * Replace `[start, end)` with `text`, recording it before applying so the
   * history sees the pre-image.
   * @param {number} start
   * @param {number} end
   * @param {string} text
   * @returns {Edit}
   */
  replace(start, end, text) {
    const change = this.buffer.stage(start, end, text);
    this.#record(change);
    this.buffer.apply(change);
    return change.edit;
  }

  /**
   * Observe a change about to be applied.
   * @param {Change} change
   */
  #record(change) {
    if (change.removed === change.inserted) return;
    if (this.depth > 0) {
      this.groups.length = this.at;
      this.open.push(change);
    } else {
      // Ungrouped, so there is no caret to bracket it with.
      this.#push({ changes: [change], before: null, after: null });
    }
  }

  /**
   * Open a group. Changes through the matching `endGroup` undo as one step.
   * `cursor` is the caret the group starts from, restored by undo. Only the
   * outermost group's value is kept, so nesting lets an inner bracket be a
   * no-op.
   * @param {number | null} cursor
   */
  beginGroup(cursor) {
    if (this.depth === 0) this.openFrom = cursor;
    this.depth += 1;
  }

  /**
   * Close the innermost open group. `cursor` is the caret it ends at, restored
   * by redo.
   * @param {number | null} cursor
   */
  endGroup(cursor) {
    this.depth = Math.max(this.depth - 1, 0);
    if (this.depth > 0) return;
    const before = this.openFrom;
    this.openFrom = null;
    if (this.open.length > 0) {
      this.#push({ changes: this.open, before, after: cursor });
      this.open = [];
    }
  }

  /**
   * Changes that undo the most recent step, or an empty step when exhausted.
   * @returns {Step}
   */
  undo() {
    if (this.at === 0) return EMPTY_STEP;
    this.at -= 1;
    const group = this.groups[this.at];
    // Reverse order: a group applied c1, c2, c3 is undone by inv(c3), inv(c2),
    // inv(c1).
    const changes = group.changes.map(invertChange).reverse();
    for (const change of changes) this.buffer.apply(change);
    return { changes, cursor: group.before };
  }

  /**
   * Changes that reapply the most recently undone step.
   * @returns {Step}
   */
  redo() {
    if (this.at >= this.groups.length) return EMPTY_STEP;
    const group = this.groups[this.at];
    this.at += 1;
    for (const change of group.changes) this.buffer.apply(change);
    return { changes: group.changes, cursor: group.after };
  }

  /**
   * @param {Group} group
   */
  #push(group) {
    this.groups.length = this.at;
    this.groups.push(group);
    this.at = this.groups.length;
    if (this.limit !== null && this.groups.length > this.limit) {
      const excess = this.groups.length - this.limit;
      this.groups.splice(0, excess);
      this.at = Math.max(this.at - excess, 0);
    }
  }

  /**
   * Keep at most `limit` undo steps, discarding the oldest immediately.
   * @param {number | null} limit
   */
  setLimit(limit) {
    this.limit = limit;
    if (limit !== null && this.groups.length > limit) {
      const excess = this.groups.length - limit;
      this.groups.splice(0, excess);
      this.at = Math.max(this.at - excess, 0);
    }
  }
}

/** @type {Step} */
const EMPTY_STEP = { changes: [], cursor: null };
