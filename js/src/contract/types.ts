/** Host-supplied indent policy. Matches `vici::Indent`. */
export type Indent = {
  shiftWidth: number;
  tabWidth: number;
  useTabs: boolean;
};

/** Host-supplied viewport facts. Matches `vici::Viewport`. */
export type Viewport = {
  topRow: number;
  height: number;
};

/** Buffer position. `col` is a UTF-8 byte offset into the row. */
export type Point = {
  row: number;
  col: number;
};

/** Mode as Rust `Debug` prints it — the snapshot strings, not a rename. */
export type Mode =
  | "Normal"
  | "Insert"
  | "Replace"
  | "Visual(Char)"
  | "Visual(Line)";

/** Viewport-move effect. Names match `vici::Scroll` Debug. */
export type Scroll =
  | "HalfPageDown"
  | "HalfPageUp"
  | "PageDown"
  | "PageUp"
  | "Center"
  | "Top"
  | "Bottom";

/** Tree-sitter-shaped edit geometry. Matches `vici::Edit`. */
export type Edit = {
  startByte: number;
  oldEndByte: number;
  newEndByte: number;
  startPoint: Point;
  oldEndPoint: Point;
  newEndPoint: Point;
};

/**
 * Effects the host must act on. Queryable state is not duplicated here.
 * Discriminants match the Rust `vici::Effect` variants used by `render_effect`.
 */
export type Effect =
  | { type: "Edit"; edit: Edit }
  | { type: "ModeChanged"; mode: Mode }
  | { type: "Scroll"; scroll: Scroll }
  | { type: "CommandPrompt" }
  | { type: "Bell" }
  | { type: "RecordingStarted"; register: string }
  | { type: "RecordingStopped"; register: string };

/**
 * Physical key. `Char` covers anything that produces text.
 * Matches `vici::KeyCode`.
 */
export type KeyCode =
  | { type: "Char"; char: string }
  | { type: "Esc" }
  | { type: "Enter" }
  | { type: "Tab" }
  | { type: "Backspace" }
  | { type: "Delete" }
  | { type: "Insert" }
  | { type: "Left" }
  | { type: "Right" }
  | { type: "Up" }
  | { type: "Down" }
  | { type: "Home" }
  | { type: "End" }
  | { type: "PageUp" }
  | { type: "PageDown" }
  | { type: "F"; n: number };

/**
 * Modifier flags. Hand-rolled like `vici::Mods` — CTRL=1, ALT=2, SHIFT=4.
 */
export const Mods = {
  NONE: 0,
  CTRL: 1,
  ALT: 2,
  SHIFT: 4,
} as const;

export type Mods = number;

/** A key press. Matches `vici::Key`. */
export type Key = {
  code: KeyCode;
  mods: Mods;
};

/**
 * Shared façade both engines implement. WASM does not export Buffer / Keymap
 * / Pending — only this.
 */
export interface Engine {
  /** Throws on key-notation parse error. */
  typeKeys(spec: string): Effect[];
  handleKey(key: Key): Effect[];
  setText(text: string): void;
  setIndent(indent: Indent): void;
  setViewport(viewport: Viewport): void;
  text(): string;
  /** UTF-8 byte offset. */
  cursor(): number;
  cursorPoint(): Point;
  mode(): Mode;
  selection(): { start: number; end: number } | null;
  register(): { text: string; linewise: boolean };
  undoDepth(): number;
  redoDepth(): number;
  jumps(): number[];
  /** a–z plus `< > [ ] ^`. */
  marks(): { name: string; offset: number }[];
  /** `vici::render(pending_keys)`. */
  pending(): string;
  lastChange(): string;
  recording(): string | null;
}

/** Optional `with` line on a fixture case. */
export type Settings = {
  viewport?: Viewport;
  indent?: Indent;
};

/** One `editor.vici` case after parse + unescape. */
export type Case = {
  name: string;
  text: string;
  keys: string;
  settings: Settings;
};
