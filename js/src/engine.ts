// Single-dispatcher Engine. Matches vici outcomes for 4e: case operators,
// `.` key-replay, macros, marks / jumps, and nvim-surround-shaped cs/ds/S.
// Operator-pending is parser state, not a mode.

import type {
  Edit,
  Effect,
  Engine,
  Indent,
  Key,
  Mode,
  Point,
  Scroll,
  Viewport,
} from "./contract/index.js";
import { Mods } from "./contract/index.js";

import { JsBuffer } from "./buffer-js.js";
import { recase, swapCase } from "./case.js";
import { shift } from "./edit.js";
import { Document } from "./document.js";
import { asDigit, asText, keys, render } from "./key.js";
import type { BufferFactory } from "./text-buffer.js";
import { utf8Len } from "./utf8.js";
import {
  STICKY_END,
  clamp,
  delimiters,
  findOf,
  graphemeCol,
  isInclusive,
  isLinewise,
  objectSpan,
  resolve,
  rowSpan,
  searchOf,
  spanContentRange,
  spanDeleteRange,
  spanHome,
  spanIsLinewise,
  type Bound,
  type Find,
  type LastSearch,
  type Motion,
  type ObjectScope,
  type Span,
  type TextObject,
} from "./motion.js";

const DEFAULT_INDENT: Indent = {
  shiftWidth: 4,
  tabWidth: 8,
  useTabs: false,
};

const MAX_JUMPS = 100;
const MAX_REPLAY_DEPTH = 64;

type Operator =
  | "delete"
  | "change"
  | "yank"
  | "shiftRight"
  | "shiftLeft"
  | "lower"
  | "upper"
  | "swapCase";

type Target =
  | { type: "motion"; motion: Motion }
  | { type: "object"; scope: ObjectScope; object: TextObject }
  | { type: "currentRow" }
  | { type: "selection" };

type Cmd =
  | { type: "move"; motion: Motion }
  | { type: "operate"; operator: Operator; target: Target }
  | { type: "selectObject"; scope: ObjectScope; object: TextObject }
  | { type: "enterInsert"; at: InsertAt }
  | { type: "enterReplace" }
  | { type: "enterVisual"; kind: "Char" | "Line" }
  | { type: "enterNormal" }
  | { type: "deleteChar"; before: boolean }
  | { type: "replaceChar"; char: string }
  | { type: "joinRows" }
  | { type: "put"; before: boolean }
  | { type: "swapCase" }
  | { type: "undo" }
  | { type: "redo" }
  | { type: "repeat" }
  | { type: "recordMacro"; register: string }
  | { type: "playMacro"; register: string }
  | { type: "setMark"; name: string }
  | { type: "changeSurround"; from: string; to: string }
  | { type: "deleteSurround"; target: string }
  | { type: "surroundSelection"; delimiter: string }
  | { type: "scroll"; scroll: Scroll }
  | { type: "jumpBack" }
  | { type: "jumpForward" }
  | { type: "commandPrompt" };

type Awaiting =
  | "g"
  | "z"
  | "replace"
  | "recordMacro"
  | "playMacro"
  | "setMark"
  | { kind: "find"; backward: boolean; till: boolean }
  | { kind: "object"; scope: ObjectScope }
  | { kind: "surroundTarget" }
  | { kind: "surroundTo"; from: string }
  | { kind: "surroundSelection" }
  | { kind: "gotoMark"; exact: boolean };

export class JsEngine implements Engine {
  #doc: Document;
  #indent: Indent = { ...DEFAULT_INDENT };
  #viewport: Viewport = { topRow: 0, height: 0 };
  #mode: Mode = "Normal";
  #cursor = 0;
  #sticky = 0;
  #anchor: number | undefined;
  #register = { text: "", linewise: false };
  #marks = new Map<string, number>();
  #jumps: number[] = [];
  #jumpAt = 0;
  #pending: Key[] = [];
  #countBefore: number | undefined;
  #countAfter: number | undefined;
  #operator: Operator | undefined;
  #awaiting: Awaiting | undefined;
  #search: { backward: boolean; pattern: string } | undefined;
  #lastFind: Find | undefined;
  #lastSearch: LastSearch | undefined;
  #lastChange: Key[] = [];
  #changeKeys: Key[] | null = null;
  #visualKeys: Key[] = [];
  #recording: { register: string; script: Key[] } | null = null;
  #macros = new Map<string, Key[]>();
  #replayDepth = 0;
  #insertGroup = false;
  #makeBuffer: BufferFactory;

  constructor(text = "", makeBuffer: BufferFactory = jsBuffer) {
    this.#makeBuffer = makeBuffer;
    this.#doc = new Document(text, makeBuffer);
  }

  static fromText(text: string): JsEngine {
    return new JsEngine(text);
  }

  typeKeys(spec: string): Effect[] {
    const effects: Effect[] = [];
    for (const key of keys(spec)) {
      effects.push(...this.handleKey(key));
    }
    return effects;
  }

  handleKey(key: Key): Effect[] {
    // A bare `q` stops recording before the parser can treat it as
    // "await a register". Closing `q` is not recorded.
    if (
      this.#replayDepth === 0 &&
      this.#mode === "Normal" &&
      this.#isIdle() &&
      asText(key) === "q" &&
      this.#recording !== null
    ) {
      const register = this.#recording.register;
      this.#macros.set(register, this.#recording.script);
      this.#recording = null;
      return [{ type: "RecordingStopped", register }];
    }
    if (this.#replayDepth === 0 && this.#recording !== null) {
      this.#recording.script.push(key);
    }
    if (this.#changeKeys !== null) {
      this.#changeKeys.push(key);
    }
    if (this.#mode === "Insert" || this.#mode === "Replace") {
      return this.#handleInsert(key);
    }
    return this.#handleCommand(key);
  }

  setText(text: string): void {
    this.#doc = new Document(text, this.#makeBuffer);
    this.#cursor = 0;
    this.#sticky = 0;
    this.#anchor = undefined;
    this.#mode = "Normal";
    this.#register = { text: "", linewise: false };
    this.#marks.clear();
    this.#jumps = [];
    this.#jumpAt = 0;
    this.#resetPending();
    this.#lastFind = undefined;
    this.#lastSearch = undefined;
    this.#lastChange = [];
    this.#changeKeys = null;
    this.#visualKeys = [];
    this.#recording = null;
    this.#insertGroup = false;
  }

  setIndent(indent: Indent): void {
    this.#indent = indent;
  }

  setViewport(viewport: Viewport): void {
    this.#viewport = viewport;
  }

  text(): string {
    return this.#doc.toString();
  }

  cursor(): number {
    return this.#cursor;
  }

  cursorPoint(): Point {
    return this.#buffer().byteToPoint(this.#cursor);
  }

  mode(): Mode {
    return this.#mode;
  }

