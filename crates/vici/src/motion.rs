//! Resolving motions and text objects to buffer positions.
//!
//! Pure functions over a [`Buffer`]. No cursor state, no modes — the reducer owns
//! those and passes in what it has.
//!
//! # Two granularities, deliberately
//!
//! - **Graphemes** for `h`, `l`, `x`, `r`: every such motion is row-local in vi,
//!   so this module materialises the row and does grapheme arithmetic on a `&str`.
//!   Rows in an editing pane are short, and the alternative — a rope-aware
//!   incremental grapheme cursor — is a great deal of fiddly code for no
//!   observable gain here. Swappable later without touching callers.
//! - **Characters** for word motions and delimiter scanning, which cross rows.
//!   These walk ropey's bidirectional char cursor and never materialise anything.
//!
//! # Columns are grapheme counts, not display widths
//!
//! The sticky column for `j`/`k` counts graphemes. vi counts display cells, which
//! differ for tabs and CJK. That difference belongs to the view layer, which owns
//! font and tab-stop knowledge; when the view supplies a layout, this is where it
//! plugs in.

use core::ops::{Range, RangeInclusive};

use unicode_segmentation::UnicodeSegmentation;

use crate::buffer::Buffer;
use crate::command::{Motion, ObjectScope, TextObject};
use crate::host::Viewport;

/// A remembered `f`/`t`/`F`/`T` search, for `;` and `,` to repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Find {
    pub target: char,
    pub backward: bool,
    pub till: bool,
}

/// A resolved region, expressed in the granularity that produced it.
///
/// A linewise region names **rows**, not bytes. The byte range a row range
/// corresponds to depends on what is being done to it — deleting a row takes a
/// row terminator with it where rewriting the row's content must not — so the
/// conversion happens per operator, at the point of edit, and never in reverse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    /// A half-open byte range on UTF-8 boundaries.
    Chars(Range<usize>),
    /// An inclusive range of rows that exist in the buffer.
    Lines(RangeInclusive<usize>),
}

impl Span {
    #[must_use]
    pub const fn is_linewise(&self) -> bool {
        matches!(self, Self::Lines(_))
    }

    /// The bytes an operator rewrites **in place**, leaving row structure alone:
    /// from the first row's start to the last row's content end, excluding its
    /// terminator. What a linewise change or recase acts on.
    #[must_use]
    pub fn content_range(&self, buf: &Buffer) -> Range<usize> {
        match self {
            Self::Chars(range) => range.clone(),
            Self::Lines(rows) => {
                buf.row_range(*rows.start()).start..buf.row_content_range(*rows.end()).end
            }
        }
    }

    /// The bytes a delete removes, including a row terminator so that `dd`
    /// takes the row break with it — see [`row_span`] for which one.
    #[must_use]
    pub fn delete_range(&self, buf: &Buffer) -> Range<usize> {
        match self {
            Self::Chars(range) => range.clone(),
            Self::Lines(rows) => row_span(buf, *rows.start(), *rows.end()),
        }
    }

    /// Where the caret belongs once the operator is done.
    #[must_use]
    pub fn home(&self, buf: &Buffer) -> usize {
        match self {
            Self::Chars(range) => range.start,
            Self::Lines(rows) => buf.row_content_range(*rows.start()).start,
        }
    }
}

/// Where the cursor is allowed to rest.
///
/// The distinction is real vi behaviour: in normal mode the cursor sits *on* a
/// character and cannot pass the last one, but insert mode allows one position
/// beyond it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    OnChar,
    PastEnd,
}

/// Sentinel sticky column meaning "stay at the end of the row", as `$` does.
pub(crate) const STICKY_END: usize = usize::MAX;

// ---------------------------------------------------------------------------
// character stepping
// ---------------------------------------------------------------------------

fn char_at(buf: &Buffer, byte: usize) -> Option<char> {
    if byte >= buf.len_bytes() {
        None
    } else {
        buf.rope().chars_at(byte).next()
    }
}

fn advance_char(buf: &Buffer, byte: usize) -> usize {
    char_at(buf, byte).map_or(byte, |ch| byte + ch.len_utf8())
}

fn retreat_char(buf: &Buffer, byte: usize) -> usize {
    buf.rope()
        .chars_at(byte)
        .prev()
        .map_or(byte, |ch| byte - ch.len_utf8())
}

// ---------------------------------------------------------------------------
// grapheme stepping, row-local
// ---------------------------------------------------------------------------

/// Byte offsets of every grapheme boundary in `row`, absolute, including the
/// row's end.
fn boundaries(buf: &Buffer, row: usize) -> Vec<usize> {
    let range = buf.row_content_range(row);
    let text = buf.text_in(range.clone());
    let mut out: Vec<usize> = text
        .grapheme_indices(true)
        .map(|(offset, _)| range.start + offset)
        .collect();
    out.push(range.end);
    out
}

