//! The settings screen — a fifth screen, shown by `,` on the picker and by
//! `<prefix> ,` in a session: every setting the file has, a tab a table,
//! changed on the spot.
//!
//! It takes the terminal the way the help screen does ([`super::help`]), and
//! hands the same client back afterwards. Its rows are
//! [`crate::settings`]' — one per setting, so the screen cannot leave one out
//! — and every change it makes is applied as it is made ([`config::apply`]):
//! the theme shows on the screen itself on the next frame, the picker's
//! effects come back with the picker, and the rest reach what they govern as
//! the table in [`crate::settings`] says. Until a save the file is untouched,
//! and the tab row marks each tab with something unsaved on it.
//!
//! # What a row says
//!
//! The name, then the value, plain, whether it is the default or the file's.
//! Dim means one thing: the setting has no effect as things stand, and the row
//! says why at its end — `needs fade`. A `•` beside it, and the value in bold,
//! mean changed and not saved. The line under the rows is the selected
//! setting's help, and an error takes its place, in bold.
//!
//! # Keys
//!
//! Read raw rather than through [`super::translate`] alone: changing the
//! prefix means seeing the chord that is pressed, as the first-run screen does
//! ([`super::setup`]), and the same reading is what recognises the prefix
//! itself. That one is swallowed with the key after it, as on the help screen,
//! so a habitual `<prefix> d` cannot change a setting — but for `<prefix> ,`,
//! which closes the screen as it opened it.
//!
//! # The mouse
//!
//! By the picker's rules ([`super::app::App::on_mouse`]): pointing at a row
//! selects it, a press on the selected row arms its release to do what `Enter`
//! does, and the wheel moves the selection a row at a time, stopping at the
//! ends. A click on a tab opens it. A click ends an edit without keeping it, as
//! `Esc` does, and the wheel does not. Nothing is dragged.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, Event, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::app::Key;
use super::draw;
use super::prompt::Field;
use super::setup;
use crate::config::{self, Settings, ThemeColor};
use crate::error::Result;
use crate::keys::{self, BINDINGS};
use crate::palette::Rgb;
use crate::settings::{self as registry, Kind, Setting, Tab, Value};

/// The selected row's marker, and every other row's indent to match: the
/// picker's.
const MARKER: &str = "▸ ";
const INDENT: &str = "  ";

/// Between a row's name and its value, and its value and why it is dim.
const GAP: &str = "  ";

/// Columns a value is given. Wide enough for the session command at its
/// default, `nvim --headless --listen {sock}`.
const VALUE_WIDTH: usize = 31;

/// Beside a row whose value has changed and is not saved, and after a tab
/// holding one.
const UNSAVED: &str = "•";

/// Between the tab row's names.
const TAB_GAP: &str = "   ";

/// The find's cursor, as the picker's filter has it.
const CURSOR: &str = "▋";

/// Where a value too long for its room is cut.
const ELLIPSIS: &str = "…";

/// The terminal's own colours `←` and `→` step `[theme] color` through after
/// `terminal`, by their ANSI indices 1 to 6, and what each is called.
const COLOUR_NAMES: [&str; 6] = ["red", "green", "yellow", "blue", "magenta", "cyan"];

const DIM: Style = Style::new().add_modifier(Modifier::DIM);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const BAR: Style = Style::new().add_modifier(Modifier::REVERSED.union(Modifier::BOLD));

/// What the screen is doing between keys.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    /// Moving between rows, and changing what changes in place.
    Browse,
    /// The prefix being changed: the chord pressed so far, which `Enter`
    /// keeps.
    Chord(Option<u8>),
    /// A command's key being changed: the next key is it.
    Key,
    /// A number, a colour or the session command being typed.
    Text(Field),
}

/// `/`: what has been typed, and which of the settings it matches is
/// selected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Find {
    query: String,
    at: usize,
}

/// What one key or gesture left for the caller to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Stay,
    Close,
    /// `w`: write what is unsaved to the file.
    Save,
}

/// One key as the screen reads it: as every screen does, and as the chord the
/// first-run screen would make of it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Press {
    key: Key,
    chord: setup::Step,
}

fn press(k: KeyEvent) -> Press {
    let ctrl = k
        .modifiers
        .contains(ratatui::crossterm::event::KeyModifiers::CONTROL);
    Press {
        key: super::translate(k),
        chord: setup::interpret(k.code, ctrl),
    }
}

/// What the pointer is over: a tab's name on the tab row, or a row of
/// settings, by its place among the rows on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    Tab(Tab),
    Row(usize),
}

/// A mouse gesture, with what it landed on already resolved. The picker's
/// ([`super::app::Mouse`]), with the tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gesture {
    Hover(Option<Hit>),
    Press(Option<Hit>),
    Drag(Option<Hit>),
    Release,
    Up,
    Down,
}

fn gesture(m: MouseEvent, hit: Option<Hit>) -> Option<Gesture> {
    match m.kind {
        MouseEventKind::Moved => Some(Gesture::Hover(hit)),
        MouseEventKind::Down(MouseButton::Left) => Some(Gesture::Press(hit)),
        MouseEventKind::Drag(MouseButton::Left) => Some(Gesture::Drag(hit)),
        MouseEventKind::Up(MouseButton::Left) => Some(Gesture::Release),
        MouseEventKind::ScrollUp => Some(Gesture::Up),
        MouseEventKind::ScrollDown => Some(Gesture::Down),
        MouseEventKind::Down(_)
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => None,
    }
}

/// Which tab and row the screen was left on, for the next time it opens in
/// this run.
static SPOT: Mutex<Option<(Tab, usize)>> = Mutex::new(None);

/// The screen, between keys. Pure, as the picker's [`super::app::App`] is:
/// the caller reads the keys, applies what changed, and draws.
#[derive(Debug, Clone)]
struct State {
    /// The settings as changed here: what nvmux runs on.
    live: Settings,
    /// What the file holds. What `live` differs from it in is unsaved.
    saved: Settings,
    tab: Tab,
    /// The selected row, among the tab's.
    selected: usize,
    mode: Mode,
    find: Option<Find>,
    /// What the last key did, said on the hint row in place of the hints.
    message: Option<String>,
    /// Why the last key did not do what it asked, in bold in place of the
    /// selected setting's help.
    error: Option<String>,
    /// The row the left button went down on, and whether it was the selected
    /// one, until it comes up. See [`State::on_mouse`].
    pressed: Option<(usize, bool)>,
    /// The prefix has been pressed: the next key is swallowed.
    armed: bool,
    /// The settings a save was refused for because the file had changed the
    /// same ones: a second `w` with nothing changed since overwrites them.
    clash: Option<Settings>,
    /// The terminal's colours 1 to 6, for `[theme] color`.
    colours: [Rgb; 6],
}

impl State {
    fn new(live: Settings, saved: Settings, colours: [Rgb; 6]) -> Self {
        Self {
            live,
            saved,
            tab: Tab::Keys,
            selected: 0,
            mode: Mode::Browse,
            find: None,
            message: None,
            error: None,
            pressed: None,
            armed: false,
            clash: None,
            colours,
        }
    }

    /// The screen as nvmux has it now, on the tab and row it was last left
    /// on.
    fn open() -> Self {
        let mut state = Self::new(
            (*config::get()).clone(),
            (*config::saved()).clone(),
            terminal_colours(),
        );
        if let Some((tab, selected)) = *SPOT.lock().unwrap_or_else(|e| e.into_inner()) {
            state.tab = tab;
            state.selected = selected.min(state.rows().len().saturating_sub(1));
        }
        state
    }

    /// Keep where the screen is for the next time it opens.
    fn remember(&self) {
        *SPOT.lock().unwrap_or_else(|e| e.into_inner()) = Some((self.tab, self.selected));
    }

