// Layer tests, for the seams the 411 editor cases cannot see: key notation
// errors, buffer geometry, history bracketing, and the row index's pending
// shift — which no small fixture would ever stress.

import { strict as assert } from 'node:assert';
import { test } from 'node:test';

import { Editor, TextBuffer, Document, keys, key, render, vim, L_NORMAL } from '../src/index.js';
import { KeyError } from '../src/keys.js';
import * as M from '../src/motion.js';

test('aliases parse to their canonical keys', () => {
  for (const [spec, expected] of [
    ['<Esc>', '<Esc>'],
    ['<Enter>', '<CR>'],
    ['<Return>', '<CR>'],
    ['<Space>', '<Space>'],
    ['<lt>', '<lt>'],
    ['<gt>', '>'],
    ['<F12>', '<F12>'],
    ['<C-->', '<C-->'],
    ['<C-A-x>', '<C-M-x>'],
  ]) {
    assert.equal(key(spec), expected, spec);
  }
});

test('shift normalises characters but not key codes', () => {
  assert.equal(key('<S-a>'), 'A');
  assert.equal(key('A'), 'A');
  assert.equal(key('<S-Tab>'), '<S-Tab>');
});

test('malformed notation reports parse errors', () => {
  assert.throws(() => keys('<C-d'), KeyError);
  assert.throws(() => keys('<Nope>'), KeyError);
  assert.throws(() => keys('<'), KeyError);
});

test('a key sequence round-trips through its rendering', () => {
  for (const spec of ['2dw', 'cwSELECT<Esc>', '<C-r>u', 'qa~jq200@a', '/needle<CR>n', 'ds(']) {
    assert.equal(render(keys(spec)), spec, spec);
  }
});

test('rows are counted by LF only', () => {
  const buffer = new TextBuffer('a\rb\nc');
  assert.equal(buffer.rowCount, 2);
  assert.equal(buffer.textIn(buffer.rowStart(0), buffer.rowContentEnd(0)), 'a\rb');
  assert.deepEqual(buffer.pointAt(4), { row: 1, col: 0 });
});

test('crlf is preserved as a single terminator', () => {
  const buffer = new TextBuffer('a\r\nb');
  assert.equal(buffer.rowStart(0), 0);
  assert.equal(buffer.rowEnd(0), 3);
  assert.equal(buffer.rowContentEnd(0), 1);
  assert.equal(buffer.toString(), 'a\r\nb');
});

test('an empty buffer has one empty row', () => {
  const buffer = new TextBuffer();
  assert.equal(buffer.length, 0);
  assert.equal(buffer.rowCount, 1);
  assert.equal(buffer.rowContentEnd(0), 0);
  assert.deepEqual(buffer.pointAt(0), { row: 0, col: 0 });
});

test('byte offsets are UTF-8, and columns are byte offsets within a row', () => {
  const buffer = new TextBuffer('-- café\nx');
  assert.equal(buffer.length, 10);
  // `é` occupies bytes 6 and 7, so the position after it is column 8, not 7 —
  // the whole point of byte columns.
  assert.deepEqual(buffer.pointAt(8), { row: 0, col: 8 });
  assert.deepEqual(buffer.pointAt(9), { row: 1, col: 0 });
  assert.equal(buffer.charAt(6), 'é'.codePointAt(0));
  assert.equal(buffer.nextChar(6), 8);
  assert.equal(buffer.prevChar(8), 6);
});

test('nested groups are one undo step', () => {
  const document = new Document('');
  document.beginGroup(0);
  document.beginGroup(0);
  document.replace(0, 0, 'a');
  document.endGroup(1);
  document.replace(1, 1, 'b');
  document.endGroup(2);
  assert.equal(document.undoDepth, 1);
  document.undo();
  assert.equal(document.buffer.toString(), '');
});

test('a new change truncates the redo tail', () => {
  const document = new Document('a');
  document.replace(1, 1, 'b');
  document.undo();
  assert.equal(document.redoDepth, 1);
  document.replace(1, 1, 'c');
  assert.equal(document.redoDepth, 0);
  assert.equal(document.buffer.toString(), 'ac');
});

test('a limit discards the oldest steps', () => {
  const document = new Document('');
  document.setLimit(2);
  for (const [at, text] of [[0, 'a'], [1, 'b'], [2, 'c']]) {
    document.replace(at, at, /** @type {string} */ (text));
  }
  assert.equal(document.undoDepth, 2);
  document.undo();
  document.undo();
  assert.equal(document.buffer.toString(), 'a');
  assert.equal(document.undo().changes.length, 0);
});

