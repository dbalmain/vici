//! An interactive harness for driving [`vici`] by hand.
//!
//! This is the host that `vici` is designed not to know about. Everything the
//! core refuses to own shows up here as real work:
//!
//! * translating a terminal event into a [`Key`],
//! * owning the viewport, and fulfilling [`Effect::Scroll`],
//! * expanding tabs and measuring display width,
//! * answering `:` prompts,
//! * touching the filesystem.
//!
//! Run it with `cargo run -p vici-harness -- [file]`, defaulting to `FEATURES.txt`.
//! `F10` quits from anywhere, `F2` hides the effect log, and `<C-c>` quits from
//! normal mode, so a wedged keymap can never trap you.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::PathBuf;

use ratatui::crossterm::cursor::SetCursorStyle;
use ratatui::crossterm::event::{self, Event, KeyCode as Ct, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use unicode_width::UnicodeWidthChar;

use vici::{
    Editor, Effect, Indent, Key, KeyCode, Mode, Mods, Scroll, Viewport, VisualKind, render,
};

/// Tab width. Purely a view decision — the buffer stores a single `\t` byte.
///
/// The core is handed this same number as [`Indent::tab_width`], because `<<` on
/// a tab-indented row has to remove what the screen shows. One constant with two
/// readers; a second copy is how the view and the shift arithmetic would drift.
const TABSTOP: usize = 8;
/// Columns one `>>` moves by. vim's `shiftwidth`.
const SHIFTWIDTH: usize = 4;
/// Render new indentation with tabs. Inverse of vim's `expandtab`.
const INDENT_WITH_TABS: bool = false;
const LOG_CAP: usize = 500;

fn main() -> io::Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("FEATURES.txt"), PathBuf::from);

    let (text, message) = match fs::read_to_string(&path) {
        Ok(text) => (text, format!("{}", path.display())),
        Err(err) => (String::new(), format!("{}: {err}", path.display())),
    };

    let mut terminal = ratatui::init();
    let mut app = App::new(path, &text, message);
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}

struct Prompt {
    input: String,
}

impl Prompt {
    const fn sigil() -> char {
        ':'
    }
}

struct App {
    editor: Editor,
    path: PathBuf,
    /// First visible row. The core does not own this.
    top: usize,
    /// Text-area height from the last frame, needed to fulfil `Scroll` effects.
    height: usize,
    log: VecDeque<(String, Color)>,
    prompt: Option<Prompt>,
    message: String,
    modified: bool,
    quit: bool,
    show_log: bool,
    cursor_shape: Option<Mode>,
}