    /// The rows on screen: the tab's, or while finding, every setting that
    /// matches.
    fn rows(&self) -> Vec<&'static Setting> {
        match &self.find {
            Some(find) => matches(&find.query),
            None => registry::on(self.tab).collect(),
        }
    }

    /// Which of the rows on screen is selected.
    fn at(&self) -> usize {
        let at = match &self.find {
            Some(find) => find.at,
            None => self.selected,
        };
        at.min(self.rows().len().saturating_sub(1))
    }

    fn select(&mut self, at: usize) {
        match &mut self.find {
            Some(find) => find.at = at,
            None => self.selected = at,
        }
    }

    /// The selected setting, if there is one: a find that matches nothing
    /// has none.
    fn current(&self) -> Option<&'static Setting> {
        self.rows().get(self.at()).copied()
    }

    /// The settings changed and not saved, in the file's order.
    fn unsaved(&self) -> Vec<&'static Setting> {
        registry::changed(&self.live, &self.saved)
    }

    fn changed(&self, setting: &Setting) -> bool {
        setting.value(&self.live) != setting.value(&self.saved)
    }

    /// Whether a save now should overwrite what the file changed too: asked
    /// for by a second `w`, with nothing changed since the first.
    fn overwrite(&self) -> bool {
        self.clash.as_ref() == Some(&self.live)
    }

    /// What came of a save: on the hint row, whatever it was; and saved, the
    /// file holds what runs.
    fn report_save(&mut self, result: std::result::Result<config::Saved, crate::error::SaveError>) {
        self.clash = None;
        match result {
            Ok(saved) => {
                self.saved = self.live.clone();
                self.message = Some(saved.to_string());
            }
            Err(e) => {
                self.message = Some(match e.changed_in_file() {
                    Some(n) => {
                        self.clash = Some(self.live.clone());
                        format!(
                            "{e} — w again overwrites {}",
                            if n == 1 { "it" } else { "them" }
                        )
                    }
                    None => e.to_string(),
                });
            }
        }
    }

    /// The prefix's wait ran out with no key after it.
    fn disarm(&mut self) {
        self.armed = false;
    }

    /// A change this screen made that nvmux would not run on: put back.
    fn refused(&mut self, why: String, running: Settings) {
        self.live = running;
        self.error = Some(why);
    }

    fn on_key(&mut self, press: Press) -> Step {
        self.message = None;
        self.error = None;
        if self.armed {
            // The key after the prefix: a command elsewhere, and nothing
            // here — but for the one that opened the screen.
            self.armed = false;
            let settings = self.live.keys.bind.key(keys::Action::Settings);
            return if settings.is_some() && byte_of(press.key) == settings {
                Step::Close
            } else {
                Step::Stay
            };
        }
        if !matches!(self.mode, Mode::Chord(_))
            && press.chord == setup::Step::Choose(self.live.keys.prefix)
        {
            self.armed = true;
            return Step::Stay;
        }
        match self.mode.clone() {
            Mode::Chord(pending) => {
                self.on_chord(press.chord, pending);
                Step::Stay
            }
            Mode::Key => {
                self.on_capture(press.key);
                Step::Stay
            }
            Mode::Text(_) => {
                self.on_text(press.key);
                Step::Stay
            }
            Mode::Browse if self.find.is_some() => {
                self.on_find(press.key);
                Step::Stay
            }
            Mode::Browse => self.on_browse(press.key),
        }
    }

    fn on_browse(&mut self, key: Key) -> Step {
        let last = self.rows().len().saturating_sub(1);
        match key {
            Key::Up | Key::Char('k') | Key::CtrlP => self.selected = self.at().saturating_sub(1),
            Key::Down | Key::Char('j') | Key::CtrlN => self.selected = (self.at() + 1).min(last),
            Key::Home | Key::Char('g') => self.selected = 0,
            Key::End | Key::Char('G') => self.selected = last,
            Key::Tab | Key::Char(']') => self.turn(1),
            Key::BackTab | Key::Char('[') => self.turn(Tab::ALL.len() - 1),
            // Closes rather than quits, as on the help screen: quitting is
            // the picker's.
            Key::Esc | Key::Char('q') | Key::CtrlC => return Step::Close,
            Key::Char('/') => self.find = Some(Find::default()),
            Key::Char('w') if self.unsaved().is_empty() => {
                self.message = Some("nothing to save: every setting is as the file has it".into());
            }
            Key::Char('w') => return Step::Save,
            _ => {
                let Some(setting) = self.current() else {
                    return Step::Stay;
                };
                match key {
                    Key::Left | Key::Char('h') => self.step(setting, false),
                    Key::Right | Key::Char('l') => self.step(setting, true),
                    Key::Enter | Key::Char(' ') => self.activate(setting),
                    Key::Char('r') => self.reset(setting),
                    _ => {}
                }
            }
        }
        Step::Stay
    }

    /// Open the tab `by` along from this one, wrapping, on its first row.
    fn turn(&mut self, by: usize) {
        let at = Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0);
        self.tab = Tab::ALL[(at + by) % Tab::ALL.len()];
        self.selected = 0;
    }

    fn on_find(&mut self, key: Key) {
        let Some(find) = self.find.as_mut() else {
            return;
        };
        let found = matches(&find.query).len();
        match key {
            Key::Esc | Key::CtrlC => self.find = None,
            Key::Enter => self.go_to_match(),
            Key::Backspace => {
                find.query.pop();
                find.at = best(&find.query);
            }
            Key::Up | Key::CtrlP => find.at = find.at.saturating_sub(1),
            Key::Down | Key::CtrlN => find.at = (find.at + 1).min(found.saturating_sub(1)),
            Key::Char(c) => {
                find.query.push(c);
                find.at = best(&find.query);
            }
            _ => {}
        }
    }

    /// Leave the find for the selected match, on its own tab.
    fn go_to_match(&mut self) {
        let target = self.current();
        self.find = None;
        if let Some(setting) = target {
            self.tab = setting.tab;
            self.selected = registry::on(setting.tab)
                .position(|s| std::ptr::eq(s, setting))
                .unwrap_or(0);
        }
    }

    /// `←` or `→`: a switch turned over, a choice or a colour stepped through,
    /// wrapping, and a number stepped along its grid, held to its range. The
    /// rest change on `Enter`.
    fn step(&mut self, setting: &'static Setting, up: bool) {
        let value = match (setting.kind, setting.value(&self.live)) {
            (_, Value::Switch(on)) => Value::Switch(!on),
            (Kind::Choice { options, .. }, Value::Choice(name)) => {
                Value::Choice(cycle(options, |o| *o == name, up))
            }
            (Kind::Number { lo, hi, step, .. }, Value::Number(n)) => {
                Value::Number(along(n, step, up).clamp(lo, hi))
            }
            (Kind::Colour, Value::Colour(colour)) => {
                Value::Colour(cycle(&self.colour_options(), |c| *c == colour, up))
            }
            _ => return,
        };
        self.put(setting, value);
    }

    /// `Enter`, or a click on the selected row: what `→` does for a switch
    /// or a choice, and for the rest the edit it takes.
    fn activate(&mut self, setting: &'static Setting) {
        self.mode = match (setting.kind, setting.value(&self.live)) {
            (Kind::Switch { .. } | Kind::Choice { .. }, _) => return self.step(setting, true),
            (Kind::Prefix, _) => Mode::Chord(None),
            (Kind::Key(_), _) => Mode::Key,
            (_, Value::Number(n)) => Mode::Text(Field::with(n.to_string())),
            (_, Value::Text(command)) => Mode::Text(Field::with(command)),
            // A colour of the terminal's own is a hex once it is typed at:
            // `#` alone where nothing is chosen, to type after.
            (_, Value::Colour(ThemeColor::Terminal)) => Mode::Text(Field::with("#".into())),
            (_, Value::Colour(colour)) => Mode::Text(Field::with(colour.name())),
            _ => Mode::Browse,
        };
    }

    /// `r`: back to the default. A command's key goes back by the swap a
    /// key put on it any other way would make.
    fn reset(&mut self, setting: &'static Setting) {
        let default = setting.default_value();
        if setting.value(&self.live) == default {
            return;
        }
        match (setting.kind, default) {
            (Kind::Key(row), Value::Key(byte)) => self.set_key(row, byte),
            (_, value) => {
                self.put(setting, value);
            }
        }
        if self.message.is_none() && self.error.is_none() {
            self.message = Some(format!("reset: {} is back to its default", setting.path()));
        }
    }

    fn on_chord(&mut self, chord: setup::Step, pending: Option<u8>) {
        match chord {
            setup::Step::Choose(byte) => self.mode = Mode::Chord(Some(byte)),
            setup::Step::Reject(why) => self.error = Some(why),
            setup::Step::Confirm => match pending {
                Some(byte) => {
                    self.mode = Mode::Browse;
                    if let Some(prefix) = registry::find("keys.prefix") {
                        self.put(prefix, Value::Prefix(byte));
                    }
                }
                None => self.error = Some("press a Ctrl chord first".into()),
            },
            setup::Step::Skip => self.mode = Mode::Browse,
            setup::Step::Ignore => {
                self.error = Some("press Ctrl and a letter, or Ctrl-Space".into());
            }
        }
    }

    fn on_capture(&mut self, key: Key) {
        let label = match key {
            Key::Esc | Key::CtrlC => {
                self.mode = Mode::Browse;
                return;
            }
            Key::Enter => keys::ENTER_NAME.to_string(),
            Key::Char(c) => c.to_string(),
            _ => return,
        };
        let Some(Kind::Key(row)) = self.current().map(|s| s.kind) else {
            self.mode = Mode::Browse;
            return;
        };
        match keys::parse_key(&label) {
            Ok(byte) => {
                self.mode = Mode::Browse;
                self.set_key(row, byte);
            }
            Err(why) => self.error = Some(why),
        }
    }

    fn on_text(&mut self, key: Key) {
        let number = matches!(self.current().map(|s| s.kind), Some(Kind::Number { .. }));
        let Mode::Text(field) = &mut self.mode else {
            return;
        };
        match key {
            Key::Esc | Key::CtrlC => self.mode = Mode::Browse,
            Key::Enter => self.submit(),
            Key::Backspace => field.backspace(),
            Key::Left => field.left(),
            Key::Right => field.right(),
            Key::Home => field.home(),
            Key::End => field.end(),
            Key::Char(c) if !number || c.is_ascii_digit() => field.insert(c),
            _ => {}
        }
    }

    /// `Enter` on what has been typed: kept if it is a value the setting can
    /// take, and otherwise said why, with the typing left to go on with.
    fn submit(&mut self) {
        let (Some(setting), Mode::Text(field)) = (self.current(), &self.mode) else {
            return;
        };
        let text = field.input().trim().to_string();
        let value = match setting.kind {
            Kind::Number { lo, hi, .. } => match text.parse::<u64>() {
                Ok(n) if (lo..=hi).contains(&n) => Ok(Value::Number(n)),
                // Digits, and too many of them for the range or for a u64.
                _ if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
                    Err(format!("{} must be between {lo} and {hi}", setting.path()))
                }
                _ => Err(format!("{} must be a whole number", setting.path())),
            },
            Kind::Colour => ThemeColor::parse(&text).map(Value::Colour),
            Kind::Command => Ok(Value::Text(text)),
            _ => return,
        };
        match value {
            Ok(value) => {
                if self.put(setting, value) {
                    self.mode = Mode::Browse;
                }
            }
            Err(why) => self.error = Some(why),
        }
    }

    /// Put `value` in the settings, if they would still be ones nvmux runs
    /// on; and if not, say why. Whether it went in.
    fn put(&mut self, setting: &Setting, value: Value) -> bool {
        let mut next = self.live.clone();
        setting.set(&mut next, &value);
        self.commit(next)
    }

    /// Put the command on row `row` on `byte`. A command already there is
    /// put on the key this one leaves, and the hint row says so: the two
    /// trade, and putting the first back where it was trades them back.
    fn set_key(&mut self, row: usize, byte: u8) {
        let mut next = self.live.clone();
        let Some(old) = next.keys.bind.bindings().nth(row).map(|b| b.key) else {
            return;
        };
        if old == byte {
            return;
        }
        let other = next
            .keys
            .bind
            .bindings()
            .enumerate()
            .find(|(i, b)| *i != row && b.key == byte);
        if let Some((_, b)) = other {
            next.keys.bind.bind(b.hint, old);
            self.message = Some(format!(
                "swapped: {} is now on {}",
                b.hint,
                keys::key_label(old)
            ));
        }
        next.keys.bind.bind(BINDINGS[row].hint, byte);
        if !self.commit(next) {
            self.message = None;
        }
    }

    fn commit(&mut self, next: Settings) -> bool {
        match next.validate() {
            Ok(()) => {
                self.live = next;
                true
            }
            Err(why) => {
                self.error = Some(why);
                false
            }
        }
    }

    /// What `←` and `→` step `[theme] color` through: the terminal's own,
    /// and then its colours.
    fn colour_options(&self) -> Vec<ThemeColor> {
        std::iter::once(ThemeColor::Terminal)
            .chain(self.colours.iter().copied().map(ThemeColor::Mono))
            .collect()
    }

    fn on_mouse(&mut self, gesture: Gesture) -> Step {
        if matches!(gesture, Gesture::Press(_) | Gesture::Up | Gesture::Down) {
            self.message = None;
            self.error = None;
            self.armed = false;
        }
        if let Gesture::Press(Some(Hit::Tab(tab))) = gesture {
            self.find = None;
            self.mode = Mode::Browse;
            self.pressed = None;
            self.tab = tab;
            self.selected = 0;
            return Step::Stay;
        }
        if self.mode != Mode::Browse {
            // A click ends an edit without keeping it, as `Esc` does; the
            // wheel is not deliberate enough to.
            if matches!(gesture, Gesture::Press(_)) {
                self.mode = Mode::Browse;
                self.pressed = None;
            }
            return Step::Stay;
        }
        let rows = self.rows().len();
        match gesture {
            Gesture::Hover(Some(Hit::Row(i))) if i < rows => self.select(i),
            Gesture::Press(Some(Hit::Row(i))) if i < rows => {
                self.pressed = Some((i, i == self.at()));
                self.select(i);
            }
            Gesture::Press(_) => self.pressed = None,
            // Off the row it went down on, a press is no click.
            Gesture::Drag(hit) => {
                if self.pressed.is_some_and(|(i, _)| hit != Some(Hit::Row(i))) {
                    self.pressed = None;
                }
            }
            Gesture::Release => {
                if let Some((i, true)) = self.pressed.take() {
                    if i == self.at() {
                        if self.find.is_some() {
                            self.go_to_match();
                        } else if let Some(setting) = self.current() {
                            self.activate(setting);
                        }
                    }
                }
            }
            Gesture::Down => self.select((self.at() + 1).min(rows.saturating_sub(1))),
            Gesture::Up => self.select(self.at().saturating_sub(1)),
            Gesture::Hover(_) => {}
        }
        Step::Stay
    }

    /// A value as its row shows it.
    fn shown(&self, setting: &Setting, value: &Value) -> String {
        match value {
            Value::Switch(on) => if *on { "on" } else { "off" }.to_string(),
            Value::Number(n) => match setting.kind {
                Kind::Number {
                    zero: Some(zero), ..
                } if *n == 0 => zero.to_string(),
                Kind::Number { unit, .. } => format!("{n}{unit}"),
                _ => n.to_string(),
            },
            Value::Choice(name) => name.to_string(),
            Value::Prefix(byte) => keys::prefix_label(*byte),
            Value::Key(byte) => keys::key_label(*byte),
            Value::Text(text) => text.clone(),
            Value::Colour(ThemeColor::Terminal) => ThemeColor::Terminal.name(),
            // One of the terminal's own by its name, with the hex the file
            // gets; any other by its hex.
            Value::Colour(colour @ ThemeColor::Mono(rgb)) => {
                match self.colours.iter().position(|c| c == rgb) {
                    Some(i) => format!("{} ({})", COLOUR_NAMES[i], colour.name()),
                    None => colour.name(),
                }
            }
        }
    }
}