/// The highest column the cursor may occupy on `row`.
fn max_col(boundaries: &[usize], bound: Bound) -> usize {
    let last = boundaries.len() - 1;
    match bound {
        Bound::PastEnd => last,
        // One short of the row end, so the cursor rests on a character. An empty
        // row still has column 0.
        Bound::OnChar => last.saturating_sub(1),
    }
}

/// The grapheme column of `byte` within its row.
#[must_use]
pub fn grapheme_col(buf: &Buffer, byte: usize) -> usize {
    let row = buf.byte_to_point(byte).row;
    let boundaries = boundaries(buf, row);
    boundaries
        .iter()
        .rposition(|&offset| offset <= byte)
        .unwrap_or(0)
}

/// The byte offset of grapheme column `col` on `row`, clamped.
fn byte_at_col(buf: &Buffer, row: usize, col: usize, bound: Bound) -> usize {
    let boundaries = boundaries(buf, row);
    boundaries[col.min(max_col(&boundaries, bound))]
}

/// Pull `byte` back to a legal cursor position for `bound`, snapping it to a
/// grapheme boundary. This is the single guarantee that callers which compute
/// offsets in bytes cannot leave the cursor mid-character; a future public
/// `jump_to(offset)` will rely on it.
#[must_use]
pub fn clamp(buf: &Buffer, byte: usize, bound: Bound) -> usize {
    let byte = byte.min(buf.len_bytes());
    let row = buf.byte_to_point(byte).row;
    let boundaries = boundaries(buf, row);
    let boundaries = &boundaries[..=max_col(&boundaries, bound)];
    boundaries[boundaries
        .partition_point(|&offset| offset <= byte)
        .saturating_sub(1)]
}

fn next_grapheme(buf: &Buffer, byte: usize, bound: Bound) -> usize {
    let row = buf.byte_to_point(byte).row;
    byte_at_col(buf, row, grapheme_col(buf, byte) + 1, bound)
}

fn prev_grapheme(buf: &Buffer, byte: usize, bound: Bound) -> usize {
    let row = buf.byte_to_point(byte).row;
    byte_at_col(buf, row, grapheme_col(buf, byte).saturating_sub(1), bound)
}

fn first_non_blank(buf: &Buffer, row: usize) -> usize {
    let range = buf.row_content_range(row);
    let text = buf.text_in(range.clone());
    let offset = text
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map_or(0, |(i, _)| i);
    range.start + offset
}

// ---------------------------------------------------------------------------
// word classes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Blank,
    Word,
    Punct,
}

