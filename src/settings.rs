//! One row per setting the settings screen ([`crate::ui::settings`]) can
//! change: its file path, name, tab, help, value kind with accessors into
//! [`Settings`], and what it needs to take effect. A test keeps the rows and
//! the first-run template's settings in sync both ways.

use std::sync::LazyLock;

use crate::config::{
    Centre, ParticleMode, Predict, Settings, ThemeColor, Ui, MAX_FADE_MS, MAX_TIMEOUT_MS,
};
use crate::keys::BINDINGS;

/// The screen's tabs, one per table of the file; `Keys` holds both `[keys]`
/// and `[keys.bind]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    Keys,
    Session,
    Client,
    Style,
    Effects,
}

impl Tab {
    /// In the file's order, which is the drawing order.
    pub const ALL: [Tab; 5] = [
        Tab::Keys,
        Tab::Session,
        Tab::Client,
        Tab::Style,
        Tab::Effects,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Tab::Keys => "keys",
            Tab::Session => "session",
            Tab::Client => "client",
            Tab::Style => "style",
            Tab::Effects => "effects",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Switch(bool),
    Number(u64),
    /// As the file spells it.
    Choice(&'static str),
    /// The prefix chord's control byte.
    Prefix(u8),
    /// A command's key: the byte typed after the prefix.
    Key(u8),
    Text(String),
    Colour(ThemeColor),
}

/// A setting's value type, and how it is read from and written to
/// [`Settings`].
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Switch {
        get: fn(&Settings) -> bool,
        set: fn(&mut Settings, bool),
    },
    /// `lo..=hi`; `←` and `→` step by `step`.
    Number {
        get: fn(&Settings) -> u64,
        set: fn(&mut Settings, u64),
        lo: u64,
        hi: u64,
        step: u64,
        /// Shown after the number: ` ms`, or empty.
        unit: &'static str,
        /// Label for 0, where it means something other than a number.
        zero: Option<&'static str>,
    },
    Choice {
        options: &'static [&'static str],
        get: fn(&Settings) -> &'static str,
        set: fn(&mut Settings, &str),
    },
    /// `[keys] prefix`.
    Prefix,
    /// A `[keys.bind]` key, by its row in [`BINDINGS`].
    Key(usize),
    /// `[session] command`.
    Command,
    /// `[style] color`.
    Colour,
}

/// A switch a setting needs to take effect. `[effects] enabled` and nvmux's
/// own client are checked separately by [`Setting::inert`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Needs {
    Fade,
    Centre,
    Particles,
}

