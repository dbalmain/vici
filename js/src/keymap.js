// The keymap: bindings indexed by key sequence.
//
// Layered the way vi's own `:map` commands are, because the ambiguity is real:
// `i` enters insert mode in normal mode but means *inner* while an operator is
// pending. One flat table cannot express that. Operator and visual layers fall
// back to normal, so a motion bound once is available everywhere a motion
// makes sense.
//
// Each layer is a trie of `Map`s: a `Map` value is a subtree, anything else is
// a binding. Two-key sequences are the deepest vi goes, so a probe is one or
// two hash lookups.

import * as C from './command.js';
import * as M from './motion.js';
import { keys } from './keys.js';

/** @typedef {import('./keys.js').Key} Key */
/** @typedef {{ b: number, [field: string]: any }} Binding */

/** Layers. */
export const L_NORMAL = 0;
export const L_OPERATOR = 1;
export const L_VISUAL = 2;
export const L_INSERT = 3;

/** Walk outcomes. */
export const UNBOUND = 0;
export const PREFIX = 1;

/** The layer a mode uses when no operator is pending. Replace shares insert. */
export const layerOf = (mode) =>
  mode === C.NORMAL ? L_NORMAL : mode >= C.VISUAL ? L_VISUAL : L_INSERT;

/** @param {number} k @param {object} [rest] @returns {Binding} */
const move = (motion) => ({ b: C.B_MOTION, motion });
/** @param {object} command @returns {Binding} */
const cmd = (command) => ({ b: C.B_COMMAND, command });

export class Keymap {
  constructor() {
    /** @type {Map<string, any>[]} */
    this.layers = [new Map(), new Map(), new Map(), new Map()];
    /** @type {Map<string, import('./motion.js').TextObject>} */
    this.objects = new Map();
  }

  /**
   * Bind a key sequence written in vi notation. Overwrites whatever occupied
   * that path, including a whole subtree: binding `g` discards any existing
   * `gg`.
   * @param {number} layer
   * @param {string} spec
   * @param {Binding} binding
   * @returns {this}
   */
  bind(layer, spec, binding) {
    const path = keys(spec);
    let node = this.layers[layer];
    for (let i = 0; i < path.length - 1; i += 1) {
      let next = node.get(path[i]);
      if (!(next instanceof Map)) {
        next = new Map();
        node.set(path[i], next);
      }
      node = next;
    }
    node.set(path[path.length - 1], binding);
    return this;
  }

  /**
   * Walk `path` through `layer`, applying its fallback rules.
   * @param {number} layer
   * @param {readonly Key[]} path
   * @param {number} from first index of `path` to consider
   * @returns {Binding | number} a binding, or `PREFIX` / `UNBOUND`
   */
  walk(layer, path, from) {
    let node = this.layers[layer];
    for (let i = from; i < path.length; i += 1) {
      const next = node.get(path[i]);
      if (next === undefined) {
        // A layer that produces neither a binding nor a prefix defers to its
        // fallback, so visual inherits every normal-mode motion while still
        // being able to shadow individual keys.
        return layer === L_OPERATOR || layer === L_VISUAL
          ? this.walk(L_NORMAL, path, from)
          : UNBOUND;
      }
      if (!(next instanceof Map)) return i + 1 === path.length ? next : UNBOUND;
      node = next;
    }
    return PREFIX;
  }

  /**
   * The text object a key selects after `i` or `a`.
   * @param {Key} k
   * @returns {import('./motion.js').TextObject | undefined}
   */
  object(k) {
    return this.objects.get(k);
  }
}

/**
 * The default scheme: modes, motions, operators, text objects, counts,
 * dot-repeat, macros, marks, undo, jump navigation, surround and ex prompts.
 * No named registers.
 * @returns {Keymap}
 */