/// vi's three character classes. A `big` word (`W`, `B`, `E`) collapses word and
/// punctuation into one, so `foo.bar` is one WORD but three words.
fn class(ch: char, big: bool) -> Class {
    if ch.is_whitespace() {
        Class::Blank
    } else if big || ch.is_alphanumeric() || ch == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

fn class_at(buf: &Buffer, byte: usize, big: bool) -> Option<Class> {
    char_at(buf, byte).map(|ch| class(ch, big))
}

/// `w` / `W`: the start of the next word.
fn word_forward(buf: &Buffer, from: usize, big: bool) -> usize {
    let mut pos = from;
    if let Some(start) = class_at(buf, pos, big)
        && start != Class::Blank
    {
        while class_at(buf, pos, big) == Some(start) {
            pos = advance_char(buf, pos);
        }
    }
    while class_at(buf, pos, big) == Some(Class::Blank) {
        pos = advance_char(buf, pos);
    }
    pos
}

/// `b` / `B`: the start of this word, or of the previous one.
fn word_backward(buf: &Buffer, from: usize, big: bool) -> usize {
    let mut pos = retreat_char(buf, from);
    while pos > 0 && class_at(buf, pos, big) == Some(Class::Blank) {
        pos = retreat_char(buf, pos);
    }
    let Some(current) = class_at(buf, pos, big) else {
        return pos;
    };
    if current == Class::Blank {
        return pos;
    }
    while pos > 0 {
        let prev = retreat_char(buf, pos);
        if class_at(buf, prev, big) == Some(current) {
            pos = prev;
        } else {
            break;
        }
    }
    pos
}

/// `e` / `E`: the last character of this word, or of the next one.
fn word_end(buf: &Buffer, from: usize, big: bool) -> usize {
    let mut pos = advance_char(buf, from);
    while class_at(buf, pos, big) == Some(Class::Blank) {
        pos = advance_char(buf, pos);
    }
    let Some(current) = class_at(buf, pos, big) else {
        return retreat_char(buf, pos);
    };
    loop {
        let next = advance_char(buf, pos);
        if class_at(buf, next, big) == Some(current) {
            pos = next;
        } else {
            return pos;
        }
    }
}

/// The run of same-class characters containing `at`.
fn word_run(buf: &Buffer, at: usize, big: bool) -> Range<usize> {
    let Some(current) = class_at(buf, at, big) else {
        return at..at;
    };
    let mut start = at;
    while start > 0 {
        let prev = retreat_char(buf, start);
        if class_at(buf, prev, big) == Some(current) {
            start = prev;
        } else {
            break;
        }
    }
    let mut end = at;
    while class_at(buf, end, big) == Some(current) {
        end = advance_char(buf, end);
    }
    start..end
}

/// End of the whitespace run at `at`, stopping at the row's newline.
///
/// The newline is whitespace as far as [`class`] is concerned, but a word object
/// that swallowed it would join two rows, so it bounds every one of these walks.
fn blank_run_end(buf: &Buffer, at: usize, big: bool) -> usize {
    let mut end = at;
    while class_at(buf, end, big) == Some(Class::Blank) && char_at(buf, end) != Some('\n') {
        end = advance_char(buf, end);
    }
    end
}

/// Start of the whitespace run ending at `at`, stopping at the previous newline.
fn blank_run_start(buf: &Buffer, at: usize, big: bool) -> usize {
    let mut start = at;
    while start > 0 {
        let prev = retreat_char(buf, start);
        if class_at(buf, prev, big) == Some(Class::Blank) && char_at(buf, prev) != Some('\n') {
            start = prev;
        } else {
            break;
        }
    }
    start
}

// ---------------------------------------------------------------------------
// find, row-local
// ---------------------------------------------------------------------------

/// Row-local char step, used only to nudge a search origin.
fn next_char(text: &str, at: usize) -> usize {
    text.get(at..)
        .and_then(|rest| rest.chars().next())
        .map_or(at, |ch| at + ch.len_utf8())
}

fn prev_char(text: &str, at: usize) -> usize {
    text.get(..at)
        .and_then(|head| head.chars().next_back())
        .map_or(at, |ch| at - ch.len_utf8())
}

/// Find `find.target` within `from`'s row.
///
/// `skip_adjacent` is for `;` and `,`. A `t`/`T` parks one character short of its
/// target, so repeating it re-finds that same target and resolves to the position
/// the cursor is already on — the repeat appears to do nothing. Stepping the origin
/// one character along excludes exactly that target and nothing else, since a
/// target one step away is precisely the one whose till-position is the cursor.
/// This is vi's default behaviour, i.e. `cpoptions` without `;`.
fn find_in_row(
    buf: &Buffer,
    from: usize,
    find: Find,
    count: usize,
    skip_adjacent: bool,
) -> Option<usize> {
    let row = buf.byte_to_point(from).row;
    let range = buf.row_content_range(row);
    let text = buf.text_in(range.clone());
    let cursor = from - range.start;
    let origin = if skip_adjacent && find.till {
        if find.backward {
            prev_char(&text, cursor)
        } else {
            next_char(&text, cursor)
        }
    } else {
        cursor
    };

    let mut hit = None;
    if find.backward {
        let mut seen = 0;
        for (offset, ch) in text.char_indices().rev() {
            if offset >= origin {
                continue;
            }
            if ch == find.target {
                seen += 1;
                if seen == count {
                    hit = Some(range.start + offset);
                    break;
                }
            }
        }
    } else {
        let mut seen = 0;
        for (offset, ch) in text.char_indices() {
            if offset <= origin {
                continue;
            }
            if ch == find.target {
                seen += 1;
                if seen == count {
                    hit = Some(range.start + offset);
                    break;
                }
            }
        }
    }

    let hit = hit?;
    if !find.till {
        return Some(hit);
    }
    // `t`/`T` stop one character short of the target.
    Some(if find.backward {
        advance_char(buf, hit)
    } else {
        retreat_char(buf, hit)
    })
}

// ---------------------------------------------------------------------------
// motion resolution
// ---------------------------------------------------------------------------

/// Byte range covering rows `first..=last`, including the final row's newline
/// when there is one. This is what a linewise operator deletes.
#[must_use]
pub fn row_span(buf: &Buffer, first: usize, last: usize) -> Range<usize> {
    let last = last.min(buf.len_rows() - 1);
    let start = buf.row_range(first).start;
    let end = buf.row_range(last).end;
    // No trailing newline on the final row: take the preceding one instead, so
    // `dd` on the last row does not leave a blank.
    if end == buf.len_bytes() && first > 0 {
        return buf.row_range(first - 1).end - 1..end;
    }
    start..end
}

/// `%` — the match of the first delimiter on the row at or after the cursor.
///
/// Brackets are looked for first, so `%` does exactly what vi does wherever vi
/// does anything at all: it takes the first bracket on the row and jumps to its
/// partner, in either direction. Quotes are ours — vi rings on them — and are
/// only consulted when the row holds no bracket, so no vi behaviour is displaced.
///
/// The search is row-local, as vi's is: a bracket on the next row is not this
/// row's business.
fn match_pair(buf: &Buffer, at: usize) -> Option<usize> {
    const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];
    const QUOTES: [char; 3] = ['"', '\'', '`'];

    let end = buf.row_content_range(buf.byte_to_point(at).row).end;
    let mut pos = at;
    let mut quote = None;
    while pos < end {
        let ch = char_at(buf, pos)?;
        if let Some(&(open, close)) = PAIRS
            .iter()
            .find(|&&(open, close)| ch == open || ch == close)
        {
            let (start, finish) = enclosing_pair(buf, pos, open, close)?;
            return Some(if pos == start { finish } else { start });
        }
        if quote.is_none() && QUOTES.contains(&ch) {
            quote = Some((pos, ch));
        }
        pos = advance_char(buf, pos);
    }

    let (pos, ch) = quote?;
    let (start, finish) = enclosing_quotes(buf, pos, ch)?;
    Some(if pos == start { finish } else { start })
}

