//! Normalised key input, independent of any terminal or windowing system.
//!
//! Hosts translate their own events into [`Key`] — crossterm's `KeyEvent`, a
//! browser `KeyboardEvent`, whatever. This crate never sees a platform type.
//!
//! # Notation
//!
//! [`keys`] parses vi's own notation, which makes keystroke scripts writable as
//! plain strings — the foundation of the test suite, and later of dot-repeat and
//! macro storage:
//!
//! ```
//! # use vici::keys;
//! let script = keys("2dw<Esc><C-r>").unwrap();
//! assert_eq!(script.len(), 5);
//! ```

use core::fmt;
use core::ops::BitOr;

/// A physical key, with `Char` covering anything that produces text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyCode {
    Char(char),
    Esc,
    Enter,
    Tab,
    Backspace,
    Delete,
    Insert,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

/// Modifier flags.
///
/// Deliberately hand-rolled rather than pulling in `bitflags` — twenty lines
/// against a dependency in the crate's foundation.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Mods(u8);

impl Mods {
    pub const NONE: Self = Self(0);
    pub const CTRL: Self = Self(1);
    pub const ALT: Self = Self(2);
    pub const SHIFT: Self = Self(4);

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

impl BitOr for Mods {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl fmt::Debug for Mods {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("NONE");
        }
        let mut first = true;
        for (flag, name) in [
            (Self::CTRL, "CTRL"),
            (Self::ALT, "ALT"),
            (Self::SHIFT, "SHIFT"),
        ] {
            if self.contains(flag) {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

/// A key press.
///
/// # Normalisation
///
/// `SHIFT` is never combined with [`KeyCode::Char`] — the character case already
/// carries it, so `D` is `Char('D')` with no modifier, never `Char('d')` plus
/// `SHIFT`. [`Key::new`] enforces this by upcasing and dropping the flag.
///
/// This matters because terminals disagree: crossterm reports `SHIFT` alongside
/// uppercase characters, browsers generally do not. Without normalisation a
/// keymap entry for `D` matches on one platform and not the other — a bug that
/// only shows up on the platform you didn't develop on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key {
    pub code: KeyCode,
    pub mods: Mods,
}

impl Key {
    #[must_use]
    pub fn new(code: KeyCode, mods: Mods) -> Self {
        match code {
            KeyCode::Char(ch) if mods.contains(Mods::SHIFT) => Self {
                code: KeyCode::Char(ch.to_ascii_uppercase()),
                mods: mods.without(Mods::SHIFT),
            },
            _ => Self { code, mods },
        }
    }

    #[must_use]
    pub fn char(ch: char) -> Self {
        Self::new(KeyCode::Char(ch), Mods::NONE)
    }

    #[must_use]
    pub fn ctrl(ch: char) -> Self {
        Self::new(KeyCode::Char(ch), Mods::CTRL)
    }

    #[must_use]
    pub fn code(code: KeyCode) -> Self {
        Self::new(code, Mods::NONE)
    }

    /// The character this key would insert as text, if any.
    ///
    /// Returns `None` for anything modified, so `<C-w>` in insert mode reaches a
    /// binding rather than inserting a `w`.
    #[must_use]
    pub fn as_text(self) -> Option<char> {
        match self.code {
            KeyCode::Char(ch) if self.mods.is_empty() => Some(ch),
            _ => None,
        }
    }

    /// The unmodified ASCII digit this key represents, if any.
    #[must_use]
    pub fn as_digit(self) -> Option<u32> {
        self.as_text().and_then(|ch| ch.to_digit(10))
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.code {
            KeyCode::Char(' ') => "Space".to_owned(),
            KeyCode::Char('<') => "lt".to_owned(),
            KeyCode::Char(ch) if self.mods.is_empty() => return f.write_char(ch),
            KeyCode::Char(ch) => ch.to_string(),
            KeyCode::Esc => "Esc".to_owned(),
            KeyCode::Enter => "CR".to_owned(),
            KeyCode::Tab => "Tab".to_owned(),
            KeyCode::Backspace => "BS".to_owned(),
            KeyCode::Delete => "Del".to_owned(),
            KeyCode::Insert => "Insert".to_owned(),
            KeyCode::Left => "Left".to_owned(),
            KeyCode::Right => "Right".to_owned(),
            KeyCode::Up => "Up".to_owned(),
            KeyCode::Down => "Down".to_owned(),
            KeyCode::Home => "Home".to_owned(),
            KeyCode::End => "End".to_owned(),
            KeyCode::PageUp => "PageUp".to_owned(),
            KeyCode::PageDown => "PageDown".to_owned(),
            KeyCode::F(n) => format!("F{n}"),
        };
        f.write_char('<')?;
        if self.mods.contains(Mods::CTRL) {
            f.write_str("C-")?;
        }
        if self.mods.contains(Mods::ALT) {
            f.write_str("M-")?;
        }
        if self.mods.contains(Mods::SHIFT) {
            f.write_str("S-")?;
        }
        f.write_str(&name)?;
        f.write_char('>')
    }
}

/// Why a key sequence could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// A `<` was opened but never closed.
    UnterminatedBracket,
    /// The text between `<` and `>` was not a recognised key name.
    UnknownKey(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnterminatedBracket => f.write_str("unterminated `<` in key sequence"),
            Self::UnknownKey(name) => write!(f, "unknown key name `<{name}>`"),
        }
    }
}

impl core::error::Error for ParseError {}

/// Parse a key sequence in vi notation.
///
/// Bare characters map to themselves; `<...>` forms name special keys and
/// modifiers (`<C-d>`, `<M-x>`, `<S-Tab>`, `<Esc>`, `<CR>`, `<Space>`, `<lt>`,
/// `<F5>`).
pub fn keys(spec: &str) -> Result<Vec<Key>, ParseError> {
    let mut out = Vec::new();
    let mut chars = spec.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '<' {
            out.push(Key::char(ch));
            continue;
        }
        let mut name = String::new();
        loop {
            match chars.next() {
                None => return Err(ParseError::UnterminatedBracket),
                Some('>') => break,
                Some(c) => name.push(c),
            }
        }
        out.push(parse_bracketed(&name)?);
    }
    Ok(out)
}