  selection(): { start: number; end: number } | null {
    if (this.#anchor === undefined) {
      return null;
    }
    const buffer = this.#buffer();
    if (this.#mode === "Visual(Char)") {
      const start = Math.min(this.#anchor, this.#cursor);
      const end = Math.max(this.#anchor, this.#cursor);
      return {
        start,
        end: resolve(buffer, end, "Right", undefined, 0, "PastEnd") ?? end,
      };
    }
    if (this.#mode === "Visual(Line)") {
      const first = buffer.byteToPoint(Math.min(this.#anchor, this.#cursor)).row;
      const last = buffer.byteToPoint(Math.max(this.#anchor, this.#cursor)).row;
      return rowSpan(buffer, first, last);
    }
    return null;
  }

  register(): { text: string; linewise: boolean } {
    return { text: this.#register.text, linewise: this.#register.linewise };
  }

  undoDepth(): number {
    return this.#doc.undoDepth();
  }

  redoDepth(): number {
    return this.#doc.redoDepth();
  }

  jumps(): number[] {
    return this.#jumps.slice();
  }

  marks(): { name: string; offset: number }[] {
    return [...this.#marks.entries()].map(([name, offset]) => ({
      name,
      offset,
    }));
  }

  pending(): string {
    return render(this.#pending);
  }

  lastChange(): string {
    return render(this.#lastChange);
  }

  recording(): string | null {
    return this.#recording?.register ?? null;
  }

  #buffer() {
    return this.#doc.buffer;
  }

  #bound(): Bound {
    return this.#mode === "Insert" || this.#mode === "Replace"
      ? "PastEnd"
      : "OnChar";
  }

  #isVisual(): boolean {
    return this.#mode === "Visual(Char)" || this.#mode === "Visual(Line)";
  }

  #handleInsert(key: Key): Effect[] {
    if (isCode(key, "Esc") || isCtrl(key, "c")) {
      return this.#finish({ type: "enterNormal" }, [key]);
    }
    if (isCode(key, "Enter")) {
      return this.#execute({ type: "insertNewline" }, key);
    }
    if (isCode(key, "Backspace")) {
      return this.#execute({ type: "deleteBack" }, key);
    }
    if (isCtrl(key, "w")) {
      return this.#execute({ type: "deleteWordBack" }, key);
    }
    if (isCode(key, "Tab")) {
      return this.#execute({ type: "insertText", text: "\t" }, key);
    }
    const motion = insertMotion(key);
    if (motion !== undefined) {
      return this.#execute({ type: "insertMove", motion }, key);
    }
    const ch = asText(key);
    if (ch !== undefined) {
      return this.#execute({ type: "insertText", text: ch }, key);
    }
    return [{ type: "Bell" }];
  }

  #handleCommand(key: Key): Effect[] {
    if (isCode(key, "Esc") && !this.#isIdle()) {
      this.#resetPending();
      return [];
    }

    if (this.#search !== undefined) {
      this.#pending.push(key);
      return this.#feedSearch(key);
    }

    if (this.#awaiting === "replace") {
      this.#pending.push(key);
      const ch = asText(key);
      if (ch === undefined) {
        return this.#reject();
      }
      return this.#finish({ type: "replaceChar", char: ch });
    }

    if (this.#awaiting === "recordMacro") {
      this.#pending.push(key);
      const ch = asText(key);
      if (ch === undefined) {
        return this.#reject();
      }
      return this.#finish({ type: "recordMacro", register: ch });
    }

    if (this.#awaiting === "playMacro") {
      this.#pending.push(key);
      const ch = asText(key);
      if (ch === undefined) {
        return this.#reject();
      }
      return this.#finish({ type: "playMacro", register: ch });
    }

    if (this.#awaiting === "setMark") {
      this.#pending.push(key);
      const ch = asText(key);
      if (ch === undefined || !isAsciiLower(ch)) {
        return this.#reject();
      }
      return this.#finish({ type: "setMark", name: ch });
    }

    if (typeof this.#awaiting === "object" && this.#awaiting.kind === "gotoMark") {
      this.#pending.push(key);
      const ch = asText(key);
      const exact = this.#awaiting.exact;
      if (ch === undefined || !isGotoMarkName(ch)) {
        return this.#reject();
      }
      return this.#finishMotion({ type: "Mark", name: ch, exact });
    }

    if (
      typeof this.#awaiting === "object" &&
      this.#awaiting.kind === "surroundTarget"
    ) {
      this.#pending.push(key);
      const ch = asText(key);
      if (ch === undefined) {
        return this.#reject();
      }
      if (this.#operator === "delete") {
        return this.#finish({ type: "deleteSurround", target: ch });
      }
      if (this.#operator === "change") {
        this.#awaiting = { kind: "surroundTo", from: ch };
        return [];
      }
      return this.#reject();
    }

    if (
      typeof this.#awaiting === "object" &&
      this.#awaiting.kind === "surroundTo"
    ) {
      this.#pending.push(key);
      const ch = asText(key);
      if (ch === undefined) {
        return this.#reject();
      }
      return this.#finish({
        type: "changeSurround",
        from: this.#awaiting.from,
        to: ch,
      });
    }

    if (
      typeof this.#awaiting === "object" &&
      this.#awaiting.kind === "surroundSelection"
    ) {
      this.#pending.push(key);
      const ch = asText(key);
      if (ch === undefined) {
        return this.#reject();
      }
      return this.#finish({ type: "surroundSelection", delimiter: ch });
    }

    if (this.#awaiting === "g") {
      this.#pending.push(key);
      this.#awaiting = undefined;
      if (asText(key) === "g") {
        return this.#finishMotion("GotoFirstRow");
      }
      const caseOp = caseOperatorOf(asText(key));
      if (caseOp !== undefined) {
        return this.#applyOperator(caseOp);
      }
      return this.#reject();
    }

    if (this.#awaiting === "z") {
      this.#pending.push(key);
      const ch = asText(key);
      if (ch === "z") {
        return this.#finish({ type: "scroll", scroll: "Center" });
      }
      if (ch === "t") {
        return this.#finish({ type: "scroll", scroll: "Top" });
      }
      if (ch === "b") {
        return this.#finish({ type: "scroll", scroll: "Bottom" });
      }
      return this.#reject();
    }

    if (typeof this.#awaiting === "object" && this.#awaiting.kind === "find") {
      this.#pending.push(key);
      const ch = asText(key);
      if (ch === undefined) {
        return this.#reject();
      }
      const find = this.#awaiting;
      return this.#finishMotion({
        type: "Find",
        target: ch,
        backward: find.backward,
        till: find.till,
      });
    }

    if (typeof this.#awaiting === "object" && this.#awaiting.kind === "object") {
      this.#pending.push(key);
      const object = textObjectOf(key);
      if (object === undefined) {
        return this.#reject();
      }
      const scope = this.#awaiting.scope;
      if (this.#operator !== undefined) {
        return this.#finish({
          type: "operate",
          operator: this.#operator,
          target: { type: "object", scope, object },
        });
      }
      return this.#finish({ type: "selectObject", scope, object });
    }

    const digit = asDigit(key);
    if (digit !== undefined && !(digit === 0 && this.#countSlot() === undefined)) {
      this.#pending.push(key);
      this.#addDigit(digit);
      return [];
    }

    this.#pending.push(key);

    const op = operatorOf(key);
    if (op !== undefined) {
      return this.#applyOperator(op);
    }

    const ch = asText(key);
    if (this.#isVisual() && ch === "x") {
      return this.#finish({
        type: "operate",
        operator: "delete",
        target: { type: "selection" },
      });
    }
    if (this.#isVisual() && ch === "s") {
      return this.#applyOperator("change");
    }
    if (this.#isVisual() && ch === "S") {
      this.#awaiting = { kind: "surroundSelection" };
      return [];
    }
    const caseOp = caseOperatorOf(ch);
    if (caseOp !== undefined && (this.#operator !== undefined || this.#isVisual())) {
      return this.#applyOperator(caseOp);
    }
    if (ch === "s" && this.#operator !== undefined) {
      if (this.#operator === "change" || this.#operator === "delete") {
        this.#awaiting = { kind: "surroundTarget" };
        return [];
      }
      return this.#reject();
    }

    if (ch === "g") {
      this.#awaiting = "g";
      return [];
    }

    if (ch === "'") {
      this.#awaiting = { kind: "gotoMark", exact: false };
      return [];
    }
    if (ch === "`") {
      this.#awaiting = { kind: "gotoMark", exact: true };
      return [];
    }

    if (
      (this.#operator !== undefined || this.#isVisual()) &&
      (ch === "i" || ch === "a")
    ) {
      this.#awaiting = {
        kind: "object",
        scope: ch === "i" ? "Inner" : "Around",
      };
      return [];
    }

    const find = findOfKey(ch);
    if (find !== undefined) {
      this.#awaiting = find;
      return [];
    }

    if (ch === "/" || ch === "?") {
      this.#search = { backward: ch === "?", pattern: "" };
      return [];
    }

    const motion = commandMotion(key);
    if (motion !== undefined) {
      return this.#finishMotion(motion);
    }

    const scroll = scrollOf(key);
    if (scroll !== undefined) {
      if (this.#operator !== undefined) {
        return this.#reject();
      }
      return this.#finish({ type: "scroll", scroll });
    }

    if (ch === "z") {
      if (this.#operator !== undefined) {
        return this.#reject();
      }
      this.#awaiting = "z";
      return [];
    }

    if (this.#operator !== undefined) {
      return this.#reject();
    }

    if (ch === ".") {
      return this.#finish({ type: "repeat" });
    }
    if (ch === "q") {
      this.#awaiting = "recordMacro";
      return [];
    }
    if (ch === "@") {
      this.#awaiting = "playMacro";
      return [];
    }
    if (ch === "m") {
      this.#awaiting = "setMark";
      return [];
    }

    if (ch === "v") {
      return this.#finish({ type: "enterVisual", kind: "Char" });
    }
    if (ch === "V") {
      return this.#finish({ type: "enterVisual", kind: "Line" });
    }

    if (this.#isVisual()) {
      if (isCode(key, "Esc")) {
        return this.#finish({ type: "enterNormal" });
      }
      return this.#reject();
    }

    const insertAt = insertEntry(key);
    if (insertAt !== undefined) {
      return this.#finish({ type: "enterInsert", at: insertAt });
    }

    if (ch === "R") {
      return this.#finish({ type: "enterReplace" });
    }
    if (ch === "x" || isCode(key, "Delete")) {
      return this.#finish({ type: "deleteChar", before: false });
    }
    if (ch === "X") {
      return this.#finish({ type: "deleteChar", before: true });
    }
    if (ch === "u") {
      return this.#finish({ type: "undo" });
    }
    if (isCtrl(key, "r")) {
      return this.#finish({ type: "redo" });
    }
    if (isCtrl(key, "o")) {
      return this.#finish({ type: "jumpBack" });
    }
    if (isCtrl(key, "i")) {
      return this.#finish({ type: "jumpForward" });
    }
    if (isCode(key, "Esc")) {
      return this.#finish({ type: "enterNormal" });
    }
    if (ch === "r") {
      this.#awaiting = "replace";
      return [];
    }
    if (ch === "p") {
      return this.#finish({ type: "put", before: false });
    }
    if (ch === "P") {
      return this.#finish({ type: "put", before: true });
    }
    if (ch === "J") {
      return this.#finish({ type: "joinRows" });
    }
    if (ch === "~") {
      return this.#finish({ type: "swapCase" });
    }
    if (ch === "D") {
      return this.#finish({
        type: "operate",
        operator: "delete",
        target: { type: "motion", motion: "LastColumn" },
      });
    }
    if (ch === "C") {
      return this.#finish({
        type: "operate",
        operator: "change",
        target: { type: "motion", motion: "LastColumn" },
      });
    }
    if (ch === ":") {
      return this.#finish({ type: "commandPrompt" });
    }

    return this.#reject();
  }

  #feedSearch(key: Key): Effect[] {
    const search = this.#search;
    if (search === undefined) {
      return this.#reject();
    }
    if (isCode(key, "Enter")) {
      if (search.pattern === "") {
        return this.#reject();
      }
      return this.#finishMotion({
        type: "Search",
        pattern: search.pattern,
        backward: search.backward,
      });
    }
    if (isCode(key, "Backspace")) {
      if (search.pattern.length === 0) {
        this.#resetPending();
        return [];
      }
      search.pattern = search.pattern.slice(0, -1);
      this.#pending.pop();
      this.#pending.pop();
      return [];
    }
    const ch = asText(key);
    if (ch === undefined) {
      return this.#reject();
    }
    search.pattern += ch;
    return [];
  }

  #finishMotion(motion: Motion): Effect[] {
    if (this.#operator !== undefined) {
      return this.#finish({
        type: "operate",
        operator: this.#operator,
        target: { type: "motion", motion },
      });
    }
    return this.#finish({ type: "move", motion });
  }

  #applyOperator(op: Operator): Effect[] {
    if (this.#isVisual()) {
      return this.#finish({
        type: "operate",
        operator: op,
        target: { type: "selection" },
      });
    }
    if (this.#operator === undefined) {
      this.#operator = op;
      return [];
    }
    if (this.#operator === op) {
      return this.#finish({
        type: "operate",
        operator: op,
        target: { type: "currentRow" },
      });
    }
    return this.#reject();
  }

  #finish(cmd: Cmd, insertConsumed?: readonly Key[]): Effect[] {
    const count = this.#effectiveCount();
    const consumed = insertConsumed ?? this.#takePending();
    const wasVisual = this.#isVisual();
    const effects = this.#run(cmd, count);
    if (this.#isVisual()) {
      if (!wasVisual) {
        this.#visualKeys = [];
      }
      this.#visualKeys.push(...consumed);
    }
    this.#noteChange(cmd, consumed);
    return effects;
  }

  #execute(
    insert: InsertAction,
    key: Key,
  ): Effect[] {
    return this.#runInsert(insert, [key]);
  }

  #run(cmd: Cmd, count: number | undefined): Effect[] {
    const before = this.#cursor;
    this.#doc.history.beginGroup(before);
    const effects: Effect[] = [];
    this.#dispatch(cmd, count, effects);
    this.#rememberChange(effects);
    this.#doc.history.endGroup(this.#cursor);
    return effects;
  }

  #runInsert(action: InsertAction, _consumed: readonly Key[]): Effect[] {
    const before = this.#cursor;
    this.#doc.history.beginGroup(before);
    const effects: Effect[] = [];
    switch (action.type) {
      case "insertText":
        this.#insertText(action.text, effects);
        break;
      case "insertNewline":
        this.#insertNewline(effects);
        break;
      case "deleteBack":
        this.#deleteBack(effects);
        break;
      case "deleteWordBack":
        this.#deleteWordBack(effects);
        break;
      case "insertMove":
        this.#move(action.motion, 1, effects);
        break;
    }
    this.#rememberChange(effects);
    this.#doc.history.endGroup(this.#cursor);
    return effects;
  }

  #dispatch(cmd: Cmd, count: number | undefined, effects: Effect[]): void {
    const repeat = count ?? 1;
    switch (cmd.type) {
      case "move": {
        const motion = this.#resolveMark(cmd.motion);
        if (motion === undefined) {
          effects.push({ type: "Bell" });
          break;
        }
        this.#rememberSearch(motion);
        const landed = this.#resolveMotion(motion, count, this.#bound());
        if (landed === undefined) {
          effects.push({ type: "Bell" });
          break;
        }
        if (landed !== this.#cursor && pushesJump(motion)) {
          this.#pushJump();
        }
        this.#cursor = landed;
        this.#rememberFind(motion);
        this.#updateSticky(motion);
        break;
      }
      case "operate": {
        this.#rememberTargetFind(cmd.target);
        this.#rememberTargetSearch(cmd.target);
        const span = this.#spanOf(cmd.operator, cmd.target, count);
        if (span === undefined) {
          effects.push({ type: "Bell" });
          break;
        }
        const amount = this.#isVisual() ? repeat : 1;
        this.#operate(cmd.operator, span, amount, effects);
        break;
      }
      case "selectObject": {
        const span = objectSpan(
          this.#buffer(),
          this.#cursor,
          cmd.scope,
          cmd.object,
          repeat,
        );
        if (span === undefined) {
          effects.push({ type: "Bell" });
          break;
        }
        const range =
          span.kind === "chars"
            ? { start: span.start, end: span.end }
            : {
                start: this.#buffer().rowRange(span.first).start,
                end: this.#buffer().rowRange(span.last).end,
              };
        this.#anchor = range.start;
        this.#placeCursor(Math.max(0, range.end - 1));
        break;
      }
      case "enterInsert":
        this.#enterInsert(cmd.at, effects);
        break;
      case "enterReplace":
        this.#openInsertGroup();
        this.#setMode("Replace", effects);
        break;
      case "enterVisual":
        this.#enterVisual(cmd.kind, effects);
        break;
      case "enterNormal":
        this.#enterNormal(effects);
        break;
      case "deleteChar":
        this.#deleteChar(cmd.before, repeat, effects);
        break;
      case "replaceChar":
        this.#replaceChar(cmd.char, repeat, effects);
        break;
      case "joinRows":
        this.#joinRows(Math.max(repeat, 2), effects);
        break;
      case "put":
        this.#put(cmd.before, repeat, effects);
        break;
      case "swapCase":
        this.#swapCase(repeat, effects);
        break;
      case "undo":
        this.#undo(effects);
        break;
      case "redo":
        this.#redo(effects);
        break;
      case "repeat": {
        const script = this.#lastChange.slice();
        if (script.length === 0) {
          effects.push({ type: "Bell" });
        } else {
          effects.push(...this.#replay(script, repeat));
        }
        break;
      }
      case "recordMacro":
        this.#recording = { register: cmd.register, script: [] };
        effects.push({ type: "RecordingStarted", register: cmd.register });
        break;
      case "playMacro": {
        const script = this.#macros.get(cmd.register);
        if (script === undefined) {
          effects.push({ type: "Bell" });
        } else {
          effects.push(...this.#replay(script, repeat));
        }
        break;
      }
      case "setMark":
        this.#setMark(cmd.name, this.#cursor);
        break;
      case "changeSurround":
        this.#changeSurround(cmd.from, cmd.to, effects);
        break;
      case "deleteSurround":
        this.#deleteSurround(cmd.target, effects);
        break;
      case "surroundSelection":
        this.#surroundSelection(cmd.delimiter, effects);
        break;
      case "commandPrompt":
        effects.push({ type: "CommandPrompt" });
        break;
      case "scroll":
        this.#scroll(cmd.scroll, effects);
        break;
      case "jumpBack":
        this.#jumpBack(effects);
        break;
      case "jumpForward":
        this.#jumpForward(effects);
        break;
    }
  }

  #noteChange(cmd: Cmd, consumed: readonly Key[]): void {
    const script =
      (cmd.type === "operate" && cmd.target.type === "selection") ||
      cmd.type === "surroundSelection"
        ? this.#visualKeys.splice(0)
        : [];
    script.push(...consumed);

    switch (cmd.type) {
      case "enterInsert":
      case "enterReplace":
        this.#changeKeys = script;
        break;
      case "operate":
        if (cmd.operator === "change") {
          this.#changeKeys = script;
        } else if (
          this.#changeKeys === null &&
          (cmd.operator === "delete" ||
            cmd.operator === "lower" ||
            cmd.operator === "upper" ||
            cmd.operator === "swapCase" ||
            cmd.operator === "shiftRight" ||
            cmd.operator === "shiftLeft")
        ) {
          this.#lastChange = script;
        }
        break;
      case "enterNormal":
        if (this.#changeKeys !== null) {
          this.#lastChange = this.#changeKeys;
          this.#changeKeys = null;
        }
        break;
      case "deleteChar":
      case "replaceChar":
      case "joinRows":
      case "put":
      case "swapCase":
      case "changeSurround":
      case "deleteSurround":
      case "surroundSelection":
        if (this.#changeKeys === null) {
          this.#lastChange = script;
        }
        break;
      default:
        break;
    }
  }

  #spanOf(
    operator: Operator,
    target: Target,
    count: number | undefined,
  ): Span | undefined {
    const buffer = this.#buffer();
    let span: Span;
    switch (target.type) {
      case "motion": {
        const resolved = this.#resolveMark(target.motion);
        if (resolved === undefined) {
          return undefined;
        }
        let motion = resolved;
        if (
          operator === "change" &&
          (motion === "WordForward" || motion === "BigWordForward")
        ) {
          motion = motion === "BigWordForward" ? "BigWordEnd" : "WordEnd";
        }
        const { linewise, inclusive } = this.#motionSemantics(motion);
        const bound: Bound = inclusive ? "OnChar" : "PastEnd";
        const landed = this.#resolveMotion(motion, count, bound);
        if (landed === undefined) {
          return undefined;
        }
        if (linewise) {
          const first = buffer.byteToPoint(Math.min(this.#cursor, landed)).row;
          const last = buffer.byteToPoint(Math.max(this.#cursor, landed)).row;
          span = { kind: "lines", first, last };
        } else {
          const start = Math.min(this.#cursor, landed);
          let end = Math.max(this.#cursor, landed);
          if (inclusive) {
            end =
              resolve(buffer, end, "Right", undefined, 0, "PastEnd") ?? end;
          }
          span = { kind: "chars", start, end };
        }
        break;
      }
      case "object": {
        const found = objectSpan(
          buffer,
          this.#cursor,
          target.scope,
          target.object,
          count ?? 1,
        );
        if (found === undefined) {
          return undefined;
        }
        span = found;
        break;
      }
      case "currentRow": {
        const first = this.cursorPoint().row;
        const last = Math.min(
          first + (count ?? 1) - 1,
          buffer.lenRows() - 1,
        );
        span = { kind: "lines", first, last };
        break;
      }
      case "selection": {
        if (this.#mode === "Visual(Char)") {
          const selection = this.selection();
          if (selection === null) {
            return undefined;
          }
          span = { kind: "chars", start: selection.start, end: selection.end };
        } else if (this.#mode === "Visual(Line)" && this.#anchor !== undefined) {
          const first = buffer.byteToPoint(
            Math.min(this.#anchor, this.#cursor),
          ).row;
          const last = buffer.byteToPoint(
            Math.max(this.#anchor, this.#cursor),
          ).row;
          span = { kind: "lines", first, last };
        } else {
          return undefined;
        }
        break;
      }
    }
    if (forcesLinewise(operator) && span.kind === "chars") {
      const first = buffer.byteToPoint(span.start).row;
      const last = buffer.byteToPoint(
        Math.max(span.end - 1, span.start),
      ).row;
      return { kind: "lines", first, last };
    }
    return span;
  }

  #operate(
    operator: Operator,
    span: Span,
    amount: number,
    effects: Effect[],
  ): void {
    const wasVisual = this.#isVisual();
    if (wasVisual) {
      this.#rememberVisualSelection();
    }
    const empty =
      span.kind === "chars"
        ? span.start === span.end
        : this.#buffer().lenBytes() === 0;
    if (empty && operator !== "change" && !forcesLinewise(operator)) {
      effects.push({ type: "Bell" });
      if (this.#isVisual()) {
        this.#leaveVisual(false, effects);
      }
      return;
    }
    if (yanks(operator)) {
      this.#yank(span);
    }
    switch (operator) {
      case "shiftRight":
      case "shiftLeft": {
        if (span.kind !== "lines") {
          throw new Error("shift spans are widened to rows");
        }
        this.#shiftRows(span.first, span.last, operator, amount, effects);
        this.#cursor = this.#buffer().rowContentRange(span.first).start;
        this.#cursor = this.#step("FirstNonBlank", 1, "OnChar");
        break;
      }
      case "lower":
      case "upper":
      case "swapCase": {
        const range = spanContentRange(this.#buffer(), span);
        const start = spanHome(this.#buffer(), span);
        const linewise = spanIsLinewise(span);
        const text = this.#buffer().textIn(range.start, range.end);
        this.#edit(range.start, range.end, recase(text, operator), effects);
        this.#cursor = clamp(this.#buffer(), start, this.#bound());
        if (linewise) {
          this.#cursor = this.#step("FirstNonBlank", 1, "OnChar");
        }
        break;
      }
      case "yank": {
        const home = spanHome(this.#buffer(), span);
        this.#cursor = clamp(this.#buffer(), home, this.#bound());
        break;
      }
      case "delete": {
        const range = spanDeleteRange(this.#buffer(), span);
        const start = spanHome(this.#buffer(), span);
        const linewise = spanIsLinewise(span);
        this.#edit(range.start, range.end, "", effects);
        this.#cursor = clamp(this.#buffer(), start, this.#bound());
        if (linewise) {
          this.#cursor = this.#step("FirstNonBlank", 1, "OnChar");
        }
        break;
      }
      case "change": {
        const range = spanContentRange(this.#buffer(), span);
        this.#edit(range.start, range.end, "", effects);
        this.#cursor = range.start;
        this.#openInsertGroup();
        this.#setMode("Insert", effects);
        break;
      }
    }
    if (wasVisual && this.#isVisual()) {
      this.#leaveVisual(false, effects);
    }
    this.#sticky = graphemeCol(this.#buffer(), this.#cursor);
  }

  #shiftRows(
    first: number,
    last: number,
    operator: "shiftRight" | "shiftLeft",
    amount: number,
    effects: Effect[],
  ): void {
    const columns = Math.max(0, this.#indent.shiftWidth) * Math.max(0, amount);
    const tabWidth = Math.max(1, this.#indent.tabWidth);
    for (let row = last; row >= first; row--) {
      const content = this.#buffer().rowContentRange(row);
      if (content.start === content.end) {
        continue;
      }
      const text = this.#buffer().textIn(content.start, content.end);
      let indentLen = 0;
      while (indentLen < text.length) {
        const ch = text[indentLen];
        if (ch !== " " && ch !== "\t") {
          break;
        }
        indentLen += 1;
      }
      let old = 0;
      for (let i = 0; i < indentLen; i++) {
        if (text[i] === " ") {
          old += 1;
        } else {
          old += tabWidth - (old % tabWidth);
        }
      }
      const next =
        operator === "shiftRight"
          ? old + columns
          : Math.max(0, old - columns);
      const rendered = this.#indent.useTabs
        ? "\t".repeat(Math.floor(next / tabWidth)) + " ".repeat(next % tabWidth)
        : " ".repeat(next);
      if (rendered !== text.slice(0, indentLen)) {
        this.#edit(
          content.start,
          content.start + indentLen,
          rendered,
          effects,
        );
      }
    }
  }

  #yank(span: Span): void {
    const buffer = this.#buffer();
    if (span.kind === "chars") {
      this.#register = {
        text: buffer.textIn(span.start, span.end),
        linewise: false,
      };
      this.#setMark("[", span.start);
      this.#setMark("]", this.#previousGrapheme(span.end));
      return;
    }
    const start = buffer.rowRange(span.first).start;
    const end = buffer.rowRange(span.last).end;
    let text = buffer.textIn(start, end);
    if (!text.endsWith("\n")) {
      text += "\n";
    }
    this.#register = { text, linewise: true };
    this.#setMark("[", start);
    this.#setMark("]", this.#previousGrapheme(end));
  }

  #enterVisual(kind: "Char" | "Line", effects: Effect[]): void {
    const mode: Mode = kind === "Char" ? "Visual(Char)" : "Visual(Line)";
    if (this.#mode === mode) {
      this.#leaveVisual(true, effects);
      return;
    }
    this.#anchor = this.#cursor;
    this.#setMode(mode, effects);
  }

  #leaveVisual(rememberSelection: boolean, effects: Effect[]): void {
    if (rememberSelection) {
      this.#rememberVisualSelection();
    }
    this.#anchor = undefined;
    this.#setMode("Normal", effects);
  }

  #rememberVisualSelection(): void {
    const selection = this.selection();
    if (selection === null) {
      return;
    }
    this.#setMark("<", selection.start);
    this.#setMark(">", this.#previousGrapheme(selection.end));
  }

  #move(motion: Motion, count: number | undefined, _effects: Effect[]): void {
    const landed = this.#resolveMotion(motion, count, this.#bound());
    if (landed === undefined) {
      return;
    }
    if (landed !== this.#cursor && pushesJump(motion)) {
      this.#pushJump();
    }
    this.#cursor = landed;
    this.#updateSticky(motion);
  }

  #enterInsert(at: InsertAt, effects: Effect[]): void {
    this.#openInsertGroup();
    switch (at) {
      case "Cursor":
        break;
      case "After":
        this.#cursor = this.#step("Right", 1, "PastEnd");
        break;
      case "FirstNonBlank":
        this.#cursor = this.#step("FirstNonBlank", 1, "OnChar");
        break;
      case "EndOfRow":
        this.#cursor = this.#buffer().rowContentRange(this.cursorPoint().row).end;
        break;
      case "RowBelow": {
        const end = this.#buffer().rowContentRange(this.cursorPoint().row).end;
        this.#edit(end, end, "\n", effects);
        this.#cursor = end + 1;
        break;
      }
      case "RowAbove": {
        const start = this.#buffer().rowRange(this.cursorPoint().row).start;
        this.#edit(start, start, "\n", effects);
        this.#cursor = start;
        break;
      }
    }
    this.#setMode("Insert", effects);
    this.#sticky = graphemeCol(this.#buffer(), this.#cursor);
  }

  #enterNormal(effects: Effect[]): void {
    const leavingInsert = this.#mode === "Insert" || this.#mode === "Replace";
    this.#closeInsertGroup();
    if (leavingInsert) {
      this.#cursor = this.#step("Left", 1, "PastEnd");
      this.#setMark("^", this.#cursor);
    }
    if (this.#isVisual()) {
      this.#leaveVisual(true, effects);
    } else {
      this.#anchor = undefined;
      this.#setMode("Normal", effects);
    }
    if (leavingInsert) {
      this.#sticky = graphemeCol(this.#buffer(), this.#cursor);
    }
  }

  #insertText(text: string, effects: Effect[]): void {
    if (this.#mode === "Replace") {
      const end = this.#step("Right", 1, "PastEnd");
      this.#edit(this.#cursor, end, text, effects);
    } else {
      this.#edit(this.#cursor, this.#cursor, text, effects);
    }
    this.#cursor += utf8Len(text);
    this.#sticky = bumpSticky(this.#sticky, text) ??
      graphemeCol(this.#buffer(), this.#cursor);
  }

  #insertNewline(effects: Effect[]): void {
    this.#edit(this.#cursor, this.#cursor, "\n", effects);
    this.#cursor += 1;
    this.#sticky = 0;
  }

  #deleteBack(effects: Effect[]): void {
    const start = this.#prevPosition();
    if (start === this.#cursor) {
      effects.push({ type: "Bell" });
      return;
    }
    this.#edit(start, this.#cursor, "", effects);
    this.#cursor = start;
    this.#sticky = graphemeCol(this.#buffer(), this.#cursor);
  }

  #deleteWordBack(effects: Effect[]): void {
    const start = resolve(
      this.#buffer(),
      this.#cursor,
      "WordBackward",
      1,
      this.#sticky,
      "PastEnd",
    );
    if (start === undefined || start >= this.#cursor) {
      effects.push({ type: "Bell" });
      return;
    }
    this.#edit(start, this.#cursor, "", effects);
    this.#cursor = start;
    this.#sticky = graphemeCol(this.#buffer(), this.#cursor);
  }

  #deleteChar(before: boolean, repeat: number, effects: Effect[]): void {
    const range = before
      ? { start: this.#step("Left", repeat, "OnChar"), end: this.#cursor }
      : { start: this.#cursor, end: this.#step("Right", repeat, "PastEnd") };
    if (range.start === range.end) {
      effects.push({ type: "Bell" });
      return;
    }
    this.#yank({ kind: "chars", start: range.start, end: range.end });
    this.#edit(range.start, range.end, "", effects);
    this.#placeCursor(range.start);
  }

  #replaceChar(ch: string, repeat: number, effects: Effect[]): void {
    const end = this.#step("Right", repeat, "PastEnd");
    if (end === this.#cursor) {
      effects.push({ type: "Bell" });
      return;
    }
    this.#edit(this.#cursor, end, ch.repeat(repeat), effects);
  }

  #swapCase(repeat: number, effects: Effect[]): void {
    const end = this.#step("Right", repeat, "PastEnd");
    if (end === this.#cursor) {
      effects.push({ type: "Bell" });
      return;
    }
    const swapped = [...this.#buffer().textIn(this.#cursor, end)]
      .map(swapCase)
      .join("");
    this.#edit(this.#cursor, end, swapped, effects);
    this.#placeCursor(end);
  }

  #joinRows(rows: number, effects: Effect[]): void {
    for (let i = 1; i < Math.max(rows, 2); i++) {
      const row = this.cursorPoint().row;
      if (row + 1 >= this.#buffer().lenRows()) {
        effects.push({ type: "Bell" });
        return;
      }
      const end = this.#buffer().rowContentRange(row).end;
      const next = this.#buffer().rowRange(row + 1);
      const nextText = this.#buffer().textIn(next.start, next.end);
      const trimmed = nextText.trimStart();
      const leading = nextText.length - trimmed.length;
      const separator =
        trimmed === "" || end === this.#buffer().rowRange(row).start ? "" : " ";
      this.#edit(end, next.start + leading, separator, effects);
      this.#cursor = end;
    }
    this.#sticky = graphemeCol(this.#buffer(), this.#cursor);
  }

  #put(before: boolean, repeat: number, effects: Effect[]): void {
    if (this.#register.text === "") {
      effects.push({ type: "Bell" });
      return;
    }
    let text = this.#register.text.repeat(repeat);
    if (this.#register.linewise) {
      const row = this.cursorPoint().row;
      const rows = this.#buffer().rowRange(row);
      if (!text.endsWith("\n")) {
        text += "\n";
      }
      const lastByte = this.#buffer().lenBytes();
      const breakFirst =
        !before &&
        rows.end === lastByte &&
        lastByte > 0 &&
        this.#buffer().byte(lastByte - 1) !== 0x0a;
      let at: number;
      if (before) {
        at = rows.start;
      } else if (breakFirst) {
        at = rows.end;
        text = `\n${text.endsWith("\n") ? text.slice(0, -1) : text}`;
      } else {
        at = rows.end;
      }
      this.#edit(at, at, text, effects);
      const home = breakFirst ? at + 1 : at;
      this.#cursor = this.#stepFrom(home, "FirstNonBlank", 1, "OnChar");
    } else {
      const at = before
        ? this.#cursor
        : this.#step("Right", 1, "PastEnd");
      this.#edit(at, at, text, effects);
      this.#cursor = clamp(this.#buffer(), at + utf8Len(text) - 1, "OnChar");
    }
    this.#sticky = graphemeCol(this.#buffer(), this.#cursor);
  }

  #changeSurround(from: string, to: string, effects: Effect[]): void {
    const pair = surroundPair(to);
    if (pair === undefined) {
      effects.push({ type: "Bell" });
      return;
    }
    const found = this.#surroundOffsets(from);
    if (found === undefined) {
      effects.push({ type: "Bell" });
      return;
    }
    const { open, close, openWidth, closeWidth } = found;
    const oldPadding = this.#surroundHasPadding(open, close, openWidth);
    const closeStart = oldPadding ? close - 1 : close;
    const closeText = pair.padding ? ` ${pair.close}` : pair.close;
    this.#edit(closeStart, close + closeWidth, closeText, effects);
    const openEnd = open + openWidth + (oldPadding ? 1 : 0);
    const openText = pair.padding ? `${pair.open} ` : pair.open;
    this.#edit(open, openEnd, openText, effects);
    this.#placeCursor(open);
  }

  #deleteSurround(target: string, effects: Effect[]): void {
    const found = this.#surroundOffsets(target);
    if (found === undefined) {
      effects.push({ type: "Bell" });
      return;
    }
    const { open, close, openWidth, closeWidth } = found;
    const padding = this.#surroundHasPadding(open, close, openWidth);
    const closeStart = padding ? close - 1 : close;
    this.#edit(closeStart, close + closeWidth, "", effects);
    const openEnd = open + openWidth + (padding ? 1 : 0);
    this.#edit(open, openEnd, "", effects);
    this.#placeCursor(open);
  }

  #surroundSelection(delimiter: string, effects: Effect[]): void {
    const pair = surroundPair(delimiter);
    if (pair === undefined) {
      effects.push({ type: "Bell" });
      return;
    }
    const selection = this.selection();
    if (selection === null) {
      effects.push({ type: "Bell" });
      return;
    }
    this.#rememberVisualSelection();
    let home: number;
    if (this.#mode === "Visual(Line)") {
      const anchor = this.#anchor ?? this.#cursor;
      const buffer = this.#buffer();
      const first = buffer.byteToPoint(Math.min(anchor, this.#cursor)).row;
      const last = buffer.byteToPoint(Math.max(anchor, this.#cursor)).row;
      const start = buffer.rowRange(first).start;
      const end = buffer.rowContentRange(last).end;
      this.#edit(end, end, `\n${pair.close}`, effects);
      this.#edit(start, start, `${pair.open}\n`, effects);
      home = start;
    } else {
      const closeText = pair.padding ? ` ${pair.close}` : pair.close;
      this.#edit(selection.end, selection.end, closeText, effects);
      const openText = pair.padding ? `${pair.open} ` : pair.open;
      this.#edit(selection.start, selection.start, openText, effects);
      home = selection.start;
    }
    this.#leaveVisual(false, effects);
    this.#placeCursor(home);
  }

  #surroundOffsets(
    target: string,
  ):
    | { open: number; close: number; openWidth: number; closeWidth: number }
    | undefined {
    const object = textObjectOfChar(target);
    if (object === undefined) {
      return undefined;
    }
    let openCh: string;
    let closeCh: string;
    if (object.type === "Delimited") {
      openCh = object.open;
      closeCh = object.close;
    } else if (object.type === "Quoted") {
      openCh = object.quote;
      closeCh = object.quote;
    } else {
      return undefined;
    }
    const found = delimiters(this.#buffer(), this.#cursor, object);
    if (found === undefined) {
      return undefined;
    }
    return {
      open: found.start,
      close: found.end,
      openWidth: utf8Len(openCh),
      closeWidth: utf8Len(closeCh),
    };
  }

  #surroundHasPadding(open: number, close: number, openWidth: number): boolean {
    const innerStart = open + openWidth;
    return (
      innerStart < close - 1 &&
      this.#buffer().byte(innerStart) === 0x20 &&
      this.#buffer().byte(close - 1) === 0x20
    );
  }

  #replay(script: readonly Key[], times: number): Effect[] {
    if (this.#replayDepth >= MAX_REPLAY_DEPTH) {
      return [{ type: "Bell" }];
    }
    this.#replayDepth += 1;
    const effects: Effect[] = [];
    for (let i = 0; i < times; i++) {
      for (const key of script) {
        effects.push(...this.handleKey(key));
      }
    }
    this.#replayDepth -= 1;
    return effects;
  }

  #resolveMark(motion: Motion): Motion | undefined {
    if (typeof motion !== "object" || motion.type !== "Mark") {
      return motion;
    }
    const offset =
      motion.name === "'" || motion.name === "`"
        ? this.#jumps[this.#jumps.length - 1]
        : this.#marks.get(motion.name);
    if (offset === undefined) {
      return undefined;
    }
    return { type: "ToOffset", offset, linewise: !motion.exact };
  }

  #undo(effects: Effect[]): void {
    this.#revert(this.#doc.undo(), effects);
  }

  #redo(effects: Effect[]): void {
    this.#revert(this.#doc.redo(), effects);
  }

  #revert(
    step: { changes: { edit: Edit }[]; cursor?: number },
    effects: Effect[],
  ): void {
    if (step.changes.length === 0) {
      effects.push({ type: "Bell" });
      return;
    }
    for (const change of step.changes) {
      this.#shiftPositions(change.edit);
      effects.push({ type: "Edit", edit: change.edit });
    }
    const last = step.changes[step.changes.length - 1];
    const at = step.cursor ?? last?.edit.startByte ?? 0;
    this.#placeCursor(at);
  }

  #edit(start: number, end: number, text: string, effects: Effect[]): void {
    if (start === end && text === "") {
      return;
    }
    const edit = this.#doc.replace(start, end, text);
    this.#shiftPositions(edit);
    effects.push({ type: "Edit", edit });
  }

  #setMode(mode: Mode, effects: Effect[]): void {
    if (this.#mode === mode) {
      return;
    }
    this.#mode = mode;
    this.#cursor = clamp(this.#buffer(), this.#cursor, this.#bound());
    effects.push({ type: "ModeChanged", mode });
  }

  #resolveMotion(
    motion: Motion,
    count: number | undefined,
    bound: Bound,
  ): number | undefined {
    return resolve(
      this.#buffer(),
      this.#cursor,
      motion,
      count,
      this.#sticky,
      bound,
      this.#lastFind,
      this.#lastSearch,
      this.#viewport,
    );
  }

  #step(motion: Motion, times: number, bound: Bound): number {
    return this.#stepFrom(this.#cursor, motion, times, bound);
  }

  #stepFrom(at: number, motion: Motion, times: number, bound: Bound): number {
    return (
      resolve(
        this.#buffer(),
        at,
        motion,
        times,
        this.#sticky,
        bound,
        this.#lastFind,
        this.#lastSearch,
        this.#viewport,
      ) ?? at
    );
  }

  #motionSemantics(motion: Motion): { linewise: boolean; inclusive: boolean } {
    if (
      (motion === "RepeatFind" || motion === "RepeatFindReverse") &&
      this.#lastFind !== undefined
    ) {
      const backward =
        motion === "RepeatFindReverse"
          ? !this.#lastFind.backward
          : this.#lastFind.backward;
      return { linewise: false, inclusive: !backward };
    }
    return { linewise: isLinewise(motion), inclusive: isInclusive(motion) };
  }

  #rememberFind(motion: Motion): void {
    const find = findOf(motion);
    if (find !== undefined) {
      this.#lastFind = find;
    }
  }

  #rememberTargetFind(target: Target): void {
    if (target.type === "motion") {
      this.#rememberFind(target.motion);
    }
  }

  #rememberSearch(motion: Motion): void {
    const search = searchOf(motion);
    if (search !== undefined) {
      this.#lastSearch = search;
    }
  }

  #rememberTargetSearch(target: Target): void {
    if (target.type === "motion") {
      this.#rememberSearch(target.motion);
    }
  }

  #scroll(scroll: Scroll, effects: Effect[]): void {
    if (this.#viewport.height !== 0) {
      if (scroll === "Center" || scroll === "Top" || scroll === "Bottom") {
        effects.push({ type: "Scroll", scroll });
        return;
      }
      const motion: Motion =
        scroll === "HalfPageDown" || scroll === "PageDown" ? "Down" : "Up";
      const rows =
        scroll === "HalfPageDown" || scroll === "HalfPageUp"
          ? Math.max(1, Math.floor(this.#viewport.height / 2))
          : Math.max(1, this.#viewport.height - 2);
      const landed = this.#step(motion, rows, this.#bound());
      if (landed !== this.#cursor) {
        this.#pushJump();
        this.#cursor = landed;
      }
    }
    effects.push({ type: "Scroll", scroll });
  }

  #jumpBack(effects: Effect[]): void {
    if (this.#jumps.length === 0) {
      effects.push({ type: "Bell" });
      return;
    }
    if (this.#jumpAt === this.#jumps.length) {
      this.#pushJump();
      this.#jumpAt = this.#jumps.length - 2;
    } else if (this.#jumpAt === 0) {
      effects.push({ type: "Bell" });
      return;
    } else {
      this.#jumpAt -= 1;
    }
    this.#placeCursor(this.#jumps[this.#jumpAt] ?? 0);
  }

  #jumpForward(effects: Effect[]): void {
    const next = this.#jumpAt + 1;
    const offset = this.#jumps[next];
    if (offset === undefined) {
      effects.push({ type: "Bell" });
      return;
    }
    this.#placeCursor(offset);
    this.#jumpAt = next + 1 === this.#jumps.length ? this.#jumps.length : next;
  }

  #prevPosition(): number {
    const point = this.cursorPoint();
    if (point.col > 0) {
      return this.#step("Left", 1, "PastEnd");
    }
    if (point.row === 0) {
      return this.#cursor;
    }
    return this.#buffer().rowContentRange(point.row - 1).end;
  }

  #previousGrapheme(byte: number): number {
    const limited = Math.min(byte, this.#buffer().lenBytes());
    const point = this.#buffer().byteToPoint(limited);
    if (limited > 0 && point.col === 0) {
      return this.#buffer().rowContentRange(point.row - 1).end;
    }
    return (
      resolve(
        this.#buffer(),
        limited,
        "Left",
        1,
        this.#sticky,
        "PastEnd",
      ) ?? limited
    );
  }

  #placeCursor(byte: number): void {
    this.#cursor = clamp(this.#buffer(), byte, this.#bound());
    this.#sticky = graphemeCol(this.#buffer(), this.#cursor);
  }

  #updateSticky(motion: Motion): void {
    if (motion === "Up" || motion === "Down") {
      return;
    }
    if (motion === "LastColumn") {
      this.#sticky = STICKY_END;
      return;
    }
    this.#sticky = graphemeCol(this.#buffer(), this.#cursor);
  }

  #setMark(name: string, offset: number): void {
    this.#marks.set(name, offset);
  }

  #pushJump(): void {
    this.#jumps.length = this.#jumpAt;
    this.#jumps.push(this.#cursor);
    if (this.#jumps.length > MAX_JUMPS) {
      this.#jumps.shift();
    }
    this.#jumpAt = this.#jumps.length;
  }

  #shiftPositions(edit: Edit): void {
    for (let i = 0; i < this.#jumps.length; i++) {
      this.#jumps[i] = shift(edit, this.#jumps[i] ?? 0);
    }
    for (const [name, offset] of this.#marks) {
      this.#marks.set(name, shift(edit, offset));
    }
  }

  #rememberChange(effects: readonly Effect[]): void {
    const edits: Edit[] = [];
    for (const effect of effects) {
      if (effect.type === "Edit") {
        edits.push(effect.edit);
      }
    }
    if (edits.length === 0) {
      return;
    }
    let start = Number.POSITIVE_INFINITY;
    let end = 0;
    for (let i = 0; i < edits.length; i++) {
      const edit = edits[i]!;
      const later = edits.slice(i + 1);
      const editStart = later.reduce((offset, next) => shift(next, offset), edit.startByte);
      const editEnd = later.reduce((offset, next) => shift(next, offset), edit.newEndByte);
      start = Math.min(start, editStart);
      end = Math.max(end, editEnd);
    }
    this.#setMark("[", start);
    this.#setMark("]", this.#previousGrapheme(end));
  }

  #openInsertGroup(): void {
    if (!this.#insertGroup) {
      this.#doc.history.beginGroup(this.#cursor);
      this.#insertGroup = true;
    }
  }

  #closeInsertGroup(): void {
    if (this.#insertGroup) {
      this.#doc.history.endGroup(this.#cursor);
      this.#insertGroup = false;
    }
  }

  #countSlot(): number | undefined {
    return this.#operator === undefined ? this.#countBefore : this.#countAfter;
  }

  #addDigit(digit: number): void {
    if (this.#operator === undefined) {
      this.#countBefore = (this.#countBefore ?? 0) * 10 + digit;
    } else {
      this.#countAfter = (this.#countAfter ?? 0) * 10 + digit;
    }
  }

  #effectiveCount(): number | undefined {
    if (this.#countBefore === undefined && this.#countAfter === undefined) {
      return undefined;
    }
    return (this.#countBefore ?? 1) * (this.#countAfter ?? 1);
  }

  #isIdle(): boolean {
    return (
      this.#pending.length === 0 &&
      this.#countBefore === undefined &&
      this.#countAfter === undefined &&
      this.#operator === undefined &&
      this.#awaiting === undefined &&
      this.#search === undefined
    );
  }

  #takePending(): Key[] {
    const consumed = this.#pending;
    this.#resetPending();
    return consumed;
  }

  #resetPending(): void {
    this.#pending = [];
    this.#countBefore = undefined;
    this.#countAfter = undefined;
    this.#operator = undefined;
    this.#awaiting = undefined;
    this.#search = undefined;
  }

  #reject(): Effect[] {
    this.#resetPending();
    return [{ type: "Bell" }];
  }
}