fn screen_motion(
    buf: &Buffer,
    motion: &Motion,
    repeat: usize,
    viewport: Viewport,
) -> Option<usize> {
    // A zero height means no host has reported a screen, so screen-relative
    // motions have no meaningful target rather than pretending row zero is it.
    if viewport.height == 0 {
        return None;
    }

    let last = buf.len_rows() - 1;
    let top = viewport.top_row.min(last);
    let bottom = viewport
        .top_row
        .saturating_add(viewport.height.saturating_sub(1))
        .min(last);
    let row = match motion {
        Motion::ScreenTop => viewport
            .top_row
            .saturating_add(repeat.saturating_sub(1))
            .min(last),
        Motion::ScreenMiddle => top + (bottom - top) / 2,
        Motion::ScreenBottom => bottom.saturating_sub(repeat.saturating_sub(1)),
        _ => return None,
    };
    Some(first_non_blank(buf, row))
}

/// Find the counted literal match in `direction`, wrapping at either end.
fn search(
    buf: &Buffer,
    from: usize,
    pattern: &str,
    backward: bool,
    repeat: usize,
) -> Option<usize> {
    if pattern.is_empty() {
        return None;
    }

    // Whole-buffer materialisation is acceptable for the one literal matching
    // policy this crate has today. A rope-aware matcher can replace this private
    // function if profiling ever makes that extra machinery worthwhile.
    let text = buf.to_string();
    let sensitive = pattern.chars().any(char::is_uppercase);
    let folded = (!sensitive).then(|| {
        pattern
            .chars()
            .flat_map(char::to_lowercase)
            .collect::<String>()
    });
    let matches: Vec<_> = text
        .grapheme_indices(true)
        .map(|(offset, _)| offset)
        .filter(|&offset| literal_prefix(&text[offset..], pattern, folded.as_deref()))
        .collect();
    if matches.is_empty() {
        return None;
    }

    let mut landed = from;
    for _ in 0..repeat {
        landed = if backward {
            matches
                .iter()
                .rev()
                .copied()
                .find(|&offset| offset < landed)
                .unwrap_or_else(|| *matches.last().expect("matches is not empty"))
        } else {
            matches
                .iter()
                .copied()
                .find(|&offset| offset > landed)
                .unwrap_or(matches[0])
        };
    }
    Some(landed)
}

fn literal_prefix(text: &str, pattern: &str, folded_pattern: Option<&str>) -> bool {
    let Some(folded_pattern) = folded_pattern else {
        return text.starts_with(pattern);
    };
    let mut candidate = String::new();
    for ch in text.chars() {
        candidate.extend(ch.to_lowercase());
        if candidate == folded_pattern {
            return true;
        }
        if !folded_pattern.starts_with(&candidate) {
            return false;
        }
    }
    false
}