/// Parse a single key in vi notation.
pub fn key(spec: &str) -> Result<Key, ParseError> {
    let mut parsed = keys(spec)?;
    match parsed.len() {
        1 => Ok(parsed.remove(0)),
        _ => Err(ParseError::UnknownKey(spec.to_owned())),
    }
}

fn parse_bracketed(name: &str) -> Result<Key, ParseError> {
    let mut mods = Mods::NONE;
    let mut rest = name;
    loop {
        let (flag, tail) = match rest.split_at_checked(2) {
            Some(("C-", tail)) => (Mods::CTRL, tail),
            Some(("M-" | "A-", tail)) => (Mods::ALT, tail),
            Some(("S-", tail)) => (Mods::SHIFT, tail),
            _ => break,
        };
        // A trailing `-` is the key itself, as in `<C-->`.
        if tail.is_empty() {
            break;
        }
        mods = mods | flag;
        rest = tail;
    }

    let code = match rest {
        "Esc" => KeyCode::Esc,
        "CR" | "Enter" | "Return" => KeyCode::Enter,
        "Tab" => KeyCode::Tab,
        "BS" => KeyCode::Backspace,
        "Del" => KeyCode::Delete,
        "Insert" => KeyCode::Insert,
        "Space" => KeyCode::Char(' '),
        "lt" => KeyCode::Char('<'),
        "gt" => KeyCode::Char('>'),
        "bslash" => KeyCode::Char('\\'),
        "Left" => KeyCode::Left,
        "Right" => KeyCode::Right,
        "Up" => KeyCode::Up,
        "Down" => KeyCode::Down,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        _ => {
            let mut chars = rest.chars();
            match (chars.next(), chars.next(), chars.as_str()) {
                (Some(ch), None, _) => KeyCode::Char(ch),
                (Some('F'), Some(first), tail) => {
                    let digits: String = core::iter::once(first).chain(tail.chars()).collect();
                    let n = digits
                        .parse()
                        .map_err(|_| ParseError::UnknownKey(name.to_owned()))?;
                    KeyCode::F(n)
                }
                _ => return Err(ParseError::UnknownKey(name.to_owned())),
            }
        }
    };
    Ok(Key::new(code, mods))
}

/// Render a key sequence back into vi notation.
#[must_use]
pub fn render(sequence: &[Key]) -> String {
    sequence.iter().map(ToString::to_string).collect()
}

// `write_char` lives on this trait.
use core::fmt::Write as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_parse_to_their_canonical_keys() {
        for (spec, expected) in [
            ("<Esc>", Key::code(KeyCode::Esc)),
            ("<CR>", Key::code(KeyCode::Enter)),
            ("<Space>", Key::char(' ')),
            ("<lt>", Key::char('<')),
            ("<F12>", Key::code(KeyCode::F(12))),
            ("<C-->", Key::ctrl('-')),
        ] {
            assert_eq!(key(spec), Ok(expected), "{spec}");
        }
    }

    #[test]
    fn shift_normalises_characters_but_not_key_codes() {
        assert_eq!(key("<S-a>"), Ok(Key::char('A')));
        assert_eq!(key("A"), Ok(Key::char('A')));
        assert_eq!(key("<S-Tab>"), Ok(Key::new(KeyCode::Tab, Mods::SHIFT)));
    }

    #[test]
    fn malformed_notation_reports_parse_errors() {
        assert_eq!(keys("<C-d"), Err(ParseError::UnterminatedBracket));
        assert_eq!(
            keys("<Nope>"),
            Err(ParseError::UnknownKey("Nope".to_owned()))
        );
        assert!(keys("<").is_err());
    }
}