export function vim() {
  const map = new Keymap();

  // -- motions, shared by every command layer --------------------------------
  const motions = [
    ['h', M.LEFT],
    ['l', M.RIGHT],
    ['<Space>', M.RIGHT],
    ['j', M.DOWN],
    ['k', M.UP],
    ['<Left>', M.LEFT],
    ['<Right>', M.RIGHT],
    ['<Down>', M.DOWN],
    ['<Up>', M.UP],
    ['0', M.FIRST_COLUMN],
    ['<Home>', M.FIRST_COLUMN],
    ['^', M.FIRST_NON_BLANK],
    ['$', M.LAST_COLUMN],
    ['<End>', M.LAST_COLUMN],
    ['{', M.PARAGRAPH],
    ['}', M.PARAGRAPH],
    ['G', M.GOTO_ROW],
    ['gg', M.GOTO_FIRST_ROW],
    ['%', M.MATCH_PAIR],
    ['H', M.SCREEN_TOP],
    ['M', M.SCREEN_MIDDLE],
    ['L', M.SCREEN_BOTTOM],
  ];
  for (const [spec, k] of motions) {
    map.bind(L_NORMAL, spec, move(spec === '{' ? { k, backward: true } : { k }));
  }
  for (const [spec, big] of [['w', false], ['W', true]]) {
    map.bind(L_NORMAL, spec, move({ k: M.WORD_FORWARD, big }));
  }
  for (const [spec, big] of [['b', false], ['B', true]]) {
    map.bind(L_NORMAL, spec, move({ k: M.WORD_BACKWARD, big }));
  }
  for (const [spec, big] of [['e', false], ['E', true]]) {
    map.bind(L_NORMAL, spec, move({ k: M.WORD_END, big }));
  }
  map.bind(L_NORMAL, ';', move({ k: M.REPEAT_FIND, reverse: false }));
  map.bind(L_NORMAL, ',', move({ k: M.REPEAT_FIND, reverse: true }));
  map.bind(L_NORMAL, 'n', move({ k: M.REPEAT_SEARCH, reverse: false }));
  map.bind(L_NORMAL, 'N', move({ k: M.REPEAT_SEARCH, reverse: true }));
  map.bind(L_NORMAL, '/', { b: C.B_SEARCH, backward: false });
  map.bind(L_NORMAL, '?', { b: C.B_SEARCH, backward: true });

  // `f`/`t` and friends need one more key before they mean anything.
  for (const [spec, backward, till] of [
    ['f', false, false],
    ['F', true, false],
    ['t', false, true],
    ['T', true, true],
  ]) {
    map.bind(L_NORMAL, spec, { b: C.B_AWAIT, await: C.AWAIT_FIND, backward, till });
  }
  map.bind(L_NORMAL, 'm', { b: C.B_AWAIT, await: C.AWAIT_SET_MARK });
  map.bind(L_NORMAL, '`', { b: C.B_AWAIT, await: C.AWAIT_GOTO_MARK, exact: true });
  map.bind(L_NORMAL, "'", { b: C.B_AWAIT, await: C.AWAIT_GOTO_MARK, exact: false });

  // -- operators -------------------------------------------------------------
  for (const [spec, operator] of [
    ['d', C.DELETE],
    ['c', C.CHANGE],
    ['y', C.YANK],
    ['>', C.SHIFT_RIGHT],
    // `<` starts bracketed key names in the notation parser, so its spelling
    // here must be `<lt>`.
    ['<lt>', C.SHIFT_LEFT],
    ['gu', C.LOWER],
    ['gU', C.UPPER],
    ['g~', C.SWAP],
  ]) {
    map.bind(L_NORMAL, spec, { b: C.B_OPERATOR, operator });
  }
  // The bare keys, so the doubled row forms work: an operator is doubled when
  // the same operator arrives twice, and vi accepts the short second half —
  // `gUU` as well as `gUgU`. With an operator pending these keys were a syntax
  // error anyway, so shadowing `u` here costs nothing.
  map.bind(L_OPERATOR, 'u', { b: C.B_OPERATOR, operator: C.LOWER });
  map.bind(L_OPERATOR, 'U', { b: C.B_OPERATOR, operator: C.UPPER });
  map.bind(L_OPERATOR, '~', { b: C.B_OPERATOR, operator: C.SWAP });

  // `D` and `C` are `d$` and `c$` pre-applied. Binding them as whole commands
  // rather than as an operator awaiting a target is what lets them take a
  // count: `2D` clears to the end of the following row, as in vi.
  for (const [spec, operator] of [['D', C.DELETE], ['C', C.CHANGE]]) {
    map.bind(
      L_NORMAL,
      spec,
      cmd({ c: C.OPERATE, operator, target: { t: C.T_MOTION, motion: { k: M.LAST_COLUMN } } }),
    );
  }

  // -- mode changes ----------------------------------------------------------
  for (const [spec, at] of [
    ['i', C.AT_CURSOR],
    ['a', C.AT_AFTER],
    ['I', C.AT_FIRST_NON_BLANK],
    ['A', C.AT_END_OF_ROW],
    ['o', C.AT_ROW_BELOW],
    ['O', C.AT_ROW_ABOVE],
  ]) {
    map.bind(L_NORMAL, spec, cmd({ c: C.ENTER_INSERT, at }));
  }
  map.bind(L_NORMAL, 'v', cmd({ c: C.ENTER_VISUAL, kind: C.VISUAL }));
  map.bind(L_NORMAL, 'V', cmd({ c: C.ENTER_VISUAL, kind: C.VISUAL_LINE }));
  map.bind(L_NORMAL, 'R', cmd({ c: C.ENTER_REPLACE }));
  map.bind(L_NORMAL, '<Esc>', cmd({ c: C.ENTER_NORMAL }));

  // -- simple edits ----------------------------------------------------------
  map.bind(L_NORMAL, 'x', cmd({ c: C.DELETE_CHAR, before: false }));
  map.bind(L_NORMAL, '<Del>', cmd({ c: C.DELETE_CHAR, before: false }));
  map.bind(L_NORMAL, 'X', cmd({ c: C.DELETE_CHAR, before: true }));
  map.bind(L_NORMAL, 'J', cmd({ c: C.JOIN_ROWS }));
  map.bind(L_NORMAL, 'p', cmd({ c: C.PUT, before: false }));
  map.bind(L_NORMAL, 'P', cmd({ c: C.PUT, before: true }));
  map.bind(L_NORMAL, '~', cmd({ c: C.SWAP_CASE }));
  map.bind(L_NORMAL, 'r', { b: C.B_AWAIT, await: C.AWAIT_REPLACE_CHAR });

  // -- history and repetition ------------------------------------------------
  map.bind(L_NORMAL, 'u', cmd({ c: C.UNDO }));
  map.bind(L_NORMAL, '<C-r>', cmd({ c: C.REDO }));
  // Normal-layer bindings. Insert-mode `<C-o>` remains deliberately unbound.
  map.bind(L_NORMAL, '<C-o>', cmd({ c: C.JUMP_BACK }));
  map.bind(L_NORMAL, '<C-i>', cmd({ c: C.JUMP_FORWARD }));
  map.bind(L_NORMAL, '.', cmd({ c: C.REPEAT }));
  map.bind(L_NORMAL, 'q', { b: C.B_AWAIT, await: C.AWAIT_RECORD });
  map.bind(L_NORMAL, '@', { b: C.B_AWAIT, await: C.AWAIT_PLAY });

  // -- viewport and prompts --------------------------------------------------
  for (const [spec, scroll] of [
    ['<C-d>', C.HALF_PAGE_DOWN],
    ['<C-u>', C.HALF_PAGE_UP],
    ['<C-f>', C.PAGE_DOWN],
    ['<C-b>', C.PAGE_UP],
    ['zz', C.CENTER],
    ['zt', C.TOP],
    ['zb', C.BOTTOM],
  ]) {
    map.bind(L_NORMAL, spec, cmd({ c: C.SCROLL, scroll }));
  }
  map.bind(L_NORMAL, ':', cmd({ c: C.COMMAND_PROMPT }));

  // -- operator-pending: `i`/`a` become object scopes ------------------------
  map.bind(L_OPERATOR, 'i', { b: C.B_SCOPE, around: false });
  map.bind(L_OPERATOR, 'a', { b: C.B_SCOPE, around: true });
  map.bind(L_OPERATOR, 's', { b: C.B_AWAIT, await: C.AWAIT_SURROUND_TARGET });

  // -- visual ----------------------------------------------------------------
  map.bind(L_VISUAL, 'i', { b: C.B_SCOPE, around: false });
  map.bind(L_VISUAL, 'a', { b: C.B_SCOPE, around: true });
  map.bind(L_VISUAL, 'x', { b: C.B_OPERATOR, operator: C.DELETE });
  map.bind(L_VISUAL, 's', { b: C.B_OPERATOR, operator: C.CHANGE });
  // Over a selection these are case changes, not undo and not a one-character
  // swap. `gu`/`gU`/`g~` reach the same operators through the normal layer.
  map.bind(L_VISUAL, 'u', { b: C.B_OPERATOR, operator: C.LOWER });
  map.bind(L_VISUAL, 'U', { b: C.B_OPERATOR, operator: C.UPPER });
  map.bind(L_VISUAL, '~', { b: C.B_OPERATOR, operator: C.SWAP });
  map.bind(L_VISUAL, 'S', { b: C.B_AWAIT, await: C.AWAIT_SURROUND_SELECTION });
  map.bind(L_VISUAL, 'v', cmd({ c: C.ENTER_VISUAL, kind: C.VISUAL }));
  map.bind(L_VISUAL, 'V', cmd({ c: C.ENTER_VISUAL, kind: C.VISUAL_LINE }));

  // -- insert ----------------------------------------------------------------
  map.bind(L_INSERT, '<Esc>', cmd({ c: C.ENTER_NORMAL }));
  map.bind(L_INSERT, '<C-c>', cmd({ c: C.ENTER_NORMAL }));
  map.bind(L_INSERT, '<CR>', cmd({ c: C.INSERT_NEWLINE }));
  map.bind(L_INSERT, '<BS>', cmd({ c: C.DELETE_BACK }));
  map.bind(L_INSERT, '<C-w>', cmd({ c: C.DELETE_WORD_BACK }));
  map.bind(L_INSERT, '<Tab>', cmd({ c: C.INSERT_TEXT, text: '\t' }));
  map.bind(L_INSERT, '<Left>', cmd({ c: C.MOVE, motion: { k: M.LEFT } }));
  map.bind(L_INSERT, '<Right>', cmd({ c: C.MOVE, motion: { k: M.RIGHT } }));
  map.bind(L_INSERT, '<Up>', cmd({ c: C.MOVE, motion: { k: M.UP } }));
  map.bind(L_INSERT, '<Down>', cmd({ c: C.MOVE, motion: { k: M.DOWN } }));

  // -- text objects ----------------------------------------------------------
  map.objects.set('w', { o: M.OBJ_WORD, big: false });
  map.objects.set('W', { o: M.OBJ_WORD, big: true });
  map.objects.set('p', { o: M.OBJ_PARAGRAPH });
  for (const [open, close, alias] of [
    ['(', ')', 'b'],
    ['{', '}', 'B'],
    ['[', ']', null],
    ['<', '>', null],
  ]) {
    const object = { o: M.OBJ_DELIMITED, open, close };
    map.objects.set(open === '<' ? '<lt>' : open, object);
    map.objects.set(close, object);
    if (alias) map.objects.set(alias, object);
  }
  for (const quote of ['"', "'", '`']) map.objects.set(quote, { o: M.OBJ_QUOTED, quote });

  return map;
}