/// Where `motion` lands, starting from `from`.
///
/// `sticky` is the remembered grapheme column for `j`/`k`.
/// `last_find` supplies the target for [`Motion::RepeatFind`].
/// `last_search` supplies the pattern and direction for [`Motion::RepeatSearch`].
/// `viewport` is the host's current screen fact for `H`/`M`/`L`.
///
/// Returns `None` when the motion cannot be performed at all.
#[must_use]
// Resolution is public and each input describes an independent part of vi state.
// Folding them into a private context object would make the small public primitive
// less direct for hosts that resolve motions themselves.
#[allow(clippy::too_many_arguments)]
pub fn resolve(
    buf: &Buffer,
    from: usize,
    motion: Motion,
    count: Option<usize>,
    sticky: usize,
    last_find: Option<Find>,
    last_search: Option<(&str, bool)>,
    viewport: Viewport,
    bound: Bound,
) -> Option<usize> {
    let repeat = count.unwrap_or(1);
    let row = buf.byte_to_point(from).row;
    let rows = buf.len_rows();

    let target = match motion {
        Motion::Left => {
            let mut pos = from;
            for _ in 0..repeat {
                pos = prev_grapheme(buf, pos, bound);
            }
            pos
        }
        Motion::Right => {
            let mut pos = from;
            for _ in 0..repeat {
                pos = next_grapheme(buf, pos, bound);
            }
            pos
        }
        Motion::Down => {
            let target_row = (row + repeat).min(rows - 1);
            byte_at_col(buf, target_row, sticky, bound)
        }
        Motion::Up => {
            let target_row = row.saturating_sub(repeat);
            byte_at_col(buf, target_row, sticky, bound)
        }
        Motion::FirstColumn => buf.row_content_range(row).start,
        Motion::FirstNonBlank => first_non_blank(buf, row),
        Motion::LastColumn => {
            let target_row = (row + repeat - 1).min(rows - 1);
            byte_at_col(buf, target_row, STICKY_END, bound)
        }
        Motion::WordForward { big } => {
            let mut pos = from;
            for _ in 0..repeat {
                pos = word_forward(buf, pos, big);
            }
            pos
        }
        Motion::WordBackward { big } => {
            let mut pos = from;
            for _ in 0..repeat {
                pos = word_backward(buf, pos, big);
            }
            pos
        }
        Motion::WordEnd { big } => {
            let mut pos = from;
            for _ in 0..repeat {
                pos = word_end(buf, pos, big);
            }
            pos
        }
        Motion::Find(find) => find_in_row(buf, from, find, repeat, false)?,
        Motion::RepeatFind { reverse } => {
            let mut find = last_find?;
            if reverse {
                find.backward = !find.backward;
            }
            find_in_row(buf, from, find, repeat, true)?
        }
        Motion::Search { pattern, backward } => search(buf, from, &pattern, backward, repeat)?,
        Motion::RepeatSearch { reverse } => {
            let (pattern, backward) = last_search?;
            search(buf, from, pattern, backward != reverse, repeat)?
        }
        // `G` and `gg` take the count as an absolute row, 1-based.
        Motion::GotoRow => {
            let target_row = count.map_or(rows - 1, |n| n.saturating_sub(1).min(rows - 1));
            first_non_blank(buf, target_row)
        }
        Motion::GotoFirstRow => {
            let target_row = count.map_or(0, |n| n.saturating_sub(1).min(rows - 1));
            first_non_blank(buf, target_row)
        }
        Motion::MatchPair => match_pair(buf, from)?,
        Motion::ScreenTop | Motion::ScreenMiddle | Motion::ScreenBottom => {
            screen_motion(buf, &motion, repeat, viewport)?
        }
        // Marks belong to Editor's navigation state. It turns them into the
        // concrete `ToOffset` vocabulary before this pure resolver is called.
        Motion::Mark { .. } => return None,
        Motion::ToOffset { offset, linewise } => {
            let offset = clamp(buf, offset, bound);
            if linewise {
                first_non_blank(buf, buf.byte_to_point(offset).row)
            } else {
                offset
            }
        }
    };
    Some(clamp(buf, target, bound))
}

// ---------------------------------------------------------------------------
// text objects
// ---------------------------------------------------------------------------

/// The span a text object covers with the cursor at `at`.
///
/// `count` means something different for each kind of object, following vi:
/// nesting levels for a delimited pair, runs of text for a word, paragraphs for a
/// paragraph. Quotes do not nest and have nothing to count, so they ignore it.
#[must_use]
pub fn object_span(
    buf: &Buffer,
    at: usize,
    scope: ObjectScope,
    object: TextObject,
    count: usize,
) -> Option<Span> {
    let count = count.max(1);
    match object {
        TextObject::Word { big } => word_object(buf, at, scope, big, count),
        TextObject::Delimited { open, close } => {
            let (start, end) = pair_at_level(buf, at, open, close, count)?;
            Some(pair_span(buf, start, end, scope))
        }
        TextObject::Quoted(quote) => {
            let (start, end) = enclosing_quotes(buf, at, quote)?;
            Some(pair_span(buf, start, end, scope))
        }
        TextObject::Paragraph => Some(paragraph_object(buf, at, scope, count)),
    }
}

/// `iw` / `aw`, with a count taking in further runs of text.
///
/// A stretch of whitespace is a run of its own, so `3iw` is word, space, word,
/// while `3aw` is three words each with the space that follows it.
fn word_object(
    buf: &Buffer,
    at: usize,
    scope: ObjectScope,
    big: bool,
    count: usize,
) -> Option<Span> {
    let run = word_run(buf, at, big);
    if run.is_empty() {
        return None;
    }
    // A word object never joins rows, so the newline ending this one is where a
    // count runs out.
    let spent = |end: usize| matches!(char_at(buf, end), None | Some('\n'));
    let range = match scope {
        ObjectScope::Inner => {
            let mut end = run.end;
            for _ in 1..count {
                if spent(end) {
                    break;
                }
                end = if class_at(buf, end, big) == Some(Class::Blank) {
                    blank_run_end(buf, end, big)
                } else {
                    word_run(buf, end, big).end
                };
            }
            run.start..end
        }
        // `aw` takes the trailing whitespace too, or the leading run when there is
        // none after.
        ObjectScope::Around => {
            let mut end = blank_run_end(buf, run.end, big);
            let trailing = end > run.end;
            for _ in 1..count {
                if spent(end) {
                    break;
                }
                end = blank_run_end(buf, word_run(buf, end, big).end, big);
            }
            let start = if trailing {
                run.start
            } else {
                blank_run_start(buf, run.start, big)
            };
            start..end
        }
    };
    Some(Span::Chars(range))
}

