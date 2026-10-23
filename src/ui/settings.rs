//! The settings screen (`,` on the picker, `<prefix> ,` in a session): one
//! tab per table and one row per [`crate::settings`] entry, so none can be
//! left out.
//!
//! Over the full-screen picker it takes the terminal like [`super::help`].
//! From a session (or the picker floating over one) it floats over the
//! running session in a relay-drawn [`Panel`] (see `crate::pty::panel`),
//! unless `[style] float` is off or the terminal is too small
//! ([`panel_area`]); then it goes full screen and hands the same client back
//! afterwards. Toggling `[style] float` here switches at once
//! ([`Outcome::Float`]).
//!
//! Changes apply immediately ([`config::apply`]); the file is untouched until
//! `w`, and tabs with unsaved changes are marked. A dim row has no effect as
//! things stand and says why (`needs fade`); a `•` and a bold value mark an
//! unsaved change. The line under the rows is the selected setting's help, or
//! an error in bold.
//!
//! Keys are read raw, not only via [`super::translate`], so changing the
//! prefix can see the chord (as [`super::setup`] does); that also detects the
//! prefix itself. The prefix and the next key are swallowed, as on the help
//! screen, so a habitual `<prefix> d` changes nothing; `<prefix> ,` closes.
//!
//! The mouse follows the picker ([`super::app::App::on_mouse`]); a click on a
//! tab opens it, and a click (not the wheel) cancels an edit. The row the bar
//! leaves fades as in the picker ([`super::effects`]); only the last row
//! left fades.

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
use super::effects::{self, AFTERGLOW};
use super::float::{inside, Did, Floating};
use super::prompt::Field;
use super::setup;
use crate::config::{self, Settings, ThemeColor};
use crate::error::Result;
use crate::keys::{self, BINDINGS};
use crate::palette::Rgb;
use crate::settings::{self as registry, Kind, Setting, Tab, Value};

/// The picker's selection marker, and the matching indent of other rows.
const MARKER: &str = "▸ ";
const INDENT: &str = "  ";

/// Between a row's name and its value, and its value and why it is dim.
const GAP: &str = "  ";

/// Value column width; fits the default session command,
/// `nvim --headless --listen {sock}`.
const VALUE_WIDTH: usize = 31;

/// Marks unsaved rows and the tabs holding them.
const UNSAVED: &str = "•";

const TAB_GAP: &str = "   ";

const CURSOR: &str = "▋";

const ELLIPSIS: &str = "…";

/// Names of ANSI colours 1 to 6, which `←`/`→` step `[style] color` through
/// after `terminal`.
const COLOUR_NAMES: [&str; 6] = ["red", "green", "yellow", "blue", "magenta", "cyan"];

const DIM: Style = Style::new().add_modifier(Modifier::DIM);
const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const BAR: Style = Style::new().add_modifier(Modifier::REVERSED.union(Modifier::BOLD));

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    /// Moving between rows; switches and choices change in place.
    Browse,
    /// Changing the prefix: the chord pressed so far, kept by `Enter`.
    Chord(Option<u8>),
    /// Changing a command's key: the next key is it.
    Key,
    /// A number, a colour or the session command being typed.
    Text(Field),
}

/// `/`: the query, and the selected match.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Find {
    query: String,
    at: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Stay,
    Close,
    /// `w`: write what is unsaved to the file.
    Save,
}

/// A key as every screen reads it, and as a first-run screen chord.
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

/// What the pointer is over: a tab, or a row by its index on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    Tab(Tab),
    Row(usize),
}

/// A mouse gesture with its target resolved: the picker's
/// ([`super::app::Mouse`]), plus tabs.
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

/// The tab and row last left on, restored on the next open in this run.
static SPOT: Mutex<Option<(Tab, usize)>> = Mutex::new(None);

/// The screen's state. Pure, like [`super::app::App`]: the caller reads
/// keys, applies changes and draws.
#[derive(Debug, Clone)]
struct State {
    /// The settings as changed here: what nvmux runs on.
    live: Settings,
    /// What the file holds; where `live` differs, it is unsaved.
    saved: Settings,
    tab: Tab,
    /// The selected row among the tab's (a find has its own).
    selected: usize,
    mode: Mode,
    find: Option<Find>,
    /// What the last key did, shown in place of the hints.
    message: Option<String>,
    /// Why the last key failed, shown in bold in place of the help.
    error: Option<String>,
    /// The row the left button went down on, and whether it was selected,
    /// until release. See [`State::on_mouse`].
    pressed: Option<(usize, bool)>,
    /// The prefix has been pressed: the next key is swallowed.
    armed: bool,
    /// Settings whose save was refused because the file changed the same
    /// ones; a second `w` with nothing changed since overwrites them.
    clash: Option<Settings>,
    /// The terminal's colours 1 to 6, for `[style] color`.
    colours: [Rgb; 6],
    /// The row the bar last left, fading. See [`State::tick`].
    glow: Option<Glow>,
}