/// The terminal's colours 1 to 6, as it said they are, or xterm's where it
/// did not say.
fn terminal_colours() -> [Rgb; 6] {
    let ansi = crate::palette::get().map_or(crate::palette::XTERM_ANSI, |p| p.ansi);
    std::array::from_fn(|i| ansi[i + 1])
}

/// The byte a key is after the prefix, if it is one a command can be on.
fn byte_of(key: Key) -> Option<u8> {
    match key {
        Key::Enter => keys::parse_key(keys::ENTER_NAME).ok(),
        Key::Char(c) => u8::try_from(c).ok(),
        _ => None,
    }
}

/// The option after (or before) the one `here` finds, wrapping; the first
/// (or last) when it finds none.
fn cycle<T: Copy>(options: &[T], here: impl Fn(&T) -> bool, up: bool) -> T {
    let n = options.len();
    let next = match options.iter().position(here) {
        Some(at) if up => (at + 1) % n,
        Some(at) => (at + n - 1) % n,
        None if up => 0,
        None => n - 1,
    };
    options[next]
}

/// The next number along a grid of `step`: so a typed 1250 steps up to 1300,
/// and back down to 1200.
fn along(n: u64, step: u64, up: bool) -> u64 {
    if up {
        (n / step).saturating_mul(step).saturating_add(step)
    } else {
        n.div_ceil(step).saturating_mul(step).saturating_sub(step)
    }
}

/// Where a query's letters fall in `text`, in order, ignoring case: the
/// columns to underline, or `None` when they do not all fall.
fn subsequence(query: &str, text: &str) -> Option<Vec<usize>> {
    let mut at = Vec::new();
    let mut chars = text.chars().enumerate();
    for q in query.chars() {
        let q = q.to_ascii_lowercase();
        loop {
            let (i, c) = chars.next()?;
            if c.to_ascii_lowercase() == q {
                at.push(i);
                break;
            }
        }
    }
    Some(at)
}

/// The settings a find matches: by name, and by the whole path once the
/// query has a `.` in it — the path's letters would otherwise let a short
/// query match nearly everything.
fn matches(query: &str) -> Vec<&'static Setting> {
    registry::all()
        .iter()
        .filter(|s| {
            query.is_empty()
                || subsequence(query, s.name).is_some()
                || (query.contains('.') && subsequence(query, &s.path()).is_some())
        })
        .collect()
}

/// Where a query lands among its matches: the first name that starts with
/// it, else the first that holds it, else the first match.
fn best(query: &str) -> usize {
    let found = matches(query);
    let q = query.to_ascii_lowercase();
    found
        .iter()
        .position(|s| s.name.to_ascii_lowercase().starts_with(&q))
        .or_else(|| {
            found
                .iter()
                .position(|s| s.name.to_ascii_lowercase().contains(&q))
        })
        .unwrap_or(0)
}

/// Show the settings until the user closes them, on a terminal of its own,
/// handing the session back afterwards.
pub fn run() -> Result<()> {
    // Reached from a session that has just dissolved out, so dissolve in.
    super::owning(|terminal| run_on(terminal, true))
}

