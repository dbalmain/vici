// Linear undo stack of self-inverting changes. Grouping matches
// `/home/dave/w/vici/crates/vici/src/history.rs`.

import { invertChange, isNoopChange, type Change } from "./edit.js";

export type Step = {
  changes: Change[];
  cursor?: number;
};

type Group = {
  changes: Change[];
  before?: number;
  after?: number;
};

export class History {
  /** `groups[..cursor]` are applied; `groups[cursor..]` is the redo tail. */
  #groups: Group[] = [];
  #cursor = 0;
  #depth = 0;
  #open: Change[] = [];
  #openFrom?: number;
  #limit?: number;

  setLimit(limit?: number): void {
    this.#limit = limit;
    this.#trim();
  }

  undoDepth(): number {
    return this.#cursor;
  }

  redoDepth(): number {
    return this.#groups.length - this.#cursor;
  }

  record(change: Change): void {
    if (isNoopChange(change)) {
      return;
    }
    if (this.#depth > 0) {
      this.#groups.length = this.#cursor;
      this.#open.push(change);
    } else {
      this.#pushGroup({ changes: [change] });
    }
  }

  beginGroup(cursor?: number): void {
    if (this.#depth === 0) {
      this.#openFrom = cursor;
    }
    this.#depth += 1;
  }

  endGroup(cursor?: number): void {
    this.#depth = Math.max(0, this.#depth - 1);
    if (this.#depth > 0) {
      return;
    }
    const before = this.#openFrom;
    this.#openFrom = undefined;
    if (this.#open.length === 0) {
      return;
    }
    const changes = this.#open;
    this.#open = [];
    this.#pushGroup({ changes, before, after: cursor });
  }

  undo(): Step {
    if (this.#cursor === 0) {
      return { changes: [] };
    }
    this.#cursor -= 1;
    const group = this.#groups[this.#cursor]!;
    const changes = group.changes.map(invertChange).reverse();
    return step(changes, group.before);
  }

  redo(): Step {
    if (this.#cursor >= this.#groups.length) {
      return { changes: [] };
    }
    const group = this.#groups[this.#cursor]!;
    this.#cursor += 1;
    return step(group.changes.slice(), group.after);
  }

  #pushGroup(group: Group): void {
    this.#groups.length = this.#cursor;
    this.#groups.push(group);
    this.#cursor = this.#groups.length;
    this.#trim();
  }

  #trim(): void {
    const limit = this.#limit;
    if (limit === undefined || this.#groups.length <= limit) {
      return;
    }
    const excess = this.#groups.length - limit;
    this.#groups.splice(0, excess);
    this.#cursor = Math.max(0, this.#cursor - excess);
  }
}

function step(changes: Change[], cursor?: number): Step {
  return cursor === undefined ? { changes } : { changes, cursor };
}
