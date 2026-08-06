//! The keymap: bindings indexed by key sequence.
//!
//! Layered the way vi's own `:map` commands are, because the ambiguity is real:
//! `i` enters insert mode in normal mode but means *inner* while an operator is
//! pending. One flat table cannot express that.

use std::collections::BTreeMap;

use crate::command::{
    AwaitChar, Command, InsertAt, Mode, Motion, ObjectScope, Operator, Scroll, Target, TextObject,
    VisualKind,
};
use crate::key::{Key, KeyCode, keys};

/// Which table a lookup consults. Mirrors vi's `nmap` / `omap` / `vmap` / `imap`.
///
/// [`Layer::Operator`] and [`Layer::Visual`] fall back to [`Layer::Normal`], so a
/// motion bound once is available everywhere a motion makes sense.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Layer {
    Normal,
    /// Consulted while an operator is pending — `d`_`iw`_, `c`_`w`_.
    Operator,
    Visual,
    Insert,
}

impl Layer {
    /// The layer a mode uses when no operator is pending.
    ///
    /// Replace mode shares the insert layer; bind it there.
    #[must_use]
    pub const fn of(mode: Mode) -> Self {
        match mode {
            Mode::Normal => Self::Normal,
            Mode::Insert | Mode::Replace => Self::Insert,
            Mode::Visual(_) => Self::Visual,
        }
    }

    const fn fallback(self) -> Option<Self> {
        match self {
            Self::Operator | Self::Visual => Some(Self::Normal),
            Self::Normal | Self::Insert => None,
        }
    }
}

/// What a key sequence maps to.
///
/// Not every binding is a complete command — operators, object scopes and
/// character-argument bindings all leave the parser expecting more input. That is
/// why this is a separate type from [`Command`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// Complete on its own.
    Command(Command),
    /// Awaits a target: a motion, a text object, or a doubling.
    Operator(Operator),
    /// A movement, which becomes a target if an operator is pending.
    Motion(Motion),
    /// `i` / `a`, awaiting an object key.
    ObjectScope(ObjectScope),
    /// Awaits one literal character.
    Await(AwaitChar),
    /// Awaits a literal search pattern terminated by `<CR>`.
    Search { backward: bool },
}

/// The result of walking a key path through one layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Walk {
    /// A valid prefix — more keys needed.
    Prefix,
    /// A complete binding.
    Bound(Binding),
    /// No binding and no prefix.
    Unbound,
}

/// Key sequences to bindings, per layer.
#[derive(Debug, Clone, Default)]
pub struct Keymap {
    normal: BTreeMap<Vec<Key>, Binding>,
    operator: BTreeMap<Vec<Key>, Binding>,
    visual: BTreeMap<Vec<Key>, Binding>,
    insert: BTreeMap<Vec<Key>, Binding>,
    objects: BTreeMap<Key, TextObject>,
}