/// Show the settings on a terminal the caller already owns — how the picker
/// answers `,`. `animate` is whether to dissolve in on the way in and out on
/// the way out; from the picker the screen is already up, so it does not.
pub(super) fn run_on(terminal: &mut ratatui::DefaultTerminal, animate: bool) -> Result<()> {
    let mut state = State::open();
    if animate {
        crate::fade::fade_in(terminal, |f| draw(f, &state))?;
    }
    // When the prefix's wait runs out, while it is armed: the clock lives
    // here, so the state stays pure, as `pty::pump` keeps the relay's.
    let mut armed_until: Option<Instant> = None;
    loop {
        // The area kept for the mouse: a click is resolved against the screen
        // as it was last drawn, which is the one the user clicked on.
        let area = terminal
            .draw(|f| {
                draw(f, &state);
                crate::theme::paint(f.buffer_mut());
            })?
            .area;

        if !event::poll(super::TICK)? {
            if armed_until.is_some_and(|until| Instant::now() >= until) {
                armed_until = None;
                state.disarm();
            }
            continue;
        }
        let step = match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => state.on_key(press(k)),
            Event::Mouse(m) => match gesture(m, hit(&state, area, m.column, m.row)) {
                Some(g) => state.on_mouse(g),
                None => continue,
            },
            _ => continue,
        };
        armed_until = state.armed.then(|| {
            armed_until.unwrap_or_else(|| {
                Instant::now() + Duration::from_millis(state.live.keys.timeout_ms)
            })
        });

        // What the key changed, nvmux runs on from now: the next frame
        // already shows a new theme.
        let running = config::get();
        if state.live != *running {
            if let Err(why) = config::apply(state.live.clone()) {
                state.refused(why, (*running).clone());
            }
        }

        if step == Step::Save {
            let result = config::save(&state.live, &state.saved, state.overwrite());
            if result.is_ok() {
                config::set_saved(state.live.clone());
            }
            state.report_save(result);
        }

        if step == Step::Close {
            state.remember();
            if animate {
                crate::fade::fade_out(terminal, |f| draw(f, &state))?;
            }
            return Ok(());
        }
    }
}

/// Where the screen's parts go on `area`: one function for the drawing and
/// for the mouse, so a click lands on what the eye sees.
#[derive(Debug, Clone, Copy)]
struct Layout {
    /// The tab row, or where the find's query goes in its place.
    tabs: Option<u16>,
    /// The rows of settings.
    body: Rect,
    /// The selected setting's help.
    help: Option<u16>,
    hint: Rect,
    /// Where the rows' block starts, and how wide it is.
    x: u16,
    width: u16,
}

/// How wide a row's name column is: the longest name's.
fn name_width() -> usize {
    registry::all()
        .iter()
        .map(|s| s.name.width())
        .max()
        .unwrap_or(0)
}

fn layout(area: Rect) -> Option<Layout> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let (_, hint) = draw::split_hint_row(area);
    let h = area.height;
    // A row of space either side of the tabs, and above the help, where there
    // is room for it; one row at a time as there is not.
    let (tabs, body, help) = if h >= 8 {
        (Some(area.y + 1), (area.y + 3, h - 6), Some(area.y + h - 2))
    } else if h >= 4 {
        (Some(area.y), (area.y + 1, h - 3), Some(area.y + h - 2))
    } else if h >= 2 {
        (Some(area.y), (area.y + 1, h - 2), None)
    } else {
        (None, (area.y, 0), None)
    };
    let full = INDENT.width() + name_width() + GAP.width() + VALUE_WIDTH;
    let width = (full as u16).min(area.width.saturating_sub(4)).max(1);
    Some(Layout {
        tabs,
        body: Rect::new(area.x, body.0, area.width, body.1),
        help,
        hint,
        x: area.x + (area.width.saturating_sub(width)) / 2,
        width,
    })
}

/// One line of the rows' block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    /// A row of settings, by its place among the rows on screen.
    Row(usize),
    /// While finding, a tab's name over its matches.
    Head(Tab),
    Blank,
}

/// The block's lines: the tab's rows, with a blank line where the commands'
/// keys begin after the prefix's settings; or while finding, the matches under
/// their tabs' names.
fn lines(state: &State) -> Vec<Line> {
    let rows = state.rows();
    let mut lines = Vec::new();
    for (i, setting) in rows.iter().enumerate() {
        let before = i.checked_sub(1).map(|j| rows[j]);
        if state.find.is_some() {
            if before.is_none_or(|b| b.tab != setting.tab) {
                if before.is_some() {
                    lines.push(Line::Blank);
                }
                lines.push(Line::Head(setting.tab));
            }
        } else if before.is_some_and(|b| is_command(b) != is_command(setting)) {
            // Not at every table: each effect is a table of its own, and the
            // effects read as one list.
            lines.push(Line::Blank);
        }
        lines.push(Line::Row(i));
    }
    lines
}

/// Whether a row is a command's key, under `[keys.bind]`.
fn is_command(setting: &Setting) -> bool {
    matches!(setting.kind, Kind::Key(_))
}

/// Which line goes on the body's first row, and how many rows down it goes:
/// centred when the lines fit, and otherwise scrolled to keep the selection
/// in the middle.
fn placed(lines: &[Line], selected: usize, height: usize) -> (usize, usize) {
    if lines.len() <= height {
        return (0, (height - lines.len()) / 2);
    }
    let at = lines
        .iter()
        .position(|l| *l == Line::Row(selected))
        .unwrap_or(0);
    (at.saturating_sub(height / 2).min(lines.len() - height), 0)
}

/// Where each tab's name starts on the tab row.
fn tab_columns(area: Rect) -> Vec<(Tab, u16)> {
    let names: Vec<&str> = Tab::ALL.iter().map(|t| t.name()).collect();
    let width = names.join(TAB_GAP).width() as u16;
    let mut x = area.x + area.width.saturating_sub(width) / 2;
    Tab::ALL
        .iter()
        .map(|tab| {
            let at = x;
            x += (tab.name().width() + TAB_GAP.width()) as u16;
            (*tab, at)
        })
        .collect()
}

/// What is under terminal cell (`column`, `row`), with `area` the frame as
/// last drawn. A row is the whole width of the terminal, as the picker's
/// are; a tab is its name and a column either side.
fn hit(state: &State, area: Rect, column: u16, row: u16) -> Option<Hit> {
    let l = layout(area)?;
    if column < area.x || column >= area.x + area.width {
        return None;
    }
    if state.find.is_none() && l.tabs == Some(row) {
        return tab_columns(area).into_iter().find_map(|(tab, x)| {
            let end = x + tab.name().width() as u16;
            (column + 1 >= x && column <= end).then_some(Hit::Tab(tab))
        });
    }
    if row < l.body.y || row >= l.body.y + l.body.height {
        return None;
    }
    let lines = lines(state);
    let (offset, top) = placed(&lines, state.at(), usize::from(l.body.height));
    let line = usize::from(row - l.body.y).checked_sub(top)? + offset;
    match lines.get(line)? {
        Line::Row(i) => Some(Hit::Row(*i)),
        Line::Head(_) | Line::Blank => None,
    }
}

fn draw(frame: &mut Frame, state: &State) {
    let area = frame.area();
    let Some(l) = layout(area) else {
        return;
    };
    let current = state.current();
    {
        let buf = frame.buffer_mut();
        if let Some(y) = l.tabs {
            match &state.find {
                Some(find) => centred(
                    buf,
                    area,
                    y,
                    &format!("/{}{CURSOR}", find.query),
                    Style::new(),
                ),
                None => draw_tabs(buf, area, y, state),
            }
        }
        draw_rows(buf, &l, state);
        if let Some(y) = l.help {
            match (&state.error, current) {
                (Some(error), _) => centred(buf, area, y, error, BOLD),
                (None, Some(setting)) => centred(buf, area, y, setting.help, Style::new()),
                (None, None) => {}
            }
        }
    }
    let (text, dim) = hints(state, current, usize::from(area.width.saturating_sub(2)));
    draw::draw_hint_row(frame, l.hint, &text, dim);
}

/// `text` centred on row `y`, cut to the screen with a `…` where it is cut.
fn centred(buf: &mut Buffer, area: Rect, y: u16, text: &str, style: Style) {
    let text = clip(text, usize::from(area.width));
    let x = area.x + area.width.saturating_sub(text.width() as u16) / 2;
    buf.set_stringn(x, y, &text, usize::from(area.width), style);
}

/// `text` cut to `max` columns, its last one a `…` where anything was cut.
fn clip(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let mut cut = draw::truncate(text, max.saturating_sub(ELLIPSIS.width()));
    cut.push_str(ELLIPSIS);
    cut
}

fn draw_tabs(buf: &mut Buffer, area: Rect, y: u16, state: &State) {
    let unsaved = state.unsaved();
    for (tab, x) in tab_columns(area) {
        let style = if tab == state.tab {
            BOLD.add_modifier(Modifier::UNDERLINED)
        } else {
            DIM
        };
        let end = area.x + area.width;
        if x >= end {
            break;
        }
        buf.set_stringn(x, y, tab.name(), usize::from(end - x), style);
        let after = x + tab.name().width() as u16;
        if after < end && unsaved.iter().any(|s| s.tab == tab) {
            buf.set_string(after, y, UNSAVED, DIM);
        }
    }
}

