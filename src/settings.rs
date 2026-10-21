//! Every setting the settings screen can change, a row each.
//!
//! As [`crate::keys::BINDINGS`] is one row per command, this is one row per
//! setting: where it is in the file, what the screen calls it and on which
//! tab, its one line of help, what kind of value it takes and how that is read
//! from and written to a [`Settings`], and what it has no effect without. The
//! settings screen ([`crate::ui::settings`]) draws these rows and edits
//! through them, and a save writes what they say has changed.
//!
//! The rows are held to the first-run template's settings by a test, both
//! ways: a setting cannot be added to the file without appearing on the
//! screen, nor a row name a key the file does not have.

use std::sync::LazyLock;

use crate::config::{
    Centre, ParticleMode, Predict, Settings, ThemeColor, Ui, MAX_FADE_MS, MAX_TIMEOUT_MS,
};
use crate::keys::BINDINGS;

/// The screen's tabs. Each is a table of the file, but for the keys: their
/// tab holds `[keys]` and then `[keys.bind]`, the prefix and the commands it
/// leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    Keys,
    Session,
    Client,
    Style,
    Effects,
}

impl Tab {
    /// In the file's order, which is the order the tab row draws them in.
    pub const ALL: [Tab; 5] = [
        Tab::Keys,
        Tab::Session,
        Tab::Client,
        Tab::Style,
        Tab::Effects,
    ];

    /// What the tab row calls it: its table's name.
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

/// A setting's value, of whichever kind it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Switch(bool),
    Number(u64),
    /// One of a fixed few, as the file spells it.
    Choice(&'static str),
    /// The prefix chord's control byte.
    Prefix(u8),
    /// A command's key: the byte typed after the prefix.
    Key(u8),
    /// The command a new session runs.
    Text(String),
    Colour(ThemeColor),
}

/// What a setting takes, and how it is read from and written to a
/// [`Settings`].
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Switch {
        get: fn(&Settings) -> bool,
        set: fn(&mut Settings, bool),
    },
    /// A whole number from `lo` to `hi`, which `←` and `→` step by `step`.
    Number {
        get: fn(&Settings) -> u64,
        set: fn(&mut Settings, u64),
        lo: u64,
        hi: u64,
        step: u64,
        /// After the number, as the screen shows it: ` ms`, or nothing.
        unit: &'static str,
        /// What 0 means, where it means something other than a number.
        zero: Option<&'static str>,
    },
    /// One of `options`, as the file spells them.
    Choice {
        options: &'static [&'static str],
        get: fn(&Settings) -> &'static str,
        set: fn(&mut Settings, &str),
    },
    /// `[keys] prefix`.
    Prefix,
    /// A command's key under `[keys.bind]`: its row in [`BINDINGS`].
    Key(usize),
    /// `[session] command`.
    Command,
    /// `[style] color`.
    Colour,
}

/// A switch a setting has no effect without, besides `[effects] enabled` and
/// nvmux's own client, which [`Setting::inert`] asks after for every row they
/// govern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Needs {
    Fade,
    Centre,
    Particles,
}

impl Value {
    /// The value as the file spells it: what a save writes after the `=`.
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

/// One setting: a key of the file, as the screen shows it.
#[derive(Debug, Clone, Copy)]
pub struct Setting {
    /// Its table in the file, `effects.fade`, and its key there, `enabled`.
    pub table: &'static str,
    pub key: &'static str,
    /// What the screen calls it: the key, where that says which setting it
    /// is, and otherwise enough of the table to — `fade`, `fade.duration_ms`.
    pub name: &'static str,
    pub tab: Tab,
    /// One line, under the rows, while it is selected.
    pub help: &'static str,
    pub kind: Kind,
    /// Whether only nvmux's own client acts on it, so that with `[client] ui
    /// = "nvim"` it does nothing.
    pub own: bool,
    pub needs: Option<Needs>,
}

impl Setting {
    /// Where it is in the file: `effects.fade.enabled`.
    pub fn path(&self) -> String {
        format!("{}.{}", self.table, self.key)
    }

    /// Its value in `settings`.
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

    /// Its value in the defaults.
    pub fn default_value(&self) -> Value {
        self.value(&Settings::default())
    }

    /// Put `value` in `settings`. A value of another kind than the setting's
    /// is a mistake of the caller's, and changes nothing.
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

    /// Why the setting has no effect with `settings` as they are, in a few
    /// words for the end of its row: the master switch is off, the client is
    /// Neovim's own, or a switch it needs is. `None` when it does what it says.
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

/// A row with only what every row has; the few with more say so after.
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

/// One of nvmux's own client's settings.
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

/// The setting under a name of its own, for one whose key is not enough.
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
    // A row a command, from the table rather than written out here, so a
    // command cannot be added without one.
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
            "predict",
            Tab::Client,
            "over a slow link, scrolls show before Neovim answers; auto: over ssh",
            Kind::Choice {
                options: &["auto", "on", "off"],
                get: |s| s.client.predict.name(),
                set: |s, name| {
                    if let Some(way) = Predict::ALL.into_iter().find(|w| w.name() == name) {
                        s.client.predict = way;
                    }
                },
            },
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
            "the picker, settings and help float over the session; off, full screen",
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

/// Every setting, in the file's order.
pub fn all() -> &'static [Setting] {
    &ROWS
}

/// The settings on `tab`, in the file's order.
pub fn on(tab: Tab) -> impl Iterator<Item = &'static Setting> {
    all().iter().filter(move |s| s.tab == tab)
}

/// The setting at `path` in the file, `effects.fade.enabled`, if there is one.
pub fn find(path: &str) -> Option<&'static Setting> {
    all().iter().find(|s| s.path() == path)
}

/// The settings `ours` differs from `theirs` in, in the file's order: what is
/// unsaved, with the file's settings for `theirs`.
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

    /// Every setting the first-run template documents, by its path: each
    /// commented line uncommented, as the template is meant to be edited, and
    /// read back as TOML.
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

    /// A setting cannot be added to the file without a row on the screen, and
    /// no row can name a key the file does not have.
    #[test]
    fn every_setting_in_the_file_is_a_row_and_every_row_is_in_the_file() {
        let rows: BTreeSet<String> = all().iter().map(Setting::path).collect();
        assert_eq!(rows.len(), all().len(), "two rows for one key");
        assert_eq!(rows, template_paths());
    }

    /// A value the row does not start at: the next one along.
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

    /// Each row reads and writes its own setting and no other's, and what it
    /// writes is a setting nvmux would load.
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

    /// A number is held to its range on the way in, whatever the caller asks.
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

    /// The keys tab is the prefix, the timeout, and then every command, each
    /// under the name `[keys.bind]` knows it by.
    #[test]
    fn the_keys_tab_holds_the_prefix_the_timeout_and_every_command() {
        let names: Vec<&str> = on(Tab::Keys).map(|s| s.name).collect();
        let mut want = vec!["prefix", "timeout_ms"];
        want.extend(BINDINGS.iter().map(|b| b.hint));
        assert_eq!(names, want);
    }

    /// Every row is on the tab of its table, and the tabs are in the file's
    /// order: walking the tabs in turn walks the rows in turn.
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

    /// Names are what the screen and its find go by, so no two rows share one.
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

    /// At the defaults only what waits on two settings that start off —
    /// `centre` and the particles — has no effect.
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

    /// The master switch off silences every effect but itself, before
    /// anything else is asked.
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

    /// Neovim's own client acts on none of nvmux's client's settings.
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

    /// A switch a setting needs, off, silences it and only it; and `centre`
    /// silences `window_width` only off, either way of centring giving it a
    /// say.
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
