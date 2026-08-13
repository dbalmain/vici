// The pending-input parser: vi's command grammar.
//
// A trie alone cannot express vi's normal-mode syntax, because counts and
// operators *compose* rather than sequence:
//
//     [count] operator [count] motion | textobject
//     [count] command
//
// So this type holds the compositional state — counts, a pending operator, a
// pending object scope, a pending character argument — and defers the rest to
// the keymap. It touches no buffer, which makes the whole grammar testable as
// keys in, command out.

import * as C from './command.js';
import * as M from './motion.js';
import { L_INSERT, L_OPERATOR, PREFIX, UNBOUND, layerOf } from './keymap.js';
import { keyDigit, keyText } from './keys.js';

/** Resolutions. */
export const PENDING = 0;
export const COMMAND = 1;
export const REJECTED = 2;
export const CANCELLED = 3;

/** @typedef {import('./keys.js').Key} Key */
/** @typedef {{ r: number, command?: any, count?: number | null, keys?: Key[] }} Resolution */

const PENDING_RESULT = { r: PENDING };

export class Pending {
  constructor() {
    /** @type {Key[]} */
    this.keys = [];
    /** Start of the partial path through the current layer's bindings. */
    this.pathStart = 0;
    /** @type {number | null} */
    this.countBefore = null;
    /** @type {number | null} */
    this.countAfter = null;
    /** @type {number | null} */
    this.operator = null;
    /** @type {boolean | null} `i` or `a`, awaiting an object key. */
    this.scope = null;
    /** @type {any} */
    this.awaiting = null;
    /** @type {{ pattern: string, backward: boolean } | null} */
    this.search = null;
  }

  /** Nothing accumulated — the next key starts a fresh command. @returns {boolean} */
  get idle() {
    return (
      this.pathStart === this.keys.length &&
      this.countBefore === null &&
      this.countAfter === null &&
      this.operator === null &&
      this.scope === null &&
      this.awaiting === null &&
      this.search === null
    );
  }

  reset() {
    this.keys = [];
    this.pathStart = 0;
    this.countBefore = null;
    this.countAfter = null;
    this.operator = null;
    this.scope = null;
    this.awaiting = null;
    this.search = null;
  }

  /**
   * Feed one key. `mode` selects the grammar: command modes get counts and
   * operators, insert and replace get a straight keymap lookup with unbound
   * printable keys falling through to text.
   * @param {Key} key
   * @param {number} mode
   * @param {import('./keymap.js').Keymap} keymap
   * @returns {Resolution}
   */
  feed(key, mode, keymap) {
    if (!C.isCommandMode(mode)) {
      this.keys.push(key);
      return this.#insert(key, keymap);
    }

    // `<Esc>` abandons a partial sequence. When nothing is pending it falls
    // through to the keymap, where it means "return to normal mode".
    const idle = this.idle;
    const fresh = this.pathStart === this.keys.length;
    this.keys.push(key);
    if (key === '<Esc>' && !idle) return this.#end(CANCELLED);

    if (this.search !== null) return this.#feedSearch(key);

    if (this.awaiting !== null) {
      const text = keyText(key);
      return text === null ? this.#end(REJECTED) : this.#await(this.awaiting, text);
    }

    if (this.scope !== null) {
      const object = keymap.object(key);
      return object === undefined ? this.#end(REJECTED) : this.#object(this.scope, object);
    }

    // Counts accumulate only at the start of a key path, and a leading `0` is
    // the first-column motion rather than a digit — the classic ambiguity.
    if (fresh) {
      const digit = keyDigit(key);
      if (digit >= 0) {
        const before = this.operator === null;
        const slot = before ? this.countBefore : this.countAfter;
        if (!(digit === 0 && slot === null)) {
          const value = (slot ?? 0) * 10 + digit;
          if (before) this.countBefore = value;
          else this.countAfter = value;
          this.pathStart = this.keys.length;
          return PENDING_RESULT;
        }
      }
    }

    const layer = this.operator !== null ? L_OPERATOR : layerOf(mode);
    const found = keymap.walk(layer, this.keys, this.pathStart);
    if (found === PREFIX) return PENDING_RESULT;
    if (found === UNBOUND) return this.#end(REJECTED);
    return this.#apply(/** @type {import('./keymap.js').Binding} */ (found), mode);
  }