fn draw_rows(buf: &mut Buffer, l: &Layout, state: &State) {
    let rows = state.rows();
    if rows.is_empty() {
        if state.find.is_some() && l.body.height > 0 {
            let y = l.body.y + l.body.height / 2;
            centred(buf, l.body, y, "no setting matches", DIM);
        }
        return;
    }
    let lines = lines(state);
    let selected = state.at();
    let height = usize::from(l.body.height);
    let (offset, top) = placed(&lines, selected, height);
    let query = state.find.as_ref().map(|f| f.query.as_str());
    for (n, line) in lines.iter().skip(offset).take(height - top).enumerate() {
        let y = l.body.y + (top + n) as u16;
        match line {
            Line::Row(i) => draw_row(buf, l, y, state, rows[*i], *i == selected, query),
            Line::Head(tab) => {
                buf.set_stringn(l.x, y, tab.name(), usize::from(l.width), DIM);
            }
            Line::Blank => {}
        }
    }
}

/// One setting's row: the marker or its indent, the name, the value, and why
/// it is dim if it is; a `•` left of it while it is unsaved. Selected, the
/// whole block's width is the bar.
fn draw_row(
    buf: &mut Buffer,
    l: &Layout,
    y: u16,
    state: &State,
    setting: &Setting,
    selected: bool,
    query: Option<&str>,
) {
    let width = usize::from(l.width);
    let changed = state.changed(setting);
    if changed && l.x >= 2 {
        buf.set_string(l.x - 2, y, UNSAVED, Style::new());
    }
    let head = format!(
        "{}{:<name$}{GAP}",
        if selected { MARKER } else { INDENT },
        setting.name,
        name = name_width()
    );
    let room = width.saturating_sub(head.width());
    let editing = selected && state.mode != Mode::Browse;
    let inert = setting.inert(&state.live);
    let (value, cursor) = match (&state.mode, editing) {
        (Mode::Chord(Some(byte)), true) => (keys::prefix_label(*byte), None),
        (Mode::Chord(None), true) => ("press a Ctrl chord…".to_string(), None),
        (Mode::Key, true) => ("press a key…".to_string(), None),
        (Mode::Text(field), true) => {
            let (text, at) = windowed(field, room);
            (text, Some(at))
        }
        _ => (
            clip(
                &state.shown(setting, &setting.value(&state.live)),
                room.min(VALUE_WIDTH),
            ),
            None,
        ),
    };
    let tag = inert.filter(|_| !editing);

    if selected {
        let mut text = format!("{head}{value}");
        if let Some(why) = tag {
            text.push_str(GAP);
            text.push_str(why);
        }
        let text = draw::truncate(&text, width);
        let pad = width.saturating_sub(text.width());
        buf.set_stringn(l.x, y, format!("{text}{}", " ".repeat(pad)), width, BAR);
        // The typing's cursor: the one cell the bar is not reversed on.
        if let Some(at) = cursor {
            let x = l.x + (head.width() + at) as u16;
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_style(Style::new().remove_modifier(Modifier::REVERSED));
            }
        }
        return;
    }

    let (head_style, value_style) = match (inert.is_some(), changed) {
        (true, _) => (DIM, DIM),
        (false, true) => (Style::new(), BOLD),
        (false, false) => (Style::new(), Style::new()),
    };
    let (x, _) = buf.set_stringn(l.x, y, &head, width, head_style);
    let (x, _) = buf.set_stringn(x, y, &value, room, value_style);
    if let Some(why) = tag {
        let used = value.width() + GAP.width();
        if room > used {
            buf.set_stringn(
                x + GAP.width() as u16,
                y,
                clip(why, room - used),
                room - used,
                DIM,
            );
        }
    }
    // A find underlines the letters it matched.
    if let Some(at) = query.and_then(|q| subsequence(q, setting.name)) {
        for i in at {
            let x = l.x + (INDENT.width() + i) as u16;
            if x < l.x + l.width {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_style(Style::new().add_modifier(Modifier::UNDERLINED));
                }
            }
        }
    }
}

/// What is being typed, cut to `room` columns around the cursor, and the
/// column the cursor is on — after the last character when it is at the end,
/// where it stands on a blank.
fn windowed(field: &Field, room: usize) -> (String, usize) {
    let cells: Vec<char> = field.input().chars().chain([' ']).collect();
    let room = room.max(1);
    let cursor = field.cursor();
    let start = if cells.len() <= room {
        0
    } else {
        (cursor + 1).saturating_sub(room).min(cells.len() - room)
    };
    let end = (start + room).min(cells.len());
    let mut shown: Vec<char> = cells[start..end].to_vec();
    // A `…` where text runs on past either edge, but never over the cursor.
    if start > 0 && cursor != start {
        shown[0] = '…';
    }
    if end < cells.len() && cursor + 1 != end {
        if let Some(last) = shown.last_mut() {
            *last = '…';
        }
    }
    (shown.into_iter().collect(), cursor - start)
}

/// What the hint row says, and whether it is dim: what the last key did, or
/// the keys of the moment — the edit's, the find's, or the screen's, as
/// many of those as fit, the least needed dropped whole first.
fn hints(state: &State, current: Option<&Setting>, width: usize) -> (String, bool) {
    if let Some(message) = &state.message {
        return (message.clone(), false);
    }
    let text = match &state.mode {
        Mode::Chord(Some(_)) => "⏎ keep  esc cancel".to_string(),
        Mode::Chord(None) => "Ctrl and a letter, or Ctrl-Space  esc cancel".to_string(),
        Mode::Key => format!("a character, {} or ⏎  esc cancel", keys::SPACE_GLYPH),
        Mode::Text(_) => match current.map(|s| s.kind) {
            Some(Kind::Number { lo, hi, unit, .. }) => {
                format!("{lo}–{hi}{unit}  ⏎ done  esc cancel")
            }
            _ => "⏎ done  esc cancel".to_string(),
        },
        Mode::Browse if state.find.is_some() => {
            if current.is_some() {
                "↑↓ move  ⏎ go to it  esc clear".to_string()
            } else {
                "esc clear".to_string()
            }
        }
        Mode::Browse => match current {
            Some(setting) => fit(
                &[
                    (Some("↑↓ move"), 0),
                    (
                        Some(match setting.kind {
                            Kind::Prefix | Kind::Key(_) | Kind::Command => "⏎ change",
                            _ => "←→ change",
                        }),
                        1,
                    ),
                    (
                        matches!(setting.kind, Kind::Number { .. } | Kind::Colour)
                            .then_some("⏎ type"),
                        8,
                    ),
                    (Some("⇥ section"), 5),
                    (Some("/ find"), 6),
                    (
                        (setting.value(&state.live) != setting.default_value())
                            .then_some("r reset"),
                        7,
                    ),
                    ((!state.unsaved().is_empty()).then_some("w save"), 2),
                    (Some("esc back"), 3),
                ],
                width,
            ),
            None => "esc back".to_string(),
        },
    };
    (text, true)
}

/// The entries that fit `width`, dropping the one least needed — the
/// highest number — whole, one at a time, as the hint bar drops its own.
fn fit(entries: &[(Option<&str>, u8)], width: usize) -> String {
    let mut kept: Vec<(&str, u8)> = entries
        .iter()
        .filter_map(|(text, rank)| text.map(|t| (t, *rank)))
        .collect();
    let joined = |kept: &[(&str, u8)]| kept.iter().map(|(t, _)| *t).collect::<Vec<_>>().join("  ");
    while kept.len() > 1 && joined(&kept).width() > width {
        let worst = kept
            .iter()
            .enumerate()
            .max_by_key(|(_, (_, rank))| *rank)
            .map_or(0, |(i, _)| i);
        kept.remove(worst);
    }
    joined(&kept)
}

#[cfg(test)]
mod tests {
    use super::super::test_support;
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    /// The xterm colours, as a terminal that never answered stands in with.
    fn colours() -> [Rgb; 6] {
        std::array::from_fn(|i| crate::palette::XTERM_ANSI[i + 1])
    }

    fn state() -> State {
        State::new(Settings::default(), Settings::default(), colours())
    }

    fn on(tab: Tab) -> State {
        let mut s = state();
        s.tab = tab;
        s
    }