impl Keymap {
    /// No bindings at all. Build your own scheme on top.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    fn layer(&self, layer: Layer) -> &BTreeMap<Vec<Key>, Binding> {
        match layer {
            Layer::Normal => &self.normal,
            Layer::Operator => &self.operator,
            Layer::Visual => &self.visual,
            Layer::Insert => &self.insert,
        }
    }

    fn layer_mut(&mut self, layer: Layer) -> &mut BTreeMap<Vec<Key>, Binding> {
        match layer {
            Layer::Normal => &mut self.normal,
            Layer::Operator => &mut self.operator,
            Layer::Visual => &mut self.visual,
            Layer::Insert => &mut self.insert,
        }
    }

    /// Bind a key sequence.
    ///
    /// Overwrites whatever occupied that path, including a whole subtree: binding
    /// `g` discards any existing `gg`.
    ///
    /// # Panics
    /// If `sequence` is empty.
    pub fn bind(&mut self, layer: Layer, sequence: &[Key], binding: Binding) -> &mut Self {
        assert!(!sequence.is_empty(), "cannot bind an empty key sequence");
        let map = self.layer_mut(layer);
        // A layer cannot hold both a binding and descendants below it. This
        // preserves the trie representation's no-ambiguity invariant.
        map.retain(|existing, _| {
            !existing.starts_with(sequence) && !sequence.starts_with(existing)
        });
        map.insert(sequence.to_vec(), binding);
        self
    }

    /// Bind a sequence written in vi notation.
    ///
    /// # Panics
    /// If `spec` is not valid key notation, or is empty.
    pub fn bind_spec(&mut self, layer: Layer, spec: &str, binding: Binding) -> &mut Self {
        let sequence = keys(spec).expect("valid key notation");
        self.bind(layer, &sequence, binding)
    }

    /// Remove a binding, and any subtree beneath it.
    pub fn unbind(&mut self, layer: Layer, sequence: &[Key]) -> &mut Self {
        self.layer_mut(layer)
            .retain(|existing, _| !existing.starts_with(sequence));
        self
    }

    /// Walk `path` through `layer`, applying its fallback rules.
    ///
    /// A layer that produces neither a binding nor a prefix defers to its
    /// fallback, so `Layer::Visual` inherits every normal-mode motion while still
    /// being able to shadow individual keys.
    #[must_use]
    pub fn walk(&self, layer: Layer, path: &[Key]) -> Walk {
        match walk(self.layer(layer), path) {
            Walk::Unbound => match layer.fallback() {
                Some(next) => self.walk(next, path),
                None => Walk::Unbound,
            },
            found => found,
        }
    }

    /// The text object a key selects after `i` or `a`.
    #[must_use]
    pub fn object(&self, key: Key) -> Option<TextObject> {
        self.objects.get(&key).copied()
    }

    pub fn bind_object(&mut self, key: Key, object: TextObject) -> &mut Self {
        self.objects.insert(key, object);
        self
    }

    /// The default scheme.
    ///
    /// Covers the subset this core targets: modes, motions, the three operators,
    /// text objects, counts, dot-repeat, macros, marks, undo, jump navigation,
    /// and ex commands. No named registers.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn vim() -> Self {
        use Binding::{
            Await, Command as Cmd, Motion as Move, ObjectScope as Scope, Operator as Op,
        };
        use Command as C;

        let mut map = Self::empty();

        // -- motions, shared by every command layer --------------------------
        for (spec, motion) in [
            ("h", Motion::Left),
            ("l", Motion::Right),
            ("<Space>", Motion::Right),
            ("j", Motion::Down),
            ("k", Motion::Up),
            ("<Left>", Motion::Left),
            ("<Right>", Motion::Right),
            ("<Down>", Motion::Down),
            ("<Up>", Motion::Up),
            ("0", Motion::FirstColumn),
            ("<Home>", Motion::FirstColumn),
            ("^", Motion::FirstNonBlank),
            ("$", Motion::LastColumn),
            ("<End>", Motion::LastColumn),
            ("w", Motion::WordForward { big: false }),
            ("W", Motion::WordForward { big: true }),
            ("b", Motion::WordBackward { big: false }),
            ("B", Motion::WordBackward { big: true }),
            ("e", Motion::WordEnd { big: false }),
            ("E", Motion::WordEnd { big: true }),
            ("G", Motion::GotoRow),
            ("gg", Motion::GotoFirstRow),
            ("%", Motion::MatchPair),
            ("H", Motion::ScreenTop),
            ("M", Motion::ScreenMiddle),
            ("L", Motion::ScreenBottom),
            (";", Motion::RepeatFind { reverse: false }),
            (",", Motion::RepeatFind { reverse: true }),
            ("n", Motion::RepeatSearch { reverse: false }),
            ("N", Motion::RepeatSearch { reverse: true }),
        ] {
            map.bind_spec(Layer::Normal, spec, Move(motion));
        }
        map.bind_spec(Layer::Normal, "/", Binding::Search { backward: false })
            .bind_spec(Layer::Normal, "?", Binding::Search { backward: true });

        // `f`/`t` and friends need one more key before they mean anything.
        for (spec, backward, till) in [
            ("f", false, false),
            ("F", true, false),
            ("t", false, true),
            ("T", true, true),
        ] {
            map.bind_spec(
                Layer::Normal,
                spec,
                Await(AwaitChar::Find { backward, till }),
            );
        }
        map.bind_spec(Layer::Normal, "m", Await(AwaitChar::SetMark))
            .bind_spec(
                Layer::Normal,
                "`",
                Await(AwaitChar::GotoMark { exact: true }),
            )
            .bind_spec(
                Layer::Normal,
                "'",
                Await(AwaitChar::GotoMark { exact: false }),
            );

        // -- operators -------------------------------------------------------
        map.bind_spec(Layer::Normal, "d", Op(Operator::Delete))
            .bind_spec(Layer::Normal, "c", Op(Operator::Change))
            .bind_spec(Layer::Normal, "y", Op(Operator::Yank))
            .bind_spec(Layer::Normal, ">", Op(Operator::ShiftRight))
            // `<` starts bracketed key names in the notation parser, so its
            // spelling here must be `<lt>`.
            .bind_spec(Layer::Normal, "<lt>", Op(Operator::ShiftLeft))
            .bind_spec(Layer::Normal, "gu", Op(Operator::Lower))
            .bind_spec(Layer::Normal, "gU", Op(Operator::Upper))
            .bind_spec(Layer::Normal, "g~", Op(Operator::SwapCase));

        // The bare keys, so that the doubled row forms work: an operator is doubled
        // when the same operator arrives twice, and vi accepts the short second half
        // — `gUU` as well as `gUgU`. With an operator pending these keys were a
        // syntax error anyway, so shadowing `u` here costs nothing.
        map.bind_spec(Layer::Operator, "u", Op(Operator::Lower))
            .bind_spec(Layer::Operator, "U", Op(Operator::Upper))
            .bind_spec(Layer::Operator, "~", Op(Operator::SwapCase));

        // `D` and `C` are `d$` and `c$` pre-applied. Binding them as whole commands
        // rather than as an operator awaiting a target is what lets them take a
        // count: `LastColumn` reads it as "this row plus count-1 more", so `2D`
        // clears to the end of the following row, as in vi.
        for (spec, operator) in [("D", Operator::Delete), ("C", Operator::Change)] {
            map.bind_spec(
                Layer::Normal,
                spec,
                Cmd(C::Operate {
                    operator,
                    target: Target::Motion(Motion::LastColumn),
                }),
            );
        }

        // -- mode changes ----------------------------------------------------
        for (spec, at) in [
            ("i", InsertAt::Cursor),
            ("a", InsertAt::After),
            ("I", InsertAt::FirstNonBlank),
            ("A", InsertAt::EndOfRow),
            ("o", InsertAt::RowBelow),
            ("O", InsertAt::RowAbove),
        ] {
            map.bind_spec(Layer::Normal, spec, Cmd(C::EnterInsert(at)));
        }
        map.bind_spec(Layer::Normal, "v", Cmd(C::EnterVisual(VisualKind::Char)))
            .bind_spec(Layer::Normal, "V", Cmd(C::EnterVisual(VisualKind::Line)))
            .bind_spec(Layer::Normal, "R", Cmd(C::EnterReplace))
            .bind_spec(Layer::Normal, "<Esc>", Cmd(C::EnterNormal));

        // -- simple edits ----------------------------------------------------
        map.bind_spec(Layer::Normal, "x", Cmd(C::DeleteChar { before: false }))
            .bind_spec(Layer::Normal, "<Del>", Cmd(C::DeleteChar { before: false }))
            .bind_spec(Layer::Normal, "X", Cmd(C::DeleteChar { before: true }))
            .bind_spec(Layer::Normal, "J", Cmd(C::JoinRows))
            .bind_spec(Layer::Normal, "p", Cmd(C::Put { before: false }))
            .bind_spec(Layer::Normal, "P", Cmd(C::Put { before: true }))
            .bind_spec(Layer::Normal, "~", Cmd(C::SwapCase))
            .bind_spec(Layer::Normal, "r", Await(AwaitChar::ReplaceChar));

        // -- history and repetition ------------------------------------------
        map.bind_spec(Layer::Normal, "u", Cmd(C::Undo))
            .bind_spec(Layer::Normal, "<C-r>", Cmd(C::Redo))
            // These are normal-layer bindings. Insert-mode `<C-o>` remains
            // deliberately unbound.
            .bind_spec(Layer::Normal, "<C-o>", Cmd(C::JumpBack))
            .bind_spec(Layer::Normal, "<C-i>", Cmd(C::JumpForward))
            .bind_spec(Layer::Normal, ".", Cmd(C::Repeat))
            .bind_spec(Layer::Normal, "q", Await(AwaitChar::RecordMacro))
            .bind_spec(Layer::Normal, "@", Await(AwaitChar::PlayMacro));

        // -- viewport and prompts --------------------------------------------
        for (spec, scroll) in [
            ("<C-d>", Scroll::HalfPageDown),
            ("<C-u>", Scroll::HalfPageUp),
            ("<C-f>", Scroll::PageDown),
            ("<C-b>", Scroll::PageUp),
            ("zz", Scroll::Center),
            ("zt", Scroll::Top),
            ("zb", Scroll::Bottom),
        ] {
            map.bind_spec(Layer::Normal, spec, Cmd(C::Scroll(scroll)));
        }
        map.bind_spec(Layer::Normal, ":", Cmd(C::CommandPrompt));

        // -- operator-pending: `i`/`a` become object scopes -------------------
        map.bind_spec(Layer::Operator, "i", Scope(ObjectScope::Inner))
            .bind_spec(Layer::Operator, "a", Scope(ObjectScope::Around));

        // -- visual ----------------------------------------------------------
        map.bind_spec(Layer::Visual, "i", Scope(ObjectScope::Inner))
            .bind_spec(Layer::Visual, "a", Scope(ObjectScope::Around))
            .bind_spec(Layer::Visual, "x", Op(Operator::Delete))
            .bind_spec(Layer::Visual, "s", Op(Operator::Change))
            // Over a selection these are case changes, not undo and not a one-
            // character swap. `gu`/`gU`/`g~` reach the same operators through the
            // normal layer, so they need no separate binding here.
            .bind_spec(Layer::Visual, "u", Op(Operator::Lower))
            .bind_spec(Layer::Visual, "U", Op(Operator::Upper))
            .bind_spec(Layer::Visual, "~", Op(Operator::SwapCase))
            .bind_spec(Layer::Visual, "v", Cmd(C::EnterVisual(VisualKind::Char)))
            .bind_spec(Layer::Visual, "V", Cmd(C::EnterVisual(VisualKind::Line)));

        // -- insert ----------------------------------------------------------
        map.bind_spec(Layer::Insert, "<Esc>", Cmd(C::EnterNormal))
            .bind_spec(Layer::Insert, "<C-c>", Cmd(C::EnterNormal))
            .bind_spec(Layer::Insert, "<CR>", Cmd(C::InsertNewline))
            .bind_spec(Layer::Insert, "<BS>", Cmd(C::DeleteBack))
            .bind_spec(Layer::Insert, "<C-w>", Cmd(C::DeleteWordBack))
            .bind_spec(Layer::Insert, "<Tab>", Cmd(C::InsertText('\t')))
            .bind_spec(Layer::Insert, "<Left>", Cmd(C::Move(Motion::Left)))
            .bind_spec(Layer::Insert, "<Right>", Cmd(C::Move(Motion::Right)))
            .bind_spec(Layer::Insert, "<Up>", Cmd(C::Move(Motion::Up)))
            .bind_spec(Layer::Insert, "<Down>", Cmd(C::Move(Motion::Down)));

        // -- text objects ----------------------------------------------------
        for (ch, object) in [
            ('w', TextObject::Word { big: false }),
            ('W', TextObject::Word { big: true }),
            ('p', TextObject::Paragraph),
        ] {
            map.bind_object(Key::char(ch), object);
        }
        for (open, close, alias) in [
            ('(', ')', Some('b')),
            ('{', '}', Some('B')),
            ('[', ']', None),
            ('<', '>', None),
        ] {
            let object = TextObject::Delimited { open, close };
            map.bind_object(Key::char(open), object)
                .bind_object(Key::char(close), object);
            if let Some(alias) = alias {
                map.bind_object(Key::char(alias), object);
            }
        }
        for quote in ['"', '\'', '`'] {
            map.bind_object(Key::char(quote), TextObject::Quoted(quote));
        }

        map
    }
}

