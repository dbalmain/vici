//! The keymap: a prefix tree from key sequences to bindings.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

#[derive(Debug, Clone)]
enum Node {
    Leaf(Binding),
    Branch(BTreeMap<Key, Node>),
}

/// The result of walking a key path through one layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    normal: BTreeMap<Key, Node>,
    operator: BTreeMap<Key, Node>,
    visual: BTreeMap<Key, Node>,
    insert: BTreeMap<Key, Node>,
    objects: BTreeMap<Key, TextObject>,
}

impl Keymap {
    /// No bindings at all. Build your own scheme on top.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    fn layer(&self, layer: Layer) -> &BTreeMap<Key, Node> {
        match layer {
            Layer::Normal => &self.normal,
            Layer::Operator => &self.operator,
            Layer::Visual => &self.visual,
            Layer::Insert => &self.insert,
        }
    }

    fn layer_mut(&mut self, layer: Layer) -> &mut BTreeMap<Key, Node> {
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
        insert_path(self.layer_mut(layer), sequence, binding);
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
        remove_path(self.layer_mut(layer), sequence);
        self
    }

    /// Walk `path` through `layer`, falling back per [`Layer::fallback`].
    ///
    /// A layer that produces neither a binding nor a prefix defers to its
    /// fallback, so `Layer::Visual` inherits every normal-mode motion while still
    /// being able to shadow individual keys.
    #[must_use]
    pub fn walk(&self, layer: Layer, path: &[Key]) -> Walk {
        match walk_path(self.layer(layer), path) {
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
    /// text objects, counts, dot-repeat, macros, undo including `U`, and prompts
    /// for search and ex commands. No marks, no named registers.
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
            ("H", Motion::ScreenTop),
            ("M", Motion::ScreenMiddle),
            ("L", Motion::ScreenBottom),
            (";", Motion::RepeatFind { reverse: false }),
            (",", Motion::RepeatFind { reverse: true }),
        ] {
            map.bind_spec(Layer::Normal, spec, Move(motion));
        }

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
            .bind_spec(Layer::Normal, "U", Cmd(C::UndoRow))
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
        map.bind_spec(Layer::Normal, "/", Cmd(C::SearchPrompt { backward: false }))
            .bind_spec(Layer::Normal, "?", Cmd(C::SearchPrompt { backward: true }))
            .bind_spec(Layer::Normal, "n", Cmd(C::SearchRepeat { reverse: false }))
            .bind_spec(Layer::Normal, "N", Cmd(C::SearchRepeat { reverse: true }))
            .bind_spec(Layer::Normal, ":", Cmd(C::CommandPrompt));

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
            .bind_spec(Layer::Insert, "<C-o>", Cmd(C::OneShotNormal))
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

fn insert_path(map: &mut BTreeMap<Key, Node>, path: &[Key], binding: Binding) {
    let Some((first, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        map.insert(*first, Node::Leaf(binding));
        return;
    }
    let entry = map
        .entry(*first)
        .or_insert_with(|| Node::Branch(BTreeMap::new()));
    if !matches!(entry, Node::Branch(_)) {
        *entry = Node::Branch(BTreeMap::new());
    }
    let Node::Branch(children) = entry else {
        unreachable!("just replaced with a branch")
    };
    insert_path(children, rest, binding);
}

fn remove_path(map: &mut BTreeMap<Key, Node>, path: &[Key]) {
    let Some((first, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        map.remove(first);
        return;
    }
    if let Some(Node::Branch(children)) = map.get_mut(first) {
        remove_path(children, rest);
    }
}

fn walk_path(map: &BTreeMap<Key, Node>, path: &[Key]) -> Walk {
    let Some((first, rest)) = path.split_first() else {
        return Walk::Prefix;
    };
    match map.get(first) {
        Some(Node::Leaf(binding)) if rest.is_empty() => Walk::Bound(*binding),
        // Nothing here, or more keys after a complete binding: either way this
        // layer has no answer, so the caller may try its fallback.
        None | Some(Node::Leaf(_)) => Walk::Unbound,
        Some(Node::Branch(children)) => walk_path(children, rest),
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
    use crate::key::key as parse_key;

    fn walk(map: &Keymap, layer: Layer, spec: &str) -> Walk {
        map.walk(layer, &keys(spec).unwrap())
    }

    #[test]
    fn simple_lookup() {
        let map = Keymap::vim();
        assert_eq!(
            walk(&map, Layer::Normal, "h"),
            Walk::Bound(Binding::Motion(Motion::Left))
        );
        assert_eq!(
            walk(&map, Layer::Normal, "d"),
            Walk::Bound(Binding::Operator(Operator::Delete))
        );
        assert_eq!(
            walk(&map, Layer::Normal, ">"),
            Walk::Bound(Binding::Operator(Operator::ShiftRight))
        );
        assert_eq!(
            walk(&map, Layer::Normal, "<lt>"),
            Walk::Bound(Binding::Operator(Operator::ShiftLeft))
        );
    }

    #[test]
    fn prefixes_are_distinguished_from_bindings() {
        let map = Keymap::vim();
        assert_eq!(walk(&map, Layer::Normal, "g"), Walk::Prefix);
        assert_eq!(
            walk(&map, Layer::Normal, "gg"),
            Walk::Bound(Binding::Motion(Motion::GotoFirstRow))
        );
        assert_eq!(walk(&map, Layer::Normal, "gx"), Walk::Unbound);
        assert_eq!(walk(&map, Layer::Normal, "z"), Walk::Prefix);
    }

    #[test]
    fn i_means_insert_in_normal_but_inner_when_operating() {
        let map = Keymap::vim();
        assert_eq!(
            walk(&map, Layer::Normal, "i"),
            Walk::Bound(Binding::Command(Command::EnterInsert(InsertAt::Cursor)))
        );
        assert_eq!(
            walk(&map, Layer::Operator, "i"),
            Walk::Bound(Binding::ObjectScope(ObjectScope::Inner))
        );
    }

    #[test]
    fn operator_and_visual_layers_inherit_normal_motions() {
        let map = Keymap::vim();
        let expected = Walk::Bound(Binding::Motion(Motion::WordForward { big: false }));
        assert_eq!(walk(&map, Layer::Operator, "w"), expected);
        assert_eq!(walk(&map, Layer::Visual, "w"), expected);
        // Multi-key motions inherit too.
        assert_eq!(
            walk(&map, Layer::Operator, "gg"),
            Walk::Bound(Binding::Motion(Motion::GotoFirstRow))
        );
        // Shift binds in normal once: operator doubling and visual mode reach it
        // through the same fallback rather than special bindings.
        assert_eq!(
            walk(&map, Layer::Visual, ">"),
            Walk::Bound(Binding::Operator(Operator::ShiftRight))
        );
    }

    #[test]
    fn a_layer_can_shadow_normal() {
        let map = Keymap::vim();
        // `x` deletes a character in normal mode, the selection in visual mode.
        assert_eq!(
            walk(&map, Layer::Normal, "x"),
            Walk::Bound(Binding::Command(Command::DeleteChar { before: false }))
        );
        assert_eq!(
            walk(&map, Layer::Visual, "x"),
            Walk::Bound(Binding::Operator(Operator::Delete))
        );
    }

    #[test]
    fn insert_layer_does_not_inherit() {
        let map = Keymap::vim();
        assert_eq!(walk(&map, Layer::Insert, "w"), Walk::Unbound);
        assert_eq!(
            walk(&map, Layer::Insert, "<Esc>"),
            Walk::Bound(Binding::Command(Command::EnterNormal))
        );
    }

    #[test]
    fn binding_a_prefix_discards_its_subtree() {
        let mut map = Keymap::vim();
        assert_eq!(
            walk(&map, Layer::Normal, "gg"),
            Walk::Bound(Binding::Motion(Motion::GotoFirstRow))
        );
        map.bind_spec(Layer::Normal, "g", Binding::Motion(Motion::Left));
        assert_eq!(
            walk(&map, Layer::Normal, "g"),
            Walk::Bound(Binding::Motion(Motion::Left))
        );
        assert_eq!(walk(&map, Layer::Normal, "gg"), Walk::Unbound);
    }

    #[test]
    fn binding_under_a_leaf_converts_it_to_a_branch() {
        let mut map = Keymap::empty();
        map.bind_spec(Layer::Normal, "d", Binding::Motion(Motion::Left));
        map.bind_spec(Layer::Normal, "dd", Binding::Motion(Motion::Right));
        assert_eq!(walk(&map, Layer::Normal, "d"), Walk::Prefix);
        assert_eq!(
            walk(&map, Layer::Normal, "dd"),
            Walk::Bound(Binding::Motion(Motion::Right))
        );
    }

    #[test]
    fn unbind_removes_a_subtree() {
        let mut map = Keymap::vim();
        map.unbind(Layer::Normal, &keys("g").unwrap());
        assert_eq!(walk(&map, Layer::Normal, "gg"), Walk::Unbound);
        assert_eq!(walk(&map, Layer::Normal, "g"), Walk::Unbound);
    }

    #[test]
    fn rebinding_for_a_custom_scheme() {
        // The extensibility claim, exercised: swap `j`/`k` without touching
        // anything else.
        let mut map = Keymap::vim();
        map.bind_spec(Layer::Normal, "j", Binding::Motion(Motion::Up))
            .bind_spec(Layer::Normal, "k", Binding::Motion(Motion::Down));
        assert_eq!(
            walk(&map, Layer::Normal, "j"),
            Walk::Bound(Binding::Motion(Motion::Up))
        );
        // And the operator layer sees it, since it falls back to normal.
        assert_eq!(
            walk(&map, Layer::Operator, "j"),
            Walk::Bound(Binding::Motion(Motion::Up))
        );
    }

    #[test]
    fn text_objects() {
        let map = Keymap::vim();
        assert_eq!(
            map.object(Key::char('w')),
            Some(TextObject::Word { big: false })
        );
        let parens = TextObject::Delimited {
            open: '(',
            close: ')',
        };
        assert_eq!(map.object(Key::char('(')), Some(parens));
        assert_eq!(map.object(Key::char(')')), Some(parens));
        assert_eq!(map.object(Key::char('b')), Some(parens));
        assert_eq!(map.object(Key::char('"')), Some(TextObject::Quoted('"')));
        assert_eq!(map.object(Key::char('z')), None);
    }

    #[test]
    fn char_awaiting_bindings() {
        let map = Keymap::vim();
        assert_eq!(
            walk(&map, Layer::Normal, "f"),
            Walk::Bound(Binding::Await(AwaitChar::Find {
                backward: false,
                till: false
            }))
        );
        assert_eq!(
            walk(&map, Layer::Normal, "T"),
            Walk::Bound(Binding::Await(AwaitChar::Find {
                backward: true,
                till: true
            }))
        );
    }

    #[test]
    fn layer_of_mode() {
        assert_eq!(Layer::of(Mode::Normal), Layer::Normal);
        assert_eq!(Layer::of(Mode::Insert), Layer::Insert);
        assert_eq!(Layer::of(Mode::Replace), Layer::Insert);
        assert_eq!(Layer::of(Mode::Visual(VisualKind::Line)), Layer::Visual);
    }

    #[test]
    fn count_digits() {
        assert!(is_count_digit(parse_key("3").unwrap()));
        assert!(is_count_digit(parse_key("0").unwrap()));
        assert!(!is_count_digit(parse_key("w").unwrap()));
        assert!(!is_count_digit(parse_key("<C-3>").unwrap()));
    }
}