    fn key(state: &mut State, code: KeyCode) -> Step {
        state.on_key(press(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn ctrl(state: &mut State, c: char) -> Step {
        state.on_key(press(KeyEvent::new(
            KeyCode::Char(c),
            KeyModifiers::CONTROL,
        )))
    }

    fn typed(state: &mut State, text: &str) {
        for c in text.chars() {
            key(state, KeyCode::Char(c));
        }
    }

    /// Select the row called `name` on its tab.
    fn to(state: &mut State, name: &str) -> &'static Setting {
        let setting = registry::all()
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no setting called {name}"));
        state.tab = setting.tab;
        state.selected = registry::on(setting.tab)
            .position(|s| std::ptr::eq(s, setting))
            .expect("on its tab");
        setting
    }

    fn render(state: &State, w: u16, h: u16) -> Vec<String> {
        test_support::render(w, h, |f| draw(f, state))
    }

    /// The column `needle` starts at in `line`, the last time it is in it:
    /// columns, not bytes, since a `•` or a `▸` before it is three bytes.
    fn column(line: &str, needle: &str) -> u16 {
        let at = line
            .rfind(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {line:?}"));
        line[..at].width() as u16
    }

    /// The line the row of `name` is on.
    fn line_of<'a>(lines: &'a [String], name: &str) -> &'a String {
        lines
            .iter()
            .find(|l| {
                let t = l.trim_start_matches([' ', '•']).trim_start_matches(MARKER);
                t.split_whitespace().next() == Some(name)
            })
            .unwrap_or_else(|| panic!("no row {name} in {lines:#?}"))
    }

    #[test]
    fn a_switch_turns_over_on_either_arrow_and_on_enter() {
        let mut s = on(Tab::Effects);
        let smear = to(&mut s, "smear");
        assert_eq!(smear.value(&s.live), Value::Switch(false));
        key(&mut s, KeyCode::Right);
        assert_eq!(smear.value(&s.live), Value::Switch(true));
        key(&mut s, KeyCode::Left);
        assert_eq!(smear.value(&s.live), Value::Switch(false));
        key(&mut s, KeyCode::Enter);
        assert_eq!(smear.value(&s.live), Value::Switch(true));
        assert_eq!(s.unsaved().len(), 1, "one change, unsaved");
        key(&mut s, KeyCode::Char(' '));
        assert!(s.unsaved().is_empty(), "set back is no change at all");
    }

    #[test]
    fn a_choice_steps_through_its_options_wrapping() {
        let mut s = state();
        let ui = to(&mut s, "ui");
        key(&mut s, KeyCode::Right);
        assert_eq!(ui.value(&s.live), Value::Choice("nvim"));
        key(&mut s, KeyCode::Right);
        assert_eq!(ui.value(&s.live), Value::Choice("nvmux"));
        key(&mut s, KeyCode::Left);
        assert_eq!(ui.value(&s.live), Value::Choice("nvim"));
    }

    /// Along the grid, so a typed value steps onto it, and held to the range.
    #[test]
    fn a_number_steps_along_its_grid_within_its_range() {
        let mut s = state();
        let timeout = to(&mut s, "timeout_ms");
        key(&mut s, KeyCode::Right);
        assert_eq!(timeout.value(&s.live), Value::Number(1100));
        s.live.keys.timeout_ms = 1250;
        key(&mut s, KeyCode::Left);
        assert_eq!(timeout.value(&s.live), Value::Number(1200));
        s.live.keys.timeout_ms = 1;
        key(&mut s, KeyCode::Left);
        assert_eq!(timeout.value(&s.live), Value::Number(1), "the floor holds");
        s.live.keys.timeout_ms = config::MAX_TIMEOUT_MS;
        key(&mut s, KeyCode::Right);
        assert_eq!(
            timeout.value(&s.live),
            Value::Number(config::MAX_TIMEOUT_MS)
        );
        assert_eq!(along(0, 10, false), 0);
        assert_eq!(along(1, 100, true), 100);
    }

    #[test]
    fn a_number_is_typed_and_held_to_its_range() {
        let mut s = state();
        let width = to(&mut s, "window_width");
        key(&mut s, KeyCode::Enter);
        assert!(matches!(s.mode, Mode::Text(_)));
        for _ in 0..3 {
            key(&mut s, KeyCode::Backspace);
        }
        typed(&mut s, "8x0");
        key(&mut s, KeyCode::Enter);
        assert_eq!(
            width.value(&s.live),
            Value::Number(80),
            "letters are not typed"
        );
        assert_eq!(s.mode, Mode::Browse);

        key(&mut s, KeyCode::Enter);
        typed(&mut s, "0000");
        key(&mut s, KeyCode::Enter);
        assert_eq!(
            s.error.as_deref(),
            Some("client.window_width must be between 0 and 65535")
        );
        assert!(
            matches!(s.mode, Mode::Text(_)),
            "the typing is still there to fix"
        );
        key(&mut s, KeyCode::Esc);
        assert_eq!(s.mode, Mode::Browse);
        assert_eq!(width.value(&s.live), Value::Number(80), "esc keeps nothing");
    }

    /// The terminal's own colours, by name, and a typed one by its hex.
    #[test]
    fn a_colour_steps_through_the_terminals_and_is_typed_as_hex() {
        let mut s = state();
        let colour = to(&mut s, "color");
        key(&mut s, KeyCode::Right);
        let Value::Colour(red) = colour.value(&s.live) else {
            panic!("a colour");
        };
        assert_eq!(s.shown(colour, &Value::Colour(red)), "red (#cd0000)");
        key(&mut s, KeyCode::Left);
        assert_eq!(colour.value(&s.live), Value::Colour(ThemeColor::Terminal));
        key(&mut s, KeyCode::Left);
        assert_eq!(
            s.shown(colour, &colour.value(&s.live)),
            "cyan (#00cdcd)",
            "back round to the last"
        );

        key(&mut s, KeyCode::Enter);
        for _ in 0..7 {
            key(&mut s, KeyCode::Backspace);
        }
        typed(&mut s, "#7aa2f7");
        key(&mut s, KeyCode::Enter);
        let typed_colour = colour.value(&s.live);
        assert_eq!(s.shown(colour, &typed_colour), "#7aa2f7");

        key(&mut s, KeyCode::Enter);
        typed(&mut s, "x");
        key(&mut s, KeyCode::Enter);
        assert!(
            s.error.as_deref().is_some_and(|e| e.starts_with("color ")),
            "{:?}",
            s.error
        );
    }

    /// As on the first-run screen: the chord pressed shows, `Enter` keeps it,
    /// and `Esc` keeps nothing.
    #[test]
    fn the_prefix_is_changed_by_pressing_the_chord_then_enter() {
        let mut s = state();
        let prefix = to(&mut s, "prefix");
        key(&mut s, KeyCode::Enter);
        assert_eq!(s.mode, Mode::Chord(None));
        key(&mut s, KeyCode::Enter);
        assert_eq!(s.error.as_deref(), Some("press a Ctrl chord first"));
        ctrl(&mut s, 'h');
        assert!(s.error.as_deref().is_some_and(|e| e.contains("Backspace")));
        ctrl(&mut s, 'a');
        assert_eq!(s.mode, Mode::Chord(Some(0x01)));
        assert_eq!(
            prefix.value(&s.live),
            Value::Prefix(keys::PREFIX),
            "not yet"
        );
        key(&mut s, KeyCode::Enter);
        assert_eq!(prefix.value(&s.live), Value::Prefix(0x01));
        assert_eq!(s.mode, Mode::Browse);

        key(&mut s, KeyCode::Enter);
        ctrl(&mut s, 't');
        key(&mut s, KeyCode::Esc);
        assert_eq!(
            prefix.value(&s.live),
            Value::Prefix(0x01),
            "esc keeps nothing"
        );
    }

    /// A key already taken trades places, and the hint row says so; putting
    /// the first command back trades them back.
    #[test]
    fn a_command_key_already_in_use_swaps() {
        let mut s = state();
        let detach = to(&mut s, "detach");
        key(&mut s, KeyCode::Enter);
        assert_eq!(s.mode, Mode::Key);
        key(&mut s, KeyCode::Char('c'));
        assert_eq!(detach.value(&s.live), Value::Key(b'c'));
        assert_eq!(s.live.keys.bind.command(b'd'), Some(keys::Action::Create));
        assert_eq!(s.message.as_deref(), Some("swapped: new is now on d"));

        key(&mut s, KeyCode::Enter);
        key(&mut s, KeyCode::Char('d'));
        assert_eq!(s.live, Settings::default(), "traded back");
    }

    #[test]
    fn a_command_key_must_be_one_a_command_can_be_on() {
        let mut s = state();
        let help = to(&mut s, "help");
        key(&mut s, KeyCode::Enter);
        key(&mut s, KeyCode::Char('1'));
        assert!(s.error.as_deref().is_some_and(|e| e.contains("digit")));
        assert_eq!(s.mode, Mode::Key, "still waiting for a key");
        key(&mut s, KeyCode::Enter);
        assert_eq!(help.value(&s.live), Value::Key(0x0d), "Enter is a key");
        key(&mut s, KeyCode::Enter);
        key(&mut s, KeyCode::Esc);
        assert_eq!(help.value(&s.live), Value::Key(0x0d), "esc keeps nothing");
    }

    /// A command that could never start a session is refused in the words a
    /// file would be, and left to fix.
    #[test]
    fn the_session_command_is_typed_and_must_name_the_socket() {
        let mut s = state();
        let command = to(&mut s, "command");
        key(&mut s, KeyCode::Enter);
        key(&mut s, KeyCode::Home);
        typed(&mut s, "env X=1 ");
        key(&mut s, KeyCode::Enter);
        assert_eq!(
            command.value(&s.live),
            Value::Text("env X=1 nvim --headless --listen {sock}".into())
        );
        key(&mut s, KeyCode::Enter);
        for _ in 0..6 {
            key(&mut s, KeyCode::Backspace);
        }
        key(&mut s, KeyCode::Enter);
        assert!(
            s.error
                .as_deref()
                .is_some_and(|e| e.starts_with("session.command ")),
            "{:?}",
            s.error
        );
        assert!(matches!(s.mode, Mode::Text(_)));
    }

    #[test]
    fn r_puts_a_setting_back_to_its_default() {
        let mut s = state();
        let timeout = to(&mut s, "timeout_ms");
        key(&mut s, KeyCode::Right);
        key(&mut s, KeyCode::Char('r'));
        assert_eq!(timeout.value(&s.live), timeout.default_value());
        assert_eq!(
            s.message.as_deref(),
            Some("reset: keys.timeout_ms is back to its default")
        );
        key(&mut s, KeyCode::Char('r'));
        assert_eq!(s.message, None, "nothing to reset, nothing said");

        // A command's key goes back by a swap, as any key would.
        let new = to(&mut s, "new");
        key(&mut s, KeyCode::Enter);
        key(&mut s, KeyCode::Char('d'));
        key(&mut s, KeyCode::Char('r'));
        assert_eq!(new.value(&s.live), Value::Key(b'c'));
        assert_eq!(s.live, Settings::default());
    }

    #[test]
    fn tabs_turn_both_ways_and_wrap() {
        let mut s = state();
        key(&mut s, KeyCode::Tab);
        assert_eq!(s.tab, Tab::Session);
        key(&mut s, KeyCode::Char(']'));
        assert_eq!(s.tab, Tab::Client);
        key(&mut s, KeyCode::BackTab);
        key(&mut s, KeyCode::Char('['));
        key(&mut s, KeyCode::Char('['));
        assert_eq!(s.tab, Tab::Effects, "back round");
        key(&mut s, KeyCode::Down);
        key(&mut s, KeyCode::Tab);
        assert_eq!((s.tab, s.selected), (Tab::Keys, 0), "on its first row");
    }

    #[test]
    fn the_selection_stops_at_the_ends() {
        let mut s = on(Tab::Client);
        key(&mut s, KeyCode::Up);
        assert_eq!(s.selected, 0);
        key(&mut s, KeyCode::End);
        let last = registry::on(Tab::Client).count() - 1;
        assert_eq!(s.selected, last);
        key(&mut s, KeyCode::Down);
        assert_eq!(s.selected, last);
    }

    /// Esc, `q` and Ctrl-c close the screen rather than quitting anything.
    #[test]
    fn esc_q_and_ctrl_c_close() {
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            assert_eq!(key(&mut state(), code), Step::Close, "{code:?}");
        }
        assert_eq!(ctrl(&mut state(), 'c'), Step::Close);
    }