const jsBuffer: BufferFactory = (text = "") => JsBuffer.fromText(text);

export function createEngine(text = ""): JsEngine {
  return new JsEngine(text, jsBuffer);
}

type InsertAt =
  | "Cursor"
  | "After"
  | "FirstNonBlank"
  | "EndOfRow"
  | "RowBelow"
  | "RowAbove";

type InsertAction =
  | { type: "insertText"; text: string }
  | { type: "insertNewline" }
  | { type: "deleteBack" }
  | { type: "deleteWordBack" }
  | { type: "insertMove"; motion: Motion };

function insertEntry(key: Key): InsertAt | undefined {
  switch (asText(key)) {
    case "i":
      return "Cursor";
    case "a":
      return "After";
    case "I":
      return "FirstNonBlank";
    case "A":
      return "EndOfRow";
    case "o":
      return "RowBelow";
    case "O":
      return "RowAbove";
    default:
      return undefined;
  }
}

function commandMotion(key: Key): Motion | undefined {
  const ch = asText(key);
  if (ch === "h") {
    return "Left";
  }
  if (ch === "l") {
    return "Right";
  }
  if (ch === "j") {
    return "Down";
  }
  if (ch === "k") {
    return "Up";
  }
  if (ch === "0") {
    return "FirstColumn";
  }
  if (ch === "^") {
    return "FirstNonBlank";
  }
  if (ch === "$") {
    return "LastColumn";
  }
  if (ch === "G") {
    return "GotoRow";
  }
  if (ch === "w") {
    return "WordForward";
  }
  if (ch === "W") {
    return "BigWordForward";
  }
  if (ch === "b") {
    return "WordBackward";
  }
  if (ch === "B") {
    return "BigWordBackward";
  }
  if (ch === "e") {
    return "WordEnd";
  }
  if (ch === "E") {
    return "BigWordEnd";
  }
  if (ch === "{") {
    return "ParagraphBackward";
  }
  if (ch === "}") {
    return "ParagraphForward";
  }
  if (ch === "%") {
    return "MatchPair";
  }
  if (ch === "H") {
    return "ScreenTop";
  }
  if (ch === "M") {
    return "ScreenMiddle";
  }
  if (ch === "L") {
    return "ScreenBottom";
  }
  if (ch === ";") {
    return "RepeatFind";
  }
  if (ch === ",") {
    return "RepeatFindReverse";
  }
  if (ch === "n") {
    return "RepeatSearch";
  }
  if (ch === "N") {
    return "RepeatSearchReverse";
  }
  if (ch === " ") {
    return "Right";
  }
  if (isCode(key, "Left")) {
    return "Left";
  }
  if (isCode(key, "Right")) {
    return "Right";
  }
  if (isCode(key, "Down")) {
    return "Down";
  }
  if (isCode(key, "Up")) {
    return "Up";
  }
  if (isCode(key, "Home")) {
    return "FirstColumn";
  }
  if (isCode(key, "End")) {
    return "LastColumn";
  }
  return undefined;
}