impl Value {
    /// The value as a save writes it after the `=`.
    pub fn toml(&self) -> toml_edit::Value {
        match self {
            Value::Switch(on) => (*on).into(),
            Value::Number(n) => i64::try_from(*n).unwrap_or(i64::MAX).into(),
            Value::Choice(name) => (*name).into(),
            Value::Prefix(byte) => crate::keys::prefix_label(*byte).into(),
            Value::Key(byte) => crate::keys::key_label(*byte).into(),
            Value::Text(text) => text.as_str().into(),
            Value::Colour(colour) => colour.name().into(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Setting {
    /// Table and key in the file, e.g. `effects.fade` and `enabled`.
    pub table: &'static str,
    pub key: &'static str,
    /// The screen's name: the key, or enough of the table to tell it apart
    /// (`fade`, `fade.duration_ms`).
    pub name: &'static str,
    pub tab: Tab,
    /// Shown under the rows while selected.
    pub help: &'static str,
    pub kind: Kind,
    /// Only nvmux's own client acts on it; inert with `[client] ui = "nvim"`.
    pub own: bool,
    pub needs: Option<Needs>,
}

impl Setting {
    /// E.g. `effects.fade.enabled`.
    pub fn path(&self) -> String {
        format!("{}.{}", self.table, self.key)
    }

    pub fn value(&self, settings: &Settings) -> Value {
        match self.kind {
            Kind::Switch { get, .. } => Value::Switch(get(settings)),
            Kind::Number { get, .. } => Value::Number(get(settings)),
            Kind::Choice { get, .. } => Value::Choice(get(settings)),
            Kind::Prefix => Value::Prefix(settings.keys.prefix),
            Kind::Key(row) => Value::Key(
                settings
                    .keys
                    .bind
                    .bindings()
                    .nth(row)
                    .map_or(BINDINGS[row].key, |b| b.key),
            ),
            Kind::Command => Value::Text(settings.session.command.clone()),
            Kind::Colour => Value::Colour(settings.style.color),
        }
    }

    pub fn default_value(&self) -> Value {
        self.value(&Settings::default())
    }

    /// Numbers are clamped and unknown choices ignored. A value of the wrong
    /// kind is a caller bug and changes nothing.
    pub fn set(&self, settings: &mut Settings, value: &Value) {
        match (self.kind, value) {
            (Kind::Switch { set, .. }, Value::Switch(on)) => set(settings, *on),
            (Kind::Number { set, lo, hi, .. }, Value::Number(n)) => {
                set(settings, (*n).clamp(lo, hi));
            }
            (Kind::Choice { options, set, .. }, Value::Choice(name)) => {
                if options.contains(name) {
                    set(settings, name);
                }
            }
            (Kind::Prefix, Value::Prefix(byte)) => settings.keys.prefix = *byte,
            (Kind::Key(row), Value::Key(byte)) => {
                settings.keys.bind.bind(BINDINGS[row].hint, *byte);
            }
            (Kind::Command, Value::Text(command)) => settings.session.command = command.clone(),
            (Kind::Colour, Value::Colour(colour)) => settings.style.color = *colour,
            _ => debug_assert!(false, "{}: a value of another kind", self.path()),
        }
    }

    /// A few words for the end of the row saying why the setting has no
    /// effect under `settings`, or `None` if it does.
    pub fn inert(&self, settings: &Settings) -> Option<&'static str> {
        if self.table.starts_with("effects.") && !settings.effects.enabled {
            return Some("effects are off");
        }
        if self.own && settings.client.ui == Ui::Nvim {
            return Some("nvmux client only");
        }
        match self.needs? {
            Needs::Fade if !settings.effects.fade.enabled => Some("needs fade"),
            Needs::Centre if settings.client.centre == Centre::Off => Some("needs centre"),
            Needs::Particles if !settings.effects.particles.enabled => Some("needs particles"),
            _ => None,
        }
    }
}

/// A switch, by its field: `switch!(effects.fade.enabled)`.
macro_rules! switch {
    ($($field:ident).+) => {
        Kind::Switch {
            get: |s| s.$($field).+,
            set: |s, on| s.$($field).+ = on,
        }
    };
}

/// A `Predict` choice, by its field: `predict!(client.predict_scrolls)`.
macro_rules! predict {
    ($($field:ident).+) => {
        Kind::Choice {
            options: &["auto", "on", "off"],
            get: |s| s.$($field).+.name(),
            set: |s, name| {
                if let Some(way) = Predict::ALL.into_iter().find(|w| w.name() == name) {
                    s.$($field).+ = way;
                }
            },
        }
    };
}

/// A row with only the common fields; wrap it in `own`, `needs` or `named`.
const fn row(
    table: &'static str,
    key: &'static str,
    tab: Tab,
    help: &'static str,
    kind: Kind,
) -> Setting {
    Setting {
        table,
        key,
        name: key,
        tab,
        help,
        kind,
        own: false,
        needs: None,
    }
}

/// Marks a setting only nvmux's own client acts on.
const fn own(setting: Setting) -> Setting {
    Setting {
        own: true,
        ..setting
    }
}

/// A setting that does nothing without `needs` on.
const fn needs(needs: Needs, setting: Setting) -> Setting {
    Setting {
        needs: Some(needs),
        ..setting
    }
}

/// A setting whose key alone is not a clear enough name.
const fn named(name: &'static str, setting: Setting) -> Setting {
    Setting { name, ..setting }
}

/// Every row, in the file's order.
static ROWS: LazyLock<Vec<Setting>> = LazyLock::new(|| {
    let mut rows = vec![
        row(
            "keys",
            "prefix",
            Tab::Keys,
            "the key before every command: a Ctrl chord",
            Kind::Prefix,
        ),
        row(
            "keys",
            "timeout_ms",
            Tab::Keys,
            "how long a lone prefix or a half-typed number waits",
            Kind::Number {
                get: |s| s.keys.timeout_ms,
                set: |s, ms| s.keys.timeout_ms = ms,
                lo: 1,
                hi: MAX_TIMEOUT_MS,
                step: 100,
                unit: " ms",
                zero: None,
            },
        ),
    ];
    // From the table, so every command gets a row.
    rows.extend(
        BINDINGS
            .iter()
            .enumerate()
            .map(|(i, b)| row("keys.bind", b.hint, Tab::Keys, b.help, Kind::Key(i))),
    );
    rows.extend([
        row(
            "session",
            "command",
            Tab::Session,
            "what a new session runs; {sock} becomes its socket",
            Kind::Command,
        ),
        row(
            "client",
            "ui",
            Tab::Client,
            "who draws a session: nvmux's client or Neovim's",
            Kind::Choice {
                options: &["nvmux", "nvim"],
                get: |s| s.client.ui.name(),
                set: |s, name| {
                    if let Some(ui) = Ui::ALL.into_iter().find(|u| u.name() == name) {
                        s.client.ui = ui;
                    }
                },
            },
        ),
        row(
            "client",
            "per_session",
            Tab::Client,
            "keep each session's client; a switch back reuses it",
            switch!(client.per_session),
        ),
        row(
            "client",
            "lazy",
            Tab::Client,
            "start a client on its visit; off starts all at launch and after a link drops",
            switch!(client.lazy),
        ),
        own(row(
            "client",
            "predict_scrolls",
            Tab::Client,
            "scrolls show before Neovim answers: auto over ssh, on everywhere",
            predict!(client.predict_scrolls),
        )),
        own(row(
            "client",
            "predict_motions",
            Tab::Client,
            "cursor motions show before Neovim answers: auto over ssh, on everywhere",
            predict!(client.predict_motions),
        )),
        own(row(
            "client",
            "predict_typing",
            Tab::Client,
            "what is typed shows before Neovim answers: auto over ssh, on everywhere",
            predict!(client.predict_typing),
        )),
        own(row(
            "client",
            "centre",
            Tab::Client,
            "each window's text in its middle: smart as wide as it wants, or fixed",
            Kind::Choice {
                options: &["off", "smart", "fixed"],
                get: |s| s.client.centre.name(),
                set: |s, name| {
                    if let Some(way) = Centre::ALL.into_iter().find(|w| w.name() == name) {
                        s.client.centre = way;
                    }
                },
            },
        )),
        own(needs(
            Needs::Centre,
            row(
                "client",
                "window_width",
                Tab::Client,
                "with centre on, how many columns a window's text gets; 0 for no limit",
                Kind::Number {
                    get: |s| u64::from(s.client.window_width),
                    set: |s, n| s.client.window_width = u16::try_from(n).unwrap_or(u16::MAX),
                    lo: 0,
                    hi: u16::MAX as u64,
                    step: 10,
                    unit: "",
                    zero: Some("no limit"),
                },
            ),
        )),
        row(
            "style",
            "color",
            Tab::Style,
            "nvmux's text and bars in that colour",
            Kind::Colour,
        ),
        row(
            "style",
            "float",
            Tab::Style,
            "the picker, settings, help and prompts float over it; off, full screen",
            switch!(style.float),
        ),
        row(
            "effects",
            "enabled",
            Tab::Effects,
            "master switch: off turns every effect below off",
            switch!(effects.enabled),
        ),
        named(
            "fade",
            row(
                "effects.fade",
                "enabled",
                Tab::Effects,
                "dissolve between screens; NO_COLOR forces this off",
                switch!(effects.fade.enabled),
            ),
        ),
        named(
            "fade.duration_ms",
            needs(
                Needs::Fade,
                row(
                    "effects.fade",
                    "duration_ms",
                    Tab::Effects,
                    "the whole dissolve, out and back",
                    Kind::Number {
                        get: |s| s.effects.fade.duration_ms,
                        set: |s, ms| s.effects.fade.duration_ms = ms,
                        lo: 1,
                        hi: MAX_FADE_MS,
                        step: 50,
                        unit: " ms",
                        zero: None,
                    },
                ),
            ),
        ),
        named(
            "fade.session",
            needs(
                Needs::Fade,
                row(
                    "effects.fade",
                    "session",
                    Tab::Effects,
                    "Neovim's own screen dissolves too",
                    switch!(effects.fade.session),
                ),
            ),
        ),
        named(
            "move",
            row(
                "effects.move",
                "enabled",
                Tab::Effects,
                "stars off a session being moved, rows stepping aside, its landing",
                switch!(effects.moving.enabled),
            ),
        ),
        named(
            "cursor",
            row(
                "effects.cursor",
                "enabled",
                Tab::Effects,
                "the row the cursor leaves fades back from the bar",
                switch!(effects.cursor.enabled),
            ),
        ),
        named(
            "attach",
            row(
                "effects.attach",
                "enabled",
                Tab::Effects,
                "the picker closes onto the name; the session opens out of it",
                switch!(effects.attach.enabled),
            ),
        ),
        named(
            "kill",
            row(
                "effects.kill",
                "enabled",
                Tab::Effects,
                "a line through the name at [y/N]; then the row is erased",
                switch!(effects.kill.enabled),
            ),
        ),
        named(
            "filter",
            row(
                "effects.filter",
                "enabled",
                Tab::Effects,
                "rows a filter keystroke drops fade out before the list closes",
                switch!(effects.filter.enabled),
            ),
        ),
        named(
            "create",
            row(
                "effects.create",
                "enabled",
                Tab::Effects,
                "c opens a gap where the new session goes, then the prompt",
                switch!(effects.create.enabled),
            ),
        ),
        own(named(
            "smear",
            row(
                "effects.smear",
                "enabled",
                Tab::Effects,
                "the cursor travels between cells, smearing across a long jump",
                switch!(effects.smear.enabled),
            ),
        )),
        own(named(
            "particles",
            row(
                "effects.particles",
                "enabled",
                Tab::Effects,
                "what flies off the cursor as it moves",
                switch!(effects.particles.enabled),
            ),
        )),
        own(named(
            "particles.mode",
            needs(
                Needs::Particles,
                row(
                    "effects.particles",
                    "mode",
                    Tab::Effects,
                    "railgun, torpedo, pixiedust, sonicboom, ripple or wireframe",
                    Kind::Choice {
                        options: &[
                            "railgun",
                            "torpedo",
                            "pixiedust",
                            "sonicboom",
                            "ripple",
                            "wireframe",
                        ],
                        get: |s| s.effects.particles.mode.name(),
                        set: |s, name| {
                            if let Some(mode) =
                                ParticleMode::ALL.into_iter().find(|m| m.name() == name)
                            {
                                s.effects.particles.mode = mode;
                            }
                        },
                    },
                ),
            ),
        )),
        own(named(
            "scroll",
            row(
                "effects.scroll",
                "enabled",
                Tab::Effects,
                "a scroll slides the text through its window",
                switch!(effects.scroll.enabled),
            ),
        )),
        own(named(
            "windows",
            row(
                "effects.windows",
                "enabled",
                Tab::Effects,
                "windows and floats move rather than jump",
                switch!(effects.windows.enabled),
            ),
        )),
        own(named(
            "blink",
            row(
                "effects.blink",
                "enabled",
                Tab::Effects,
                "a blinking block cursor fades out and back in",
                switch!(effects.blink.enabled),
            ),
        )),
    ]);
    rows
});

/// In the file's order.
pub fn all() -> &'static [Setting] {
    &ROWS
}

pub fn on(tab: Tab) -> impl Iterator<Item = &'static Setting> {
    all().iter().filter(move |s| s.tab == tab)
}

/// The setting at `path`, e.g. `effects.fade.enabled`.
pub fn find(path: &str) -> Option<&'static Setting> {
    all().iter().find(|s| s.path() == path)
}