    /// The prefix and the key after it are swallowed, as on the help screen,
    /// so `<prefix> d` changes nothing — but `<prefix> ,` closes.
    #[test]
    fn a_prefix_chord_is_swallowed_but_for_the_one_that_closes() {
        let mut s = on(Tab::Effects);
        to(&mut s, "smear");
        assert_eq!(ctrl(&mut s, ' '), Step::Stay);
        assert!(s.armed);
        assert_eq!(key(&mut s, KeyCode::Char(' ')), Step::Stay);
        assert_eq!(
            s.live,
            Settings::default(),
            "<prefix> Space changed nothing"
        );
        ctrl(&mut s, ' ');
        assert_eq!(key(&mut s, KeyCode::Char(',')), Step::Close);

        // On the prefix as it is now, and the key `settings` is on now.
        let mut s = state();
        s.live.keys.prefix = 0x01;
        s.live.keys.bind.bind("settings", b'S');
        ctrl(&mut s, ' ');
        assert!(!s.armed, "Ctrl-Space is no longer the prefix");
        ctrl(&mut s, 'a');
        assert_eq!(key(&mut s, KeyCode::Char(',')), Step::Stay);
        ctrl(&mut s, 'a');
        assert_eq!(key(&mut s, KeyCode::Char('S')), Step::Close);
    }

    /// Changing the prefix, the chord is what is being chosen, not the
    /// prefix arriving.
    #[test]
    fn the_prefix_is_a_chord_to_choose_while_the_prefix_is_changed() {
        let mut s = state();
        to(&mut s, "prefix");
        s.live.keys.prefix = 0x01;
        key(&mut s, KeyCode::Enter);
        ctrl(&mut s, 'a');
        assert!(!s.armed);
        assert_eq!(s.mode, Mode::Chord(Some(0x01)));
    }

    #[test]
    fn find_searches_every_tab_and_goes_to_the_setting() {
        let mut s = state();
        key(&mut s, KeyCode::Char('/'));
        typed(&mut s, "sm");
        let found: Vec<&str> = s.rows().iter().map(|r| r.name).collect();
        assert!(found.contains(&"smear"), "{found:?}");
        assert!(found.contains(&"smart_width"), "{found:?}");
        assert_eq!(
            s.current().map(|r| r.name),
            Some("smart_width"),
            "the first that starts so"
        );
        typed(&mut s, "e");
        assert_eq!(s.current().map(|r| r.name), Some("smear"));
        key(&mut s, KeyCode::Enter);
        assert_eq!(s.find, None);
        assert_eq!(s.tab, Tab::Effects);
        assert_eq!(s.current().map(|r| r.name), Some("smear"));
    }

    /// A query matches names alone, so a few letters do not match every
    /// path; with a `.` in it, the path too.
    #[test]
    fn find_goes_by_name_and_by_path_once_there_is_a_dot() {
        assert!(!matches("sm").iter().any(|s| s.name == "timeout_ms"));
        assert!(matches("fade.du")
            .iter()
            .any(|s| s.name == "fade.duration_ms"));
        assert!(matches("bind.go").iter().any(|s| s.name == "go"));
        assert_eq!(matches("").len(), registry::all().len());
    }

    #[test]
    fn esc_clears_a_find_and_changes_nothing() {
        let mut s = on(Tab::Client);
        key(&mut s, KeyCode::Down);
        key(&mut s, KeyCode::Char('/'));
        typed(&mut s, "blink");
        key(&mut s, KeyCode::Esc);
        assert_eq!((s.tab, s.selected, s.find.clone()), (Tab::Client, 1, None));
    }

    // --- saving -------------------------------------------------------------

    #[test]
    fn w_saves_what_is_unsaved_and_says_so_when_nothing_is() {
        let mut s = state();
        assert_eq!(key(&mut s, KeyCode::Char('w')), Step::Stay);
        assert_eq!(
            s.message.as_deref(),
            Some("nothing to save: every setting is as the file has it")
        );
        to(&mut s, "lazy");
        key(&mut s, KeyCode::Right);
        assert!(hints(&s, s.current(), 78).0.contains("w save"));
        assert_eq!(key(&mut s, KeyCode::Char('w')), Step::Save);

        s.report_save(Ok(config::Saved {
            path: "/x/config.toml".into(),
            count: 1,
        }));
        assert!(s.unsaved().is_empty(), "the file holds what runs");
        assert_eq!(
            s.message.as_deref(),
            Some("saved 1 change to /x/config.toml")
        );
        assert!(!hints(&s, s.current(), 78).0.contains("w save"));
    }

    /// A setting the file changed too is not overwritten until a second `w`,
    /// with nothing changed in between.
    #[test]
    fn a_second_w_overwrites_what_the_file_changed_too() {
        let mut s = state();
        to(&mut s, "lazy");
        key(&mut s, KeyCode::Right);
        assert!(!s.overwrite());
        s.report_save(Err(crate::error::SaveError::Changed {
            path: "/x/config.toml".into(),
            settings: vec!["client.lazy".into()],
        }));
        assert_eq!(
            s.message.as_deref(),
            Some("client.lazy changed in config.toml too — w again overwrites it")
        );
        assert!(s.overwrite(), "the next w overwrites");
        key(&mut s, KeyCode::Down);
        key(&mut s, KeyCode::Right);
        assert!(!s.overwrite(), "not once anything has changed since");
    }

    // --- the mouse ----------------------------------------------------------

    fn mouse(state: &mut State, g: Gesture) {
        state.on_mouse(g);
    }

    /// Pointing selects; a click on the selected row is `Enter`; a click on
    /// another row only selects it, for a terminal that does not report
    /// motion.
    #[test]
    fn a_click_on_the_selected_row_is_enter() {
        let mut s = on(Tab::Effects);
        let smear = registry::on(Tab::Effects)
            .position(|r| r.name == "smear")
            .unwrap();
        mouse(&mut s, Gesture::Hover(Some(Hit::Row(smear))));
        assert_eq!(s.selected, smear);
        mouse(&mut s, Gesture::Press(Some(Hit::Row(smear))));
        mouse(&mut s, Gesture::Release);
        assert_eq!(s.unsaved().len(), 1, "smear turned on");

        let fade = 1;
        mouse(&mut s, Gesture::Press(Some(Hit::Row(fade))));
        mouse(&mut s, Gesture::Release);
        assert_eq!(s.selected, fade);
        assert_eq!(s.unsaved().len(), 1, "a click elsewhere only selects");
        mouse(&mut s, Gesture::Press(Some(Hit::Row(fade))));
        mouse(&mut s, Gesture::Drag(Some(Hit::Row(fade + 1))));
        mouse(&mut s, Gesture::Release);
        assert_eq!(s.unsaved().len(), 1, "dragged off, no click");
    }