function insertMotion(key: Key): Motion | undefined {
  if (isCode(key, "Left")) {
    return "Left";
  }
  if (isCode(key, "Right")) {
    return "Right";
  }
  if (isCode(key, "Down")) {
    return "Down";
  }
  if (isCode(key, "Up")) {
    return "Up";
  }
  return undefined;
}

function operatorOf(key: Key): Operator | undefined {
  switch (asText(key)) {
    case "d":
      return "delete";
    case "c":
      return "change";
    case "y":
      return "yank";
    case ">":
      return "shiftRight";
    case "<":
      return "shiftLeft";
    default:
      return undefined;
  }
}

function yanks(operator: Operator): boolean {
  return operator === "delete" || operator === "change" || operator === "yank";
}

function forcesLinewise(operator: Operator): boolean {
  return operator === "shiftRight" || operator === "shiftLeft";
}

function pushesJump(motion: Motion): boolean {
  if (typeof motion === "object") {
    return (
      motion.type === "Search" ||
      motion.type === "ToOffset" ||
      motion.type === "Mark"
    );
  }
  return (
    motion === "GotoRow" ||
    motion === "GotoFirstRow" ||
    motion === "ParagraphForward" ||
    motion === "ParagraphBackward" ||
    motion === "MatchPair" ||
    motion === "ScreenTop" ||
    motion === "ScreenMiddle" ||
    motion === "ScreenBottom" ||
    motion === "RepeatSearch" ||
    motion === "RepeatSearchReverse"
  );
}

