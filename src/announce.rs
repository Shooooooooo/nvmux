//! The brief "you are now on this session" announcement.
//!
//! Cycling with `<prefix> n` and `<prefix> p` means changing session without
//! naming one, so an attach has to say where it landed. That is easy in the
//! picker and awkward here: while a session is attached, nvmux owns no cells at
//! all. `nvim --remote-ui` draws the screen and [`crate::pty`] copies its bytes
//! through without looking at them, so there is nothing to composite into and
//! nothing to read back.
//!
//! Three mechanisms answer that, and the config file chooses between them (see
//! [`crate::config::PopupStyle`]) because none of them is obviously right:
//!
//! * **`overlay`** — nvmux writes a box over the session's screen itself, and
//!   `pty::repaint` then puts back what it covered: the same request a resume
//!   makes, since only the server knows what was underneath. It touches no
//!   editor state at all, and it is the only one that still says something when
//!   the editor is wedged. It is also the only one that writes into the
//!   child-to-terminal direction — see the carve-out in [`crate::pty`]'s module
//!   docs.
//! * **`float`** — the session's own Neovim opens a floating window and closes
//!   it on its own timer. Never torn, never covered, correct by construction —
//!   at the cost of nvmux putting a scratch buffer and a window inside the
//!   user's editor.
//! * **`echo`** — one line on the message row. The least intrusive by a wide
//!   margin, and the least like a popup.
//!
//! # The lull
//!
//! All three fire at the same moment: the first pass of the relay loop on which
//! the child has been quiet for [`SETTLE`]. For `overlay` that is the *only*
//! safe moment — `pump` never parses the child's output, so it cannot otherwise
//! know that a write of its own would not land in the middle of one of the
//! child's escape sequences, and a lull is the one state where it cannot. For
//! `echo` it matters for a different reason: a message emitted before the new UI
//! has attached is a message to nobody. For `float` it makes no difference, and
//! one rule is better than three.
//!
//! If no lull arrives within [`GIVE_UP`], nothing is shown. A screen that has
//! been busy for two solid seconds is one where a box dropped on top is as
//! likely to be corruption as information, and the name is not worth that.
//!
//! Nothing here is ever worth delaying or failing an attach for. Every failure
//! — a terminal too small for the box, an editor too busy to answer — is
//! silence.

use std::path::Path;
use std::time::{Duration, Instant};

use rmpv::Value;
use unicode_width::UnicodeWidthStr;

use crate::config::PopupStyle;
use crate::pty::PtySize;
use crate::rpc;
use crate::ui::draw::{truncate, NUM_GAP};

/// How long the child must have been quiet before the announcement is shown.
///
/// Not zero, and this is the whole reason: a child whose own write blocked
/// because the pty buffer filled leaves the master momentarily unreadable in the
/// *middle* of a frame. That gap is microseconds; a wait this long steps over
/// it, and is still short enough that the announcement reads as part of the
/// attach rather than as something that happened afterwards.
const SETTLE: Duration = Duration::from_millis(25);

/// How long to wait for that lull before giving up on the announcement.
const GIVE_UP: Duration = Duration::from_secs(2);

/// Columns between the box's border and its text.
const PAD: usize = 1;

/// The smallest screen the box fits on: two borders, two padding columns and a
/// column of text across, and three rows down.
const MIN_COLS: u16 = 5;
const MIN_ROWS: u16 = 3;

/// `2  dotfiles` — a picker row without its selection marker.
///
/// The gap comes from the picker itself, so the list and the announcement
/// cannot come to spell a session differently.
///
/// Control characters are dropped. [`crate::session::validate_name`] already
/// refuses them, but it guards the *creating* path only: `<id>.json` is a file
/// on disk that a person can write, and this is the one place a name reaches a
/// raw terminal — `OPOST` off, no escaping between here and the wire — where an
/// escape sequence smuggled into a name would be executed rather than shown.
pub fn label(num: u32, name: &str) -> String {
    let name: String = name.chars().filter(|c| !c.is_control()).collect();
    format!("{num}{NUM_GAP}{name}")
}