#[derive(Debug, Clone, Copy)]
struct Glow {
    setting: &'static Setting,
    age: Duration,
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
            glow: None,
        }
    }

    /// Current settings, on the tab and row last left on.
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

    fn remember(&self) {
        *SPOT.lock().unwrap_or_else(|e| e.into_inner()) = Some((self.tab, self.selected));
    }

    /// The tab's rows, or while finding, every match.
    fn rows(&self) -> Vec<&'static Setting> {
        match &self.find {
            Some(find) => matches(&find.query),
            None => registry::on(self.tab).collect(),
        }
    }

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

    /// The selected setting; `None` when a find matches nothing.
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

    /// Whether to overwrite the file's own changes: a second `w` with nothing
    /// changed since the first.
    fn overwrite(&self) -> bool {
        self.clash.as_ref() == Some(&self.live)
    }

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

    /// The selected setting while it is a bar that leaves a glow: browsing or
    /// finding, not editing.
    fn cursor(&self) -> Option<&'static Setting> {
        (self.mode == Mode::Browse)
            .then(|| self.current())
            .flatten()
    }

    /// If the bar moved off `was`, start that row's glow, replacing any
    /// other. Skipped if the row left the screen (another tab, or filtered by
    /// a find) or with `[effects.cursor]` off.
    fn glow_from(&mut self, was: Option<&'static Setting>) {
        if !self.live.effects.cursor_enabled() {
            return;
        }
        let (Some(was), Some(now)) = (was, self.cursor()) else {
            return;
        };
        if std::ptr::eq(was, now) || !self.rows().iter().any(|s| std::ptr::eq(*s, was)) {
            return;
        }
        self.glow = Some(Glow {
            setting: was,
            age: Duration::ZERO,
        });
    }

    /// Advance the glow by `elapsed`; the caller keeps the clock so the state
    /// stays pure.
    fn tick(&mut self, elapsed: Duration) {
        if let Some(glow) = &mut self.glow {
            glow.age += elapsed;
            if glow.age >= AFTERGLOW {
                self.glow = None;
            }
        }
    }

    fn glowing(&self) -> bool {
        self.glow.is_some()
    }

    /// Revert a change nvmux cannot run on, showing why.
    fn refused(&mut self, why: String, running: Settings) {
        self.live = running;
        self.error = Some(why);
    }

    fn on_key(&mut self, press: Press) -> Step {
        let was = self.cursor();
        let step = self.on_key_mode(press);
        self.glow_from(was);
        step
    }

    fn on_key_mode(&mut self, press: Press) -> Step {
        self.message = None;
        self.error = None;
        if self.armed {
            // The key after the prefix does nothing here, except the one
            // that opened the screen.
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
            // Closes rather than quits, as on the help screen.
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

    /// `←`/`→`: toggle a switch, cycle a choice or colour, or step a number
    /// along its grid within its range. Other kinds change on `Enter`.
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

    /// `Enter` or a click on the selected row: `→` for a switch or choice,
    /// otherwise start the matching edit.
    fn activate(&mut self, setting: &'static Setting) {
        self.mode = match (setting.kind, setting.value(&self.live)) {
            (Kind::Switch { .. } | Kind::Choice { .. }, _) => return self.step(setting, true),
            (Kind::Prefix, _) => Mode::Chord(None),
            (Kind::Key(_), _) => Mode::Key,
            (_, Value::Number(n)) => Mode::Text(Field::with(n.to_string())),
            (_, Value::Text(command)) => Mode::Text(Field::with(command)),
            // The terminal's own colour has no hex: start typing from `#`.
            (_, Value::Colour(ThemeColor::Terminal)) => Mode::Text(Field::with("#".into())),
            (_, Value::Colour(colour)) => Mode::Text(Field::with(colour.name())),
            _ => Mode::Browse,
        };
    }

    /// `r`: back to the default. A command's key is reset via the same swap
    /// as any rebinding.
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

    /// `Enter` while typing: keep a valid value, or show why not and keep
    /// editing.
    fn submit(&mut self) {
        let (Some(setting), Mode::Text(field)) = (self.current(), &self.mode) else {
            return;
        };
        let text = field.input().trim().to_string();
        let value = match setting.kind {
            Kind::Number { lo, hi, .. } => match text.parse::<u64>() {
                Ok(n) if (lo..=hi).contains(&n) => Ok(Value::Number(n)),
                // All digits, but out of range or too big for a u64.
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

    /// Set `value` if the result validates, else show why. Returns whether
    /// it was set.
    fn put(&mut self, setting: &Setting, value: Value) -> bool {
        let mut next = self.live.clone();
        setting.set(&mut next, &value);
        self.commit(next)
    }

    /// Bind the command on row `row` to `byte`. A command already there takes
    /// the freed key (said on the hint row), so rebinding back swaps back.
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

    fn colour_options(&self) -> Vec<ThemeColor> {
        std::iter::once(ThemeColor::Terminal)
            .chain(self.colours.iter().copied().map(ThemeColor::Mono))
            .collect()
    }

    fn on_mouse(&mut self, gesture: Gesture) -> Step {
        let was = self.cursor();
        let step = self.on_mouse_mode(gesture);
        self.glow_from(was);
        step
    }

    fn on_mouse_mode(&mut self, gesture: Gesture) -> Step {
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
            // A click cancels an edit like `Esc`; the wheel is too casual to.
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
            // Dragged off its row, a press is no longer a click.
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
            // A terminal colour by name plus the hex the file gets; others
            // by hex.
            Value::Colour(colour @ ThemeColor::Mono(rgb)) => {
                match self.colours.iter().position(|c| c == rgb) {
                    Some(i) => format!("{} ({})", COLOUR_NAMES[i], colour.name()),
                    None => colour.name(),
                }
            }
        }
    }
}

/// The terminal's colours 1 to 6 as it reported them, else xterm's.
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

/// Case-insensitive subsequence match of `query` in `text`: the matched
/// columns (to underline), or `None`.
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

/// Settings matching a find: by name, and by path once the query has a `.`
/// (otherwise a short query would match nearly every path).
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Closed,
    /// `[style] float` was turned on with a session behind and room to float:
    /// dissolved out, to reopen floating over the returning session.
    Float,
}

/// Show the settings full screen until closed or turned to float
/// ([`Outcome::Float`]). `remote` is whether sessions are on another machine,
/// where Neovim's own client needs a local `nvim`.
pub fn run(remote: bool) -> Result<Outcome> {
    // Reached from a session that has just dissolved out, so dissolve in.
    super::owning(|terminal| run_on(terminal, true, remote, true))
}

/// Show the settings on a terminal the caller owns (the picker's `,`).
/// `animate` dissolves in and out; the picker is already up, so it does not.
/// `over_session` is whether a session is behind them to float over. Turned
/// to float, they always dissolve out.
pub(super) fn run_on(
    terminal: &mut ratatui::DefaultTerminal,
    animate: bool,
    remote: bool,
    over_session: bool,
) -> Result<Outcome> {
    let mut state = State::open();
    if animate {
        crate::fade::fade_in(terminal, |f| draw(f, &state))?;
    }
    // Clocks live here so the state stays pure (as in `pty::pump`): when the
    // armed prefix's wait runs out, and when the glow was last advanced.
    let mut armed_until: Option<Instant> = None;
    let mut ticked = Instant::now();
    // `[style] float` before the last key, to detect it being turned on.
    let mut floating = state.live.style.float;
    loop {
        tick(&mut state, &mut ticked);
        // Clicks resolve against the area last drawn, which the user saw.
        let area = terminal
            .draw(|f| {
                let area = f.area();
                paint_on(f.buffer_mut(), area, &state);
            })?
            .area;

        let wait = if state.glowing() {
            crate::fade::FRAME
        } else {
            super::TICK
        };
        if !event::poll(wait)? {
            if armed_until.is_some_and(|until| Instant::now() >= until) {
                armed_until = None;
                state.disarm();
            }
            continue;
        }
        let event = event::read()?;
        // Advance to the event first, so a glow it starts begins at zero.
        tick(&mut state, &mut ticked);
        let step = match event {
            Event::Key(k) if k.kind == KeyEventKind::Press => state.on_key(press(k)),
            Event::Mouse(m) => match gesture(m, hit(&state, area, m.column, m.row)) {
                Some(g) => state.on_mouse(g),
                None => continue,
            },
            _ => continue,
        };
        follow(&mut state, step, remote, &mut armed_until, Instant::now());

        let turned = turned_to_float(floating, &state.live, over_session, area);
        floating = state.live.style.float;
        if turned {
            state.remember();
            // Whatever opened them, the session comes next.
            crate::fade::fade_out(terminal, |f| draw(f, &state))?;
            return Ok(Outcome::Float);
        }
        if step == Step::Close {
            state.remember();
            if animate {
                crate::fade::fade_out(terminal, |f| draw(f, &state))?;
            }
            return Ok(Outcome::Closed);
        }
    }
}

/// Whether full-screen settings should hand over to floating: `[style] float`
/// just turned on, a session behind, and room on `screen` ([`panel_area`]).
/// Without room they stay, as they would open full screen anyway.
fn turned_to_float(was: bool, live: &Settings, over_session: bool, screen: Rect) -> bool {
    !was && live.style.float && over_session && panel_area(screen).is_some()
}

fn tick(state: &mut State, ticked: &mut Instant) {
    let now = Instant::now();
    state.tick(now.saturating_duration_since(*ticked));
    *ticked = now;
}

/// After every key or gesture, full screen or floating: start the armed
/// prefix's timeout, apply changes (the next frame shows a new theme), and
/// do `w`'s save.
fn follow(
    state: &mut State,
    step: Step,
    remote: bool,
    armed_until: &mut Option<Instant>,
    now: Instant,
) {
    *armed_until = state.armed.then(|| {
        armed_until.unwrap_or_else(|| now + Duration::from_millis(state.live.keys.timeout_ms))
    });
    let running = config::get();
    if state.live != *running {
        if let Err(why) =
            runnable(&state.live, &running, remote).and_then(|()| config::apply(state.live.clone()))
        {
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
}

/// Largest floating panel: fits the longest section's rows and the longest
/// help.
const PANEL_WIDTH: u16 = 72;
const PANEL_HEIGHT: u16 = 23;

/// Minimum session left visible around the panel, both sides together: in
/// columns, and in rows.
const PANEL_SIDES: u16 = 16;
const PANEL_ENDS: u16 = 6;

/// Smallest screen the settings float on; below it they go full screen.
const FLOAT_COLS: u16 = 72;
const FLOAT_ROWS: u16 = 20;

/// The centred floating panel on `screen`: up to [`PANEL_WIDTH`] by
/// [`PANEL_HEIGHT`], keeping the session margins; `None` under
/// [`FLOAT_COLS`] by [`FLOAT_ROWS`].
pub fn panel_area(screen: Rect) -> Option<Rect> {
    if screen.width < FLOAT_COLS || screen.height < FLOAT_ROWS {
        return None;
    }
    let width = PANEL_WIDTH.min(screen.width - PANEL_SIDES);
    let height = PANEL_HEIGHT.min(screen.height - PANEL_ENDS);
    Some(Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    ))
}

/// The settings floating over a session (see `crate::pty::panel`). The relay
/// owns the terminal, forwards keys and mouse, and picks the area; the rules
/// are the full screen's.
#[derive(Debug)]
pub struct Panel {
    state: State,
    remote: bool,
    /// When the prefix's wait runs out, while it is armed (see `run_on`).
    armed_until: Option<Instant>,
    /// The area last drawn, which clicks resolve against.
    drawn: Rect,
    /// When the glow was last advanced (see [`State::tick`]).
    ticked: Instant,
}

impl Panel {
    /// Opened by the relay, not `main`, so `client.ui` changes are checked
    /// against [`config::remote`], recorded at startup.
    pub fn open() -> Self {
        Self {
            state: State::open(),
            remote: config::remote(),
            armed_until: None,
            drawn: Rect::default(),
            ticked: Instant::now(),
        }
    }

    fn move_on(&mut self, now: Instant) {
        self.state.tick(now.saturating_duration_since(self.ticked));
        self.ticked = self.ticked.max(now);
    }

    fn after(&mut self, step: Step, now: Instant) -> Did {
        follow(
            &mut self.state,
            step,
            self.remote,
            &mut self.armed_until,
            now,
        );
        if step == Step::Close {
            Did::Close
        } else {
            Did::Stay
        }
    }
}

impl Floating for Panel {
    fn area(&self, screen: Rect) -> Option<Rect> {
        panel_area(screen)
    }

    /// Releases are ignored. `Esc`, `q` and `<prefix> ,` close.
    fn on_key(&mut self, key: KeyEvent, now: Instant) -> Did {
        if key.kind == KeyEventKind::Release {
            return Did::Stay;
        }
        // Advance first, so a glow the key starts begins at zero.
        self.move_on(now);
        let step = self.state.on_key(press(key));
        self.after(step, now)
    }

    /// By the screen's rules; presses outside the panel are the relay's.
    fn on_mouse(&mut self, mouse: MouseEvent, now: Instant) -> Did {
        let at = hit(&self.state, self.drawn, mouse.column, mouse.row);
        let Some(g) = gesture(mouse, at) else {
            return Did::Stay;
        };
        self.move_on(now);
        let step = self.state.on_mouse(g);
        self.after(step, now)
    }

    fn wake_at(&self) -> Option<Instant> {
        self.armed_until
    }

    /// Advance the glow and expire the armed prefix's wait; returns whether
    /// anything changed.
    fn tick(&mut self, now: Instant) -> bool {
        let glowed = self.state.glowing();
        self.move_on(now);
        if self.armed_until.is_some_and(|until| now >= until) {
            self.armed_until = None;
            self.state.disarm();
            return true;
        }
        glowed
    }

    fn moving(&self) -> bool {
        self.state.glowing()
    }

    fn draw(&mut self, area: Rect) -> Buffer {
        let mut buf = Buffer::empty(area);
        self.drawn = inside(area);
        paint_on(&mut buf, self.drawn, &self.state);
        buf
    }

    /// Remember the position for the next open.
    fn close(&self) {
        self.state.remember();
    }
}

/// Checks beyond validation: switching to Neovim's own client for remote
/// sessions needs a local `nvim`, as `main` checks at startup.
fn runnable(live: &Settings, running: &Settings, remote: bool) -> std::result::Result<(), String> {
    let to_nvim = live.client.ui == config::Ui::Nvim && running.client.ui != config::Ui::Nvim;
    if !(remote && to_nvim) {
        return Ok(());
    }
    crate::nvim::check_local().map(|_| ()).map_err(|e| {
        let why = e.to_string();
        format!(
            "client.ui = \"nvim\" needs nvim here — {}",
            why.lines().next().unwrap_or_default()
        )
    })
}

/// Where the screen's parts go; shared by drawing and the mouse so clicks
/// land on what is drawn.
#[derive(Debug, Clone, Copy)]
struct Layout {
    /// The tab row, or where the find's query goes in its place.
    tabs: Option<u16>,
    body: Rect,
    help: Option<u16>,
    hint: Rect,
    /// Where the rows' block starts, and how wide it is.
    x: u16,
    width: u16,
}

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
    // Blank rows around the tabs and above the help where there is room,
    // dropped one at a time as the height shrinks.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    /// A setting, by its index among the rows on screen.
    Row(usize),
    /// While finding, a tab's name over its matches.
    Head(Tab),
    Blank,
}

/// The tab's rows, with a blank line before the command keys; or while
/// finding, the matches grouped under their tabs' names.
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
            // Not between every table: each effect is its own table, but they
            // read as one list.
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

/// The first line shown and its offset from the body's top: centred when the
/// lines fit, else scrolled to keep the selection in the middle.
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

/// What is at (`column`, `row`) in the last drawn `area`. A row spans the
/// whole width, as in the picker; a tab is its name plus a column each side.
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

/// Where `setting`'s row is drawn, as wide as the bar, or `None` if off
/// screen: what the afterglow paints over.
fn row_rect(state: &State, area: Rect, setting: &Setting) -> Option<Rect> {
    let l = layout(area)?;
    let i = state
        .rows()
        .iter()
        .position(|s| std::ptr::eq(*s, setting))?;
    let lines = lines(state);
    let height = usize::from(l.body.height);
    let (offset, top) = placed(&lines, state.at(), height);
    let line = lines.iter().position(|line| *line == Line::Row(i))?;
    let n = line.checked_sub(offset).filter(|n| top + n < height)?;
    Some(Rect::new(l.x, l.body.y + (top + n) as u16, l.width, 1))
}

fn draw(frame: &mut Frame, state: &State) {
    let area = frame.area();
    draw_on(frame.buffer_mut(), area, state);
}

/// Draw a frame: the screen, then the theme, then the glow on top, as the
/// picker's effects go over its frame (see [`super::effects`]).
fn paint_on(buf: &mut Buffer, area: Rect, state: &State) {
    draw_on(buf, area, state);
    crate::theme::paint(buf);
    afterglow(buf, area, state, effects::colours().as_ref());
}

/// The glow on the row the bar last left: in colour if the terminal reported
/// its colours, else briefly reversed and dim, as in the picker.
fn afterglow(buf: &mut Buffer, area: Rect, state: &State, colours: Option<&effects::Colours>) {
    let Some(glow) = state.glow else {
        return;
    };
    // Back under the bar: no glow.
    if state
        .cursor()
        .is_some_and(|s| std::ptr::eq(s, glow.setting))
    {
        return;
    }
    if let Some(rect) = row_rect(state, area, glow.setting) {
        let progress = glow.age.as_secs_f32() / AFTERGLOW.as_secs_f32();
        effects::glow(buf, rect, colours, progress);
    }
}

/// Draw on `area`: the whole terminal, or the inside of the floating panel.
fn draw_on(buf: &mut Buffer, area: Rect, state: &State) {
    let Some(l) = layout(area) else {
        return;
    };
    let current = state.current();
    {
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
    hint_row(buf, l.hint, &text, dim);
}

/// The hint row as every screen draws it (see [`draw::draw_hint_row`]); dim
/// for hints, plain for messages.
fn hint_row(buf: &mut Buffer, row: Rect, text: &str, dim: bool) {
    let text = draw::truncate(text, usize::from(row.width));
    let style = if dim { DIM } else { Style::new() };
    let x = row.x + row.width.saturating_sub(text.width() as u16) / 2;
    buf.set_stringn(x, row.y, &text, usize::from(row.width), style);
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

/// One setting's row: marker or indent, name, value, and why it is dim; a
/// `•` to its left while unsaved. Selected, the bar spans the block.
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

/// The typed text windowed to `room` columns around the cursor, and the
/// cursor's column (on a trailing blank when at the end).
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

/// The hint row's text and whether it is dim: the last key's message, or the
/// current keys, as many as fit (see [`fit`]).
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

/// Join `entries` within `width`, dropping the highest-ranked whole, one at
/// a time, as the hint bar does. Also used by the floating picker.
pub(super) fn fit(entries: &[(Option<&str>, u8)], width: usize) -> String {
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

    /// The column of the last `needle` in `line`: columns, not bytes, since
    /// `•` and `▸` are three bytes.
    fn column(line: &str, needle: &str) -> u16 {
        let at = line
            .rfind(needle)
            .unwrap_or_else(|| panic!("{needle:?} in {line:?}"));
        line[..at].width() as u16
    }

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

    /// A typed off-grid value steps onto the grid.
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

    /// As on the first-run screen: the chord shows, `Enter` keeps it, `Esc`
    /// discards it.
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

    /// The hint row says so, and rebinding back swaps back.
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

    /// A command that could never start a session is refused with the file's
    /// error, and left to fix.
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

        // A command's key is reset by a swap, like any rebinding.
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

    /// Close, not quit.
    #[test]
    fn esc_q_and_ctrl_c_close() {
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            assert_eq!(key(&mut state(), code), Step::Close, "{code:?}");
        }
        assert_eq!(ctrl(&mut state(), 'c'), Step::Close);
    }

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

        // Uses the current prefix and `settings` key.
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

    /// While the prefix is being changed, its chord is a choice, not the
    /// prefix.
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
        typed(&mut s, "wi");
        let found: Vec<&str> = s.rows().iter().map(|r| r.name).collect();
        assert!(found.contains(&"windows"), "{found:?}");
        assert!(found.contains(&"window_width"), "{found:?}");
        assert_eq!(
            s.current().map(|r| r.name),
            Some("window_width"),
            "the first that starts so"
        );
        typed(&mut s, "s");
        assert_eq!(s.current().map(|r| r.name), Some("windows"));
        key(&mut s, KeyCode::Enter);
        assert_eq!(s.find, None);
        assert_eq!(s.tab, Tab::Effects);
        assert_eq!(s.current().map(|r| r.name), Some("windows"));
    }

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

    /// Switching to Neovim's client for remote sessions needs a local `nvim`.
    #[test]
    fn only_neovims_client_for_another_machine_needs_anything_more() {
        let running = Settings::default();
        let mut live = Settings::default();
        live.client.ui = config::Ui::Nvim;
        assert_eq!(runnable(&live, &running, false), Ok(()), "sessions here");
        let mut other = Settings::default();
        other.client.centre = config::Centre::Smart;
        assert_eq!(runnable(&other, &running, true), Ok(()), "anything else");
        assert_eq!(runnable(&live, &live, true), Ok(()), "already so");
    }

    // --- the mouse ----------------------------------------------------------

    fn mouse(state: &mut State, g: Gesture) {
        state.on_mouse(g);
    }

    /// A click on another row only selects it (for terminals that do not
    /// report motion).
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
        mouse(&mut s, Gesture::Press(Some(Hit::Tab(Tab::Style))));
        assert_eq!((s.tab, s.selected), (Tab::Style, 0));
    }

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

    /// On 80×24: tabs on row 1, rows centred, help on row 22, hints last.
    #[test]
    fn the_screen_is_tabs_rows_help_and_hints() {
        let s = on(Tab::Client);
        let lines = render(&s, 80, 24);
        assert_eq!(
            lines[1].trim(),
            "keys   session   client   style   effects",
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

    /// A value is plain whether it is the default or not.
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

    /// A `•` beside the row and after its tab's name, and the value in bold.
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

    /// A row spans the terminal; a tab is its name plus a column each side.
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
        let x = column(&lines[1], "style");
        assert_eq!(hit(&s, area, x - 1, 1), Some(Hit::Tab(Tab::Style)));
        assert_eq!(hit(&s, area, x + 5, 1), Some(Hit::Tab(Tab::Style)));
        assert_eq!(hit(&s, area, 0, 1), None);
    }

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

    /// So `NO_COLOR` holds by construction; the theme is a separate pass.
    #[test]
    fn nothing_sets_a_colour() {
        let mut s = on(Tab::Effects);
        to(&mut s, "smear");
        key(&mut s, KeyCode::Right);
        test_support::assert_no_colour(80, 24, |f| draw(f, &s));
    }

    /// At most 72×23, leaving at least 8 columns each side and 3 rows above
    /// and below; no float under 72×20.
    #[test]
    fn the_panel_floats_in_the_middle_with_the_session_round_it() {
        let screen = |w, h| Rect::new(0, 0, w, h);
        assert_eq!(panel_area(screen(120, 36)), Some(Rect::new(24, 6, 72, 23)));
        assert_eq!(panel_area(screen(80, 24)), Some(Rect::new(8, 3, 64, 18)));
        assert_eq!(panel_area(screen(200, 60)), Some(Rect::new(64, 18, 72, 23)));
        assert_eq!(panel_area(screen(72, 20)), Some(Rect::new(8, 3, 56, 14)));
        assert_eq!(panel_area(screen(71, 40)), None, "too narrow");
        assert_eq!(panel_area(screen(120, 19)), None, "too short");
    }

    /// Only on an off-to-on change, with a session behind, and with room.
    #[test]
    fn turned_to_float_they_go_to_float_over_the_session() {
        let screen = |w, h| Rect::new(0, 0, w, h);
        let mut s = state();
        s.live.style.float = false;
        to(&mut s, "float");
        assert_eq!(key(&mut s, KeyCode::Right), Step::Stay);
        assert!(s.live.style.float, "turned on");
        assert!(turned_to_float(false, &s.live, true, screen(120, 36)));
        assert!(
            !turned_to_float(false, &s.live, false, screen(120, 36)),
            "no session behind them"
        );
        assert!(
            !turned_to_float(false, &s.live, true, screen(71, 40)),
            "no room to float"
        );
        assert!(
            !turned_to_float(true, &s.live, true, screen(120, 36)),
            "on already: some other key"
        );
        key(&mut s, KeyCode::Right);
        assert!(!s.live.style.float);
        assert!(
            !turned_to_float(true, &s.live, true, screen(120, 36)),
            "turned off"
        );
    }

    /// Only the last row left glows, each from the start, until
    /// [`AFTERGLOW`] is up.
    #[test]
    fn the_row_the_bar_leaves_fades_back_from_it() {
        let mut s = on(Tab::Effects);
        let first = s.current().expect("a row");
        key(&mut s, KeyCode::Char('j'));
        let second = s.current().expect("a row");
        let glow = s.glow.expect("the row left glows");
        assert!(std::ptr::eq(glow.setting, first));
        assert_eq!(glow.age, Duration::ZERO);
        s.tick(AFTERGLOW / 2);
        assert!(s.glowing(), "half way");
        key(&mut s, KeyCode::Char('j'));
        let glow = s.glow.expect("the row just left glows");
        assert!(std::ptr::eq(glow.setting, second), "only the last row left");
        assert_eq!(glow.age, Duration::ZERO, "from the start");
        s.tick(AFTERGLOW);
        assert!(!s.glowing(), "done");
    }

    /// Also none with every effect off, or when the bar did not move.
    #[test]
    fn no_row_glows_with_the_effect_off_or_the_row_gone() {
        let mut s = on(Tab::Effects);
        s.live.effects.cursor.enabled = false;
        key(&mut s, KeyCode::Char('j'));
        assert!(!s.glowing(), "[effects.cursor] off");
        let mut s = on(Tab::Effects);
        s.live.effects.enabled = false;
        key(&mut s, KeyCode::Char('j'));
        assert!(!s.glowing(), "every effect off");
        let mut s = on(Tab::Keys);
        key(&mut s, KeyCode::Tab);
        assert!(!s.glowing(), "the row left is on another tab");
        let mut s = on(Tab::Keys);
        key(&mut s, KeyCode::Char('k'));
        assert!(!s.glowing(), "already on the first row");
    }

    /// It starts as the bar was (ink background, background-coloured text)
    /// and fades to plain. Without colours it is briefly reversed and dim.
    #[test]
    fn the_row_left_is_painted_back_from_the_bar() {
        use ratatui::style::Color;
        let palette = crate::test_support::palette();
        let colours = effects::Colours {
            palette,
            theme: None,
        };
        let area = Rect::new(0, 0, 80, 24);
        let painted = |s: &State, colours: Option<&effects::Colours>| {
            let mut buf = Buffer::empty(area);
            draw_on(&mut buf, area, s);
            afterglow(&mut buf, area, s, colours);
            buf
        };
        let mut s = on(Tab::Effects);
        key(&mut s, KeyCode::Char('j'));
        let left = s.glow.expect("glowing").setting;
        let rect = row_rect(&s, area, left).expect("on screen");

        let buf = painted(&s, Some(&colours));
        let (fg, bg) = (palette.fg, palette.bg);
        for x in rect.x..rect.x + rect.width {
            assert_eq!(buf[(x, rect.y)].bg, Color::Rgb(fg.0, fg.1, fg.2), "at {x}");
            assert_eq!(buf[(x, rect.y)].fg, Color::Rgb(bg.0, bg.1, bg.2), "at {x}");
        }
        let text: String = (rect.x..rect.x + rect.width)
            .map(|x| buf[(x, rect.y)].symbol())
            .collect();
        assert!(text.contains(left.name), "{text:?}");
        s.tick(AFTERGLOW * 9 / 10);
        let buf = painted(&s, Some(&colours));
        let Color::Rgb(r, g, b) = buf[(rect.x, rect.y)].bg else {
            panic!("a colour");
        };
        let near = |a: u8, b: u8| a.abs_diff(b) <= 8;
        assert!(
            near(r, bg.0) && near(g, bg.1) && near(b, bg.2),
            "nearly plain"
        );

        let mut s = on(Tab::Effects);
        key(&mut s, KeyCode::Char('j'));
        let held = Modifier::REVERSED | Modifier::DIM;
        let buf = painted(&s, None);
        assert!(buf[(rect.x, rect.y)].modifier.contains(held), "held");
        s.tick(AFTERGLOW / 2);
        let buf = painted(&s, None);
        assert!(!buf[(rect.x, rect.y)].modifier.contains(held), "let go");
    }

    /// The glow is as wide as the bar, whether the rows fit or scroll.
    #[test]
    fn a_row_glows_where_it_is_drawn() {
        for (w, h) in [(80, 24), (60, 12)] {
            let area = Rect::new(0, 0, w, h);
            let mut s = on(Tab::Keys);
            s.selected = s.rows().len() - 1;
            let lines = render(&s, w, h);
            for setting in s.rows() {
                let drawn = lines.iter().position(|l| {
                    let t = l.trim_start_matches([' ', '•']).trim_start_matches(MARKER);
                    t.split_whitespace().next() == Some(setting.name)
                });
                let rect = row_rect(&s, area, setting);
                assert_eq!(
                    rect.map(|r| usize::from(r.y)),
                    drawn,
                    "{} on {w}x{h}",
                    setting.name
                );
            }
            let bar = row_rect(&s, area, s.current().expect("a row")).expect("on screen");
            let mut buf = Buffer::empty(area);
            draw_on(&mut buf, area, &s);
            let reversed = |x: u16| buf[(x, bar.y)].modifier.contains(Modifier::REVERSED);
            assert!((bar.x..bar.x + bar.width).all(reversed), "the bar's width");
            assert!(!reversed(bar.x - 1) && !reversed(bar.x + bar.width));
        }
    }

    fn rows_of(buf: &Buffer) -> Vec<String> {
        let area = buf.area;
        (area.y..area.y + area.height)
            .map(|y| {
                (area.x..area.x + area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// A clear column each side and a clear last row; tabs on the second row,
    /// hints on the row above the last.
    #[test]
    fn the_panel_is_the_screen_inside_a_margin() {
        let mut panel = Panel::open();
        let area = Rect::new(8, 3, 64, 18);
        let buf = panel.draw(area);
        assert_eq!(buf.area, area);
        let rows = rows_of(&buf);
        assert_eq!(rows.len(), 18);
        assert!(rows[0].is_empty(), "a clear row over the sections");
        assert!(
            rows[1].contains("keys") && rows[1].contains("effects"),
            "{rows:?}"
        );
        assert!(rows[16].contains("esc"), "the hints: {rows:?}");
        assert!(rows[17].is_empty(), "a clear row under them");
        for y in area.y..area.y + area.height {
            assert_eq!(buf[(area.x, y)].symbol(), " ", "the left margin");
            assert_eq!(buf[(area.x + area.width - 1, y)].symbol(), " ", "the right");
        }
    }

    /// Tab moves a section, a release is ignored, and Esc closes.
    #[test]
    fn the_panel_takes_the_screens_keys() {
        let now = Instant::now();
        let mut panel = Panel::open();
        let was = panel.state.tab;
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(panel.on_key(tab, now), Did::Stay);
        assert_ne!(panel.state.tab, was);
        let released =
            KeyEvent::new_with_kind(KeyCode::Tab, KeyModifiers::NONE, KeyEventKind::Release);
        let turned = panel.state.tab;
        assert_eq!(panel.on_key(released, now), Did::Stay);
        assert_eq!(panel.state.tab, turned, "a release moves nothing");
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(panel.on_key(esc, now), Did::Close);
    }

    #[test]
    fn the_panel_answers_the_mouse_where_it_drew_and_its_prefix_times_out() {
        let mut panel = Panel::open();
        let now = Instant::now();
        panel.state.tab = Tab::Effects;
        panel.state.selected = 0;
        let area = Rect::new(8, 3, 64, 18);
        let buf = panel.draw(area);
        let rows = rows_of(&buf);
        let y = rows
            .iter()
            .position(|r| r.contains("smear"))
            .expect("smear is drawn") as u16;
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 20,
            row: area.y + y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(panel.on_mouse(click, now), Did::Stay);
        assert_eq!(panel.state.current().map(|s| s.name), Some("smear"));
        // The row the click moved the bar off glows, then finishes.
        assert!(panel.moving());
        assert!(panel.tick(now + AFTERGLOW), "the last of the glow");
        assert!(!panel.moving());
        let prefix = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL);
        panel.on_key(prefix, now);
        let until = panel.wake_at().expect("armed");
        assert!(!panel.tick(until - Duration::from_millis(1)));
        assert!(panel.tick(until));
        assert_eq!(panel.wake_at(), None);
    }
}