/// The delimiter pair `count` nesting levels from the cursor.
///
/// Which direction that counts in depends on where the cursor is, and both
/// directions are vi's. Inside a pair the count climbs *outward*: `2di{` takes the
/// pair around the pair the cursor is in. Inside none, vi seeks forward to the
/// next pair and the count descends *inward* from it, so `2di{` on a function's
/// signature row reaches a block nested inside its body. Either way the count is
/// the nesting level you asked for, counted from where you stand.
fn pair_at_level(
    buf: &Buffer,
    at: usize,
    open: char,
    close: char,
    count: usize,
) -> Option<(usize, usize)> {
    match enclosing_pair(buf, at, open, close) {
        Some(pair) => climb_out(buf, pair, open, close, count),
        None => descend_into(buf, seek_pair(buf, at, open, close)?, open, close, count),
    }
}

/// Climb `count - 1` levels out of the pair the cursor is in.
fn climb_out(
    buf: &Buffer,
    (mut start, mut end): (usize, usize),
    open: char,
    close: char,
    count: usize,
) -> Option<(usize, usize)> {
    for _ in 1..count {
        if start == 0 {
            return None;
        }
        let (outer_start, outer_end) = enclosing_pair(buf, retreat_char(buf, start), open, close)?;
        // Searching from just before this pair finds an adjacent sibling as readily
        // as an enclosing one — `(a)(b)` has no second level — so only a pair that
        // genuinely contains this one counts as a level.
        if outer_start >= start || outer_end <= end {
            return None;
        }
        (start, end) = (outer_start, outer_end);
    }
    Some((start, end))
}

/// Descend `count - 1` levels into the first pair nested inside this one.
fn descend_into(
    buf: &Buffer,
    (mut start, mut end): (usize, usize),
    open: char,
    close: char,
    count: usize,
) -> Option<(usize, usize)> {
    for _ in 1..count {
        // The *first* pair nested inside, so `2di{` on `{ a {b} c {d} e }` takes
        // `{b}`. Siblings are not levels: with nothing nested inside, there is
        // nowhere to descend to and the object fails rather than settling for the
        // pair we already have.
        let inner = next_open(buf, advance_char(buf, start), end, open, close)?;
        (start, end) = enclosing_pair(buf, inner, open, close)?;
    }
    Some((start, end))
}

/// The pair that starts next, for a cursor sitting inside none.
///
/// vi does not seek backwards, so a pair the cursor has already passed is out of
/// reach — `di{` after a `{…}` does nothing rather than reaching back for it.
fn seek_pair(buf: &Buffer, at: usize, open: char, close: char) -> Option<(usize, usize)> {
    let start = next_open(buf, at, buf.len_bytes(), open, close)?;
    enclosing_pair(buf, start, open, close)
}

/// The first `open` in `from..limit`, or `None` if a `close` turns up first.
///
/// Giving up on a `close` is what vi does, and it costs nothing here: in balanced
/// text an unmatched `close` ahead of the cursor means the cursor is inside a pair,
/// which [`enclosing_pair`] has already found. Only unbalanced text can reach it.
fn next_open(buf: &Buffer, from: usize, limit: usize, open: char, close: char) -> Option<usize> {
    let mut pos = from;
    while pos < limit {
        match char_at(buf, pos) {
            Some(ch) if ch == open => return Some(pos),
            Some(ch) if ch == close => return None,
            _ => {}
        }
        pos = advance_char(buf, pos);
    }
    None
}

/// The span between a pair of delimiters, with or without the delimiters.
fn pair_span(buf: &Buffer, start: usize, end: usize, scope: ObjectScope) -> Span {
    let range = match scope {
        ObjectScope::Inner => inner_span(buf, start, end),
        ObjectScope::Around => start..advance_char(buf, end),
    };
    Span::Chars(range)
}