/// What the relay must do about the announcement on this pass of its loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// Nothing this time round.
    Idle,
    /// Write these bytes to the terminal, over the session's screen.
    Paint(Vec<u8>),
    /// The overlay's time is up. Take it off the screen, then drop the popup.
    Erase,
    /// Nothing more will happen. Drop the popup.
    Done,
}

/// An announcement waiting for its moment, and then living out its time.
///
/// Owned by the relay loop, one per attach. Told the time rather than reading a
/// clock, like [`crate::keys::Prefix`] is told its bytes, so every state it can
/// reach is reachable from a test.
///
/// The deadlines are absolute [`Instant`]s for the reason written down in
/// [`crate::pty`]: a child that keeps producing output keeps `poll` returning
/// early, and only the *quiet* clock may restart on that. If the other two
/// restarted with it, a session with a spinner in its statusline would hold the
/// box on screen for ever.
#[derive(Debug)]
pub struct Popup {
    label: String,
    style: PopupStyle,
    duration: Duration,
    /// When the child was last seen with nothing to say, or `None` if it spoke
    /// on the last pass.
    quiet_since: Option<Instant>,
    /// When to stop waiting for a lull that is not coming.
    give_up_at: Instant,
    /// Set when the box is first painted: the end of its life.
    until: Option<Instant>,
    /// False whenever the child may have drawn over the box since it was
    /// painted, which is the cue to paint it again at the next lull.
    painted: bool,
}

impl Popup {
    /// `None` when there is nothing to say, or the configured style says
    /// nothing — so the relay carries no state at all for a user who turned
    /// this off.
    pub fn arm(label: Option<String>, now: Instant) -> Option<Self> {
        let settings = crate::config::popup();
        let label = label.filter(|_| settings.style != PopupStyle::Off)?;
        Some(Self {
            label,
            style: settings.style,
            duration: Duration::from_millis(settings.duration_ms),
            quiet_since: Some(now),
            give_up_at: now + GIVE_UP,
            until: None,
            painted: false,
        })
    }

    /// When the relay must next wake up on this popup's account.
    pub fn wake_at(&self, now: Instant) -> Instant {
        let settle = self.quiet_since.unwrap_or(now) + SETTLE;
        match self.until {
            // Repaint at the next lull, but never past the end of its life.
            Some(until) if !self.painted => settle.min(until),
            Some(until) => until,
            None => settle.min(self.give_up_at),
        }
    }

    /// One pass of the relay loop. `child_spoke` is whether the pty master had
    /// anything for the terminal this time round.
    ///
    /// `sock` is the session's socket, used only by the two editor-side styles.
    /// They are asked exactly once, as a notification, so an editor that is busy
    /// cannot make the relay wait — see [`crate::rpc::Client::notify`].
    pub fn step(&mut self, now: Instant, child_spoke: bool, sock: &Path, size: PtySize) -> Act {
        if child_spoke {
            self.quiet_since = None;
            // Whatever was on screen may have been drawn over.
            self.painted = false;
        } else {
            self.quiet_since.get_or_insert(now);
        }

        if self.until.is_some_and(|until| now >= until) {
            return Act::Erase;
        }
        let settled = self
            .quiet_since
            .is_some_and(|quiet| now.duration_since(quiet) >= SETTLE);
        if !settled {
            // Nothing is on screen yet and the editor has been busy throughout:
            // let it be.
            return if self.until.is_none() && now >= self.give_up_at {
                Act::Done
            } else {
                Act::Idle
            };
        }

        match self.style {
            // `arm` refuses to build one of these, so this is unreachable in
            // practice; saying `Done` rather than panicking keeps a config
            // reloaded mid-relay from taking a session with it.
            PopupStyle::Off => Act::Done,
            PopupStyle::Float | PopupStyle::Echo => {
                via_nvim(self.style, sock, &self.label, self.duration, size);
                Act::Done
            }
            PopupStyle::Overlay => {
                if self.painted {
                    return Act::Idle;
                }
                let Some(bytes) = overlay_bytes(&self.label, size) else {
                    // Too small to draw on. Saying nothing is the right answer,
                    // and there is nothing to erase either.
                    return Act::Done;
                };
                self.painted = true;
                self.until.get_or_insert(now + self.duration);
                Act::Paint(bytes)
            }
        }
    }
}

