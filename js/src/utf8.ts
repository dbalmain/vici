// UTF-8 length and index conversion. ASCII is identity: JS index === byte.

const encoder = new TextEncoder();

export function isAscii(text: string): boolean {
  for (let i = 0; i < text.length; i++) {
    if (text.charCodeAt(i) > 0x7f) {
      return false;
    }
  }
  return true;
}

/** UTF-8 byte length. ASCII does not allocate. */
export function utf8Len(text: string): number {
  const n = text.length;
  for (let i = 0; i < n; i++) {
    if (text.charCodeAt(i) > 0x7f) {
      return encoder.encode(text).length;
    }
  }
  return n;
}

/**
 * JS string index of a UTF-8 byte offset. Mid-character offsets snap to
 * the containing scalar (the editor only hands us boundaries).
 */
export function utf8ToJs(text: string, byte: number): number {
  if (byte <= 0) {
    return 0;
  }
  if (isAscii(text)) {
    return byte > text.length ? text.length : byte;
  }
  let seen = 0;
  for (let i = 0; i < text.length; i++) {
    if (seen >= byte) {
      return i;
    }
    const c = text.charCodeAt(i);
    if (c < 0x80) {
      seen += 1;
    } else if (c < 0x800) {
      seen += 2;
    } else if (c >= 0xd800 && c <= 0xdbff) {
      seen += 4;
      i += 1;
    } else {
      // BMP non-ASCII, or a lone surrogate (TextEncoder → U+FFFD, 3 bytes).
      seen += 3;
    }
  }
  return text.length;
}

/** UTF-8 byte offset of a JS string index. */
export function jsToUtf8(text: string, js: number): number {
  if (js <= 0) {
    return 0;
  }
  if (js >= text.length) {
    return utf8Len(text);
  }
  if (isAscii(text)) {
    return js;
  }
  return utf8Len(text.slice(0, js));
}