/// The settings `ours` differs from `theirs` in, in the file's order; with
/// the file's settings as `theirs`, what is unsaved.
pub fn changed(ours: &Settings, theirs: &Settings) -> Vec<&'static Setting> {
    all()
        .iter()
        .filter(|s| s.value(ours) != s.value(theirs))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::palette::Rgb;

    /// Paths of every setting in the first-run template, read by uncommenting
    /// each commented setting line and parsing as TOML.
    fn template_paths() -> BTreeSet<String> {
        let rendered = crate::config::render_default_config(crate::keys::PREFIX);
        let is_setting = |rest: &&str| {
            rest.split_once(" = ").is_some_and(|(key, _)| {
                let key = key.trim_end();
                !key.is_empty() && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
            })
        };
        let uncommented: String = rendered
            .lines()
            .map(|line| line.strip_prefix("# ").filter(is_setting).unwrap_or(line))
            .map(|line| format!("{line}\n"))
            .collect();
        let table: toml::Table = toml::from_str(&uncommented).expect("valid toml");
        let mut paths = BTreeSet::new();
        flatten(&table, "", &mut paths);
        paths
    }

    fn flatten(table: &toml::Table, at: &str, paths: &mut BTreeSet<String>) {
        for (key, value) in table {
            let path = if at.is_empty() {
                key.clone()
            } else {
                format!("{at}.{key}")
            };
            match value {
                toml::Value::Table(inner) => flatten(inner, &path, paths),
                _ => {
                    paths.insert(path);
                }
            }
        }
    }

    #[test]
    fn every_setting_in_the_file_is_a_row_and_every_row_is_in_the_file() {
        let rows: BTreeSet<String> = all().iter().map(Setting::path).collect();
        assert_eq!(rows.len(), all().len(), "two rows for one key");
        assert_eq!(rows, template_paths());
    }

    /// A value other than the default.
    fn another(setting: &Setting) -> Value {
        match (setting.kind, setting.default_value()) {
            (_, Value::Switch(on)) => Value::Switch(!on),
            (Kind::Number { hi, step, .. }, Value::Number(n)) => Value::Number((n + step).min(hi)),
            (Kind::Choice { options, .. }, Value::Choice(name)) => {
                let at = options.iter().position(|o| *o == name).expect("an option");
                Value::Choice(options[(at + 1) % options.len()])
            }
            (_, Value::Prefix(_)) => Value::Prefix(0x01),
            // On no command's key.
            (_, Value::Key(_)) => Value::Key(b'z'),
            (_, Value::Text(_)) => Value::Text("nvim -u NONE --headless --listen {sock}".into()),
            (_, Value::Colour(_)) => Value::Colour(ThemeColor::Mono(Rgb(0x61, 0xaf, 0xef))),
            (kind, value) => panic!("{kind:?} cannot hold {value:?}"),
        }
    }

    /// Also checks that what it writes passes validation.
    #[test]
    fn each_row_reads_and_writes_only_its_own_setting() {
        for setting in all() {
            let mut settings = Settings::default();
            let value = another(setting);
            assert_ne!(value, setting.default_value(), "{}", setting.path());
            setting.set(&mut settings, &value);
            assert_eq!(setting.value(&settings), value, "{}", setting.path());
            assert_eq!(
                changed(&settings, &Settings::default())
                    .iter()
                    .map(|s| s.path())
                    .collect::<Vec<_>>(),
                vec![setting.path()],
                "{} moved another setting",
                setting.path()
            );
            settings
                .validate()
                .unwrap_or_else(|e| panic!("{}: {e}", setting.path()));
        }
    }

    #[test]
    fn a_number_is_held_to_its_range() {
        let timeout = find("keys.timeout_ms").expect("the timeout");
        let mut settings = Settings::default();
        timeout.set(&mut settings, &Value::Number(0));
        assert_eq!(settings.keys.timeout_ms, 1);
        timeout.set(&mut settings, &Value::Number(u64::MAX));
        assert_eq!(settings.keys.timeout_ms, MAX_TIMEOUT_MS);
        let width = find("client.window_width").expect("the width");
        width.set(&mut settings, &Value::Number(1 << 20));
        assert_eq!(settings.client.window_width, u16::MAX);
    }

    #[test]
    fn the_keys_tab_holds_the_prefix_the_timeout_and_every_command() {
        let names: Vec<&str> = on(Tab::Keys).map(|s| s.name).collect();
        let mut want = vec!["prefix", "timeout_ms"];
        want.extend(BINDINGS.iter().map(|b| b.hint));
        assert_eq!(names, want);
    }

    /// Also checks every row is on the tab of its table.
    #[test]
    fn the_tabs_walk_the_rows_in_the_files_order() {
        let walked: Vec<String> = Tab::ALL
            .iter()
            .flat_map(|tab| on(*tab).map(Setting::path))
            .collect();
        let rows: Vec<String> = all().iter().map(Setting::path).collect();
        assert_eq!(walked, rows);
        for s in all() {
            let top = s.table.split('.').next().expect("a table");
            assert_eq!(top, s.tab.name(), "{}", s.path());
        }
    }

    /// The screen and its find go by name.
    #[test]
    fn no_two_rows_share_a_name() {
        let names: BTreeSet<&str> = all().iter().map(|s| s.name).collect();
        assert_eq!(names.len(), all().len());
    }

    fn inert_paths(settings: &Settings) -> Vec<(String, &'static str)> {
        all()
            .iter()
            .filter_map(|s| s.inert(settings).map(|why| (s.path(), why)))
            .collect()
    }

    /// `centre` and the particles start off.
    #[test]
    fn at_the_defaults_only_what_waits_on_a_switch_that_starts_off_is_silent() {
        assert_eq!(
            inert_paths(&Settings::default()),
            vec![
                ("client.window_width".to_string(), "needs centre"),
                ("effects.particles.mode".to_string(), "needs particles"),
            ]
        );
    }

    #[test]
    fn the_master_switch_off_silences_every_effect_but_itself() {
        let mut settings = Settings::default();
        settings.effects.enabled = false;
        for s in all() {
            let off = s.table.starts_with("effects.");
            assert_eq!(
                s.inert(&settings) == Some("effects are off"),
                off,
                "{}",
                s.path()
            );
        }
    }

    #[test]
    fn neovims_client_silences_nvmuxs_clients_settings() {
        let mut settings = Settings::default();
        settings.client.ui = Ui::Nvim;
        settings.client.centre = Centre::Smart;
        let silenced: Vec<String> = inert_paths(&settings).into_iter().map(|(p, _)| p).collect();
        let own: Vec<String> = all().iter().filter(|s| s.own).map(Setting::path).collect();
        assert_eq!(silenced, own);
        assert!(inert_paths(&settings)
            .iter()
            .all(|(_, why)| *why == "nvmux client only"));
    }

    /// `centre` silences `window_width` only when off; both other modes use it.
    #[test]
    fn a_switch_a_setting_needs_silences_it() {
        for centre in [Centre::Smart, Centre::Fixed] {
            let mut settings = Settings::default();
            settings.client.centre = centre;
            settings.effects.fade.enabled = false;
            assert_eq!(
                inert_paths(&settings),
                vec![
                    ("effects.fade.duration_ms".to_string(), "needs fade"),
                    ("effects.fade.session".to_string(), "needs fade"),
                    ("effects.particles.mode".to_string(), "needs particles"),
                ],
                "{centre:?}"
            );
        }
    }
}