/// The overlay as terminal bytes: a bordered box across the top of the screen,
/// centred horizontally.
///
/// Pure, so the geometry can be tested without a terminal. `None` when the
/// screen cannot hold even a one-column box.
///
/// The top rather than the middle: the middle is where the text you have just
/// switched to is, and the bottom two rows are the statusline and the message
/// row. The top is the one band a reader is least likely to be looking at.
///
/// What the sequence has to get right, in order:
///
/// * a synchronized-update span, so the box cannot be seen half-drawn;
/// * `DECSC`/`DECRC` (`ESC 7` / `ESC 8`) around everything — without them the
///   cursor would sit in the box's corner for as long as the box is up, and the
///   editor's own SGR attributes would be left as whatever this drew with. It
///   is a single shared save slot, which is safe here only because nothing is
///   written except at a lull, between the child's frames;
/// * `ESC [ 0 m` on each row, because the interior is spaces and a space is
///   erased with *the current background* — which is whatever the editor last
///   set, and would otherwise bleed into the box;
/// * an absolute `CUP` per row, and not one newline or carriage return
///   anywhere. `OPOST` is off in raw mode (see [`crate::term`]), so those move
///   the cursor rather than wrapping and are the classic way to corrupt a raw
///   overlay. It also means a full-width box never scrolls the screen: the
///   pending-wrap flag its last cell sets is discarded by the next `CUP`, and
///   the last thing written is `DECRC`.
pub fn overlay_bytes(label: &str, size: PtySize) -> Option<Vec<u8>> {
    if size.cols < MIN_COLS || size.rows < MIN_ROWS {
        return None;
    }

    // Measured on the truncated text's own width, not on the room it was given:
    // a wide character that did not fit leaves the text narrower than its
    // budget, and a box sized to the budget would sit visibly loose around it.
    let text = truncate(label, usize::from(size.cols) - 2 - 2 * PAD);
    if text.is_empty() {
        return None;
    }
    let inner = text.width() + 2 * PAD;
    let width = inner + 2;

    let left = (usize::from(size.cols) - width) / 2 + 1;
    let pad = " ".repeat(PAD);
    let bar = "─".repeat(inner);
    let rows = [
        format!("╭{bar}╮"),
        format!("│{pad}{text}{pad}│"),
        format!("╰{bar}╯"),
    ];

    let mut out = String::from("\x1b[?2026h\x1b7");
    for (i, row) in rows.iter().enumerate() {
        out.push_str(&format!("\x1b[{};{left}H\x1b[0m{row}", i + 1));
    }
    out.push_str("\x1b8\x1b[?2026l");
    Some(out.into_bytes())
}

/// Ask the session's Neovim to show the announcement itself.
///
/// Best effort, exactly like [`crate::transport::install_detach_alias`]: a
/// session that cannot be reached simply says nothing, and the attach carries
/// on.
fn via_nvim(style: PopupStyle, sock: &Path, label: &str, duration: Duration, size: PtySize) {
    let (method, params) = match style {
        PopupStyle::Float => (
            "nvim_exec_lua",
            vec![
                Value::String(FLOAT_LUA.into()),
                // The label is an *argument*, never spliced into the source, so
                // there is no quoting question to get wrong.
                Value::Array(vec![
                    Value::String(label.into()),
                    Value::from(duration.as_millis().min(u128::from(u64::MAX)) as u64),
                ]),
            ],
        ),
        PopupStyle::Echo => (
            "nvim_echo",
            vec![
                // A one-element chunk: no highlight group, so this stays as
                // colourless as everything else nvmux puts on a screen.
                Value::Array(vec![Value::Array(vec![Value::String(
                    echo_label(label, size.cols).into(),
                )])]),
                // Not in `:messages`: an announcement is not history.
                Value::Boolean(false),
                Value::Map(vec![]),
            ],
        ),
        PopupStyle::Overlay | PopupStyle::Off => return,
    };

    let send = || -> Result<(), crate::error::RpcError> {
        let mut client = rpc::Client::connect(sock, rpc::CONNECT_TIMEOUT)?;
        client.notify(method, params)
    };
    if let Err(e) = send() {
        tracing::debug!(error = %e, method, "could not announce the session");
    }
}