fn walk(map: &BTreeMap<Vec<Key>, Binding>, path: &[Key]) -> Walk {
    if path.is_empty() {
        return Walk::Prefix;
    }
    if let Some(binding) = map.get(path) {
        return Walk::Bound(binding.clone());
    }
    // The first sequence at or after `path` is the only candidate: anything
    // extending `path` sorts immediately after it, so one lookup settles it.
    match map.range(path.to_vec()..).next() {
        Some((sequence, _)) if sequence.starts_with(path) => Walk::Prefix,
        _ => Walk::Unbound,
    }
}

/// Convenience for hosts: is this key a plain digit usable as a count?
#[must_use]
pub fn is_count_digit(key: Key) -> bool {
    matches!(key.code, KeyCode::Char(ch) if ch.is_ascii_digit()) && key.mods.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn walk(map: &Keymap, layer: Layer, spec: &str) -> Walk {
        map.walk(layer, &keys(spec).expect("valid test notation"))
    }

    #[test]
    fn lookup_distinguishes_hits_prefixes_and_unbound_paths() {
        let map = Keymap::vim();
        for (spec, expected) in [
            ("h", Walk::Bound(Binding::Motion(Motion::Left))),
            ("g", Walk::Prefix),
            ("gg", Walk::Bound(Binding::Motion(Motion::GotoFirstRow))),
            ("gx", Walk::Unbound),
        ] {
            assert_eq!(walk(&map, Layer::Normal, spec), expected, "{spec}");
        }
    }

    #[test]
    fn layers_fall_back_and_shadow() {
        let map = Keymap::vim();
        assert_eq!(
            walk(&map, Layer::Operator, "w"),
            Walk::Bound(Binding::Motion(Motion::WordForward { big: false }))
        );
        assert_eq!(
            walk(&map, Layer::Visual, "x"),
            Walk::Bound(Binding::Operator(Operator::Delete))
        );
        assert_eq!(walk(&map, Layer::Insert, "w"), Walk::Unbound);
    }

    #[test]
    fn keys_features_calls_unbound_really_are() {
        // Keep in step with FEATURES.txt §17. D/C were bound while both the
        // checklist and the README still called them unbound.
        for spec in ["s", "S", "{", "}", "<C-v>", "\"", "U"] {
            let path = keys(spec).expect("valid notation");
            assert_eq!(
                Keymap::vim().walk(Layer::Normal, &path),
                Walk::Unbound,
                "{spec} is bound but FEATURES §17 says it rings"
            );
        }

        let path = keys("<C-o>").expect("valid notation");
        assert_eq!(
            Keymap::vim().walk(Layer::Insert, &path),
            Walk::Unbound,
            "<C-o> is bound but FEATURES §17 says it rings in insert mode"
        );
    }

    #[test]
    fn binding_and_unbinding_replace_subtrees() {
        let mut map = Keymap::vim();
        map.bind_spec(Layer::Normal, "g", Binding::Motion(Motion::Left));
        assert_eq!(
            walk(&map, Layer::Normal, "g"),
            Walk::Bound(Binding::Motion(Motion::Left))
        );
        assert_eq!(walk(&map, Layer::Normal, "gg"), Walk::Unbound);

        let mut map = Keymap::empty();
        map.bind_spec(Layer::Normal, "d", Binding::Motion(Motion::Left));
        map.bind_spec(Layer::Normal, "dd", Binding::Motion(Motion::Right));
        assert_eq!(walk(&map, Layer::Normal, "d"), Walk::Prefix);
        map.unbind(Layer::Normal, &keys("d").expect("valid test notation"));
        assert_eq!(walk(&map, Layer::Normal, "d"), Walk::Unbound);
        assert_eq!(walk(&map, Layer::Normal, "dd"), Walk::Unbound);
    }

    #[test]
    fn custom_mappings_flow_to_inheriting_layers() {
        let mut map = Keymap::vim();
        map.bind_spec(Layer::Normal, "j", Binding::Motion(Motion::Up));
        assert_eq!(
            walk(&map, Layer::Operator, "j"),
            Walk::Bound(Binding::Motion(Motion::Up))
        );
    }
}