    #[test]
    fn the_wheel_steps_and_stops_at_the_ends() {
        let mut s = on(Tab::Client);
        mouse(&mut s, Gesture::Up);
        assert_eq!(s.selected, 0);
        mouse(&mut s, Gesture::Down);
        mouse(&mut s, Gesture::Down);
        assert_eq!(s.selected, 2);
        for _ in 0..20 {
            mouse(&mut s, Gesture::Down);
        }
        assert_eq!(s.selected, registry::on(Tab::Client).count() - 1);
    }

    #[test]
    fn a_click_on_a_tab_opens_it() {
        let mut s = state();
        mouse(&mut s, Gesture::Press(Some(Hit::Tab(Tab::Theme))));
        assert_eq!((s.tab, s.selected), (Tab::Theme, 0));
    }

    /// A click ends an edit, keeping nothing; the wheel does not.
    #[test]
    fn a_click_ends_an_edit_and_the_wheel_does_not() {
        let mut s = state();
        to(&mut s, "timeout_ms");
        key(&mut s, KeyCode::Enter);
        typed(&mut s, "5");
        mouse(&mut s, Gesture::Down);
        assert!(matches!(s.mode, Mode::Text(_)));
        mouse(&mut s, Gesture::Press(None));
        assert_eq!(s.mode, Mode::Browse);
        assert_eq!(s.live, Settings::default());
    }

    // --- the screen -----------------------------------------------------------

    /// The decided layout on 80×24: the tabs on row 1, the rows centred
    /// between, the help on row 22, the hints on the last.
    #[test]
    fn the_screen_is_tabs_rows_help_and_hints() {
        let s = on(Tab::Client);
        let lines = render(&s, 80, 24);
        assert_eq!(
            lines[1].trim(),
            "keys   session   client   theme   effects",
            "{lines:#?}"
        );
        assert!(line_of(&lines, "ui").contains("▸ ui"), "{lines:#?}");
        assert!(line_of(&lines, "per_session").contains("on"));
        assert_eq!(
            lines[22].trim(),
            "who draws a session: nvmux's client or Neovim's"
        );
        assert_eq!(
            lines[23].trim(),
            "↑↓ move  ←→ change  ⇥ section  / find  esc back"
        );
        test_support::assert_no_borders(&lines);
    }

    /// Dim means one thing — no effect as things stand — and the row says
    /// why; a value is plain whether it is the default or not.
    #[test]
    fn a_row_with_no_effect_is_dim_and_says_why() {
        let s = on(Tab::Client);
        let lines = render(&s, 80, 24);
        assert!(line_of(&lines, "window_width").ends_with("120  needs centre"));
        let buf = test_support::buffer(80, 24, |f| draw(f, &s));
        let y = lines
            .iter()
            .position(|l| l.contains("window_width"))
            .expect("the row") as u16;
        let x = column(&lines[usize::from(y)], "window_width");
        assert!(buf[(x, y)].modifier.contains(Modifier::DIM));
        let y = lines
            .iter()
            .position(|l| l.contains("per_session"))
            .unwrap() as u16;
        let x = column(&lines[usize::from(y)], "on");
        assert!(!buf[(x, y)]
            .modifier
            .intersects(Modifier::DIM | Modifier::BOLD));
    }

    /// Changed and unsaved: a `•` beside the row, the value in bold, and a
    /// `•` after its tab's name.
    #[test]
    fn an_unsaved_change_is_marked_on_its_row_and_its_tab() {
        let mut s = on(Tab::Effects);
        to(&mut s, "smear");
        key(&mut s, KeyCode::Right);
        key(&mut s, KeyCode::Up);
        let lines = render(&s, 80, 24);
        let row = line_of(&lines, "smear");
        assert!(row.trim_start().starts_with("•"), "{row:?}");
        assert!(lines[1].contains("effects•"), "{:?}", lines[1]);
        assert!(!lines[1].contains("keys•"));
        let buf = test_support::buffer(80, 24, |f| draw(f, &s));
        let y = lines.iter().position(|l| l == row).unwrap() as u16;
        let x = column(row, "on");
        assert!(buf[(x, y)].modifier.contains(Modifier::BOLD));
    }

    /// The keys tab holds two tables, a blank line between.
    #[test]
    fn a_blank_line_parts_the_prefix_from_the_commands() {
        let s = state();
        let lines = render(&s, 80, 24);
        let timeout = lines.iter().position(|l| l.contains("timeout_ms")).unwrap();
        assert_eq!(lines[timeout + 1], "");
        assert!(lines[timeout + 2].contains("picker"));
        assert!(line_of(&lines, "settings").contains(','));
    }

    #[test]
    fn a_find_shows_its_query_and_its_matches_under_their_tabs() {
        let mut s = state();
        key(&mut s, KeyCode::Char('/'));
        typed(&mut s, "zzz");
        let lines = render(&s, 80, 24);
        assert_eq!(lines[1].trim(), "/zzz▋");
        assert!(lines.iter().any(|l| l.trim() == "no setting matches"));
        assert_eq!(lines[23].trim(), "esc clear");
        key(&mut s, KeyCode::Backspace);
        key(&mut s, KeyCode::Backspace);
        key(&mut s, KeyCode::Backspace);
        typed(&mut s, "fade");
        let lines = render(&s, 80, 24);
        assert!(lines.iter().any(|l| l.trim() == "effects"), "{lines:#?}");
        assert!(line_of(&lines, "fade").contains("▸ fade"));
        assert_eq!(lines[23].trim(), "↑↓ move  ⏎ go to it  esc clear");
    }

    /// A row is the whole width of the terminal; a tab is its name, and a
    /// column either side.
    #[test]
    fn the_mouse_finds_rows_and_tabs_where_they_are_drawn() {
        let s = on(Tab::Client);
        let area = Rect::new(0, 0, 80, 24);
        let lines = render(&s, 80, 24);
        let y = lines.iter().position(|l| l.contains("lazy")).unwrap() as u16;
        let lazy = registry::on(Tab::Client)
            .position(|r| r.name == "lazy")
            .unwrap();
        assert_eq!(hit(&s, area, 0, y), Some(Hit::Row(lazy)));
        assert_eq!(hit(&s, area, 79, y), Some(Hit::Row(lazy)));
        assert_eq!(hit(&s, area, 40, 22), None, "the help is nothing");
        let x = column(&lines[1], "theme");
        assert_eq!(hit(&s, area, x - 1, 1), Some(Hit::Tab(Tab::Theme)));
        assert_eq!(hit(&s, area, x + 5, 1), Some(Hit::Tab(Tab::Theme)));
        assert_eq!(hit(&s, area, 0, 1), None);
    }

    /// On a short terminal the effects tab scrolls with the selection.
    #[test]
    fn a_short_terminal_scrolls_the_rows_with_the_selection() {
        let mut s = on(Tab::Effects);
        key(&mut s, KeyCode::End);
        let lines = render(&s, 80, 14);
        assert!(line_of(&lines, "blink").contains("▸ blink"), "{lines:#?}");
        assert!(!lines.iter().any(|l| l.contains("▸ enabled")));
    }

    #[test]
    fn the_hints_drop_whole_entries_to_fit() {
        let s = on(Tab::Client);
        let hints = |w| hints(&s, s.current(), w).0;
        assert_eq!(hints(78), "↑↓ move  ←→ change  ⇥ section  / find  esc back");
        let narrow = hints(30);
        assert!(narrow.width() <= 30, "{narrow:?}");
        assert!(narrow.starts_with("↑↓ move"), "{narrow:?}");
        assert!(!narrow.contains("/ fi"), "{narrow:?}");
    }

    #[test]
    fn typing_keeps_the_cursor_in_view() {
        let mut field = Field::with("nvim --headless --listen {sock}".into());
        let (text, at) = windowed(&field, 10);
        assert_eq!(text.chars().count(), 10);
        assert_eq!(at, 9, "the cursor on the blank past the end");
        assert!(text.starts_with('…'));
        field.home();
        let (text, at) = windowed(&field, 10);
        assert_eq!((at, text.starts_with('n')), (0, true));
        assert!(text.ends_with('…'));
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let mut s = on(Tab::Effects);
        for &(w, h) in test_support::TINY_SIZES {
            let _ = render(&s, w.max(1), h.max(1));
        }
        key(&mut s, KeyCode::Char('/'));
        for &(w, h) in test_support::TINY_SIZES {
            let _ = render(&s, w.max(1), h.max(1));
        }
    }

    /// No SGR colour, so the screen is correct under `NO_COLOR` by
    /// construction; the theme is a pass over it, as over every screen.
    #[test]
    fn nothing_sets_a_colour() {
        let mut s = on(Tab::Effects);
        to(&mut s, "smear");
        key(&mut s, KeyCode::Right);
        test_support::assert_no_colour(80, 24, |f| draw(f, &s));
    }
}