  /**
   * @param {Key} key
   * @param {import('./keymap.js').Keymap} keymap
   * @returns {Resolution}
   */
  #insert(key, keymap) {
    const walked = keymap.walk(L_INSERT, this.keys, this.pathStart);
    if (walked === PREFIX) return PENDING_RESULT;
    if (walked !== UNBOUND) {
      // Operators and object scopes are meaningless while inserting.
      const found = /** @type {import('./keymap.js').Binding} */ (walked);
      return found.b === C.B_COMMAND ? this.#finish(found.command) : this.#end(REJECTED);
    }
    // An unbound printable key is text.
    const text = this.keys.length - this.pathStart === 1 ? keyText(key) : null;
    return text === null ? this.#end(REJECTED) : this.#finish({ c: C.INSERT_TEXT, text });
  }

  /**
   * @param {import('./keymap.js').Binding} binding
   * @param {number} mode
   * @returns {Resolution}
   */
  #apply(binding, mode) {
    this.pathStart = this.keys.length;
    switch (binding.b) {
      case C.B_COMMAND:
        // An operator needs a target; a plain command is not one.
        return this.operator === null ? this.#finish(binding.command) : this.#end(REJECTED);
      case C.B_MOTION:
        return this.#finish(this.#withMotion(binding.motion));
      case C.B_OPERATOR: {
        // In visual mode an operator applies to the selection at once.
        if (C.isVisual(mode)) {
          return this.#finish({
            c: C.OPERATE,
            operator: binding.operator,
            target: SELECTION,
          });
        }
        if (this.operator === null) {
          this.operator = binding.operator;
          return PENDING_RESULT;
        }
        // A doubled operator is linewise: `dd`, `cc`, `yy`.
        if (this.operator === binding.operator) {
          return this.#finish({ c: C.OPERATE, operator: binding.operator, target: CURRENT_ROW });
        }
        return this.#end(REJECTED);
      }
      case C.B_SCOPE:
        if (this.operator === null && !C.isVisual(mode)) return this.#end(REJECTED);
        this.scope = binding.around;
        return PENDING_RESULT;
      case C.B_AWAIT:
        if (
          binding.await === C.AWAIT_SURROUND_TARGET &&
          this.operator !== C.CHANGE &&
          this.operator !== C.DELETE
        ) {
          // `Yank` is where `ys` can hook in when its target grammar is built.
          return this.#end(REJECTED);
        }
        this.awaiting = binding;
        return PENDING_RESULT;
      default:
        this.search = { pattern: '', backward: binding.backward };
        return PENDING_RESULT;
    }
  }

  /**
   * @param {Key} key
   * @returns {Resolution}
   */
  #feedSearch(key) {
    const search = /** @type {{ pattern: string, backward: boolean }} */ (this.search);
    if (key === '<CR>') {
      if (search.pattern === '') return this.#end(REJECTED);
      this.search = null;
      return this.#finish(
        this.#withMotion({ k: M.SEARCH, pattern: search.pattern, backward: search.backward }),
      );
    }
    if (key === '<BS>') {
      if (search.pattern === '') return this.#end(CANCELLED);
      search.pattern = search.pattern.slice(0, -1);
      // `keys` doubles as showcmd. Keep its displayed pattern in step with
      // editing; replaying this normalised spelling has the same result as
      // replaying the literal backspace.
      this.keys.length -= 2;
      return PENDING_RESULT;
    }
    const text = keyText(key);
    if (text === null) return this.#end(REJECTED);
    search.pattern += text;
    return PENDING_RESULT;
  }

  /**
   * @param {any} awaiting
   * @param {string} ch
   * @returns {Resolution}
   */
  #await(awaiting, ch) {
    this.awaiting = null;
    const free = this.operator === null;
    switch (awaiting.await) {
      case C.AWAIT_FIND:
        return this.#finish(
          this.#withMotion({ k: M.FIND, target: ch, backward: awaiting.backward, till: awaiting.till }),
        );
      // These take no target, so a pending operator is a syntax error.
      case C.AWAIT_REPLACE_CHAR:
        return free ? this.#finish({ c: C.REPLACE_CHAR, text: ch }) : this.#end(REJECTED);
      case C.AWAIT_RECORD:
        return free ? this.#finish({ c: C.RECORD_MACRO, register: ch }) : this.#end(REJECTED);
      case C.AWAIT_PLAY:
        return free ? this.#finish({ c: C.PLAY_MACRO, register: ch }) : this.#end(REJECTED);
      case C.AWAIT_SET_MARK:
        return free && ch >= 'a' && ch <= 'z'
          ? this.#finish({ c: C.SET_MARK, name: ch })
          : this.#end(REJECTED);
      // `'` and `` ` `` name the position before the latest jump. They go
      // through the motion path like any other mark, so `d''` is linewise and
      // ``d`` `` characterwise, as `d'a` and ``d`a`` are.
      case C.AWAIT_GOTO_MARK:
        return (ch >= 'a' && ch <= 'z') || MARK_NAMES.includes(ch)
          ? this.#finish(this.#withMotion({ k: M.MARK, name: ch, exact: awaiting.exact }))
          : this.#end(REJECTED);
      case C.AWAIT_SURROUND_TARGET:
        if (this.operator === C.DELETE) return this.#finish({ c: C.DELETE_SURROUND, target: ch });
        if (this.operator === C.CHANGE) {
          this.awaiting = { await: C.AWAIT_SURROUND_TO, from: ch };
          return PENDING_RESULT;
        }
        return this.#end(REJECTED);
      case C.AWAIT_SURROUND_TO:
        return this.operator === C.CHANGE
          ? this.#finish({ c: C.CHANGE_SURROUND, from: awaiting.from, to: ch })
          : this.#end(REJECTED);
      default:
        return free ? this.#finish({ c: C.SURROUND_SELECTION, delimiter: ch }) : this.#end(REJECTED);
    }
  }

  /**
   * @param {boolean} around
   * @param {import('./motion.js').TextObject} object
   * @returns {Resolution}
   */
  #object(around, object) {
    this.scope = null;
    return this.#finish(
      this.operator === null
        ? { c: C.SELECT_OBJECT, around, object }
        : { c: C.OPERATE, operator: this.operator, target: { t: C.T_OBJECT, around, object } },
    );
  }

  /**
   * A motion is a movement on its own, or an operator's target.
   * @param {import('./motion.js').Motion} motion
   * @returns {object}
   */
  #withMotion(motion) {
    return this.operator === null
      ? { c: C.MOVE, motion }
      : { c: C.OPERATE, operator: this.operator, target: { t: C.T_MOTION, motion } };
  }

  /**
   * vi multiplies the two counts: `2d3w` deletes six words.
   * @returns {number | null}
   */
  #count() {
    if (this.countBefore === null && this.countAfter === null) return null;
    return (this.countBefore ?? 1) * (this.countAfter ?? 1);
  }

  /**
   * @param {object} command
   * @returns {Resolution}
   */
  #finish(command) {
    const count = this.#count();
    const keys = this.keys;
    this.reset();
    return { r: COMMAND, command, count, keys };
  }

  /**
   * @param {number} r
   * @returns {Resolution}
   */
  #end(r) {
    const keys = this.keys;
    this.reset();
    return { r, keys };
  }
}

const SELECTION = { t: C.T_SELECTION };
const CURRENT_ROW = { t: C.T_CURRENT_ROW };
const MARK_NAMES = ['<', '>', '[', ']', '^', "'", '`'];