/// The inside of a pair, following vi's rule for delimiters that own their rows.
///
/// When the opening delimiter is the last thing on its row, the inside starts at
/// the row below rather than at the newline behind it; when the closing delimiter
/// has nothing but indent before it, the inside ends where the row above ends. So
/// `di{` on a function body takes the body's rows and leaves the braces where they
/// were, instead of dragging them together onto one row.
///
/// Note that vi *shrinks the span* here rather than promoting the object to
/// linewise — `vi{` on such a block is still a characterwise selection. The two
/// adjustments can meet in the middle on `{\n}`, which leaves an empty span: vi
/// fails the object outright, and an empty span rings, which is the same answer.
fn inner_span(buf: &Buffer, open: usize, close: usize) -> Range<usize> {
    let after_open = advance_char(buf, open);
    let open_row = buf.byte_to_point(open).row;
    // The open ends its row, so the inside begins on the row below it.
    let starts_below =
        after_open == buf.row_content_range(open_row).end && open_row + 1 < buf.len_rows();
    let start = if starts_below {
        buf.row_range(open_row + 1).start
    } else {
        after_open
    };

    let close_row = buf.byte_to_point(close).row;
    // Only indent between the row's start and the delimiter. On a single-row pair
    // this cannot hold, since the opening delimiter is itself in the way.
    let ends_above = buf
        .text_in(buf.row_range(close_row).start..close)
        .trim()
        .is_empty();
    let end = if ends_above {
        let above = buf.row_content_range(close_row - 1).end;
        // The row break above the delimiter is the object's to take only when the
        // span already begins at a row boundary. Otherwise the front of that row
        // survives and needs its newline: `di{` on a whole block takes the body's
        // rows outright, while on `x { body` + `}` it takes ` body` and leaves the
        // two rows to close up by themselves.
        if starts_below { above + 1 } else { above }
    } else {
        close
    };

    start..end.max(start)
}

/// `ip` / `ap`, blank-row delimited and always linewise.
fn paragraph_object(buf: &Buffer, at: usize, scope: ObjectScope, count: usize) -> Span {
    let rows = buf.len_rows();
    let row = buf.byte_to_point(at).row;
    let blank = |r: usize| buf.row_text(r).trim().is_empty();
    let mut first = row;
    while first > 0 && !blank(first - 1) {
        first -= 1;
    }
    let mut last = row;
    while last + 1 < rows && !blank(last + 1) {
        last += 1;
    }
    // Counts work as they do for words, with a run of blank rows standing in for a
    // run of whitespace: `3ip` is paragraph, gap, paragraph, while `2ap` is two
    // paragraphs each with the gap that follows it.
    match scope {
        ObjectScope::Inner => {
            for _ in 1..count {
                if last + 1 >= rows {
                    break;
                }
                let want = blank(last + 1);
                last += 1;
                while last + 1 < rows && blank(last + 1) == want {
                    last += 1;
                }
            }
        }
        ObjectScope::Around => {
            for step in 0..count {
                if step > 0 {
                    if last + 1 >= rows || blank(last + 1) {
                        break;
                    }
                    last += 1;
                    while last + 1 < rows && !blank(last + 1) {
                        last += 1;
                    }
                }
                while last + 1 < rows && blank(last + 1) {
                    last += 1;
                }
            }
        }
    }
    Span::Lines(first..=last)
}

/// Byte offsets of the delimiters enclosing `at`, counting nesting.
///
/// A cursor sitting *on* either delimiter counts as inside, which is what makes
/// `ci(` work with the cursor on the paren itself.
fn enclosing_pair(buf: &Buffer, at: usize, open: char, close: char) -> Option<(usize, usize)> {
    let start = if char_at(buf, at) == Some(open) {
        at
    } else {
        let mut depth = 0usize;
        let mut pos = at;
        loop {
            if pos == 0 {
                return None;
            }
            pos = retreat_char(buf, pos);
            match char_at(buf, pos) {
                Some(ch) if ch == close => depth += 1,
                Some(ch) if ch == open => match depth.checked_sub(1) {
                    Some(remaining) => depth = remaining,
                    None => break pos,
                },
                _ => {}
            }
        }
    };

    let mut depth = 0usize;
    let mut pos = advance_char(buf, start);
    loop {
        match char_at(buf, pos) {
            None => return None,
            Some(ch) if ch == open => depth += 1,
            Some(ch) if ch == close => match depth.checked_sub(1) {
                Some(remaining) => depth = remaining,
                None => return Some((start, pos)),
            },
            _ => {}
        }
        pos = advance_char(buf, pos);
    }
}

