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

use core::ops::Range;

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

/// A resolved region, and whether an operator should treat it as whole rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub linewise: bool,
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
pub const STICKY_END: usize = usize::MAX;

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

/// Pull `byte` back to a legal cursor position for `bound`.
#[must_use]
pub fn clamp(buf: &Buffer, byte: usize, bound: Bound) -> usize {
    let byte = byte.min(buf.len_bytes());
    let row = buf.byte_to_point(byte).row;
    let boundaries = boundaries(buf, row);
    let limit = boundaries[max_col(&boundaries, bound)];
    byte.min(limit)
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

/// The rows a span covers.
///
/// Nearly the inverse of [`row_span`], but not simply the rows its end bytes sit
/// on: a linewise span that reaches the final row *starts* at the newline ending
/// the row above it, because that is the newline `dd` has to take. Reading the
/// first row straight off `range.start` therefore lands a row too high.
///
/// Correcting for that is only safe where the row holding that byte has content
/// of its own. A span starting on a genuinely empty row starts on that row's own
/// newline, which is indistinguishable byte for byte — so the emptiness of the
/// row is what tells the two apart.
#[must_use]
pub fn span_rows(buf: &Buffer, range: &Range<usize>) -> (usize, usize) {
    let mut first = buf.byte_to_point(range.start).row;
    let content = buf.row_content_range(first);
    if !content.is_empty() && range.start >= content.end && first + 1 < buf.len_rows() {
        first += 1;
    }
    let last = buf
        .byte_to_point(range.end.saturating_sub(1).max(range.start))
        .row;
    (first, last.max(first))
}

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

fn screen_motion(buf: &Buffer, motion: Motion, repeat: usize, viewport: Viewport) -> Option<usize> {
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

/// Where `motion` lands, starting from `from`.
///
/// `sticky` is the remembered column for `j`/`k`; [`STICKY_END`] means "row end".
/// `last_find` supplies the target for [`Motion::RepeatFind`].
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
        Motion::Find {
            target,
            backward,
            till,
        } => find_in_row(
            buf,
            from,
            Find {
                target,
                backward,
                till,
            },
            repeat,
            false,
        )?,
        Motion::RepeatFind { reverse } => {
            let mut find = last_find?;
            if reverse {
                find.backward = !find.backward;
            }
            find_in_row(buf, from, find, repeat, true)?
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
        Motion::ScreenTop | Motion::ScreenMiddle | Motion::ScreenBottom => {
            screen_motion(buf, motion, repeat, viewport)?
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
    Some(Span {
        range,
        linewise: false,
    })
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
    Span {
        range,
        linewise: false,
    }
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
    Span {
        range: row_span(buf, first, last),
        linewise: true,
    }
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

    const SQL: &str = "select id, name\nfrom users\nwhere id = 1";

    fn buf() -> Buffer {
        Buffer::from_text(SQL)
    }

    // Most motion tests are deliberately viewport-agnostic. Screen motions have
    // their own tests; this keeps the older scripts focused on their subject.
    fn resolve(
        buf: &Buffer,
        from: usize,
        motion: Motion,
        count: Option<usize>,
        sticky: usize,
        last_find: Option<Find>,
        bound: Bound,
    ) -> Option<usize> {
        super::resolve(
            buf,
            from,
            motion,
            count,
            sticky,
            last_find,
            Viewport::default(),
            bound,
        )
    }

    fn go(text: &str, from: usize, motion: Motion, count: Option<usize>) -> usize {
        let buf = Buffer::from_text(text);
        resolve(&buf, from, motion, count, 0, None, Bound::OnChar).expect("motion resolves")
    }

    #[test]
    fn horizontal_steps_stay_on_the_row() {
        let buf = buf();
        // Column 0 of row 1 cannot go left into row 0.
        assert_eq!(
            resolve(&buf, 16, Motion::Left, None, 0, None, Bound::OnChar),
            Some(16)
        );
        // Nor right past the last character.
        assert_eq!(
            resolve(&buf, 25, Motion::Right, None, 0, None, Bound::OnChar),
            Some(25)
        );
        assert_eq!(
            resolve(&buf, 16, Motion::Right, Some(4), 0, None, Bound::OnChar),
            Some(20)
        );
    }

    #[test]
    fn normal_mode_cannot_rest_past_the_last_character() {
        let buf = buf();
        // Row 1 is `from users`, bytes 16..26, so the last character starts at 25.
        assert_eq!(clamp(&buf, 26, Bound::OnChar), 25);
        // Insert mode may.
        assert_eq!(clamp(&buf, 26, Bound::PastEnd), 26);
    }

    #[test]
    fn graphemes_not_chars() {
        // `e` + combining acute is one grapheme, two chars, three bytes.
        let buf = Buffer::from_text("ae\u{301}b");
        assert_eq!(buf.len_bytes(), 5);
        let after_a = resolve(&buf, 0, Motion::Right, None, 0, None, Bound::OnChar).unwrap();
        assert_eq!(after_a, 1);
        let after_combined =
            resolve(&buf, after_a, Motion::Right, None, 0, None, Bound::OnChar).unwrap();
        // Skipped both the `e` and its combining mark.
        assert_eq!(after_combined, 4);
        assert_eq!(grapheme_col(&buf, 4), 2);
    }

    #[test]
    fn vertical_movement_uses_the_sticky_column() {
        let buf = buf();
        // Column 8 of row 0 down to row 1.
        let down = resolve(&buf, 8, Motion::Down, None, 8, None, Bound::OnChar).unwrap();
        assert_eq!(buf.byte_to_point(down), crate::Point::new(1, 8));
        // A short row clamps without losing the sticky column.
        let short = Buffer::from_text("longer row\nab\nlonger row");
        let onto_short = resolve(&short, 8, Motion::Down, None, 8, None, Bound::OnChar).unwrap();
        assert_eq!(short.byte_to_point(onto_short), crate::Point::new(1, 1));
        let back = resolve(
            &short,
            onto_short,
            Motion::Down,
            None,
            8,
            None,
            Bound::OnChar,
        )
        .unwrap();
        assert_eq!(short.byte_to_point(back), crate::Point::new(2, 8));
    }

    #[test]
    fn sticky_end_tracks_row_ends() {
        let short = Buffer::from_text("longer row\nab\nlonger row");
        let down = resolve(
            &short,
            0,
            Motion::Down,
            None,
            STICKY_END,
            None,
            Bound::OnChar,
        )
        .unwrap();
        assert_eq!(short.byte_to_point(down), crate::Point::new(1, 1));
    }

    #[test]
    fn row_ends_and_starts() {
        let buf = Buffer::from_text("  indented text\nsecond");
        assert_eq!(
            go("  indented text\nsecond", 8, Motion::FirstColumn, None),
            0
        );
        assert_eq!(
            go("  indented text\nsecond", 8, Motion::FirstNonBlank, None),
            2
        );
        // `$` rests on the last character, not past it.
        assert_eq!(
            resolve(&buf, 0, Motion::LastColumn, None, 0, None, Bound::OnChar),
            Some(14)
        );
    }

    #[test]
    fn word_forward() {
        // `select id, name` — offsets 0 s, 7 i, 9 comma, 11 n
        assert_eq!(go(SQL, 0, Motion::WordForward { big: false }, None), 7);
        assert_eq!(go(SQL, 7, Motion::WordForward { big: false }, None), 9);
        assert_eq!(go(SQL, 9, Motion::WordForward { big: false }, None), 11);
        assert_eq!(go(SQL, 0, Motion::WordForward { big: false }, Some(3)), 11);
    }

    #[test]
    fn big_words_swallow_punctuation() {
        // `id,` is one WORD, so `W` skips the comma that `w` stops on.
        assert_eq!(go(SQL, 7, Motion::WordForward { big: true }, None), 11);
    }

    #[test]
    fn word_motions_cross_rows() {
        // From `name` at the end of row 0 into `from` on row 1.
        assert_eq!(go(SQL, 11, Motion::WordForward { big: false }, None), 16);
        assert_eq!(go(SQL, 16, Motion::WordBackward { big: false }, None), 11);
    }

    #[test]
    fn word_backward_and_end() {
        assert_eq!(go(SQL, 11, Motion::WordBackward { big: false }, None), 9);
        assert_eq!(go(SQL, 9, Motion::WordBackward { big: false }, None), 7);
        // `e` lands on the last character of the word, hence 5 not 6.
        assert_eq!(go(SQL, 0, Motion::WordEnd { big: false }, None), 5);
        assert_eq!(go(SQL, 5, Motion::WordEnd { big: false }, None), 8);
    }

    #[test]
    fn find_within_the_row() {
        let find = |target, backward, till| Motion::Find {
            target,
            backward,
            till,
        };
        // `select id, name`, comma at 9.
        assert_eq!(go(SQL, 0, find(',', false, false), None), 9);
        // `t,` stops one short.
        assert_eq!(go(SQL, 0, find(',', false, true), None), 8);
        // `select id, name` has `e` at 1, 3 and 14; searching back from 14 finds 3.
        assert_eq!(go(SQL, 14, find('e', true, false), None), 3);
        assert_eq!(go(SQL, 14, find('e', true, false), Some(2)), 1);
        // `Te` stops one past the target, going backwards.
        assert_eq!(go(SQL, 14, find('e', true, true), None), 4);
        // Counts pick the nth occurrence.
        assert_eq!(go("a.b.c.d", 0, find('.', false, false), Some(2)), 3);
    }

    #[test]
    fn find_does_not_leave_the_row() {
        let buf = buf();
        // No `z` anywhere, and `f` must not wander onto row 1.
        assert_eq!(
            resolve(
                &buf,
                0,
                Motion::Find {
                    target: 'u',
                    backward: false,
                    till: false
                },
                None,
                0,
                None,
                Bound::OnChar
            ),
            None
        );
    }

    #[test]
    fn repeat_find_and_reverse() {
        let buf = Buffer::from_text("a.b.c");
        let find = Find {
            target: '.',
            backward: false,
            till: false,
        };
        let first = resolve(
            &buf,
            0,
            Motion::RepeatFind { reverse: false },
            None,
            0,
            Some(find),
            Bound::OnChar,
        );
        assert_eq!(first, Some(1));
        let back = resolve(
            &buf,
            3,
            Motion::RepeatFind { reverse: true },
            None,
            0,
            Some(find),
            Bound::OnChar,
        );
        assert_eq!(back, Some(1));
        // Nothing remembered, nothing to repeat.
        assert_eq!(
            resolve(
                &buf,
                0,
                Motion::RepeatFind { reverse: false },
                None,
                0,
                None,
                Bound::OnChar
            ),
            None
        );
    }

    #[test]
    fn goto_row_uses_the_count_as_an_absolute() {
        // No count: last row for `G`, first for `gg`.
        assert_eq!(go(SQL, 0, Motion::GotoRow, None), 27);
        assert_eq!(go(SQL, 30, Motion::GotoFirstRow, None), 0);
        // With a count, both mean "that row", 1-based.
        assert_eq!(go(SQL, 0, Motion::GotoRow, Some(2)), 16);
        assert_eq!(go(SQL, 30, Motion::GotoFirstRow, Some(2)), 16);
        // Out of range clamps.
        assert_eq!(go(SQL, 0, Motion::GotoRow, Some(99)), 27);
    }

    #[test]
    fn goto_row_lands_on_the_first_non_blank() {
        assert_eq!(go("first\n    indented", 0, Motion::GotoRow, None), 10);
    }

    #[test]
    fn linewise_row_span_includes_the_newline() {
        let buf = buf();
        assert_eq!(row_span(&buf, 1, 1), 16..27);
        assert_eq!(row_span(&buf, 0, 1), 0..27);
    }

    #[test]
    fn deleting_the_last_row_takes_the_preceding_newline() {
        let buf = buf();
        // Otherwise `dd` on the final row would leave an empty row behind.
        assert_eq!(row_span(&buf, 2, 2), 26..39);
    }

    /// An uncounted object, which is what most of these are about.
    fn obj(buf: &Buffer, at: usize, scope: ObjectScope, object: TextObject) -> Option<Span> {
        object_span(buf, at, scope, object, 1)
    }

    const WORD: TextObject = TextObject::Word { big: false };
    const PARENS: TextObject = TextObject::Delimited {
        open: '(',
        close: ')',
    };
    const BRACES: TextObject = TextObject::Delimited {
        open: '{',
        close: '}',
    };

    #[test]
    fn inner_and_around_word() {
        let buf = buf();
        // Cursor in `select`.
        let inner = obj(&buf, 2, ObjectScope::Inner, WORD);
        assert_eq!(
            inner,
            Some(Span {
                range: 0..6,
                linewise: false
            })
        );
        // `aw` takes the following space.
        let around = obj(&buf, 2, ObjectScope::Around, WORD);
        assert_eq!(
            around,
            Some(Span {
                range: 0..7,
                linewise: false
            })
        );
    }

    #[test]
    fn around_word_falls_back_to_leading_space() {
        let buf = Buffer::from_text("a bb");
        // No trailing space after `bb`, so `aw` takes the leading one.
        let around = obj(&buf, 2, ObjectScope::Around, WORD);
        assert_eq!(
            around,
            Some(Span {
                range: 1..4,
                linewise: false
            })
        );
    }

    #[test]
    fn counted_word_objects_take_further_runs() {
        let buf = Buffer::from_text("one two three four");
        let text = |span: Option<Span>| buf.text_in(span.expect("object resolves").range);

        // `iw` counts whitespace as a run of its own, so an odd count ends on a word
        // and an even one ends on the space after it.
        assert_eq!(
            text(object_span(&buf, 0, ObjectScope::Inner, WORD, 1)),
            "one"
        );
        assert_eq!(
            text(object_span(&buf, 0, ObjectScope::Inner, WORD, 2)),
            "one "
        );
        assert_eq!(
            text(object_span(&buf, 0, ObjectScope::Inner, WORD, 3)),
            "one two"
        );

        // `aw` counts whole words, each with the space that follows.
        assert_eq!(
            text(object_span(&buf, 0, ObjectScope::Around, WORD, 2)),
            "one two "
        );
        assert_eq!(
            text(object_span(&buf, 0, ObjectScope::Around, WORD, 3)),
            "one two three "
        );

        // A count that overruns the row stops at its end rather than joining rows.
        let buf = Buffer::from_text("a b\nc d");
        let text = |span: Option<Span>| buf.text_in(span.expect("object resolves").range);
        assert_eq!(
            text(object_span(&buf, 0, ObjectScope::Inner, WORD, 9)),
            "a b"
        );
        assert_eq!(
            text(object_span(&buf, 0, ObjectScope::Around, WORD, 9)),
            "a b"
        );
    }

    #[test]
    fn delimited_objects_count_nesting() {
        let buf = Buffer::from_text("f(a, g(b), c)");
        // Cursor on `b`, innermost pair.
        let inner = obj(&buf, 7, ObjectScope::Inner, PARENS).unwrap();
        assert_eq!(buf.text_in(inner.range.clone()), "b");
        // Cursor on the leading `a`, outer pair.
        let outer = obj(&buf, 2, ObjectScope::Inner, PARENS).unwrap();
        assert_eq!(buf.text_in(outer.range), "a, g(b), c");
        let around = obj(&buf, 2, ObjectScope::Around, PARENS).unwrap();
        assert_eq!(buf.text_in(around.range), "(a, g(b), c)");
    }

    #[test]
    fn a_count_climbs_out_of_nested_delimiters() {
        let buf = Buffer::from_text("outer { mid { deep } here } end");
        let text = |scope, count| {
            buf.text_in(
                object_span(&buf, 15, scope, BRACES, count)
                    .expect("object resolves")
                    .range,
            )
        };
        assert_eq!(text(ObjectScope::Inner, 1), " deep ");
        assert_eq!(text(ObjectScope::Inner, 2), " mid { deep } here ");
        assert_eq!(text(ObjectScope::Around, 2), "{ mid { deep } here }");

        // Beyond the outermost pair there is nothing to take.
        assert_eq!(object_span(&buf, 15, ObjectScope::Inner, BRACES, 3), None);

        // Siblings are not levels: `(a)(b)` encloses nothing, however it is read.
        let buf = Buffer::from_text("(a)(b)");
        assert_eq!(object_span(&buf, 4, ObjectScope::Inner, PARENS, 2), None);
    }

    #[test]
    fn a_delimited_object_seeks_forward_when_the_cursor_is_outside() {
        let buf = Buffer::from_text("foo { a { b { c } d } e } baz");
        let text = |count| {
            buf.text_in(
                object_span(&buf, 0, ObjectScope::Inner, BRACES, count)
                    .expect("object resolves")
                    .range,
            )
        };
        // From `foo`, level 1 is the outermost pair ahead...
        assert_eq!(text(1), " a { b { c } d } e ");
        // ...and the count descends inward from there, rather than climbing out of
        // a pair the cursor was never in.
        assert_eq!(text(2), " b { c } d ");
        assert_eq!(text(3), " c ");
        assert_eq!(object_span(&buf, 0, ObjectScope::Inner, BRACES, 4), None);
    }

    #[test]
    fn seeking_descends_into_the_first_nested_pair() {
        let buf = Buffer::from_text("foo { a {b} c {d} e }");
        let span = object_span(&buf, 0, ObjectScope::Inner, BRACES, 2).unwrap();
        assert_eq!(buf.text_in(span.range), "b");

        // Siblings are no more a level to descend into than one to climb out of.
        let buf = Buffer::from_text("foo {a} {b} baz");
        let span = obj(&buf, 0, ObjectScope::Inner, BRACES).unwrap();
        assert_eq!(buf.text_in(span.range), "a");
        assert_eq!(object_span(&buf, 0, ObjectScope::Inner, BRACES, 2), None);
    }

    #[test]
    fn seeking_crosses_rows() {
        let buf = Buffer::from_text("fn f()\n{\n    body\n}\n");
        // From the signature row, `i{` reaches the block below it — which is the
        // whole point of seeking, and why it is not row-scoped the way quotes are.
        let span = obj(&buf, 3, ObjectScope::Inner, BRACES).unwrap();
        assert_eq!(buf.text_in(span.range), "    body\n");
    }

    #[test]
    fn delimiters_that_own_their_rows_shrink_the_inside() {
        // The brace is the last thing on its row and the closing brace has only
        // indent before it, so the inside is the body's rows — not the newline
        // behind the `{` through to the `}`.
        let buf = Buffer::from_text("f() {\n  a\n  b\n  }");
        let span = obj(&buf, 0, ObjectScope::Inner, BRACES).unwrap();
        assert_eq!(buf.text_in(span.range), "  a\n  b\n");
        // `a{` is untouched by the rule.
        let span = obj(&buf, 0, ObjectScope::Around, BRACES).unwrap();
        assert_eq!(buf.text_in(span.range), "{\n  a\n  b\n  }");

        // Each half applies on its own. Open mid-row, close owning its row — and
        // the row break stays, because `x {` is still there to need it.
        let buf = Buffer::from_text("x { body\n}");
        let span = obj(&buf, 0, ObjectScope::Inner, BRACES).unwrap();
        assert_eq!(buf.text_in(span.range), " body");
        // Open owning its row, close mid-row:
        let buf = Buffer::from_text("{\n  body }");
        let span = obj(&buf, 0, ObjectScope::Inner, BRACES).unwrap();
        assert_eq!(buf.text_in(span.range), "  body ");

        // Nothing between the two rows leaves nothing to take.
        let buf = Buffer::from_text("{\n}");
        assert!(
            obj(&buf, 0, ObjectScope::Inner, BRACES)
                .unwrap()
                .range
                .is_empty()
        );

        // And a pair that shares its row is unaffected, indent or not.
        let buf = Buffer::from_text("    { a }");
        let span = obj(&buf, 0, ObjectScope::Inner, BRACES).unwrap();
        assert_eq!(buf.text_in(span.range), " a ");
    }

    #[test]
    fn seeking_does_not_look_backwards() {
        let buf = Buffer::from_text("{ a } foo");
        // On `foo`, with the only pair behind: vi leaves it alone.
        assert_eq!(obj(&buf, 6, ObjectScope::Inner, BRACES), None);
    }

    #[test]
    fn an_unmatched_close_stops_the_seek() {
        let buf = Buffer::from_text("x } y { a } z");
        assert_eq!(obj(&buf, 0, ObjectScope::Inner, BRACES), None);
        // Past the stray close, the pair is reachable again.
        let span = obj(&buf, 4, ObjectScope::Inner, BRACES).unwrap();
        assert_eq!(buf.text_in(span.range), " a ");
    }

    #[test]
    fn cursor_on_the_delimiter_counts_as_inside() {
        let buf = Buffer::from_text("f(abc)");
        let inner = obj(&buf, 1, ObjectScope::Inner, PARENS).unwrap();
        assert_eq!(buf.text_in(inner.range), "abc");
    }

    #[test]
    fn unbalanced_delimiters_resolve_to_nothing() {
        let buf = Buffer::from_text("no parens here");
        assert_eq!(obj(&buf, 3, ObjectScope::Inner, PARENS), None);
    }

    #[test]
    fn quoted_objects() {
        let buf = Buffer::from_text("where name = 'dave'");
        let quoted = TextObject::Quoted('\'');
        let inner = obj(&buf, 15, ObjectScope::Inner, quoted).unwrap();
        assert_eq!(buf.text_in(inner.range), "dave");
        let around = obj(&buf, 15, ObjectScope::Around, quoted).unwrap();
        assert_eq!(buf.text_in(around.range), "'dave'");
        // Quotes do not nest, so a count has nothing to climb and is ignored.
        let counted = object_span(&buf, 15, ObjectScope::Inner, quoted, 3).unwrap();
        assert_eq!(buf.text_in(counted.range), "dave");
    }

    #[test]
    fn paragraph_objects_are_linewise() {
        let buf = Buffer::from_text("one\ntwo\n\nthree");
        let span = obj(&buf, 0, ObjectScope::Inner, TextObject::Paragraph).unwrap();
        assert!(span.linewise);
        assert_eq!(buf.text_in(span.range), "one\ntwo\n");
    }

    #[test]
    fn counted_paragraph_objects() {
        let buf = Buffer::from_text("one\n\ntwo\n\nthree");
        let para = TextObject::Paragraph;
        let text = |scope, count| {
            buf.text_in(
                object_span(&buf, 0, scope, para, count)
                    .expect("object resolves")
                    .range,
            )
        };
        // A gap is a run of its own for `ip`, just as whitespace is for `iw`.
        assert_eq!(text(ObjectScope::Inner, 2), "one\n\n");
        assert_eq!(text(ObjectScope::Inner, 3), "one\n\ntwo\n");
        // `ap` takes each paragraph with the gap after it.
        assert_eq!(text(ObjectScope::Around, 1), "one\n\n");
        assert_eq!(text(ObjectScope::Around, 2), "one\n\ntwo\n\n");
    }

    #[test]
    fn motions_on_an_empty_buffer_do_not_panic() {
        let buf = Buffer::new();
        for motion in [
            Motion::Left,
            Motion::Right,
            Motion::Up,
            Motion::Down,
            Motion::FirstColumn,
            Motion::LastColumn,
            Motion::FirstNonBlank,
            Motion::WordForward { big: false },
            Motion::WordBackward { big: false },
            Motion::WordEnd { big: false },
            Motion::GotoRow,
            Motion::GotoFirstRow,
        ] {
            let landed = resolve(&buf, 0, motion, None, 0, None, Bound::OnChar);
            assert_eq!(landed, Some(0), "{motion:?} on an empty buffer");
        }
    }

    #[test]
    fn motions_at_the_buffer_end_do_not_panic() {
        let buf = buf();
        let end = buf.len_bytes();
        for motion in [
            Motion::Right,
            Motion::Down,
            Motion::WordForward { big: false },
            Motion::WordEnd { big: false },
        ] {
            let landed = resolve(
                &buf,
                clamp(&buf, end, Bound::OnChar),
                motion,
                Some(9),
                0,
                None,
                Bound::OnChar,
            );
            assert!(landed.is_some(), "{motion:?} at the end");
        }
    }
}