function findOfKey(
  ch: string | undefined,
): { kind: "find"; backward: boolean; till: boolean } | undefined {
  if (ch === "f") {
    return { kind: "find", backward: false, till: false };
  }
  if (ch === "F") {
    return { kind: "find", backward: true, till: false };
  }
  if (ch === "t") {
    return { kind: "find", backward: false, till: true };
  }
  if (ch === "T") {
    return { kind: "find", backward: true, till: true };
  }
  return undefined;
}

function textObjectOf(key: Key): TextObject | undefined {
  const ch = asText(key);
  if (ch === undefined) {
    return undefined;
  }
  switch (ch) {
    case "w":
      return { type: "Word", big: false };
    case "W":
      return { type: "Word", big: true };
    case "p":
      return { type: "Paragraph" };
    case "(":
    case ")":
    case "b":
      return { type: "Delimited", open: "(", close: ")" };
    case "{":
    case "}":
    case "B":
      return { type: "Delimited", open: "{", close: "}" };
    case "[":
    case "]":
      return { type: "Delimited", open: "[", close: "]" };
    case "<":
    case ">":
      return { type: "Delimited", open: "<", close: ">" };
    case '"':
    case "'":
    case "`":
      return { type: "Quoted", quote: ch };
    default:
      return undefined;
  }
}