/// Keep an echoed name short enough that Neovim does not turn it into a
/// hit-enter prompt.
///
/// A name may be 64 bytes, which is wider than a narrow terminal; a message that
/// does not fit on the message row stops the editor dead until the user presses
/// return, which is a great deal worse than not knowing the session's name. The
/// margin covers the `-- INSERT --`-sized furniture the row may already carry.
fn echo_label(label: &str, cols: u16) -> String {
    truncate(label, usize::from(cols.saturating_sub(12)).max(1))
}

/// Opened unfocused, with `noautocmd` so nothing in the user's config sees a
/// window come and go, and closed by a timer *inside* Neovim — so it disappears
/// on time even if nvmux is killed first.
///
/// Wrapped in `pcall` throughout: a notification is not answered, so an error
/// raised in here has nowhere to go except the user's own message row, which
/// would make a cosmetic feature into an irritation.
const FLOAT_LUA: &str = r#"
local label, ms = ...
pcall(function()
  if vim.o.columns < 8 or vim.o.lines < 5 then return end
  local width = math.min(vim.fn.strdisplaywidth(label) + 2, vim.o.columns - 4)
  local buf = vim.api.nvim_create_buf(false, true)
  vim.api.nvim_buf_set_lines(buf, 0, -1, false, { ' ' .. label .. ' ' })
  local ok, win = pcall(vim.api.nvim_open_win, buf, false, {
    relative = 'editor',
    width = width,
    height = 1,
    row = 0,
    col = math.floor((vim.o.columns - width) / 2),
    style = 'minimal',
    border = 'rounded',
    focusable = false,
    noautocmd = true,
    zindex = 300,
  })
  if not ok then
    pcall(vim.api.nvim_buf_delete, buf, { force = true })
    return
  end
  vim.defer_fn(function()
    pcall(vim.api.nvim_win_close, win, true)
    pcall(vim.api.nvim_buf_delete, buf, { force = true })
  end, ms)
end)
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_support::TINY_SIZES;

    fn size(cols: u16, rows: u16) -> PtySize {
        PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    /// A pty socket that is not there, so the two editor-side styles fail fast
    /// and the state machine can be driven without a Neovim.
    fn nowhere() -> &'static Path {
        Path::new("/nonexistent/nvmux-test.sock")
    }

    /// Split what `overlay_bytes` emits into `(row, column)` placements and the
    /// text drawn at each. Every escape run starts with `ESC`, so the pieces
    /// between them are exactly the sequence and whatever followed it.
    fn placed(bytes: &[u8]) -> Vec<((usize, usize), String)> {
        let text = String::from_utf8(bytes.to_vec()).expect("utf-8");
        let mut out = vec![];
        let mut at = None;
        for piece in text.split('\x1b') {
            if let Some(coords) = piece.strip_prefix('[').and_then(|p| p.strip_suffix('H')) {
                let (row, col) = coords.split_once(';').expect("row;col");
                at = Some((
                    row.parse::<usize>().expect("row"),
                    col.parse::<usize>().expect("col"),
                ));
            } else if let Some(drawn) = piece.strip_prefix("[0m") {
                out.push((
                    at.take().expect("a row is placed before it is drawn"),
                    drawn.to_string(),
                ));
            }
        }
        out
    }

    /// Just the text of each row, in order.
    fn rows_of(bytes: &[u8]) -> Vec<String> {
        placed(bytes).into_iter().map(|(_, row)| row).collect()
    }

    #[test]
    fn the_label_reads_like_a_picker_row() {
        assert_eq!(label(2, "dotfiles"), format!("2{NUM_GAP}dotfiles"));
        assert_eq!(label(12, "api server"), format!("12{NUM_GAP}api server"));
    }

    /// `validate_name` guards the creating path, not `<id>.json`, which is a
    /// file a person can write. This is the one place a name reaches a raw
    /// terminal with `OPOST` off, so an escape sequence smuggled into one would
    /// be executed rather than shown.
    #[test]
    fn a_control_character_in_a_name_cannot_reach_the_terminal() {
        let sneaky = label(1, "ok\x1b[31mred\x07\n");
        assert!(!sneaky.contains('\x1b'), "{sneaky:?}");
        assert!(!sneaky.contains('\x07'), "{sneaky:?}");
        assert!(!sneaky.contains('\n'), "{sneaky:?}");
        assert_eq!(sneaky, format!("1{NUM_GAP}ok[31mred"));
    }

    /// The cursor and the editor's own attributes have to come back exactly as
    /// they were, and the box must never be presented half-drawn.
    #[test]
    fn the_overlay_saves_and_restores_inside_one_synchronized_frame() {
        let bytes = overlay_bytes("2  dotfiles", size(80, 24)).expect("drawn");
        let text = String::from_utf8(bytes).expect("utf-8");
        assert!(text.starts_with("\x1b[?2026h\x1b7"), "{text:?}");
        assert!(text.ends_with("\x1b8\x1b[?2026l"), "{text:?}");
        assert_eq!(text.matches('\x07').count(), 0);
    }

    /// With `OPOST` off a newline moves the cursor down a column rather than to
    /// the start of the next line, which is the classic way to shear a raw
    /// overlay across the screen.
    #[test]
    fn the_overlay_never_writes_a_newline_or_a_carriage_return() {
        let bytes = overlay_bytes("2  dotfiles", size(80, 24)).expect("drawn");
        assert!(!bytes.contains(&b'\n'), "a newline reached the terminal");
        assert!(
            !bytes.contains(&b'\r'),
            "a carriage return reached the terminal"
        );
    }

    /// The picker sets no colour at all so it inherits the terminal's palette;
    /// a box painted over the editor has even less business choosing one. The
    /// raw-byte counterpart of `test_support::assert_no_colour`.
    #[test]
    fn nothing_in_the_overlay_sets_a_colour() {
        let bytes = overlay_bytes("2  dotfiles", size(80, 24)).expect("drawn");
        let text = String::from_utf8(bytes).expect("utf-8");
        for sgr in text.split("\x1b[").filter_map(|p| p.strip_suffix('m')) {
            assert_eq!(sgr, "0", "the only SGR may be a reset, found {sgr:?}");
        }
    }

    /// Every row is placed absolutely and inside the screen. A box that runs
    /// off the right-hand edge wraps, and a wrap at the bottom scrolls the
    /// editor's screen up by a line — which nvmux cannot undo.
    #[test]
    fn every_row_is_positioned_absolutely_and_fits_the_screen() {
        for &(cols, rows) in &[(80u16, 24u16), (20, 5), (5, 3), (13, 3)] {
            let Some(bytes) = overlay_bytes("2  dotfiles", size(cols, rows)) else {
                continue;
            };
            let text = String::from_utf8(bytes).expect("utf-8");
            let widths: Vec<usize> = rows_of(text.as_bytes()).iter().map(|r| r.width()).collect();
            assert_eq!(widths.len(), 3, "three rows at {cols}x{rows}: {text:?}");
            assert!(
                widths.windows(2).all(|w| w[0] == w[1]),
                "the box is ragged at {cols}x{rows}: {widths:?}"
            );

            let placements: Vec<(usize, usize)> = placed(text.as_bytes())
                .into_iter()
                .map(|(at, _)| at)
                .collect();
            assert_eq!(placements.len(), 3, "one placement per row");
            for (i, (row, col)) in placements.iter().enumerate() {
                assert_eq!(*row, i + 1, "the box sits at the top of the screen");
                assert!(*row <= usize::from(rows), "row {row} past {rows}");
                assert!(
                    col + widths[i] <= usize::from(cols) + 1,
                    "{cols}x{rows}: a row runs off the right-hand edge"
                );
            }
        }
    }

    /// Two columns per character, so a Japanese name is boxed to what it
    /// occupies rather than to how many bytes it takes.
    #[test]
    fn a_wide_name_is_measured_in_columns_not_bytes() {
        let bytes = overlay_bytes("1  日本語", size(40, 10)).expect("drawn");
        let rows = rows_of(&bytes);
        assert_eq!(rows.len(), 3);
        assert!(rows[1].contains("日本語"), "{rows:?}");
        // "1" + two spaces + three double-width characters + a pad each side.
        assert_eq!(rows[1].width(), 3 + 6 + 2 * PAD + 2);
        assert!(
            rows.iter().all(|r| r.width() == rows[0].width()),
            "{rows:?}"
        );
    }

    /// A terminal with no room for the box gets no box, rather than a fragment
    /// of one wrapped across the editor's text.
    #[test]
    fn a_terminal_too_small_for_the_box_gets_nothing() {
        for &(cols, rows) in TINY_SIZES {
            let drawn = overlay_bytes("2  dotfiles", size(cols, rows));
            if let Some(bytes) = drawn {
                let widths: Vec<usize> = rows_of(&bytes).iter().map(|r| r.width()).collect();
                assert!(
                    widths.iter().all(|w| *w <= usize::from(cols)),
                    "{cols}x{rows} drew {widths:?}"
                );
                assert!(rows >= MIN_ROWS, "{cols}x{rows} drew three rows");
            }
        }
    }

    /// The lull clock restarts on every byte the child writes; the other two
    /// must not, or a session with a spinner in its statusline would hold the
    /// box on screen for ever and never reach its own deadline. The same
    /// lesson as the absolute deadline in `crate::pty`.
    #[test]
    fn the_lull_clock_restarts_on_output_but_the_life_does_not() {
        let t0 = Instant::now();
        let mut popup = Popup {
            label: "2  dotfiles".into(),
            style: PopupStyle::Overlay,
            duration: Duration::from_millis(1000),
            quiet_since: Some(t0),
            give_up_at: t0 + GIVE_UP,
            until: None,
            painted: false,
        };
        let big = size(80, 24);

        // Still chattering: nothing is drawn, however long it goes on.
        for ms in [10, 100, 500] {
            let act = popup.step(t0 + Duration::from_millis(ms), true, nowhere(), big);
            assert_eq!(act, Act::Idle, "drew at {ms}ms while the child was talking");
        }
        // Quiet, but not yet long enough.
        assert_eq!(
            popup.step(t0 + Duration::from_millis(510), false, nowhere(), big),
            Act::Idle
        );
        // Quiet for a whole SETTLE: now it paints, and the clock starts here.
        let painted = t0 + Duration::from_millis(600);
        assert!(matches!(
            popup.step(painted, false, nowhere(), big),
            Act::Paint(_)
        ));
        assert_eq!(popup.until, Some(painted + Duration::from_millis(1000)));

        // A repaint by the editor puts the box back at the *next* lull — one
        // whole SETTLE later, not the first pass after it — without buying the
        // box any more time on screen.
        assert_eq!(
            popup.step(painted + Duration::from_millis(10), true, nowhere(), big),
            Act::Idle
        );
        assert_eq!(
            popup.step(painted + Duration::from_millis(20), false, nowhere(), big),
            Act::Idle,
            "one quiet pass is not a lull"
        );
        assert!(matches!(
            popup.step(painted + Duration::from_millis(60), false, nowhere(), big),
            Act::Paint(_)
        ));
        assert_eq!(popup.until, Some(painted + Duration::from_millis(1000)));

        // And it ends on time regardless.
        assert_eq!(
            popup.step(painted + Duration::from_millis(1001), false, nowhere(), big),
            Act::Erase
        );
    }

    /// A screen that has been busy for two solid seconds is one where a box
    /// dropped on top is as likely to be corruption as information.
    #[test]
    fn an_overlay_that_never_finds_a_lull_gives_up_rather_than_drawing_late() {
        let t0 = Instant::now();
        let mut popup = Popup {
            label: "2  dotfiles".into(),
            style: PopupStyle::Overlay,
            duration: Duration::from_millis(1000),
            quiet_since: Some(t0),
            give_up_at: t0 + GIVE_UP,
            until: None,
            painted: false,
        };
        let big = size(80, 24);
        let mut at = t0;
        while at + Duration::from_millis(10) < t0 + GIVE_UP {
            at += Duration::from_millis(10);
            assert_eq!(popup.step(at, true, nowhere(), big), Act::Idle, "at {at:?}");
        }
        assert_eq!(
            popup.step(t0 + GIVE_UP, true, nowhere(), big),
            Act::Done,
            "it must stop trying rather than draw onto a busy screen"
        );
    }

    /// The two editor-side styles are asked once and then finished: nvmux has
    /// nothing more to draw and nothing to erase.
    #[test]
    fn the_editor_side_styles_ask_once_and_are_done() {
        let t0 = Instant::now();
        for style in [PopupStyle::Float, PopupStyle::Echo] {
            let mut popup = Popup {
                label: "2  dotfiles".into(),
                style,
                duration: Duration::from_millis(1000),
                quiet_since: Some(t0),
                give_up_at: t0 + GIVE_UP,
                until: None,
                painted: false,
            };
            assert_eq!(
                popup.step(
                    t0 + Duration::from_millis(1),
                    false,
                    nowhere(),
                    size(80, 24)
                ),
                Act::Idle,
                "{style:?} spoke before the lull"
            );
            assert_eq!(
                popup.step(t0 + SETTLE, false, nowhere(), size(80, 24)),
                Act::Done,
                "{style:?} should ask the editor and finish"
            );
        }
    }

    /// A name may be 64 bytes; a message too wide for the row stops the editor
    /// dead on a hit-enter prompt, which is far worse than not knowing which
    /// session you are in.
    #[test]
    fn an_echoed_name_is_kept_short_enough_not_to_stop_the_editor() {
        let long = label(1, &"x".repeat(64));
        for cols in [20u16, 40, 80, 200] {
            let shown = echo_label(&long, cols);
            assert!(
                shown.width() + 12 <= usize::from(cols).max(13),
                "{cols} columns: {shown:?}"
            );
        }
        // Even an absurdly narrow terminal gets something rather than nothing.
        assert!(!echo_label(&long, 1).is_empty());
    }

    /// The label is an argument, never spliced into the source: that is what
    /// removes every quoting question from a name that may contain quotes,
    /// backslashes and brackets.
    #[test]
    fn the_float_lua_takes_the_label_as_an_argument() {
        assert!(FLOAT_LUA.contains("local label, ms = ..."), "{FLOAT_LUA}");
        assert!(!FLOAT_LUA.contains("{}"), "nothing is formatted into it");
    }

    /// Each of these is load-bearing: focus must not move, the user's autocmds
    /// must not fire, and an error in here has nowhere to go but the user's own
    /// message row, because a notification is never answered.
    #[test]
    fn the_float_never_takes_focus_and_never_raises_into_the_users_editor() {
        for needle in [
            "focusable = false",
            "noautocmd = true",
            "style = 'minimal'",
            "zindex = 300",
            "pcall(vim.api.nvim_open_win, buf, false,",
            "pcall(",
            "vim.defer_fn(",
        ] {
            assert!(FLOAT_LUA.contains(needle), "the float lost {needle:?}");
        }
    }
}