/**
 * The row index answers from a pending shift rather than rewriting every entry
 * after an edit. This walks it against a from-scratch recomputation, which is
 * the only way to catch a pivot that drifts.
 */
test('the row index survives edits in any order', () => {
  let state = 12345;
  const next = () => {
    state = (state * 1103515245 + 12345) >>> 0;
    return state / 0x100000000;
  };
  const document = new Document('one\ntwo\nthree\nfour\nfive\n');
  for (let step = 0; step < 400; step += 1) {
    const buffer = document.buffer;
    const at = Math.floor(next() * (buffer.length + 1));
    const end = Math.min(at + Math.floor(next() * 4), buffer.length);
    const text = ['', 'x', '\n', 'ab\ncd', '\n\n', 'héllo\n'][Math.floor(next() * 6)];
    // Only edit on character boundaries, as every caller above this layer does.
    const start = buffer.rowStart(buffer.rowOf(at));
    document.replace(start, Math.max(start, buffer.rowStart(buffer.rowOf(end))), text);

    const expected = [0];
    const whole = buffer.toString();
    for (let i = 0; i < whole.length; i += 1) {
      if (whole[i] === '\n' && i + 1 <= whole.length) expected.push(Buffer.byteLength(whole.slice(0, i + 1)));
    }
    const actual = [];
    for (let row = 0; row < buffer.rowCount; row += 1) actual.push(buffer.rowStart(row));
    assert.deepEqual(actual, expected, `after step ${step}`);
    assert.equal(buffer.rowCount, expected.length);
    for (let row = 0; row < buffer.rowCount; row += 1) {
      assert.equal(buffer.rowOf(buffer.rowStart(row)), row, `row ${row} after step ${step}`);
    }
  }
});

test('the gap grows without losing text', () => {
  const document = new Document('');
  let expected = '';
  for (let i = 0; i < 500; i += 1) {
    const text = i % 7 === 0 ? '\n' : String.fromCharCode(97 + (i % 26));
    document.replace(0, 0, text);
    expected = text + expected;
    if (i % 50 === 0) assert.equal(document.buffer.toString(), expected);
  }
  assert.equal(document.buffer.toString(), expected);
  assert.equal(document.buffer.length, expected.length);
});

test('the register decodes only when asked, and yanks whole rows linewise', () => {
  const editor = new Editor('one\ntwo\nthree');
  editor.typeKeys('yy');
  assert.equal(editor.register.linewise, true);
  assert.equal(editor.register.text, 'one\n');
  editor.typeKeys('Gyy');
  // The final row has no terminator; a linewise yank still ends in one.
  assert.equal(editor.register.text, 'three\n');
});

test('a rebound key changes behaviour end to end', () => {
  const keymap = vim();
  keymap.bind(L_NORMAL, 'j', { b: 2, motion: { k: M.UP } });
  keymap.bind(L_NORMAL, 'k', { b: 2, motion: { k: M.DOWN } });
  const editor = new Editor('one\ntwo\nthree', keymap);
  editor.typeKeys('k');
  assert.deepEqual(editor.cursorPoint(), { row: 1, col: 0 });
  editor.typeKeys('gg');
  editor.typeKeys('dk');
  assert.equal(editor.text(), 'three');
});

test('host jumps clamp to a grapheme, and the list is capped', () => {
  const editor = new Editor('a🇦🇺');
  for (let i = 0; i <= 100; i += 1) editor.jumpTo(Number.MAX_SAFE_INTEGER);
  assert.equal(editor.cursor, 1);
  assert.equal(editor.jumps.length, 100);
  assert.ok(editor.jumps.every((jump) => jump === 1));
  editor.setText('new');
  assert.equal(editor.jumps.length, 0);
  assert.equal(editor.mark('a'), null);
});

test('effects carry edits in tree-sitter shape', () => {
  const editor = new Editor('select id, name\nfrom users');
  const effects = editor.typeKeys('cwSELECT<Esc>');
  assert.equal(editor.text(), 'SELECT id, name\nfrom users');
  const edits = effects.filter((effect) => effect.type === 'edit');
  assert.ok(edits.length > 0);
  for (const effect of edits) {
    const edit = /** @type {{ type: 'edit', edit: any }} */ (effect).edit;
    assert.ok(edit.startByte <= edit.oldEndByte);
    assert.deepEqual(Object.keys(edit).sort(), [
      'newEndByte',
      'newEndPoint',
      'oldEndByte',
      'oldEndPoint',
      'startByte',
      'startPoint',
    ]);
  }
});
