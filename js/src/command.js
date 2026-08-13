// The command vocabulary: what a resolved key sequence means.
//
// Everything here is plain data — small integers and frozen records, no
// closures — so a keymap can be deserialised from config later without
// redesign, and so the reducer's dispatch stays a dense switch.

/** Editing mode. Operator-pending is deliberately *not* a mode: it is parser state. */
export const NORMAL = 0;
export const INSERT = 1;
export const REPLACE = 2;
export const VISUAL = 3;
export const VISUAL_LINE = 4;

/** @param {number} mode @returns {boolean} True where the count/operator/motion grammar applies. */
export const isCommandMode = (mode) => mode === NORMAL || mode >= VISUAL;
/** @param {number} mode @returns {boolean} */
export const isVisual = (mode) => mode >= VISUAL;

/** Operators, awaiting a target. */
export const DELETE = 0;
export const CHANGE = 1;
export const YANK = 2;
export const SHIFT_RIGHT = 3;
export const SHIFT_LEFT = 4;
export const LOWER = 5;
export const UPPER = 6;
export const SWAP = 7;

/**
 * Whether an operator fills the register. Case-changing and shift operators do
 * not: `gUW` or `>>` leave whatever you last yanked alone.
 * @param {number} operator
 * @returns {boolean}
 */
export const yanks = (operator) => operator <= YANK;

/**
 * Whether an operator widens every target to complete rows. Shifting is
 * linewise even over a characterwise motion: `>w` shifts the row.
 * @param {number} operator
 * @returns {boolean}
 */
export const forcesLinewise = (operator) => operator === SHIFT_RIGHT || operator === SHIFT_LEFT;

/** What an operator acts upon. */
export const T_MOTION = 0;
export const T_OBJECT = 1;
/** A doubled operator: `dd`, `cc`, `yy`. */
export const T_CURRENT_ROW = 2;
/** The active visual selection. */
export const T_SELECTION = 3;

/** Where insert mode begins relative to the cursor. */
export const AT_CURSOR = 0;
export const AT_AFTER = 1;
export const AT_FIRST_NON_BLANK = 2;
export const AT_END_OF_ROW = 3;
export const AT_ROW_BELOW = 4;
export const AT_ROW_ABOVE = 5;

/** Viewport movements. The core does not own the viewport. */
export const HALF_PAGE_DOWN = 0;
export const HALF_PAGE_UP = 1;
export const PAGE_DOWN = 2;
export const PAGE_UP = 3;
export const CENTER = 4;
export const TOP = 5;
export const BOTTOM = 6;

/** Commands. */
export const MOVE = 0;
export const OPERATE = 1;
export const SELECT_OBJECT = 2;
export const ENTER_INSERT = 3;
export const ENTER_VISUAL = 4;
export const ENTER_REPLACE = 5;
export const ENTER_NORMAL = 6;
export const DELETE_CHAR = 7;
export const REPLACE_CHAR = 8;
export const CHANGE_SURROUND = 9;
export const DELETE_SURROUND = 10;
export const SURROUND_SELECTION = 11;
export const JOIN_ROWS = 12;
export const PUT = 13;
export const SWAP_CASE = 14;
export const UNDO = 15;
export const REDO = 16;
export const REPEAT = 17;
export const RECORD_MACRO = 18;
export const PLAY_MACRO = 19;
export const SET_MARK = 20;
export const JUMP_BACK = 21;
export const JUMP_FORWARD = 22;
export const SCROLL = 23;
export const COMMAND_PROMPT = 24;
export const INSERT_TEXT = 25;
export const INSERT_NEWLINE = 26;
export const DELETE_BACK = 27;
export const DELETE_WORD_BACK = 28;

/** Bindings that need one further key as a literal character argument. */
export const AWAIT_FIND = 0;
export const AWAIT_REPLACE_CHAR = 1;
export const AWAIT_RECORD = 2;
export const AWAIT_PLAY = 3;
export const AWAIT_SET_MARK = 4;
export const AWAIT_GOTO_MARK = 5;
export const AWAIT_SURROUND_TARGET = 6;
export const AWAIT_SURROUND_TO = 7;
export const AWAIT_SURROUND_SELECTION = 8;

/** Binding kinds, as stored in a keymap. */
export const B_COMMAND = 0;
export const B_OPERATOR = 1;
export const B_MOTION = 2;
export const B_SCOPE = 3;
export const B_AWAIT = 4;
export const B_SEARCH = 5;