impl App {
    fn new(path: PathBuf, text: &str, message: String) -> Self {
        Self {
            editor: Editor::from_text(text)
                .with_indent(Indent {
                    shift_width: SHIFTWIDTH,
                    tab_width: TABSTOP,
                    use_tabs: INDENT_WITH_TABS,
                })
                .with_viewport(Viewport {
                    top_row: 0,
                    height: 1,
                }),
            path,
            top: 0,
            height: 1,
            log: VecDeque::new(),
            prompt: None,
            message,
            modified: false,
            quit: false,
            show_log: true,
            cursor_shape: None,
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.quit {
            terminal.draw(|frame| self.draw(frame))?;
            self.sync_cursor_shape()?;
            if let Event::Key(ev) = event::read()? {
                if ev.kind == KeyEventKind::Press {
                    self.on_key(ev);
                }
            }
        }
        Ok(())
    }

    // -- input ---------------------------------------------------------------

    fn on_key(&mut self, ev: KeyEvent) {
        self.message.clear();

        // Harness controls, deliberately outside everything else so a wedged
        // keymap cannot swallow them.
        if ev.code == Ct::F(10) {
            self.quit = true;
            return;
        }
        if ev.code == Ct::F(2) {
            self.show_log = !self.show_log;
            return;
        }

        if self.prompt.is_some() {
            self.on_prompt_key(ev);
            return;
        }

        let Some(key) = translate(ev) else { return };

        // `<C-c>` is bound to `EnterNormal` in insert mode, so only claim it here.
        if self.editor.mode() == Mode::Normal
            && key.code == KeyCode::Char('c')
            && key.mods == Mods::CTRL
        {
            self.quit = true;
            return;
        }

        let effects = self.editor.handle_key(key);
        let mut scrolled = false;

        for effect in effects {
            self.log_effect(&effect);
            match effect {
                Effect::Edit(_) => self.modified = true,
                Effect::Scroll(scroll) => {
                    self.apply_scroll(scroll);
                    scrolled = true;
                }
                Effect::CommandPrompt => {
                    self.prompt = Some(Prompt {
                        input: String::new(),
                    });
                }
                Effect::Bell => self.say("bell"),
                Effect::ModeChanged(_)
                | Effect::RecordingStarted(_)
                | Effect::RecordingStopped(_) => {}
            }
        }

        // The core carries the caret for page effects; the host still owns where
        // the viewport lands, so do not immediately override that decision.
        if !scrolled {
            self.follow_cursor();
        }
    }

    fn on_prompt_key(&mut self, ev: KeyEvent) {
        match ev.code {
            Ct::Esc => self.prompt = None,
            Ct::Enter => {
                if let Some(prompt) = self.prompt.take() {
                    self.run_prompt(&prompt);
                }
            }
            Ct::Backspace => {
                let Some(prompt) = self.prompt.as_mut() else {
                    return;
                };
                if prompt.input.pop().is_none() {
                    self.prompt = None;
                }
            }
            Ct::Char(ch) => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.input.push(ch);
                }
            }
            _ => {}
        }
    }

    fn run_prompt(&mut self, prompt: &Prompt) {
        match prompt.input.trim() {
            "" => {}
            "w" => self.save(),
            "q" if self.modified => self.say("unsaved changes — `:q!` to discard"),
            "q" | "q!" => self.quit = true,
            "wq" | "x" => {
                self.save();
                self.quit = true;
            }
            other => {
                self.message =
                    format!("`:{other}` is a host concern, and this host only knows w, q, q!, wq");
            }
        }
    }

    fn say(&mut self, text: &str) {
        self.message.clear();
        self.message.push_str(text);
    }

    fn save(&mut self) {
        let text = self.editor.buffer().to_string();
        match fs::write(&self.path, text) {
            Ok(()) => {
                self.modified = false;
                self.message = format!("wrote {}", self.path.display());
            }
            Err(err) => self.message = format!("write failed: {err}"),
        }
    }

    // -- viewport ------------------------------------------------------------

    fn apply_scroll(&mut self, scroll: Scroll) {
        let height = self.height.max(1);
        let last = self.editor.buffer().len_rows().saturating_sub(1);
        let row = self.editor.cursor_point().row;
        self.top = match scroll {
            Scroll::HalfPageDown => (self.top + height / 2).min(last),
            Scroll::HalfPageUp => self.top.saturating_sub(height / 2),
            // Keep the host's view in step with the core's two-row page overlap.
            Scroll::PageDown => (self.top + height.saturating_sub(2).max(1)).min(last),
            Scroll::PageUp => self.top.saturating_sub(height.saturating_sub(2).max(1)),
            Scroll::Center => row.saturating_sub(height / 2),
            Scroll::Top => row,
            Scroll::Bottom => row.saturating_sub(height.saturating_sub(1)),
        };
        self.report_viewport();
    }

    fn follow_cursor(&mut self) {
        let row = self.editor.cursor_point().row;
        let height = self.height.max(1);
        if row < self.top {
            self.top = row;
        } else if row >= self.top + height {
            self.top = row + 1 - height;
        }
        self.report_viewport();
    }

    fn report_viewport(&mut self) {
        self.editor.set_viewport(Viewport {
            top_row: self.top,
            height: self.height,
        });
    }

    // -- rendering -----------------------------------------------------------

    fn draw(&mut self, frame: &mut Frame) {
        let [main, status] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
        // Long lines don't scroll horizontally, so the log pane gets out of the
        // way on `F2`.
        let log_width = if self.show_log { 38 } else { 0 };
        let [text_area, log_area] =
            Layout::horizontal([Constraint::Min(20), Constraint::Length(log_width)]).areas(main);

        let title = format!(
            " {}{} ",
            self.path.display(),
            if self.modified { " [+]" } else { "" }
        );
        let text_block = Block::bordered().title(title);
        let inner = text_block.inner(text_area);
        self.height = inner.height as usize;
        self.report_viewport();

        let gutter = decimals(self.editor.buffer().len_rows());
        frame.render_widget(
            Paragraph::new(self.buffer_lines(gutter)).block(text_block),
            text_area,
        );

        let log_block = Block::bordered().title(" effects ");
        let log_height = log_block.inner(log_area).height as usize;
        let lines: Vec<Line> = self
            .log
            .iter()
            .rev()
            .take(log_height)
            .rev()
            .map(|(text, colour)| Line::styled(text.clone(), Style::new().fg(*colour)))
            .collect();
        frame.render_widget(Paragraph::new(lines).block(log_block), log_area);

        frame.render_widget(Paragraph::new(self.status_line()), status);

        // Place the real terminal cursor, in display cells.
        if let Some(prompt) = self.prompt.as_ref() {
            let col = 1 + prompt.input.chars().count();
            frame.set_cursor_position(Position::new(
                status.x + u16::try_from(col).unwrap_or(u16::MAX),
                status.y,
            ));
        } else {
            let point = self.editor.cursor_point();
            if let Some(screen_row) = point.row.checked_sub(self.top) {
                if screen_row < self.height {
                    let col = gutter + 1 + self.cursor_display_col();
                    frame.set_cursor_position(Position::new(
                        inner.x + u16::try_from(col).unwrap_or(u16::MAX),
                        inner.y + u16::try_from(screen_row).unwrap_or(u16::MAX),
                    ));
                }
            }
        }
    }

    fn buffer_lines(&self, gutter: usize) -> Vec<Line<'static>> {
        let buf = self.editor.buffer();
        let selection = self.editor.selection();
        let rows = buf.len_rows();
        let gutter_style = Style::new().fg(Color::DarkGray);
        let selected = Style::new().bg(Color::Rgb(60, 60, 90));

        (self.top..(self.top + self.height).min(rows))
            .map(|row| {
                let range = buf.row_content_range(row);
                let text = buf.text_in(range.clone());

                let mut spans = vec![Span::styled(format!("{:>gutter$} ", row + 1), gutter_style)];
                let mut col = 0;

                // Split the row at the selection's byte boundaries, then expand
                // each piece with a running column so tab stops stay correct.
                let cut = selection.as_ref().and_then(|sel| {
                    let start = sel.start.max(range.start);
                    let end = sel.end.min(range.end);
                    (start < end).then(|| (start - range.start, end - range.start))
                });

                let mut push = |piece: &str, style: Style, col: &mut usize| {
                    let (expanded, next) = expand_from(piece, *col);
                    *col = next;
                    if !expanded.is_empty() {
                        spans.push(Span::styled(expanded, style));
                    }
                };

                if let Some((start, end)) = cut {
                    push(&text[..start], Style::new(), &mut col);
                    push(&text[start..end], selected, &mut col);
                    push(&text[end..], Style::new(), &mut col);
                } else {
                    push(&text, Style::new(), &mut col);
                }

                Line::from(spans)
            })
            .collect()
    }

    /// The cursor's column in display cells, which is the harness's business and
    /// not the core's — this is where CJK and tabs are accounted for.
    fn cursor_display_col(&self) -> usize {
        let point = self.editor.cursor_point();
        let buf = self.editor.buffer();
        let text = buf.text_in(buf.row_content_range(point.row));
        let upto = text.get(..point.col.min(text.len())).unwrap_or(&text);
        expand_from(upto, 0).1
    }

    fn status_line(&self) -> Line<'static> {
        if let Some(prompt) = self.prompt.as_ref() {
            return Line::from(format!("{}{}", Prompt::sigil(), prompt.input));
        }

        let (label, colour) = match self.editor.mode() {
            Mode::Normal => (" NORMAL ", Color::Blue),
            Mode::Insert => (" INSERT ", Color::Green),
            Mode::Replace => (" REPLACE ", Color::Red),
            Mode::Visual(VisualKind::Char) => (" VISUAL ", Color::Magenta),
            Mode::Visual(VisualKind::Line) => (" V-LINE ", Color::Magenta),
        };

        let point = self.editor.cursor_point();
        let mut spans = vec![
            Span::styled(
                label,
                Style::new()
                    .bg(colour)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                " {}:{} b{} ",
                point.row + 1,
                point.col,
                self.editor.cursor()
            )),
        ];

        let pending = self.editor.pending_keys();
        if !pending.is_empty() {
            spans.push(Span::styled(
                format!("cmd:{} ", render(pending)),
                Style::new().fg(Color::Yellow),
            ));
        }
        if let Some(reg) = self.editor.recording() {
            spans.push(Span::styled(
                format!("rec:@{reg} "),
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }
        let register = self.editor.register();
        if !register.text.is_empty() {
            let kind = if register.linewise { "line" } else { "char" };
            spans.push(Span::styled(
                format!("reg[{kind}]:{} ", ellipsis(&register.text, 16)),
                Style::new().fg(Color::DarkGray),
            ));
        }
        if !self.message.is_empty() {
            spans.push(Span::styled(
                self.message.clone(),
                Style::new().fg(Color::Cyan),
            ));
        }

        Line::from(spans)
    }

    /// A block cursor in normal mode and a bar in insert is the fastest way to
    /// see an off-by-one in cursor placement, which is why it is worth the escape
    /// codes.
    fn sync_cursor_shape(&mut self) -> io::Result<()> {
        let mode = self.editor.mode();
        if self.cursor_shape == Some(mode) {
            return Ok(());
        }
        self.cursor_shape = Some(mode);
        let shape = match mode {
            Mode::Insert => SetCursorStyle::SteadyBar,
            Mode::Replace => SetCursorStyle::SteadyUnderScore,
            Mode::Normal | Mode::Visual(_) => SetCursorStyle::SteadyBlock,
        };
        execute!(io::stdout(), shape)
    }

    fn log_effect(&mut self, effect: &Effect) {
        let (text, colour) = match effect {
            Effect::Edit(e) => (
                format!(
                    "edit  {}..{}→{}  ({},{})→({},{})",
                    e.start_byte,
                    e.old_end_byte,
                    e.new_end_byte,
                    e.start_point.row,
                    e.start_point.col,
                    e.new_end_point.row,
                    e.new_end_point.col,
                ),
                Color::Green,
            ),
            Effect::ModeChanged(mode) => (format!("mode  {mode:?}"), Color::Blue),
            Effect::Scroll(scroll) => (format!("scroll {scroll:?}"), Color::Cyan),
            Effect::CommandPrompt => ("command prompt :".to_owned(), Color::Yellow),
            Effect::Bell => ("bell".to_owned(), Color::Red),
            Effect::RecordingStarted(reg) => (format!("recording @{reg}"), Color::Red),
            Effect::RecordingStopped(reg) => (format!("recorded  @{reg}"), Color::Red),
        };
        self.log.push_back((text, colour));
        while self.log.len() > LOG_CAP {
            self.log.pop_front();
        }
    }
}