function scrollOf(key: Key): Scroll | undefined {
  if (isCtrl(key, "d")) {
    return "HalfPageDown";
  }
  if (isCtrl(key, "u")) {
    return "HalfPageUp";
  }
  if (isCtrl(key, "f")) {
    return "PageDown";
  }
  if (isCtrl(key, "b")) {
    return "PageUp";
  }
  return undefined;
}

function caseOperatorOf(ch: string | undefined): Operator | undefined {
  if (ch === "u") {
    return "lower";
  }
  if (ch === "U") {
    return "upper";
  }
  if (ch === "~") {
    return "swapCase";
  }
  return undefined;
}

function isAsciiLower(ch: string): boolean {
  return ch.length === 1 && ch >= "a" && ch <= "z";
}

function isGotoMarkName(ch: string): boolean {
  return isAsciiLower(ch) || "<>[]^'`".includes(ch);
}

function surroundPair(
  delimiter: string,
): { open: string; close: string; padding: boolean } | undefined {
  switch (delimiter) {
    case "(":
      return { open: "(", close: ")", padding: true };
    case "[":
      return { open: "[", close: "]", padding: true };
    case "{":
      return { open: "{", close: "}", padding: true };
    case "<":
      return { open: "<", close: ">", padding: true };
    case ")":
      return { open: "(", close: ")", padding: false };
    case "]":
      return { open: "[", close: "]", padding: false };
    case "}":
      return { open: "{", close: "}", padding: false };
    case ">":
      return { open: "<", close: ">", padding: false };
    case '"':
    case "'":
    case "`":
      return { open: delimiter, close: delimiter, padding: false };
    default:
      return undefined;
  }
}

function textObjectOfChar(ch: string): TextObject | undefined {
  return textObjectOf({ code: { type: "Char", char: ch }, mods: Mods.NONE });
}

function isCode(key: Key, type: Key["code"]["type"]): boolean {
  return key.code.type === type && key.mods === Mods.NONE;
}

function isCtrl(key: Key, ch: string): boolean {
  return (
    key.code.type === "Char" &&
    key.code.char === ch &&
    key.mods === Mods.CTRL
  );
}

/** ASCII insert: sticky is the grapheme column and each code unit is one. */
function bumpSticky(sticky: number, text: string): number | undefined {
  if (text.length === 0 || sticky === STICKY_END) {
    return sticky === STICKY_END ? STICKY_END : undefined;
  }
  for (let i = 0; i < text.length; i++) {
    const c = text.charCodeAt(i);
    if (c > 0x7f || c === 0x0a) {
      return undefined;
    }
  }
  return sticky + text.length;
}
