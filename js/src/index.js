// vici — a headless vi editing core.
//
// Modes, motions, operators, counts, text objects, visual mode, undo,
// dot-repeat, macros, marks and surround, with no view attached. The whole
// interface is one function: `handleKey(key) -> Effect[]`. Cursor, mode and
// selection are queryable, so they are not duplicated into effects; effects
// are only the things the core genuinely cannot do itself.

export { Editor } from './editor.js';
export { TextBuffer, shift, invertEdit } from './buffer.js';
export { Document } from './document.js';
export { Keymap, vim, L_NORMAL, L_OPERATOR, L_VISUAL, L_INSERT } from './keymap.js';
export { keys, key, render, keyText, KeyError } from './keys.js';
export {
  NORMAL,
  INSERT,
  REPLACE,
  VISUAL,
  VISUAL_LINE,
  DELETE,
  CHANGE,
  YANK,
  SHIFT_RIGHT,
  SHIFT_LEFT,
  LOWER,
  UPPER,
  SWAP,
  HALF_PAGE_DOWN,
  HALF_PAGE_UP,
  PAGE_DOWN,
  PAGE_UP,
  CENTER,
  TOP,
  BOTTOM,
} from './command.js';

import { Editor } from './editor.js';

/**
 * @param {string} [text]
 * @returns {Editor}
 */
export function createEditor(text = '') {
  return new Editor(text);
}