/// Byte offsets of the quote pair around `at`, searched within the row.
///
/// Quotes have no nesting, so the pairs are simply taken in order and the one
/// containing the cursor wins; failing that, the next pair after it does, which
/// matches vi's forgiving behaviour when the cursor sits before the string.
fn enclosing_quotes(buf: &Buffer, at: usize, quote: char) -> Option<(usize, usize)> {
    let row = buf.byte_to_point(at).row;
    let range = buf.row_content_range(row);
    let text = buf.text_in(range.clone());
    let positions: Vec<usize> = text
        .char_indices()
        .filter(|&(_, ch)| ch == quote)
        .map(|(offset, _)| range.start + offset)
        .collect();
    // Pairs come in order, so the first one whose closing quote is at or after the
    // cursor is either the pair containing it or the next pair along.
    positions
        .chunks_exact(2)
        .map(|pair| (pair[0], pair[1]))
        .find(|&(_, close)| at <= close)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn go(text: &str, from: usize, motion: Motion, count: Option<usize>) -> Option<usize> {
        let buffer = Buffer::from_text(text);
        super::resolve(
            &buffer,
            from,
            motion,
            count,
            0,
            None,
            None,
            Viewport::default(),
            Bound::OnChar,
        )
    }

    #[test]
    fn word_class_stepping() {
        for (text, from, motion, count, expected) in [
            (
                "select id, name",
                0,
                Motion::WordForward { big: false },
                None,
                Some(7),
            ),
            (
                "select id, name",
                7,
                Motion::WordForward { big: false },
                None,
                Some(9),
            ),
            (
                "select id, name",
                7,
                Motion::WordForward { big: true },
                None,
                Some(11),
            ),
            (
                "select id, name",
                9,
                Motion::WordBackward { big: false },
                None,
                Some(7),
            ),
            (
                "select id, name",
                0,
                Motion::WordEnd { big: false },
                None,
                Some(5),
            ),
            (
                "one two three",
                0,
                Motion::WordForward { big: false },
                Some(2),
                Some(8),
            ),
            (
                "one\ntwo",
                0,
                Motion::WordForward { big: false },
                None,
                Some(4),
            ),
        ] {
            assert_eq!(
                go(text, from, motion, count),
                expected,
                "{text:?} at {from}"
            );
        }
    }

    #[test]
    fn finds_stay_on_their_row() {
        let find = |target, backward, till| {
            Motion::Find(Find {
                target,
                backward,
                till,
            })
        };
        for (text, from, motion, count, expected) in [
            ("select id, name", 0, find(',', false, false), None, Some(9)),
            ("select id, name", 0, find(',', false, true), None, Some(8)),
            (
                "select id, name",
                14,
                find('e', true, false),
                Some(2),
                Some(1),
            ),
            ("select id, name", 14, find('e', true, true), None, Some(4)),
            ("a.b.c.d", 0, find('.', false, false), Some(2), Some(3)),
            ("a\nb", 0, find('b', false, false), None, None),
        ] {
            assert_eq!(
                go(text, from, motion, count),
                expected,
                "{text:?} at {from}"
            );
        }
    }

    #[test]
    fn delimiter_pair_scanning() {
        for (text, from, expected) in [
            ("x (a (b) c) y", 0, Some(10)),
            ("x (a (b) c) y", 2, Some(10)),
            ("x (a (b) c) y", 10, Some(2)),
            ("x (a (b) c) y", 5, Some(7)),
            ("say 'hi' there", 0, Some(7)),
            ("x\n(a)", 0, None),
        ] {
            assert_eq!(
                go(text, from, Motion::MatchPair, None),
                expected,
                "{text:?} at {from}"
            );
        }
    }

    #[test]
    fn delimited_objects_follow_nesting() {
        let braces = TextObject::Delimited {
            open: '{',
            close: '}',
        };
        let buffer = Buffer::from_text("outer { mid { deep } here } end");
        for (count, expected) in [
            (1, Some(" deep ")),
            (2, Some(" mid { deep } here ")),
            (3, None),
        ] {
            let span =
                object_span(&buffer, 15, ObjectScope::Inner, braces, count).and_then(|span| {
                    match span {
                        Span::Chars(range) => Some(buffer.text_in(range)),
                        Span::Lines(_) => None,
                    }
                });
            assert_eq!(span.as_deref(), expected);
        }
    }

    #[test]
    fn word_object_boundaries() {
        let word = TextObject::Word { big: false };
        for (text, at, scope, count, expected) in [
            ("select id", 2, ObjectScope::Inner, 1, Some("select")),
            ("select id", 2, ObjectScope::Around, 1, Some("select ")),
            ("a bb", 2, ObjectScope::Around, 1, Some(" bb")),
            ("one two three", 0, ObjectScope::Inner, 2, Some("one ")),
            ("one two three", 0, ObjectScope::Around, 2, Some("one two ")),
        ] {
            let buffer = Buffer::from_text(text);
            let span = object_span(&buffer, at, scope, word, count).and_then(|span| match span {
                Span::Chars(range) => Some(buffer.text_in(range)),
                Span::Lines(_) => None,
            });
            assert_eq!(span.as_deref(), expected, "{text:?} at {at}");
        }
    }

    #[test]
    fn paragraph_object_counts_are_linewise() {
        let buffer = Buffer::from_text("one\n\ntwo\n\nthree");
        for (scope, count, expected) in [
            (ObjectScope::Inner, 2, "one\n\n"),
            (ObjectScope::Inner, 3, "one\n\ntwo\n"),
            (ObjectScope::Around, 2, "one\n\ntwo\n\n"),
        ] {
            let Span::Lines(rows) = object_span(&buffer, 0, scope, TextObject::Paragraph, count)
                .expect("paragraph resolves")
            else {
                panic!("paragraphs are linewise")
            };
            let range = buffer.row_range(*rows.start()).start..buffer.row_range(*rows.end()).end;
            assert_eq!(buffer.text_in(range), expected);
        }
    }
}