/// Translate a crossterm event into a [`Key`].
///
/// [`Key::new`] does the important part: crossterm reports `SHIFT` alongside an
/// already-uppercased char, and the core normalises that away so a keymap entry
/// for `$` matches here and in a browser alike.
fn translate(ev: KeyEvent) -> Option<Key> {
    let mut mods = Mods::NONE;
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        mods = mods | Mods::CTRL;
    }
    if ev.modifiers.contains(KeyModifiers::ALT) {
        mods = mods | Mods::ALT;
    }
    if ev.modifiers.contains(KeyModifiers::SHIFT) {
        mods = mods | Mods::SHIFT;
    }

    let code = match ev.code {
        Ct::Char(ch) => KeyCode::Char(ch),
        Ct::Esc => KeyCode::Esc,
        Ct::Enter => KeyCode::Enter,
        Ct::Tab => KeyCode::Tab,
        Ct::BackTab => {
            mods = mods | Mods::SHIFT;
            KeyCode::Tab
        }
        Ct::Backspace => KeyCode::Backspace,
        Ct::Delete => KeyCode::Delete,
        Ct::Insert => KeyCode::Insert,
        Ct::Left => KeyCode::Left,
        Ct::Right => KeyCode::Right,
        Ct::Up => KeyCode::Up,
        Ct::Down => KeyCode::Down,
        Ct::Home => KeyCode::Home,
        Ct::End => KeyCode::End,
        Ct::PageUp => KeyCode::PageUp,
        Ct::PageDown => KeyCode::PageDown,
        Ct::F(n) => KeyCode::F(n),
        _ => return None,
    };
    Some(Key::new(code, mods))
}

/// Expand tabs, continuing from display column `col`; returns the text and the
/// column after it.
fn expand_from(text: &str, mut col: usize) -> (String, usize) {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '\t' {
            let width = TABSTOP - col % TABSTOP;
            out.extend(std::iter::repeat_n(' ', width));
            col += width;
        } else {
            out.push(ch);
            col += ch.width().unwrap_or(0);
        }
    }
    (out, col)
}

fn decimals(n: usize) -> usize {
    n.to_string().len()
}

fn ellipsis(text: &str, max: usize) -> String {
    let flat = text.replace('\n', "⏎");
    if flat.chars().count() <= max {
        return flat;
    }
    flat.chars().take(max).chain(['…']).collect()
}
