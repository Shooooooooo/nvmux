//! End-to-end relay tests through a real `nvim --remote-ui` client.
//!
//! The prefix must reach the prefix machine however the terminal encodes it.
//! The client negotiates the kitty keyboard protocol, or else xterm's
//! `modifyOtherKeys`, through the relay untouched, so `Ctrl-Space` arrives as
//! `CSI 32 ; 5 u` (kitty, Ghostty, Windows Terminal 1.25+) or
//! `CSI 27 ; 5 ; 32 ~` (xterm, WezTerm) instead of `NUL`. Before `keyseq` these
//! went straight to the editor and nobody could detach.
//!
//! The relay uses fds 0 and 1, so each test re-runs this binary on a pty as
//! [`relay_child`] and plays the terminal on the master side: it answers the
//! client's queries, waits for the protocol to be enabled, and types. Skipped
//! without a usable `nvim`.
//!
//! Also covered here, since they need a real client painting a real screen:
//! the hint bar ([`nvmux::hint`]) covering and restoring the last row (also
//! while the editor redraws it), resuming a parked client (`[client]
//! per_session`) with a busy server, a client started ahead (`[client]
//! lazy = false`) answering its startup queries on first visit, and a large
//! paste reaching the editor while the client draws.

#[macro_use]
mod common;

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::Scratch;
use nvmux::transport::Transport;
use pty_process::blocking::{Command, Pty};

/// The session socket and id for the child.
const CHILD_SOCK: &str = "NVMUX_TEST_RELAY_SOCK";
const CHILD_ID: &str = "NVMUX_TEST_RELAY_ID";
/// Give the child a palette (no real terminal to query), enabling the fade
/// and the shadow screen the hint bar restores its row from.
const CHILD_PALETTE: &str = "NVMUX_TEST_RELAY_PALETTE";
/// That palette's text grey.
const PALETTE_FG: u8 = 200;
/// Child keeps its client (`[client] per_session`): `<prefix> Space` parks
/// it for [`PARKED_FOR`] instead of opening the picker, then resumes it.
const CHILD_KEEP: &str = "NVMUX_TEST_RELAY_KEEP";

/// Long enough for the parent to make the server busy before the resume.
const PARKED_FOR: Duration = Duration::from_secs(1);

/// Overrides [`PARKED_FOR`], in milliseconds.
const CHILD_PARKED_MS: &str = "NVMUX_TEST_RELAY_PARKED_MS";

/// Child starts its client ahead (`[client] lazy = false`): pooled for
/// [`AHEAD_FOR`], then relayed for the first time.
const CHILD_AHEAD: &str = "NVMUX_TEST_RELAY_AHEAD";

/// Long enough for the parent to see the client attached with nothing yet on
/// the terminal.
const AHEAD_FOR: Duration = Duration::from_secs(2);

/// Written once the client has been started ahead.
const AHEAD: &[u8] = b"\x1b]9999;ahead\x07";

/// Overrides [`AHEAD_FOR`], in milliseconds.
const CHILD_AHEAD_MS: &str = "NVMUX_TEST_RELAY_AHEAD_MS";

/// Label announced on attach and each resume, as a switch would (a card; see
/// [`nvmux::card`]). Unset, nothing is announced.
const CHILD_LABEL: &str = "NVMUX_TEST_RELAY_LABEL";

/// Use nvmux's own client (`nvmux --client`), which draws cards.
const CHILD_OWN: &str = "NVMUX_TEST_RELAY_OWN_CLIENT";

/// Post a card during the first relay, as the notifier does, leading to the
/// session with this id, or with `buf:<n>` to the child's buffer `n`.
const CHILD_POST: &str = "NVMUX_TEST_RELAY_POST";

/// The card a switch lands with, named this, as `main` passes it.
const CHILD_LANDS: &str = "NVMUX_TEST_RELAY_LANDS";

/// Make that card a step along its row, as `<prefix> n` does: from `aa` to
/// the named session, with twelve `b`s next and `cc` leaving the far end.
const CHILD_STEPS: &str = "NVMUX_TEST_RELAY_STEPS";

/// The body of a `[keys.bind]` table, parsed as in the config file.
const CHILD_BIND: &str = "NVMUX_TEST_RELAY_BIND";

/// When the card is posted: after the first paint.
const POST_AFTER: Duration = Duration::from_millis(1500);

/// Child uses `nvmux::pty::Attachment::tell_when_painted` and writes
/// [`PAINTED`] when told, or [`UNPAINTED`] if dropped untold.
const CHILD_PAINTED: &str = "NVMUX_TEST_RELAY_PAINTED";

const PAINTED: &[u8] = b"\x1b]9999;painted\x07";
const UNPAINTED: &[u8] = b"\x1b]9999;unpainted\x07";

/// A posted card's text, for the parent to find on screen.
const POSTED: &str = "posted for the session in front";

/// Exit code for a relay ending in a switch by session id, which only
/// `<prefix> Enter` on a card requests.
const SWITCHED_BY_CARD: i32 = 5;

/// Fade duration in milliseconds; needs [`CHILD_PALETTE`].
const CHILD_FADE_MS: &str = "NVMUX_TEST_RELAY_FADE_MS";

/// Written once the client is parked, so the parent knows the relay released
/// the terminal. An OSC no terminal acts on.
const PARKED: &[u8] = b"\x1b]9999;parked\x07";

/// A second session, started ahead in the child's pool, for the picker to
/// show behind it: the floating picker then floats, and a switch is relayed
/// on as `main` does (see [`preview_relays`]). Needs [`CHILD_KEEP`].
const CHILD_OTHER_ID: &str = "NVMUX_TEST_RELAY_OTHER_ID";
const CHILD_OTHER_SOCK: &str = "NVMUX_TEST_RELAY_OTHER_SOCK";

/// Written by a [`CHILD_OTHER_ID`] child after each switch.
const SWITCHED: &[u8] = b"\x1b]9999;switched\x07";

/// Pty size for a [`Terminal`] unless its [`Asks`] give one; tests must agree
/// with the child on which row is the last (the hint bar's).
const ROWS: u16 = 40;
const COLS: u16 = 120;

/// The client's kitty keyboard query and DA1, and replies: kitty "supported,
/// no flags yet", DA1 as a VT220. Without the kitty reply, the client falls
/// back to `modifyOtherKeys`.
const KITTY_QUERY: &[u8] = b"\x1b[?u";
const KITTY_REPLY: &[u8] = b"\x1b[?0u";
const DA1: &[u8] = b"\x1b[c";
const DA1_REPLY: &[u8] = b"\x1b[?62;22c";
/// Enables `modifyOtherKeys`.
const XTERM_PUSH: &[u8] = b"\x1b[>4;2m";
/// In-band resize (DEC mode 2048) query, and a "supported, not set" reply.
const IN_BAND_QUERY: &[u8] = b"\x1b[?2048$p";
const IN_BAND_REPLY: &[u8] = b"\x1b[?2048;2$y";

/// The child half: relay the session from the environment and report how it
/// ended via the exit code. Ignored, so only the parent runs it (`--ignored
/// --exact`); without the environment it does nothing.
#[test]
#[ignore = "the child half of the relay tests; run by them, not by hand"]
fn relay_child() {
    let (Ok(sock), Ok(id)) = (std::env::var(CHILD_SOCK), std::env::var(CHILD_ID)) else {
        return;
    };
    // A command timeout no scheduler hiccup can hit (the parent's gap after
    // the prefix is meant to clear the sequence wait, not race this), and
    // clients kept only with CHILD_KEEP.
    let mut settings = nvmux::config::with_prefix(nvmux::keys::PREFIX);
    settings.keys.timeout_ms = 10_000;
    // Neovim's client negotiates the keyboard through the relay; the default
    // `nvmux --client` is not this binary.
    settings.client.ui = nvmux::config::Ui::Nvim;
    let own = std::env::var_os(CHILD_OWN).is_some();
    if own {
        settings.client.ui = nvmux::config::Ui::Nvmux;
    }
    let keep = std::env::var_os(CHILD_KEEP).is_some();
    settings.client.per_session = keep;
    let other = std::env::var(CHILD_OTHER_ID)
        .ok()
        .zip(std::env::var_os(CHILD_OTHER_SOCK));
    // A full-screen picker ends the relay, which is where these tests park
    // and observe moved picker keys; a floating one stays in the relay.
    settings.style.float = other.is_some();
    if let Ok(bind) = std::env::var(CHILD_BIND) {
        let moved: nvmux::config::Settings =
            toml::from_str(&format!("[keys.bind]\n{bind}\n")).expect("a [keys.bind] table");
        settings.keys.bind = moved.keys.bind;
    }
    if let Some(ms) = std::env::var(CHILD_FADE_MS)
        .ok()
        .and_then(|ms| ms.parse().ok())
    {
        settings.effects.fade.duration_ms = ms;
    }
    nvmux::config::init(settings);

    // Without a palette there is no fade or shadow, so the parent sees only
    // the client's bytes, as the encoding tests want.
    if std::env::var_os(CHILD_PALETTE).is_some() {
        nvmux::palette::init(Some(nvmux::palette::Palette {
            fg: nvmux::palette::Rgb(PALETTE_FG, PALETTE_FG, PALETTE_FG),
            bg: nvmux::palette::Rgb(0, 0, 0),
            ansi: [nvmux::palette::Rgb(0, 0, 0); 16],
        }));
    }

    // Empty by default: an announcement would be noise in the stream.
    let label = std::env::var(CHILD_LABEL).unwrap_or_default();
    let mut pool = nvmux::pool::Pool::configured();
    let mut attachment = if std::env::var_os(CHILD_AHEAD).is_some() {
        start_ahead(&mut pool, &id, Path::new(&sock), own)
    } else if own {
        let exe = Path::new(env!("CARGO_BIN_EXE_nvmux"));
        nvmux::pty::spawn_own_client(&id, Path::new(&sock), &label, exe).expect("attach")
    } else {
        nvmux::pty::spawn(&id, Path::new(&sock), &label).expect("attach")
    };
    let post = std::env::var(CHILD_POST)
        .ok()
        .and_then(|to| match to.strip_prefix("buf:") {
            Some(n) => n.parse().ok().map(nvmux::card::Go::Buffer),
            None => nvmux::ids::Id::new(&to).map(nvmux::card::Go::Session),
        });
    if let Some(go) = post {
        let front = id.clone();
        std::thread::spawn(move || {
            std::thread::sleep(POST_AFTER);
            let card = nvmux::card::Card::news(
                nvmux::card::next_id(),
                nvmux::card::Kind::Finished,
                "elsewhere",
                POSTED,
                0,
                "Ctrl-Space ⏎ go there",
            );
            nvmux::card::board().post(nvmux::card::Post {
                front,
                what: nvmux::card::Posted::Card(card, Some(go)),
            });
        });
    }
    assert_eq!(
        attachment.is_kept(),
        keep,
        "[client] per_session = {keep} was not honoured"
    );
    if std::env::var_os(CHILD_PAINTED).is_some() {
        let painted = attachment.tell_when_painted();
        std::thread::spawn(move || {
            let said = match painted.recv_timeout(Duration::from_secs(30)) {
                Ok(()) => PAINTED,
                Err(_) => UNPAINTED,
            };
            let mut out = std::io::stdout();
            let _ = out.write_all(said);
            let _ = out.flush();
        });
    }
    if let Some((other_id, other_sock)) = other {
        let other_sock = std::path::PathBuf::from(other_sock);
        std::process::exit(preview_relays(
            attachment,
            &id,
            (&other_id, &other_sock),
            &mut pool,
        ));
    }
    let parked_for = std::env::var(CHILD_PARKED_MS)
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map_or(PARKED_FOR, Duration::from_millis);
    let lands = std::env::var(CHILD_LANDS).ok();
    let steps = std::env::var_os(CHILD_STEPS).is_some();
    let mut landing = |_| {
        lands.as_deref().map(|name| {
            let card = nvmux::card::Card::arrival(nvmux::card::next_id(), name, false);
            if !steps {
                return card;
            }
            card.in_row(nvmux::card::Row {
                prev: Some("aa".into()),
                next: Some("b".repeat(12)),
                step: Some(nvmux::card::Step {
                    forward: true,
                    gone: "cc".into(),
                }),
            })
        })
    };
    let code = loop {
        match nvmux::pty::relay(attachment, 0, &mut landing) {
            Ok((nvmux::pty::Outcome::Detached, _)) => break 0,
            // Instead of the picker: park, wait, take back.
            Ok((nvmux::pty::Outcome::ToPicker, held)) if keep => {
                assert!(pool.set_aside(held).is_none(), "the client was not parked");
                let mut out = std::io::stdout();
                let _ = out.write_all(PARKED);
                let _ = out.flush();
                std::thread::sleep(parked_for);
                attachment = match pool.take(&id) {
                    nvmux::pool::Taken::Kept(back) => *back,
                    other => panic!("the parked client did not come back: {other:?}"),
                };
                // As `main` does for a resumed client.
                if !label.is_empty() {
                    attachment.announce_on_arrival(label.clone());
                }
            }
            Ok((nvmux::pty::Outcome::Switch(nvmux::pty::Target::Session(to)), _)) => {
                eprintln!("switched by a card to {}", to.as_str());
                break SWITCHED_BY_CARD;
            }
            Ok((other, _)) => {
                eprintln!("relay ended with {other:?}");
                break 3;
            }
            Err(e) => {
                eprintln!("relay failed: {e}");
                break 4;
            }
        }
    };
    std::process::exit(code);
}

/// The relay's way to the pool, as `main` gives it: lent clients, and the
/// one a floating picker switched to in place.
struct Neighbours<'a> {
    pool: &'a mut nvmux::pool::Pool,
    chosen: &'a mut Option<nvmux::pty::Attachment>,
}

impl nvmux::pty::Others for Neighbours<'_> {
    fn begin_next(&mut self, _: nvmux::pty::Target) -> Option<nvmux::card::Card> {
        None
    }

    fn lend(&mut self, session_id: &str) -> Option<nvmux::pty::Attachment> {
        self.pool.lend(session_id)
    }

    fn give_back(&mut self, client: nvmux::pty::Attachment) {
        self.pool.give_back(client);
    }

    fn hand_over(&mut self, client: nvmux::pty::Attachment) {
        *self.chosen = Some(client);
    }
}

/// The [`CHILD_OTHER_ID`] child: relay `front` with `other` started ahead in
/// `pool`, both rows of the floating picker, and on a switch park the
/// session left and relay the one chosen, as `main` does. Returns the exit
/// code: 0 for a detach.
fn preview_relays(
    mut attachment: nvmux::pty::Attachment,
    front: &str,
    (other, other_sock): (&str, &Path),
    pool: &mut nvmux::pool::Pool,
) -> i32 {
    let ahead = nvmux::pty::spawn_client(other, other_sock, "").expect("start");
    let probe = nvmux::pty::Probe::start(other, other_sock).expect("probe");
    pool.start_ahead(ahead, Some(probe));
    let rows: Vec<nvmux::session::Session> = [front, other]
        .into_iter()
        .zip(1..)
        .map(|(id, num)| {
            let mut s = nvmux::session::Session::new(id.into(), format!("s{num}"), 0, num);
            s.state.num = num;
            s
        })
        .collect();
    loop {
        nvmux::ui::picker::hand(&rows, None);
        let mut chosen = None;
        let ended = {
            let mut neighbours = Neighbours {
                pool: &mut *pool,
                chosen: &mut chosen,
            };
            nvmux::pty::relay(attachment, 0, &mut neighbours)
        };
        match ended {
            Ok((nvmux::pty::Outcome::Detached, _)) => return 0,
            Ok((nvmux::pty::Outcome::Switch(nvmux::pty::Target::Session(to)), held)) => {
                assert!(
                    pool.set_aside(held).is_none(),
                    "the client left was not parked"
                );
                attachment = match chosen {
                    Some(lit) => {
                        pool.took(to.as_str());
                        lit
                    }
                    None => match pool.take(to.as_str()) {
                        nvmux::pool::Taken::Kept(next) => *next,
                        other => panic!("nothing to switch to: {other:?}"),
                    },
                };
                let mut out = std::io::stdout();
                let _ = out.write_all(SWITCHED);
                let _ = out.flush();
            }
            Ok((other, _)) => {
                eprintln!("relay ended with {other:?}");
                return 3;
            }
            Err(e) => {
                eprintln!("relay failed: {e}");
                return 4;
            }
        }
    }
}

/// Start a client ahead as `main` does, pool it for [`AHEAD_FOR`] (or
/// [`CHILD_AHEAD_MS`]), and take it back unrelayed with its probe answered (a
/// local server answers well within that). nvmux's own client if `own`.
fn start_ahead(
    pool: &mut nvmux::pool::Pool,
    id: &str,
    sock: &Path,
    own: bool,
) -> nvmux::pty::Attachment {
    let attachment = if own {
        let exe = Path::new(env!("CARGO_BIN_EXE_nvmux"));
        nvmux::pty::spawn_own_client(id, sock, "", exe).expect("start")
    } else {
        nvmux::pty::spawn_client(id, sock, "").expect("start")
    };
    let probe = nvmux::pty::Probe::start(id, sock).expect("probe");
    pool.start_ahead(attachment, Some(probe));
    let mut out = std::io::stdout();
    let _ = out.write_all(AHEAD);
    let _ = out.flush();
    let held_for = std::env::var(CHILD_AHEAD_MS)
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map_or(AHEAD_FOR, Duration::from_millis);
    std::thread::sleep(held_for);
    let mut back = match pool.take(id) {
        nvmux::pool::Taken::Kept(back) => *back,
        other => panic!("the client started ahead did not come back: {other:?}"),
    };
    assert!(back.started_ahead(), "taken back as one already shown");
    assert!(back.take_probe().is_none(), "the probe never answered");
    back
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Kitty,
    /// Ignores the kitty query, so the client uses `modifyOtherKeys`.
    Xterm,
}

struct Terminal {
    protocol: Protocol,
    /// Claims in-band size reports (DEC mode 2048), which the client then
    /// uses instead of `SIGWINCH`.
    in_band: bool,
    child: std::process::Child,
    /// Set once the child exited; it must not be signalled or waited on again.
    exited: Option<u32>,
    /// The pty master. Held so the pty outlives the child, which `Drop` reaps
    /// before the fields drop.
    pty: Pty,
    output: Vec<u8>,
    /// How far `output` was scanned for queries.
    answered: usize,
    incoming: mpsc::Receiver<Vec<u8>>,
}

/// The `CHILD_*` switches for a [`Terminal`]'s child (`shadow` is
/// [`CHILD_PALETTE`]; `in_band` and `size` are the terminal's), all off by
/// default.
#[derive(Default)]
struct Asks {
    shadow: bool,
    keep: bool,
    in_band: bool,
    ahead: bool,
    ahead_ms: Option<u64>,
    parked_ms: Option<u64>,
    label: Option<&'static str>,
    fade_ms: Option<u64>,
    own: bool,
    post: Option<&'static str>,
    lands: Option<&'static str>,
    steps: bool,
    bind: Option<&'static str>,
    painted: bool,
    /// [`CHILD_OTHER_ID`] and [`CHILD_OTHER_SOCK`].
    other: Option<(String, std::path::PathBuf)>,
    /// Rows and columns, if not [`ROWS`] by [`COLS`].
    size: Option<(u16, u16)>,
}

impl Terminal {
    /// Re-run this binary as `relay_child` on a fresh pty: no palette (so no
    /// fade) and no kept client, as the encoding tests want.
    fn spawn(sock: &Path, id: &str, protocol: Protocol) -> Self {
        Self::spawn_with(sock, id, protocol, false)
    }

    /// [`Terminal::spawn`], with [`CHILD_PALETTE`] if `shadow`.
    fn spawn_with(sock: &Path, id: &str, protocol: Protocol, shadow: bool) -> Self {
        Self::spawn_child(
            sock,
            id,
            protocol,
            Asks {
                shadow,
                ..Asks::default()
            },
        )
    }

    /// A [`CHILD_KEEP`] child.
    fn spawn_keeping(sock: &Path, id: &str, in_band: bool, fade: bool) -> Self {
        Self::spawn_child(
            sock,
            id,
            Protocol::Kitty,
            Asks {
                shadow: fade,
                keep: true,
                in_band,
                ..Asks::default()
            },
        )
    }

    /// A [`CHILD_AHEAD`] child that also keeps its client, as with the
    /// default config.
    fn spawn_ahead(sock: &Path, id: &str, fade: bool) -> Self {
        Self::spawn_child(
            sock,
            id,
            Protocol::Kitty,
            Asks {
                shadow: fade,
                keep: true,
                ahead: true,
                ..Asks::default()
            },
        )
    }

    /// A [`CHILD_LABEL`] child.
    fn spawn_announcing(sock: &Path, id: &str, label: &'static str, keep: bool) -> Self {
        Self::spawn_child(
            sock,
            id,
            Protocol::Kitty,
            Asks {
                keep,
                label: Some(label),
                ..Asks::default()
            },
        )
    }

    fn spawn_child(sock: &Path, id: &str, protocol: Protocol, asks: Asks) -> Self {
        let Asks {
            shadow,
            keep,
            in_band,
            ahead,
            ahead_ms,
            parked_ms,
            label,
            fade_ms,
            own,
            post,
            lands,
            steps,
            bind,
            painted,
            other,
            size,
        } = asks;
        let (pty, pts) = pty_process::blocking::open().expect("open a pty");
        let (rows, cols) = size.unwrap_or((ROWS, COLS));
        pty.resize(pty_process::Size::new(rows, cols))
            .expect("size the pty");

        let mut cmd = Command::new(std::env::current_exe().expect("current exe"))
            // `--nocapture`, or the child's diagnostics stay in libtest's
            // capture buffer and never reach this pty.
            .args(["--ignored", "--exact", "relay_child", "--nocapture"])
            .env(CHILD_SOCK, sock)
            .env(CHILD_ID, id)
            // As most terminals set. The kitty query is sent regardless, but
            // the `modifyOtherKeys` fallback is withheld from old VTE.
            .env("TERM", "xterm-256color")
            .env_remove("VTE_VERSION")
            // `NO_COLOR` forces the fade and shadow off; keep the child
            // independent of the caller's environment.
            .env_remove("NO_COLOR");
        if shadow {
            cmd = cmd.env(CHILD_PALETTE, "1");
        }
        if keep {
            cmd = cmd.env(CHILD_KEEP, "1");
        }
        if let Some(label) = label {
            cmd = cmd.env(CHILD_LABEL, label);
        }
        if ahead {
            cmd = cmd.env(CHILD_AHEAD, "1");
        }
        if let Some(ms) = ahead_ms {
            cmd = cmd.env(CHILD_AHEAD_MS, ms.to_string());
        }
        if let Some(ms) = parked_ms {
            cmd = cmd.env(CHILD_PARKED_MS, ms.to_string());
        }
        if let Some(ms) = fade_ms {
            cmd = cmd.env(CHILD_FADE_MS, ms.to_string());
        }
        if own {
            cmd = cmd.env(CHILD_OWN, "1");
        }
        if let Some(to) = post {
            cmd = cmd.env(CHILD_POST, to);
        }
        if let Some(name) = lands {
            cmd = cmd.env(CHILD_LANDS, name);
        }
        if steps {
            cmd = cmd.env(CHILD_STEPS, "1");
        }
        if let Some(bind) = bind {
            cmd = cmd.env(CHILD_BIND, bind);
        }
        if painted {
            cmd = cmd.env(CHILD_PAINTED, "1");
        }
        if let Some((id, sock)) = other {
            cmd = cmd.env(CHILD_OTHER_ID, id).env(CHILD_OTHER_SOCK, sock);
        }
        // Moves the slave in, or the master never sees EOF after the child
        // exits.
        let child = cmd.spawn(pts).expect("spawn relay child");

        let mut reader = common::reader(&pty);
        let (tx, incoming) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
        });

        Self {
            protocol,
            in_band,
            child,
            exited: None,
            pty,
            output: Vec::new(),
            answered: 0,
            incoming,
        }
    }

    /// Read output for up to `within`, answering queries, until `done` holds
    /// for all output so far. Returns whether it did.
    fn pump_until(&mut self, within: Duration, done: impl Fn(&[u8]) -> bool) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if done(&self.output) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return done(&self.output);
            }
            match self.incoming.recv_timeout(left) {
                Ok(bytes) => {
                    self.output.extend_from_slice(&bytes);
                    self.answer();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return done(&self.output),
            }
        }
    }

    /// Answer new queries. One may straddle two reads, so the scan backs up a
    /// little and counts only matches ending past the last scan.
    fn answer(&mut self) {
        let from = self.answered.saturating_sub(8);
        let mut replies = Vec::new();
        for (query, reply) in [
            (KITTY_QUERY, KITTY_REPLY),
            (DA1, DA1_REPLY),
            (IN_BAND_QUERY, IN_BAND_REPLY),
        ] {
            if query == KITTY_QUERY && self.protocol == Protocol::Xterm {
                continue;
            }
            if query == IN_BAND_QUERY && !self.in_band {
                continue;
            }
            let mut at = from;
            while let Some(i) = find(&self.output[at..], query).map(|i| at + i) {
                if i + query.len() > self.answered {
                    replies.push((i, reply));
                }
                at = i + query.len();
            }
        }
        // In the order asked, as the client expects.
        replies.sort_by_key(|&(i, _)| i);
        for (_, reply) in replies {
            self.reply(reply);
        }
        self.answered = self.output.len();
    }

    /// Reply unless the child exited: it does not wait for replies to what it
    /// asks on exit, which may be read only afterwards, when a pty write fails
    /// on macOS.
    fn reply(&mut self, bytes: &[u8]) {
        if self.exited.is_some() {
            return;
        }
        if let Err(err) = self.pty.write_all(bytes).and_then(|()| self.pty.flush()) {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Ok(Some(status)) = self.child.try_wait() {
                    self.exited = Some(common::exit_code(status));
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("write to pty: {err}");
        }
    }

    /// Read output for `within` without keeping or answering it, for a client
    /// drawing megabytes a second that asks nothing meanwhile.
    fn drop_output_for(&mut self, within: Duration) {
        let deadline = Instant::now() + within;
        while self
            .incoming
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .is_ok()
        {}
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        self.pty.write_all(bytes).expect("write to pty");
        self.pty.flush().expect("flush pty");
    }

    /// The child's exit code if it exits within `within`, with all its output
    /// read.
    fn exit_code(&mut self, within: Duration) -> Option<u32> {
        let deadline = Instant::now() + within;
        while self.exited.is_none() && Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exited = Some(common::exit_code(status));
                break;
            }
            // Keep answering: the client's exit waits on a DA1 reply.
            self.pump_until(Duration::from_millis(50), |_| false);
        }
        // Its last frames may still be in flight: read to EOF, which comes
        // once nothing holds the slave open.
        if self.exited.is_some() {
            self.pump_until(Duration::from_secs(5), |_| false);
        }
        self.exited
    }

    /// What the child wrote after `mark`, for a failure message.
    fn since(&self, mark: usize) -> String {
        let tail = &self.output[mark.min(self.output.len())..];
        tail.iter()
            .map(|&b| match b {
                0x20..=0x7e => (b as char).to_string(),
                _ => format!("\\x{b:02x}"),
            })
            .collect()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // A reaped child's pid may have been reused.
        if self.exited.is_none() {
            common::hang_up_and_reap(&mut self.child);
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Flags of the client's kitty push `CSI > flags u`, if sent. Neovim 0.10 and
/// 0.11 push 1, 0.12 pushes 3, so match by shape.
fn kitty_push_flags(out: &[u8]) -> Option<u32> {
    let mut at = 0;
    while let Some(i) = find(&out[at..], b"\x1b[>").map(|i| at + i) {
        let digits: Vec<u8> = out[i + 3..]
            .iter()
            .copied()
            .take_while(u8::is_ascii_digit)
            .collect();
        if !digits.is_empty() && out.get(i + 3 + digits.len()) == Some(&b'u') {
            return std::str::from_utf8(&digits).ok()?.parse().ok();
        }
        at = i + 3;
    }
    None
}

/// Attach a real client, let it negotiate `protocol`, type `prefix` then `d`,
/// and expect a detach.
fn detaches_with(tag: &str, protocol: Protocol, prefix: &[u8]) {
    require_nvim!();
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");

    let mut term = Terminal::spawn(&sock, &session.id, protocol);

    // The client enables the protocol only after our replies reach it
    // through nvmux.
    let pushed = match protocol {
        Protocol::Kitty => term.pump_until(Duration::from_secs(15), |out| {
            kitty_push_flags(out).is_some_and(|flags| flags & 1 != 0)
        }),
        Protocol::Xterm => term.pump_until(Duration::from_secs(15), |out| {
            find(out, XTERM_PUSH).is_some()
        }),
    };
    assert!(
        pushed,
        "the client never turned {protocol:?} on; got: {}",
        term.since(0)
    );
    let flags = kitty_push_flags(&term.output).unwrap_or(0);
    let mark = term.output.len();

    term.type_bytes(prefix);
    if flags & 2 != 0 {
        // With release reports on, the prefix's release must not count as
        // its command.
        term.type_bytes(b"\x1b[32;5:3u");
    }
    // A human gap, well past the sequence wait.
    term.pump_until(Duration::from_millis(150), |_| false);
    term.type_bytes(b"d");

    // The detach is the only signal: a leaked prefix would leave `d` pending
    // silently as an operator.
    let code = term.exit_code(Duration::from_secs(10));
    assert_eq!(
        code,
        Some(0),
        "expected a detach (exit 0) after {prefix:?} d; the child {}; output since the push: {}",
        match code {
            Some(c) => format!("exited with {c}"),
            None => "is still attached (the prefix reached the editor?)".to_string(),
        },
        term.since(mark)
    );
}

/// The reported bug: Windows Terminal 1.25 with the kitty protocol.
#[test]
fn a_kitty_encoded_prefix_detaches_through_a_real_client() {
    detaches_with("kitty", Protocol::Kitty, b"\x1b[32;5u");
}

#[test]
fn an_xterm_encoded_prefix_detaches_through_a_real_client() {
    detaches_with("xterm", Protocol::Xterm, b"\x1b[27;5;32~");
}

/// Neovim accepts `NUL` even with the protocol on, so the machine must too.
#[test]
fn the_control_byte_still_detaches_through_a_real_client() {
    detaches_with("byte", Protocol::Kitty, b"\x00");
}

#[test]
fn the_push_is_matched_by_shape() {
    assert_eq!(kitty_push_flags(b"\x1b[?2004h\x1b[>1u\x1b[?1004h"), Some(1));
    assert_eq!(kitty_push_flags(b"\x1b[>3u"), Some(3));
    // The terminal's reply, the xterm push and a pop are not pushes.
    assert_eq!(kitty_push_flags(b"\x1b[?0u\x1b[>4;2m\x1b[<u"), None);
    assert_eq!(kitty_push_flags(b"\x1b[>"), None);
}

/// One row of the screen `out` produces. nvmux, client and fade output
/// interleave, so rows are read from a parser, not the bytes.
fn row_on_screen(out: &[u8], row: u16) -> String {
    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    parser.process(out);
    row_of(parser.screen(), row)
}

fn row_of(screen: &vt100::Screen, row: u16) -> String {
    (0..COLS)
        .map(|col| match screen.cell(row, col) {
            Some(cell) if cell.has_contents() => cell.contents(),
            _ => " ",
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Each state `row` could be presented in, with its offset, consecutive
/// repeats merged: after each byte outside a synchronized update, and at each
/// update's end. Unlike [`row_on_screen`], this catches a row wiped and
/// restored by every frame.
fn presented(out: &[u8], row: u16) -> Vec<(usize, String)> {
    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    let mut moments: Vec<(usize, String)> = Vec::new();
    let mut held = false;
    for (i, byte) in out.iter().enumerate() {
        parser.process(std::slice::from_ref(byte));
        // A mode, as in terminals: nested begins do nothing, the first end
        // releases.
        let so_far = &out[..=i];
        if so_far.ends_with(nvmux::fade::SYNC_BEGIN) {
            held = true;
        } else if so_far.ends_with(nvmux::fade::SYNC_END) {
            held = false;
        }
        if held {
            continue;
        }
        let shown = row_of(parser.screen(), row);
        if moments.last().is_none_or(|(_, last)| *last != shown) {
            moments.push((i + 1, shown));
        }
    }
    moments
}

/// Dismissed with `<prefix> <prefix>`, which keeps the relay running; the
/// picker, detach and switch clear the screen and would hide a stuck bar.
/// `shadow` picks restoring from the shadow, or blanking and asking the server
/// to repaint, as the attach notice does.
fn the_hint_row_goes_up_and_comes_down(tag: &str, shadow: bool) {
    require_nvim!();
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    // The literal prefix below reaches Neovim, whose 'showcmd' would show
    // `<20>` on the row under test for a timing-dependent moment.
    nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
        .expect("connect")
        .command("set noshowcmd")
        .expect("noshowcmd");

    let mut term = Terminal::spawn_with(&sock, &session.id, Protocol::Kitty, shadow);

    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            kitty_push_flags(out).is_some()
        }),
        "the client never started; got: {}",
        term.since(0)
    );
    // Wait for the first paint; with the fade on it is held and dissolved in.
    term.pump_until(Duration::from_secs(2), |out| {
        !row_on_screen(out, 0).is_empty()
    });
    let under = row_on_screen(&term.output, ROWS - 1);

    // The prefix alone; the child's 10 s `timeout_ms` leaves the bar time to
    // find a lull.
    term.type_bytes(&[nvmux::keys::PREFIX]);
    let mark = term.output.len();
    // Wait for the whole row, not a read that ended mid-write.
    let listed = term.pump_until(Duration::from_secs(10), |out| {
        let row = row_on_screen(out, ROWS - 1);
        row.contains("␣ picker") && row.contains("d detach")
    });
    assert!(
        listed || row_on_screen(&term.output, ROWS - 1).contains("␣ picker"),
        "the prefix put no hint row on row {ROWS}; it says {:?}, and the child wrote: {}",
        row_on_screen(&term.output, ROWS - 1),
        term.since(mark)
    );
    assert!(
        row_on_screen(&term.output, ROWS - 1).contains("d detach"),
        "the row is not the key list: {:?}",
        row_on_screen(&term.output, ROWS - 1)
    );

    // A second prefix is sent literally and the relay continues, so the bar
    // must remove itself.
    let mark = term.output.len();
    term.type_bytes(&[nvmux::keys::PREFIX]);
    let back = term.pump_until(Duration::from_secs(10), |out| {
        row_on_screen(out, ROWS - 1) == under
    });
    assert!(
        back || !row_on_screen(&term.output, ROWS - 1).contains("picker"),
        "the hint row is still on row {ROWS} after the prefix resolved: {:?}; the child wrote: {}",
        row_on_screen(&term.output, ROWS - 1),
        term.since(mark)
    );
    assert_eq!(
        row_on_screen(&term.output, ROWS - 1),
        under,
        "the editor's own row did not come back"
    );

    term.type_bytes(&[nvmux::keys::PREFIX]);
    term.pump_until(Duration::from_millis(150), |_| false);
    term.type_bytes(b"d");
    assert_eq!(
        term.exit_code(Duration::from_secs(10)),
        Some(0),
        "the relay did not survive the bar; the child wrote: {}",
        term.since(mark)
    );
}

/// With a shadow: restored locally, nothing asked of the server.
#[test]
fn a_prefix_raises_the_hint_row_and_the_next_key_takes_it_off() {
    the_hint_row_goes_up_and_comes_down("hint", true);
}

/// [`row_on_screen`] with the terminal resized to `rows` at offset `at`.
fn row_on_resized_screen(out: &[u8], at: usize, rows: u16, row: u16) -> String {
    let at = at.min(out.len());
    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    parser.process(&out[..at]);
    parser.screen_mut().set_size(rows, COLS);
    parser.process(&out[at..]);
    row_of(parser.screen(), row)
}

/// The relay reads the size at start and on `SIGWINCH` only; a missed update
/// would put the bar on the old last row.
#[test]
fn the_hint_row_goes_on_the_last_row_of_a_terminal_made_taller() {
    require_nvim!();
    let tag = "hint-taller";
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    // No 'showcmd', as above.
    nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
        .expect("connect")
        .command("set noshowcmd")
        .expect("noshowcmd");

    let mut term = Terminal::spawn_with(&sock, &session.id, Protocol::Kitty, true);
    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            kitty_push_flags(out).is_some()
        }),
        "the client never started; got: {}",
        term.since(0)
    );
    term.pump_until(Duration::from_secs(2), |out| {
        !row_on_screen(out, 0).is_empty()
    });

    let taller = ROWS + 4;
    let resized = term.output.len();
    term.pty
        .resize(pty_process::Size::new(taller, COLS))
        .expect("resize the terminal");
    term.pump_until(Duration::from_millis(500), |_| false);

    term.type_bytes(&[nvmux::keys::PREFIX]);
    let mark = term.output.len();
    assert!(
        term.pump_until(Duration::from_secs(10), |out| {
            row_on_resized_screen(out, resized, taller, taller - 1).contains("␣ picker")
        }),
        "the prefix put no hint row on the new last row, {taller}; it says {:?}, row {ROWS} \
         says {:?}, and the child wrote: {}",
        row_on_resized_screen(&term.output, resized, taller, taller - 1),
        row_on_resized_screen(&term.output, resized, taller, ROWS - 1),
        term.since(mark)
    );

    // A literal prefix takes the bar down, then detach.
    term.type_bytes(&[nvmux::keys::PREFIX]);
    term.pump_until(Duration::from_millis(300), |_| false);
    term.type_bytes(&[nvmux::keys::PREFIX]);
    term.pump_until(Duration::from_millis(150), |_| false);
    term.type_bytes(b"d");
    assert_eq!(
        term.exit_code(Duration::from_secs(10)),
        Some(0),
        "the relay did not detach after the resize; the child wrote: {}",
        term.since(mark)
    );
}

/// Fade off (e.g. `NO_COLOR`), no shadow: the row is blanked and repainted by
/// the server.
#[test]
fn the_hint_row_comes_down_through_the_server_without_a_shadow() {
    the_hint_row_goes_up_and_comes_down("hint-no-shadow", false);
}

/// Last-row redraw interval, like LSP progress while indexing, with lulls.
const TICK_MS: u64 = 50;

/// A constantly redrawn last row must never show through the bar at any
/// moment the terminal could present. The statusline (`cmdheight=0`) is one
/// letter across, changing every [`TICK_MS`] (letters: an all-digit `%{}` is
/// read as a huge number). Regression: the bar was wiped by the next tick and
/// restored only at the next lull.
///
/// `own` picks the client: Neovim's brackets nothing (its `?2026` query goes
/// unanswered), so nvmux wraps its writes; nvmux's own brackets each frame.
fn the_hint_row_stays_over_a_last_row_that_keeps_changing(tag: &str, own: bool) {
    require_nvim!();
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let mut editor = nvmux::rpc::Client::connect(&sock, Duration::from_secs(3)).expect("connect");
    // No 'showcmd', as above.
    for cmd in [
        "let g:n = 0",
        "set noshowcmd laststatus=2 cmdheight=0 \
         statusline=%{repeat(nr2char(97+g:n%26),winwidth(0))}",
    ] {
        editor.command(cmd).expect(cmd);
    }
    let mut ticks = || {
        editor
            .eval("g:n")
            .ok()
            .and_then(|n| n.as_u64())
            .expect("the tick count")
    };

    let mut term = Terminal::spawn_child(
        &sock,
        &session.id,
        Protocol::Kitty,
        Asks {
            shadow: true,
            own,
            ..Asks::default()
        },
    );
    // The statusline: one letter across the row.
    let ticking = |row: &str| {
        row.len() == usize::from(COLS)
            && row
                .bytes()
                .all(|b| b.is_ascii_lowercase() && row.starts_with(b as char))
    };
    // Also wait for the DA1 in the first paint: the row appears earlier in
    // fade frames drawn from the shadow, while the relay reads no keys, so a
    // prefix sent then would not be read until the fade ended.
    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            find(out, DA1).is_some() && ticking(&row_on_screen(out, ROWS - 1))
        }),
        "the statusline never reached the last row; it says {:?}, and the child wrote: {}",
        row_on_screen(&term.output, ROWS - 1),
        term.since(0)
    );
    nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
        .expect("connect")
        .command(&format!(
            "lua vim.fn.timer_start({TICK_MS}, function() \
             vim.g.n = vim.g.n + 1 vim.cmd.redrawstatus() end, {{ ['repeat'] = -1 }})"
        ))
        .expect("start the ticks");

    term.type_bytes(&[nvmux::keys::PREFIX]);
    let pressed = term.output.len();
    assert!(
        term.pump_until(Duration::from_secs(10), |out| {
            row_on_screen(out, ROWS - 1).contains("␣ picker")
        }),
        "the prefix put no hint row over the statusline; it says {:?}, and the child wrote: {}",
        row_on_screen(&term.output, ROWS - 1),
        term.since(pressed)
    );
    let before = ticks();
    term.pump_until(Duration::from_millis(20 * TICK_MS), |_| false);
    // At least ten ticks, within the child's 10 s prefix timeout.
    let enough = Instant::now() + Duration::from_secs(3);
    while ticks() < before + 10 && Instant::now() < enough {
        term.pump_until(Duration::from_millis(TICK_MS), |_| false);
    }
    let after = ticks();
    // Everything before this was written while the bar should be up.
    let resolved = term.output.len();
    assert!(
        after >= before + 10,
        "the statusline was not redrawn under the bar ({before} ticks, then {after}), \
         so this shows nothing"
    );

    let moments = presented(&term.output[..resolved], ROWS - 1);
    let up = moments
        .iter()
        .position(|(at, row)| *at > pressed && row.contains("␣ picker"))
        .expect("the bar was on the screen a moment ago");
    let bar = &moments[up].1;
    assert!(
        bar.contains("d detach") && bar.contains("1-n session"),
        "the bar went up only in part: {bar:?}"
    );
    let through: Vec<&str> = moments[up + 1..]
        .iter()
        .map(|(_, row)| row.as_str())
        .collect();
    assert!(
        through.is_empty(),
        "the statusline showed through the bar {} times while the prefix waited; \
         the bar is {bar:?}, and the row was presented as {:#?}, and so on",
        through.len(),
        &through[..through.len().min(3)]
    );

    // A literal prefix takes the bar down; the ticking row returns.
    term.type_bytes(&[nvmux::keys::PREFIX]);
    assert!(
        term.pump_until(Duration::from_secs(10), |out| {
            ticking(&row_on_screen(out, ROWS - 1))
        }),
        "the statusline did not come back from under the bar: {:?}; the child wrote: {}",
        row_on_screen(&term.output, ROWS - 1),
        term.since(resolved)
    );

    assert_eq!(
        detach(&mut term),
        Some(0),
        "the relay did not survive the bar; the child wrote: {}",
        term.since(resolved)
    );
}

#[test]
fn the_hint_row_stays_over_a_last_row_that_keeps_changing_under_neovims_client() {
    the_hint_row_stays_over_a_last_row_that_keeps_changing("hint-tick", false);
}

#[test]
fn the_hint_row_stays_over_a_last_row_that_keeps_changing_under_its_own_client() {
    the_hint_row_stays_over_a_last_row_that_keeps_changing("hint-tick-own", true);
}

/// Both the machine and the hint row follow `[keys.bind]`.
#[test]
fn a_moved_command_runs_on_its_new_key_through_a_real_client() {
    require_nvim!();
    let tag = "bind";
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");

    let mut term = Terminal::spawn_child(
        &sock,
        &session.id,
        Protocol::Kitty,
        Asks {
            bind: Some("detach = \"x\""),
            ..Asks::default()
        },
    );
    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            kitty_push_flags(out).is_some()
        }),
        "the client never started; got: {}",
        term.since(0)
    );
    term.pump_until(Duration::from_secs(2), |out| {
        !row_on_screen(out, 0).is_empty()
    });

    // Kitty-encoded, as this terminal now sends it.
    let mark = term.output.len();
    term.type_bytes(b"\x1b[32;5u");
    assert!(
        term.pump_until(Duration::from_secs(10), |out| {
            row_on_screen(out, ROWS - 1).contains("x detach")
        }),
        "the hint row does not name the key detach was moved to: {:?}; the child wrote: {}",
        row_on_screen(&term.output, ROWS - 1),
        term.since(mark)
    );

    term.type_bytes(b"x");
    let code = term.exit_code(Duration::from_secs(10));
    assert_eq!(
        code,
        Some(0),
        "expected a detach (exit 0) on the moved key; the child wrote: {}",
        term.since(mark)
    );
}

/// Written by a resumed relay before any client output.
const RESUMED: &[u8] = b"\x1b[?1049h\x1b[2J";

/// A resumed client's screen comes back from the kept copy (the shadow, and
/// dissolved in, with `fade`), with the server's repaint requested, and a new
/// size passed via the pty or, with `in_band`, an in-band report (those sent
/// while parked were lost).
///
/// Three resumes: same size (only the requested repaint clears the screen),
/// narrower, and with the server busy in Lua (the copy must still show).
fn a_kept_clients_screen_comes_back(tag: &str, in_band: bool, fade: bool) {
    require_nvim!();
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let marker = "kept, and painted straight back";
    let mut rpc = nvmux::rpc::Client::connect(&sock, Duration::from_secs(3)).expect("connect");
    // No swap file: these run in parallel and would race for the same one.
    rpc.command(&format!(
        "setlocal noswapfile | call setline(1, '{marker}')"
    ))
    .expect("setline");

    let mut term = Terminal::spawn_keeping(&sock, &session.id, in_band, fade);
    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            row_on_screen(out, 0).contains(marker)
                && (!in_band || find(out, b"\x1b[?2048h").is_some())
        }),
        "the session never painted; got: {}",
        term.since(0)
    );
    assert_eq!(
        find(&term.output, b"\x1b[?2048h").is_some(),
        in_band,
        "the client did not take up in-band size reports as the terminal offered them"
    );
    // A client is parked only after its DA1 is answered. With the fade the
    // marker shows in fade frames before the paint carrying the DA1, so wait
    // for the DA1 and a moment for the reply.
    assert!(
        term.pump_until(Duration::from_secs(5), |out| find(out, DA1).is_some()),
        "the client never asked its DA1; got: {}",
        term.since(0)
    );
    term.pump_until(Duration::from_millis(300), |_| false);

    let after = away_and_back(&mut term, |_| {});
    // Through row 0 of the repaint; a read can end mid-write.
    let landed = term.pump_until(Duration::from_secs(5), |out| {
        find(&out[after.min(out.len())..], b"\x1b[2J").is_some()
            && row_on_screen(out, 0).contains(marker)
    });
    assert!(
        find(&term.output[after..], b"\x1b[2J").is_some(),
        "no repaint came after the client came back; the child wrote: {}",
        term.since(after)
    );
    assert!(
        landed,
        "the repainted screen is not the session's: {:?}",
        row_on_screen(&term.output, 0)
    );
    // Mouse reporting restored after the resume (Neovim's default `'mouse'`
    // enables button tracking).
    assert!(
        find(&term.output[after..], b"\x1b[?1002h").is_some(),
        "the client's mouse reporting was not put back; the child wrote: {}",
        term.since(after)
    );

    // Narrow the terminal while parked; an in-band client ignores the pty
    // and learns the size only from nvmux's report.
    term.pump_until(Duration::from_millis(300), |_| false);
    let narrower = COLS - 20;
    away_and_back(&mut term, |term| {
        term.pty
            .resize(pty_process::Size::new(ROWS, narrower))
            .expect("resize the terminal");
    });
    let width = || {
        nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
            .and_then(|mut c| c.call("nvim_list_uis", vec![]))
            .ok()
            .and_then(|uis| {
                let ui = uis.as_array()?.first()?.as_map()?.clone();
                ui.iter()
                    .find(|(k, _)| k.as_str() == Some("width"))
                    .and_then(|(_, v)| v.as_u64())
            })
    };
    assert!(
        common::wait_until(Duration::from_secs(5), || width()
            == Some(u64::from(narrower))),
        "the server never took the new size: it is {:?} columns wide, not {narrower}",
        width()
    );

    // Again, with the server too busy to answer for longer than this waits.
    term.pump_until(Duration::from_millis(300), |_| false);
    let mark = term.output.len();
    term.type_bytes(&[nvmux::keys::PREFIX, b' ']);
    // `4 > …`, not `… < 4`: in key notation `<` would start a key name.
    rpc.input(":lua local t = os.clock() while 4 > os.clock() - t do end<CR>")
        .expect("make the server busy");
    let came_back = term.pump_until(Duration::from_secs(3), |out| {
        let since = &out[mark.min(out.len())..];
        find(since, RESUMED).is_some() && row_on_screen(out, 0).contains(marker)
    });
    // Checked after the screen came back: the server was busy then.
    let busy = nvmux::rpc::Client::connect(&sock, Duration::from_millis(300))
        .and_then(|mut c| c.get_mode())
        .is_err();
    assert!(
        came_back,
        "the kept client's screen did not come back; row 1 says {:?}, and the child wrote: {}",
        row_on_screen(&term.output, 0),
        term.since(mark)
    );
    assert!(
        busy,
        "the server answered while the screen came back, so this shows nothing about where \
         it came from"
    );

    term.type_bytes(&[nvmux::keys::PREFIX]);
    term.pump_until(Duration::from_millis(150), |_| false);
    term.type_bytes(b"d");
    assert_eq!(
        term.exit_code(Duration::from_secs(10)),
        Some(0),
        "the resumed relay did not detach; the child wrote: {}",
        term.since(mark)
    );
}

/// Send `<prefix> Space` to a keeping child, run `meanwhile` once parked, and
/// wait for the resume. Returns the offset where the resumed relay's bytes
/// begin.
fn away_and_back(term: &mut Terminal, meanwhile: impl FnOnce(&mut Terminal)) -> usize {
    let mark = term.output.len();
    term.type_bytes(&[nvmux::keys::PREFIX, b' ']);
    assert!(
        term.pump_until(Duration::from_secs(5), |out| {
            find(&out[mark.min(out.len())..], PARKED).is_some()
        }),
        "the child never parked its client; it wrote: {}",
        term.since(mark)
    );
    meanwhile(term);
    assert!(
        term.pump_until(Duration::from_secs(5), |out| {
            find(&out[mark.min(out.len())..], RESUMED).is_some()
        }),
        "the parked client never came back; the child wrote: {}",
        term.since(mark)
    );
    mark + find(&term.output[mark..], RESUMED).expect("resumed") + RESUMED.len()
}

#[test]
fn a_kept_clients_screen_comes_back_through_the_pty() {
    a_kept_clients_screen_comes_back("kept-pty", false, false);
}

/// Like kitty, Ghostty, foot: size comes in band, `SIGWINCH` is ignored.
#[test]
fn a_kept_clients_screen_comes_back_on_an_in_band_terminal() {
    a_kept_clients_screen_comes_back("kept-in-band", true, false);
}

/// The fade is on wherever the terminal answers the colour query; pen and
/// cursor are handed back to the client after.
#[test]
fn a_kept_clients_screen_dissolves_back_in() {
    a_kept_clients_screen_comes_back("kept-fade", false, true);
}

/// The longest allowed fade, so "during the fade-in" is a wide window for
/// [`a_kept_clients_repaint_is_asked_under_its_fade_in`].
const LONG_FADE_MS: u64 = 2_000;

/// The repaint is requested as the fade-in starts, not after, so the server
/// redraws under the fade and the exact screen is there when it ends.
///
/// The server counts window redraws: none while parked, some by mid-fade,
/// while no client output has reached the terminal (the repaint's clear comes
/// after the fade).
#[test]
fn a_kept_clients_repaint_is_asked_under_its_fade_in() {
    require_nvim!();
    let tag = "kept-early";
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    // `--clean`, so no config redraws on a timer while parked.
    let session = t
        .create_session(
            &common::unique(tag),
            &common::launch_with("--clean"),
            common::anywhere(),
        )
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let marker = "kept, and repainted under the fade";
    let mut rpc = nvmux::rpc::Client::connect(&sock, Duration::from_secs(3)).expect("connect");
    rpc.command(&format!(
        "setlocal noswapfile | call setline(1, '{marker}')"
    ))
    .expect("setline");
    rpc.command(
        "lua _G.redrawn = 0 \
         vim.api.nvim_set_decoration_provider(vim.api.nvim_create_namespace('redrawn'), \
         { on_win = function() _G.redrawn = _G.redrawn + 1 end })",
    )
    .expect("count the redraws");
    let redrawn = || {
        nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
            .and_then(|mut c| c.eval("luaeval('_G.redrawn')"))
            .ok()
            .and_then(|count| count.as_u64())
            .expect("the count of redraws")
    };

    let mut term = Terminal::spawn_child(
        &sock,
        &session.id,
        Protocol::Kitty,
        Asks {
            shadow: true,
            keep: true,
            fade_ms: Some(LONG_FADE_MS),
            ..Asks::default()
        },
    );
    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            row_on_screen(out, 0).contains(marker)
        }),
        "the session never painted; got: {}",
        term.since(0)
    );
    // Parked only after the DA1 is answered.
    assert!(
        term.pump_until(Duration::from_secs(5), |out| find(out, DA1).is_some()),
        "the client never asked its DA1; got: {}",
        term.since(0)
    );
    term.pump_until(Duration::from_millis(300), |_| false);

    let mark = term.output.len();
    let since = |out: &[u8], what: &[u8]| find(&out[mark.min(out.len())..], what).is_some();
    term.type_bytes(&[nvmux::keys::PREFIX, b' ']);
    assert!(
        term.pump_until(Duration::from_secs(5), |out| since(out, PARKED)),
        "the child never parked its client; it wrote: {}",
        term.since(mark)
    );
    let parked = redrawn();
    assert!(
        term.pump_until(Duration::from_secs(5), |out| since(out, RESUMED)),
        "the parked client never came back; the child wrote: {}",
        term.since(mark)
    );
    let after = mark + find(&term.output[mark..], RESUMED).expect("resumed") + RESUMED.len();
    let repainted = |out: &[u8]| find(&out[after.min(out.len())..], b"\x1b[2J").is_some();

    let fade_in = Duration::from_millis(LONG_FADE_MS / 2);
    term.pump_until(fade_in / 2, |_| false);
    assert!(
        !repainted(&term.output),
        "the fade in was over within half its length, which says nothing about when the \
         repaint was asked; the child wrote: {}",
        term.since(after)
    );
    assert!(
        redrawn() > parked,
        "half way through the fade in the server had redrawn nothing: the repaint waits \
         for the fade to be over"
    );

    let landed = term.pump_until(Duration::from_secs(5), |out| {
        repainted(out) && row_on_screen(out, 0).contains(marker)
    });
    assert!(
        repainted(&term.output),
        "no repaint came after the fade in; the child wrote: {}",
        term.since(after)
    );
    assert!(
        landed,
        "the repainted screen is not the session's: {:?}",
        row_on_screen(&term.output, 0)
    );
    assert_eq!(
        detach(&mut term),
        Some(0),
        "the resumed relay did not detach; the child wrote: {}",
        term.since(after)
    );
}

/// `[client] lazy = false`: a client started ahead is attached but writes
/// nothing until its first visit; then its startup queries are answered late
/// and acted on, and it behaves as any kept client. With `fade`, its output is
/// held and dissolved in like a fresh first paint.
fn a_client_started_ahead_negotiates_on_its_first_visit(tag: &str, fade: bool) {
    require_nvim!();
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let marker = "started ahead, and shown at last";
    nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
        .expect("connect")
        .command(&format!(
            "setlocal noswapfile | call setline(1, '{marker}')"
        ))
        .expect("setline");

    let mut term = Terminal::spawn_ahead(&sock, &session.id, fade);
    assert!(
        term.pump_until(Duration::from_secs(15), |out| find(out, AHEAD).is_some()),
        "the child never started its client; it wrote: {}",
        term.since(0)
    );
    assert!(
        common::wait_until(AHEAD_FOR / 2, || {
            nvmux::rpc::Client::connect(&sock, Duration::from_secs(2))
                .and_then(|mut c| c.list_uis())
                .is_ok_and(|n| n == 1)
        }),
        "the client started ahead never attached to its session"
    );
    assert!(
        !term.pump_until(Duration::from_millis(200), |out| {
            find(out, KITTY_QUERY).is_some()
        }),
        "the client's questions reached the terminal before its first visit: {}",
        term.since(0)
    );

    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            kitty_push_flags(out).is_some_and(|flags| flags & 1 != 0)
        }),
        "the client never turned the kitty protocol on; got: {}",
        term.since(0)
    );
    let asked = find(&term.output, KITTY_QUERY).expect("the push follows the query");
    assert!(
        asked > find(&term.output, AHEAD).expect("started"),
        "the query came before the client was started ahead"
    );
    assert!(
        term.pump_until(Duration::from_secs(5), |out| {
            row_on_screen(out, 0).contains(marker)
        }),
        "the session's screen never came up; got: {}",
        term.since(asked)
    );

    term.pump_until(Duration::from_millis(300), |_| false);
    let after = away_and_back(&mut term, |_| {});
    // Through row 0 of the repaint; a read can end mid-write.
    let landed = term.pump_until(Duration::from_secs(5), |out| {
        find(&out[after.min(out.len())..], b"\x1b[2J").is_some()
            && row_on_screen(out, 0).contains(marker)
    });
    assert!(
        find(&term.output[after..], b"\x1b[2J").is_some(),
        "no repaint came after the client came back; the child wrote: {}",
        term.since(after)
    );
    assert!(
        landed,
        "the repainted screen is not the session's: {:?}",
        row_on_screen(&term.output, 0)
    );

    let flags = kitty_push_flags(&term.output).unwrap_or(0);
    let mark = term.output.len();
    term.type_bytes(b"\x1b[32;5u");
    if flags & 2 != 0 {
        term.type_bytes(b"\x1b[32;5:3u");
    }
    term.pump_until(Duration::from_millis(150), |_| false);
    term.type_bytes(b"d");
    assert_eq!(
        term.exit_code(Duration::from_secs(10)),
        Some(0),
        "the client started ahead did not detach; the child wrote: {}",
        term.since(mark)
    );
}

#[test]
fn a_client_started_ahead_negotiates_on_its_first_visit_through_the_pty() {
    a_client_started_ahead_negotiates_on_its_first_visit("ahead", false);
}

#[test]
fn a_client_started_ahead_is_dissolved_in_on_its_first_visit() {
    a_client_started_ahead_negotiates_on_its_first_visit("ahead-fade", true);
}

/// The relay reports "painted" once, only after the screen reached the
/// terminal (held and dissolved in with `fade`). After a dropped link,
/// clients restarted behind the front session (`[client] lazy` off) wait for
/// this, since each screen goes over the same link.
fn a_relay_says_when_its_session_has_painted(tag: &str, fade: bool) {
    require_nvim!();
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let marker = "on the screen before the paint was said";
    nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
        .expect("connect")
        .command(&format!(
            "setlocal noswapfile | call setline(1, '{marker}')"
        ))
        .expect("setline");

    let mut term = Terminal::spawn_child(
        &sock,
        &session.id,
        Protocol::Kitty,
        Asks {
            shadow: fade,
            painted: true,
            ..Asks::default()
        },
    );
    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            find(out, PAINTED).is_some() || find(out, UNPAINTED).is_some()
        }),
        "the relay never said its session had painted; the child wrote: {}",
        term.since(0)
    );
    let said = find(&term.output, PAINTED).unwrap_or_else(|| {
        panic!(
            "let go of without being told; the child wrote: {}",
            term.since(0)
        )
    });
    assert!(
        find(&term.output[..said], marker.as_bytes()).is_some(),
        "said to have painted before the session's screen had reached the terminal: {}",
        term.since(0)
    );
    term.pump_until(Duration::from_millis(500), |_| false);
    assert!(
        find(&term.output[said + PAINTED.len()..], PAINTED).is_none()
            && find(&term.output, UNPAINTED).is_none(),
        "said more than once: {}",
        term.since(0)
    );
    assert_eq!(detach(&mut term), Some(0), "{}", term.since(0));
}

#[test]
fn a_relay_says_when_its_session_has_painted_through_the_pty() {
    a_relay_says_when_its_session_has_painted("painted", false);
}

#[test]
fn a_relay_says_when_its_session_has_painted_once_dissolved_in() {
    a_relay_says_when_its_session_has_painted("painted-fade", true);
}

/// A client started ahead of a constantly drawing session (a `:terminal`
/// build, a log) survives to its first visit: away, it draws one frame a
/// second (`nvmux::card::Command::Away`), staying far under
/// `nvmux::pty::AHEAD_MAX`. On the visit the screen is current, not the last
/// away frame.
#[test]
fn a_client_started_ahead_of_a_busy_session_is_kept_to_its_first_visit() {
    require_nvim!();
    let tag = "ahead-busy";
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(
            &common::unique(tag),
            &common::launch_with("--clean"),
            common::anywhere(),
        )
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let mut rpc = nvmux::rpc::Client::connect(&sock, Duration::from_secs(3)).expect("connect");
    // Every row rewritten every 5 ms: several KB per frame, so a megabyte
    // (`nvmux::pty::AHEAD_MAX`) in well under the hold.
    rpc.command(
        "setlocal noswapfile | lua vim.g.ticks = 0; vim.fn.timer_start(5, function() \
         vim.g.ticks = vim.g.ticks + 1; local d = tostring(vim.g.ticks % 10); \
         local lines = { 'tick ' .. vim.g.ticks }; \
         for _ = 2, 36 do lines[#lines + 1] = string.rep(d, 110) end; \
         vim.api.nvim_buf_set_lines(0, 0, -1, false, lines) \
         end, { ['repeat'] = -1 })",
    )
    .expect("start the ticks");

    let mut term = Terminal::spawn_child(
        &sock,
        &session.id,
        Protocol::Kitty,
        Asks {
            keep: true,
            ahead: true,
            own: true,
            ..Asks::default()
        },
    );
    assert!(
        term.pump_until(Duration::from_secs(15), |out| find(out, AHEAD).is_some()),
        "the child never started its client; it wrote: {}",
        term.since(0)
    );
    let started = term.output.len();
    let tick = |out: &[u8]| -> Option<u64> {
        row_on_screen(out, 0)
            .strip_prefix("tick ")
            .and_then(|n| n.parse().ok())
    };
    assert!(
        term.pump_until(Duration::from_secs(15), |out| tick(out).is_some()),
        "the session's screen never came up; the child wrote: {}",
        term.since(started)
    );
    // Kept current from then on, as in front. Ask Neovim first, then parse,
    // so parsing megabytes of frames on a slow machine is not counted against
    // the client.
    term.pump_until(Duration::from_millis(300), |_| false);
    let mut screen = vt100::Parser::new(ROWS, COLS, 0);
    let mut read = 0;
    for _ in 0..5 {
        term.pump_until(Duration::from_millis(100), |_| false);
        let now = rpc
            .eval("g:ticks")
            .ok()
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        screen.process(&term.output[read..]);
        read = term.output.len();
        let shown = row_of(screen.screen(), 0)
            .strip_prefix("tick ")
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        assert!(
            now.saturating_sub(shown) < 40,
            "the screen says tick {shown} of {now}"
        );
    }
    assert_eq!(detach(&mut term), Some(0), "the child did not detach");
}

/// After the terminal shrank, the first visit's held paint is drawn for the
/// new size, not the old screen cut short. The client is told it is in front
/// before the resize, or its redraw would come at the away rate
/// (`nvmux::card::Command::Away`), after the paint.
///
/// Held 2.5 s, off the one-second away beat, while the session redraws every
/// 10 ms: an away client would always have drawn within the last second and
/// have to wait. Neovim's redraw may trail the paint on a slow machine, so the
/// first frame need only be drawn for the new size.
#[test]
fn a_client_started_ahead_is_dissolved_in_at_the_size_of_its_first_visit() {
    require_nvim!();
    let tag = "ahead-size";
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(
            &common::unique(tag),
            &common::launch_with("--clean"),
            common::anywhere(),
        )
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
        .expect("connect")
        .command(
            "setlocal noswapfile | lua vim.g.ticks = 0; vim.fn.timer_start(10, function() \
             vim.g.ticks = vim.g.ticks + 1; \
             vim.api.nvim_buf_set_lines(0, 0, 1, false, { 'tick ' .. vim.g.ticks }) \
             end, { ['repeat'] = -1 })",
        )
        .expect("start the ticks");

    let mut term = Terminal::spawn_child(
        &sock,
        &session.id,
        Protocol::Kitty,
        Asks {
            shadow: true,
            keep: true,
            ahead: true,
            ahead_ms: Some(2_500),
            own: true,
            ..Asks::default()
        },
    );
    assert!(
        term.pump_until(Duration::from_secs(15), |out| find(out, AHEAD).is_some()),
        "the child never started its client; it wrote: {}",
        term.since(0)
    );
    let rows = ROWS - 10;
    let at = term.output.len();
    term.pty
        .resize(pty_process::Size::new(rows, COLS))
        .expect("resize the terminal");
    // The first frame showing the session: the fade's, whole.
    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            row_on_resized_screen(out, at, rows, 0).starts_with("tick ")
        }),
        "the session's screen never came up; the child wrote: {}",
        term.since(at)
    );
    // The old screen cut short would have `~` up to the row above the last,
    // with the status and command lines both written onto the last.
    let above = row_on_resized_screen(&term.output, at, rows, rows - 2);
    let last = row_on_resized_screen(&term.output, at, rows, rows - 1);
    assert!(
        !(above == "~" && last != "~"),
        "the first frame is the old screen cut short: {above:?} over {last:?}, and the child wrote: {}",
        term.since(at)
    );
    assert!(
        term.pump_until(Duration::from_secs(5), |out| {
            row_on_resized_screen(out, at, rows, rows - 2).contains("[No Name]")
        }),
        "no status line above the last row: {:?}",
        row_on_resized_screen(&term.output, at, rows, rows - 2)
    );
    assert_eq!(detach(&mut term), Some(0), "the child did not detach");
}

fn messages(sock: &Path) -> String {
    nvmux::rpc::Client::connect(sock, Duration::from_secs(3))
        .and_then(|mut c| c.eval("execute('messages')"))
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn detach(term: &mut Terminal) -> Option<u32> {
    prefix_then(term, b"d")
}

/// Kitty-encoded prefix then `key`; waits for the exit.
fn prefix_then(term: &mut Terminal, key: &[u8]) -> Option<u32> {
    let flags = kitty_push_flags(&term.output).unwrap_or(0);
    term.type_bytes(b"\x1b[32;5u");
    if flags & 2 != 0 {
        term.type_bytes(b"\x1b[32;5:3u");
    }
    term.pump_until(Duration::from_millis(150), |_| false);
    term.type_bytes(key);
    term.exit_code(Duration::from_secs(10))
}

fn spawn_own(sock: &Path, id: &str, label: &'static str, asks: Asks) -> Terminal {
    Terminal::spawn_child(
        sock,
        id,
        Protocol::Kitty,
        Asks {
            own: true,
            label: Some(label),
            ..asks
        },
    )
}

fn screen_rows(out: &[u8]) -> Vec<String> {
    (0..ROWS).map(|row| row_on_screen(out, row)).collect()
}

/// Whether a card for `label` is flush right on the second row.
fn carded(out: &[u8], label: &str) -> bool {
    row_on_screen(out, 1).ends_with(label)
}

/// Nothing reaches the editor's `:messages`.
#[test]
fn a_fresh_client_is_announced_by_a_card() {
    require_nvim!();
    let tag = "card-fresh";
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let label = "announced by a card";

    let mut term = spawn_own(&sock, &session.id, label, Asks::default());
    assert!(
        term.pump_until(Duration::from_secs(15), |out| carded(out, label)),
        "no card said which session this is; the screen: {:#?}",
        screen_rows(&term.output)
    );
    assert!(
        !messages(&sock).contains(label),
        "the editor was told: {}",
        messages(&sock)
    );
    assert_eq!(detach(&mut term), Some(0), "the child did not detach");
}

/// With the fade off, the card is sent as the relay starts, before the
/// client's pty is raw; an echoing pty would leave it as hex on the primary
/// screen a detach returns to.
#[test]
fn a_detach_leaves_nothing_of_the_cards_behind() {
    require_nvim!();
    let tag = "card-detach";
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let label = "leaves no trace";

    let mut term = spawn_own(&sock, &session.id, label, Asks::default());
    assert!(
        term.pump_until(Duration::from_secs(15), |out| carded(out, label)),
        "no card said which session this is; the screen: {:#?}",
        screen_rows(&term.output)
    );
    assert_eq!(detach(&mut term), Some(0), "the child did not detach");
    // Its last output may still be in flight.
    term.pump_until(Duration::from_millis(200), |_| false);
    let left: Vec<String> = screen_rows(&term.output)
        .into_iter()
        .filter(|row| !row.trim().is_empty())
        .collect();
    assert!(left.is_empty(), "the detach left the shell this: {left:#?}");
}

/// A parked client draws at the away rate, so a change just after parking
/// waits up to a second. Resumed sooner without a card (from the picker or
/// full-screen help), it draws the change on the way back and the relay waits
/// for it, so the fade-in shows the current screen instead of jumping after.
#[test]
fn a_crawling_client_comes_back_as_its_session_is_now() {
    require_nvim!();
    let tag = "crawl-back";
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(
            &common::unique(tag),
            &common::launch_with("--clean"),
            common::anywhere(),
        )
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let mut rpc = nvmux::rpc::Client::connect(&sock, Duration::from_secs(3)).expect("connect");
    rpc.command("setlocal noswapfile | call setline(1, 'as it was')")
        .expect("setline");

    let mut term = Terminal::spawn_child(
        &sock,
        &session.id,
        Protocol::Kitty,
        Asks {
            shadow: true,
            keep: true,
            own: true,
            fade_ms: Some(LONG_FADE_MS),
            parked_ms: Some(500),
            ..Asks::default()
        },
    );
    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            row_on_screen(out, 0).contains("as it was")
        }),
        "the session never painted; the screen: {:#?}",
        screen_rows(&term.output)
    );
    assert!(
        term.pump_until(Duration::from_secs(5), |out| find(out, DA1).is_some()),
        "the client never asked its DA1"
    );
    // Park, change the line, resume; up to six tries for the change to come
    // with the screen. The relay waits at most `CARD_WAIT` (see `nvmux::pty`),
    // so a client too slow on a busy machine is late without proving
    // anything; a relay that never waits is late every time, and fails.
    let mut was = String::from("as it was");
    let mut late = 0;
    loop {
        let now = format!("as it is now, {late}");
        // Long enough that the client draws the parking frame at once, then
        // nothing for a second.
        term.pump_until(Duration::from_millis(1500), |_| false);

        let mark = term.output.len();
        term.type_bytes(&[nvmux::keys::PREFIX, b' ']);
        assert!(
            term.pump_until(Duration::from_secs(5), |out| {
                find(&out[mark.min(out.len())..], PARKED).is_some()
            }),
            "the child never parked its client"
        );
        rpc.command(&format!("call setline(1, '{now}')"))
            .expect("setline");
        assert!(
            term.pump_until(Duration::from_secs(5), |out| {
                find(&out[mark.min(out.len())..], RESUMED).is_some()
            }),
            "the parked client never came back"
        );
        let after = mark + find(&term.output[mark..], RESUMED).expect("resumed") + RESUMED.len();
        term.pump_until(Duration::from_millis(LONG_FADE_MS / 4), |_| false);
        assert!(
            find(&term.output[after..], b"\x1b[2J").is_none(),
            "the fade in was over within a quarter of its length"
        );
        let row = row_on_screen(&term.output, 0);
        if row.contains(&now) {
            break;
        }
        assert!(
            row.contains(&was),
            "a quarter into the fade in, the screen coming back is {row:?}"
        );
        late += 1;
        assert!(
            late < 6,
            "the screen came back as it was, {late} times in a row"
        );
        assert!(
            term.pump_until(Duration::from_secs(5), |out| {
                row_on_screen(out, 0).contains(&now)
            }),
            "after the fade in, the screen is {:?}",
            row_on_screen(&term.output, 0)
        );
        was = now;
    }
    assert_eq!(detach(&mut term), Some(0), "the child did not detach");
}

/// On resume the card is drawn before the screen dissolves in and stays at
/// full colour through the fade-in.
#[test]
fn a_kept_clients_return_is_announced_as_its_screen_comes_back() {
    require_nvim!();
    let tag = "card-kept";
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(
            &common::unique(tag),
            &common::launch_with("--clean"),
            common::anywhere(),
        )
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let label = "back, with a card";

    let mut term = spawn_own(
        &sock,
        &session.id,
        label,
        Asks {
            shadow: true,
            keep: true,
            fade_ms: Some(LONG_FADE_MS),
            ..Asks::default()
        },
    );
    assert!(
        term.pump_until(Duration::from_secs(15), |out| carded(out, label)),
        "the first attach had no card; the screen: {:#?}",
        screen_rows(&term.output)
    );
    // Park only after the DA1 is answered and the first card is gone.
    assert!(
        term.pump_until(Duration::from_secs(5), |out| find(out, DA1).is_some()),
        "the client never asked its DA1"
    );
    // Its life plus three one-way fades (see `client::cards`).
    let gone = nvmux::card::ARRIVAL_LIFE + Duration::from_millis(3 * LONG_FADE_MS / 2 + 500);
    term.pump_until(gone, |_| false);
    assert!(!carded(&term.output, label), "the first card never went");

    // Up to three tries for the card to come with the screen: the relay
    // waits at most `CARD_WAIT` for it (see `nvmux::pty`), so a client too
    // slow on a busy machine proves nothing.
    let mut late = 0;
    while late < 3 {
        let mark = term.output.len();
        term.type_bytes(&[nvmux::keys::PREFIX, b' ']);
        assert!(
            term.pump_until(Duration::from_secs(5), |out| {
                find(&out[mark.min(out.len())..], PARKED).is_some()
            }),
            "the child never parked its client"
        );
        assert!(
            term.pump_until(Duration::from_secs(5), |out| {
                find(&out[mark.min(out.len())..], RESUMED).is_some()
            }),
            "the parked client never came back"
        );
        let after = mark + find(&term.output[mark..], RESUMED).expect("resumed") + RESUMED.len();
        // Follow frame by frame: re-parsing the whole output for each look
        // lags on a slow machine and sees the card later in its life.
        let mut frames = Frames::from(&term.output, after);
        let until = Instant::now() + Duration::from_secs(5);
        while !frames.row(1).ends_with(label) {
            assert!(
                frames.next(&mut term, until),
                "the fade in has no card; the screen: {:#?}",
                frames.rows()
            );
        }
        let name = COLS - 2 - label.len() as u16;
        let (card_first, tilde_first) = (frames.fg(1, name), frames.fg(2, 0));
        // From the first fade frame the card is final while the session is
        // still dark (a far `~`). If late, wait for it to go and retry.
        let dark =
            matches!(tilde_first, vt100::Color::Rgb(..)) && bright(tilde_first) < PALETTE_FG / 2;
        if !dark {
            late += 1;
            term.pump_until(gone, |_| false);
            assert!(!carded(&term.output, label), "the card never went");
            continue;
        }
        // Mid-fade: the `~` at half the text brightness, or the fade over.
        let over = |frames: &Frames, term: &Terminal| {
            find(&term.output[after..frames.read], b"\x1b[2J").is_some()
        };
        while bright(frames.fg(2, 0)) < PALETTE_FG / 2 && !over(&frames, &term) {
            assert!(
                frames.next(&mut term, until),
                "the screen under the card never came in half way; the screen: {:#?}",
                frames.rows()
            );
        }
        assert!(
            !over(&frames, &term),
            "the fade in was over within half its length"
        );
        assert!(
            frames.row(1).ends_with(label),
            "half way through the fade in there is no card; the screen: {:#?}",
            frames.rows()
        );
        assert_eq!(
            frames.fg(1, name),
            card_first,
            "the card came in with the screen"
        );
        assert_ne!(
            frames.fg(2, 0),
            tilde_first,
            "the screen under it did not come in"
        );
        assert_eq!(detach(&mut term), Some(0), "the child did not detach");
        return;
    }
    panic!("the card came after the screen had come in, {late} times in a row");
}

/// A card posted for the front session (as the notifier does) shows, and
/// `<prefix> Enter` ends the relay in a switch to its session.
#[test]
fn a_posted_card_comes_up_and_prefix_enter_goes_there() {
    require_nvim!();
    let tag = "card-posted";
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let elsewhere = "zzzzzzzz";

    let mut term = spawn_own(
        &sock,
        &session.id,
        "",
        Asks {
            post: Some(elsewhere),
            ..Asks::default()
        },
    );
    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            row_on_screen(out, 1).ends_with("✓ elsewhere  0s")
                && row_on_screen(out, 2).ends_with(POSTED)
        }),
        "the posted card never came up; the screen: {:#?}",
        screen_rows(&term.output)
    );
    let mark = term.output.len();
    assert_eq!(
        prefix_then(&mut term, b"\r"),
        Some(SWITCHED_BY_CARD as u32),
        "<prefix> Enter did not go where the card is about; the child wrote: {}",
        term.since(mark)
    );
    assert!(
        String::from_utf8_lossy(&term.output[mark..]).contains(elsewhere),
        "switched somewhere else: {}",
        term.since(mark)
    );
}

/// For a card about a buffer of the front session, the relay continues.
#[test]
fn prefix_enter_on_a_card_about_a_terminal_here_shows_it() {
    require_nvim!();
    let tag = "card-here";
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let mut rpc = nvmux::rpc::Client::connect(&sock, Duration::from_secs(3)).expect("connect");
    let current = |rpc: &mut nvmux::rpc::Client<std::os::unix::net::UnixStream>| {
        rpc.eval("bufnr()").ok().and_then(|v| v.as_i64())
    };
    let shown = current(&mut rpc).expect("the current buffer");
    // A buffer no window shows, like an off-screen terminal.
    let hidden = rpc
        .eval("nvim_create_buf(v:true, v:false)")
        .ok()
        .and_then(|v| v.as_i64())
        .expect("a buffer");
    assert_ne!(hidden, shown);

    let post = Box::leak(format!("buf:{hidden}").into_boxed_str());
    let mut term = spawn_own(
        &sock,
        &session.id,
        "",
        Asks {
            post: Some(post),
            ..Asks::default()
        },
    );
    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            row_on_screen(out, 2).ends_with(POSTED)
        }),
        "the posted card never came up; the screen: {:#?}",
        screen_rows(&term.output)
    );
    let flags = kitty_push_flags(&term.output).unwrap_or(0);
    term.type_bytes(b"\x1b[32;5u");
    if flags & 2 != 0 {
        term.type_bytes(b"\x1b[32;5:3u");
    }
    term.pump_until(Duration::from_millis(150), |_| false);
    term.type_bytes(b"\r");
    assert!(
        common::wait_until(Duration::from_secs(5), || current(&mut rpc) == Some(hidden)),
        "the terminal was not shown: the current buffer is {:?}",
        current(&mut rpc)
    );
    assert!(
        term.pump_until(Duration::from_secs(5), |out| {
            !screen_rows(out).iter().any(|r| r.contains(POSTED))
        }),
        "the card acted on stayed up"
    );
    assert_eq!(detach(&mut term), Some(0), "the relay did not go on");
}

fn fg_on_screen(out: &[u8], row: u16, col: u16) -> vt100::Color {
    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    parser.process(out);
    parser
        .screen()
        .cell(row, col)
        .map_or(vt100::Color::Default, vt100::Cell::fgcolor)
}

/// The colour text shows in: the default foreground is [`PALETTE_FG`]. Fade
/// frames redraw the client's default-coloured text in that RGB.
fn shown(color: vt100::Color) -> vt100::Color {
    match color {
        vt100::Color::Default => vt100::Color::Rgb(PALETTE_FG, PALETTE_FG, PALETTE_FG),
        other => other,
    }
}

/// Brightness of a grey from the test palette ([`PALETTE_FG`]); 0 for
/// terminal colours.
fn bright(color: vt100::Color) -> u8 {
    match color {
        vt100::Color::Rgb(r, _, _) => r,
        _ => 0,
    }
}

/// An incremental parser advanced one synchronized update at a time (when a
/// terminal presents), unlike [`row_on_screen`], which re-parses from the
/// start. For following a fade frame by frame.
struct Frames {
    parser: vt100::Parser,
    read: usize,
}

impl Frames {
    fn from(out: &[u8], from: usize) -> Self {
        let mut parser = vt100::Parser::new(ROWS, COLS, 0);
        parser.process(&out[..from]);
        Self { parser, read: from }
    }

    /// Take in the next frame, waiting until `until`. Returns whether one came.
    fn next(&mut self, term: &mut Terminal, until: Instant) -> bool {
        let end = nvmux::fade::SYNC_END;
        loop {
            if let Some(i) = find(&term.output[self.read..], end) {
                let to = self.read + i + end.len();
                self.parser.process(&term.output[self.read..to]);
                self.read = to;
                return true;
            }
            let had = term.output.len();
            let left = until.saturating_duration_since(Instant::now());
            if !term.pump_until(left, |out| out.len() > had) {
                return false;
            }
        }
    }

    fn row(&self, row: u16) -> String {
        row_of(self.parser.screen(), row)
    }

    fn rows(&self) -> Vec<String> {
        (0..ROWS).map(|row| self.row(row)).collect()
    }

    fn fg(&self, row: u16, col: u16) -> vt100::Color {
        self.parser
            .screen()
            .cell(row, col)
            .map_or(vt100::Color::Default, vt100::Cell::fgcolor)
    }
}

/// The client draws the card as the switch begins; it stays at full colour
/// while the session dissolves out, and is all that is left on the screen
/// handed over.
#[test]
fn a_switch_lands_with_its_card_standing() {
    require_nvim!();
    let tag = "card-lands";
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(
            &common::unique(tag),
            &common::launch_with("--clean"),
            common::anywhere(),
        )
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let under = "u".repeat(usize::from(COLS) - 1);
    nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
        .expect("connect")
        .command(&format!(
            "setlocal noswapfile | call setline(1, ['first', '{under}'])"
        ))
        .expect("setline");
    let lands = "where-it-lands";
    let mut term = spawn_own(
        &sock,
        &session.id,
        "",
        Asks {
            shadow: true,
            fade_ms: Some(LONG_FADE_MS),
            lands: Some(lands),
            ..Asks::default()
        },
    );
    assert!(
        term.pump_until(Duration::from_secs(15), |out| row_on_screen(out, 1)
            == under),
        "the session never painted; the screen: {:#?}",
        screen_rows(&term.output)
    );
    // The row is up from the fade-in's first frame; wait for its end.
    let full = vt100::Color::Rgb(PALETTE_FG, PALETTE_FG, PALETTE_FG);
    assert!(
        term.pump_until(Duration::from_secs(5), |out| {
            shown(fg_on_screen(out, 1, 0)) == full
        }),
        "the session never came in at full colour: {:?}",
        fg_on_screen(&term.output, 1, 0)
    );

    let flags = kitty_push_flags(&term.output).unwrap_or(0);
    term.type_bytes(b"\x1b[32;5u");
    if flags & 2 != 0 {
        term.type_bytes(b"\x1b[32;5:3u");
    }
    term.pump_until(Duration::from_millis(150), |_| false);
    term.type_bytes(b"3");
    // Flush right, two columns in; the first of those is the line's last.
    let standing = format!("{lands}u");
    assert!(
        term.pump_until(Duration::from_secs(5), |out| {
            row_on_screen(out, 1).ends_with(&standing)
        }),
        "the card never came up; the screen: {:#?}",
        screen_rows(&term.output)
    );
    let name = COLS - 2 - lands.len() as u16;
    let card_was = fg_on_screen(&term.output, 1, name);
    term.pump_until(Duration::from_millis(LONG_FADE_MS / 4), |_| false);
    assert_eq!(
        fg_on_screen(&term.output, 1, name),
        card_was,
        "the card dissolved with the session"
    );
    assert_ne!(
        shown(fg_on_screen(&term.output, 1, 0)),
        full,
        "half way through, the session is still as it was"
    );

    assert_eq!(
        term.exit_code(Duration::from_secs(10)),
        Some(3),
        "the relay did not end in a switch"
    );
    let rows = screen_rows(&term.output);
    assert!(
        carded(&term.output, lands),
        "the card did not stay: {rows:#?}"
    );
    // Except the child's stderr note on how the relay ended.
    assert!(
        rows.iter()
            .enumerate()
            .all(|(r, row)| r == 1 || row.is_empty() || row.starts_with("relay ended")),
        "more than the card stayed: {rows:#?}"
    );
}

/// On a step switch the row moves from the key on: the fade-out draws it at
/// full colour as it moves, so it is further along at the end than the start,
/// and the handed-over screen keeps it where it got to.
///
/// Frames are read after the relay ends, since parsing is slower than the
/// fade.
#[test]
fn a_step_lands_with_its_row_on_the_move() {
    require_nvim!();
    let tag = "card-steps";
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(
            &common::unique(tag),
            &common::launch_with("--clean"),
            common::anywhere(),
        )
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
        .expect("connect")
        .command("setlocal noswapfile | call setline(1, ['first', 'second'])")
        .expect("setline");
    let lands = "where-it-lands";
    let mut term = spawn_own(
        &sock,
        &session.id,
        "",
        Asks {
            shadow: true,
            fade_ms: Some(LONG_FADE_MS),
            lands: Some(lands),
            steps: true,
            ..Asks::default()
        },
    );
    assert!(
        term.pump_until(Duration::from_secs(15), |out| row_on_screen(out, 1)
            == "second"),
        "the session never painted; the screen: {:#?}",
        screen_rows(&term.output)
    );
    term.pump_until(Duration::from_millis(500), |_| false);
    let text_was = fg_on_screen(&term.output, 1, 0);

    let mark = term.output.len();
    let flags = kitty_push_flags(&term.output).unwrap_or(0);
    term.type_bytes(b"\x1b[32;5u");
    if flags & 2 != 0 {
        term.type_bytes(b"\x1b[32;5:3u");
    }
    term.pump_until(Duration::from_millis(150), |_| false);
    term.type_bytes(b"3");
    assert_eq!(
        term.exit_code(Duration::from_secs(10)),
        Some(3),
        "the relay did not end in a switch"
    );

    // Per frame with the card: the landing name's column and colour,
    // whether the session is still under it, and its text colour.
    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    parser.process(&term.output[..mark]);
    let rest = &term.output[mark..];
    let sync = b"\x1b[?2026h";
    let mut ends: Vec<usize> = (1..rest.len())
        .filter(|&i| rest[i..].starts_with(sync))
        .collect();
    ends.push(rest.len());
    let mut frames = Vec::new();
    let mut from = 0;
    for to in ends {
        parser.process(&rest[from..to]);
        from = to;
        let screen = parser.screen();
        let row: String = (0..COLS)
            .map(|col| match screen.cell(1, col) {
                Some(cell) if cell.has_contents() => cell.contents(),
                _ => " ",
            })
            .collect();
        let fg = |col: usize| {
            u16::try_from(col)
                .ok()
                .and_then(|col| screen.cell(1, col))
                .map_or(vt100::Color::Default, vt100::Cell::fgcolor)
        };
        if let Some(col) = row.find(lands) {
            frames.push((col, fg(col), row.starts_with("second"), fg(0)));
        }
    }
    let light = |c: vt100::Color| match c {
        vt100::Color::Rgb(r, g, b) => u32::from(r) + u32::from(g) + u32::from(b),
        // The default is the child palette's text grey.
        vt100::Color::Default => 600,
        other => panic!("not a colour a frame here is drawn in: {other:?}"),
    };
    let fading: Vec<_> = frames.iter().filter(|f| f.2).collect();
    let (first, last) = match (fading.first(), fading.last()) {
        (Some(first), Some(last)) => (first, last),
        _ => panic!("the card never stood on the session's screen: {frames:?}"),
    };
    assert!(
        last.0 + 2 <= first.0,
        "the row did not move as the session went: from column {} to {}",
        first.0,
        last.0
    );
    assert!(
        light(last.1) >= light(first.1),
        "the name it lands on went dim with the session: {frames:?}"
    );
    assert!(
        light(last.3) < light(text_was),
        "the session did not dissolve: {frames:?}"
    );
    let handed = frames
        .iter()
        .rev()
        .find(|f| !f.2)
        .expect("the card, on the screen handed over");
    assert!(
        handed.0 <= last.0,
        "the screen handed over has the row back from where it came to: {} after {}",
        handed.0,
        last.0
    );
}

/// Neovim's client cannot draw cards, and would pass one to the editor as a
/// `TermResponse`, so none is sent.
#[test]
fn neovims_own_client_is_sent_no_card() {
    require_nvim!();
    let tag = "card-nvim";
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let marker = "nothing typed here";
    nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
        .expect("connect")
        .command(&format!(
            "setlocal noswapfile | call setline(1, '{marker}')"
        ))
        .expect("setline");
    nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
        .expect("connect")
        .command(
            "lua _G.answers = {} vim.api.nvim_create_autocmd('TermResponse', \
             { callback = function(ev) table.insert(_G.answers, ev.data.sequence) end })",
        )
        .expect("listen for the terminal's answers");
    let label = "no card for this client";

    let mut term = Terminal::spawn_announcing(&sock, &session.id, label, false);
    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            row_on_screen(out, 0).contains(marker)
        }),
        "the session never painted; the child wrote: {}",
        term.since(0)
    );
    term.pump_until(Duration::from_millis(500), |_| false);
    assert!(
        !carded(&term.output, label),
        "a card was drawn: {:#?}",
        screen_rows(&term.output)
    );
    let line = nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
        .and_then(|mut c| c.eval("[getline(1), mode()]"))
        .expect("ask the editor");
    assert_eq!(
        line,
        rmpv::Value::Array(vec![rmpv::Value::from(marker), rmpv::Value::from("n")]),
        "something reached the editor as keys"
    );
    assert!(!messages(&sock).contains(label), "{}", messages(&sock));
    let answers = nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
        .and_then(|mut c| c.eval("luaeval('vim.inspect(_G.answers)')"))
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    assert!(
        !answers.contains("\\27_~"),
        "the editor was handed a card as the terminal's answer: {answers}"
    );
    assert_eq!(detach(&mut term), Some(0), "the child did not detach");
}

/// Floating, the picker shows the session under its cursor behind it: one
/// started ahead, then, after a switch, the one left, parked. Each change dips
/// through the background. `Esc` goes back to the session floated over;
/// `Enter` brings the one shown up where it is, with nothing else shown on the
/// way, and keys go to it. `j` and `Enter` read together do the same once the
/// dip gets there.
#[test]
fn the_floating_picker_shows_the_session_under_its_cursor() {
    require_nvim!();
    let scratch = Scratch::new("preview");
    let t = scratch.transport();
    let session = |tag: &str, marker: &str| {
        let session = t
            .create_session(&common::unique(tag), &common::launch(), common::anywhere())
            .expect("create");
        let sock = t.local_socket_for(&session).expect("socket path");
        let mut rpc = nvmux::rpc::Client::connect(&sock, Duration::from_secs(3)).expect("connect");
        // No swap file: these run in parallel and would race for the same one.
        rpc.command(&format!(
            "setlocal noswapfile | call setline(1, '{marker}')"
        ))
        .expect("setline");
        (session, sock)
    };
    let (front, other) = ("the session floated over", "the session under the cursor");
    let (a, a_sock) = session("preview-front", front);
    let (b, b_sock) = session("preview-other", other);
    let mut term = Terminal::spawn_child(
        &a_sock,
        &a.id,
        Protocol::Kitty,
        Asks {
            shadow: true,
            keep: true,
            fade_ms: Some(100),
            other: Some((b.id.clone(), b_sock.clone())),
            ..Asks::default()
        },
    );
    assert!(
        term.pump_until(Duration::from_secs(15), |out| row_on_screen(out, 0)
            .contains(front)),
        "the session never painted; got: {}",
        term.since(0)
    );
    // Parked by the switch only once its DA1 is answered (see
    // `a_kept_clients_screen_comes_back`).
    assert!(
        term.pump_until(Duration::from_secs(5), |out| find(out, DA1).is_some()),
        "the client never asked its DA1; got: {}",
        term.since(0)
    );
    term.pump_until(Duration::from_millis(300), |_| false);

    let floating = |out: &[u8]| row_on_screen(out, ROWS - 1).contains("esc back");
    // Row 1 is empty at some moment after `from`, before `to` first shows on
    // it: the dip's bottom.
    let dips = |out: &[u8], from: usize, to: &str| {
        presented(out, 0)
            .into_iter()
            .filter(|(at, _)| *at > from)
            .take_while(|(_, row)| !row.contains(to))
            .any(|(_, row)| row.is_empty())
    };
    let float = |term: &mut Terminal| {
        let mark = term.output.len();
        term.type_bytes(&[nvmux::keys::PREFIX, b' ']);
        assert!(
            term.pump_until(Duration::from_secs(5), floating),
            "the picker never floated; the child wrote: {}",
            term.since(mark)
        );
    };
    let open = |term: &mut Terminal, key: &[u8], shown: &str| {
        float(term);
        let keyed = term.output.len();
        term.type_bytes(key);
        assert!(
            term.pump_until(Duration::from_secs(5), |out| row_on_screen(out, 0)
                .contains(shown)),
            "{shown:?} never showed behind the picker; row 1 says {:?}",
            row_on_screen(&term.output, 0)
        );
        assert!(
            dips(&term.output, keyed, shown),
            "{shown:?} came up without dipping through the background; the child wrote: {}",
            term.since(keyed)
        );
    };
    // Through the hand-over after `entered`, and the first paint its relay
    // writes over it.
    let painted = |out: &[u8], entered: usize| {
        let from = entered + find(&out[entered.min(out.len())..], SWITCHED)? + SWITCHED.len();
        find(&out[from..], nvmux::fade::SYNC_END).map(|at| from + at + nvmux::fade::SYNC_END.len())
    };
    let reaches = |term: &mut Terminal, sock: &Path, to: &str, typed: char| {
        term.type_bytes(format!("i{typed}\x1b").as_bytes());
        let line = || {
            common::lua(sock, "return vim.api.nvim_get_current_line()")
                .as_str()
                .map(str::to_string)
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !line().is_some_and(|l| l.starts_with(typed)) && Instant::now() < deadline {
            term.pump_until(Duration::from_millis(50), |_| false);
        }
        assert!(
            line().is_some_and(|l| l.starts_with(typed)),
            "keys did not reach {to:?}: its line is {:?}",
            line()
        );
    };

    open(&mut term, b"j", other);
    let mark = term.output.len();
    term.type_bytes(b"\x1b");
    assert!(
        term.pump_until(Duration::from_secs(5), |out| !floating(out)
            && row_on_screen(out, 0).contains(front)),
        "Esc did not go back to the session floated over; the child wrote: {}",
        term.since(mark)
    );
    assert!(
        dips(&term.output, mark, front),
        "Esc went back without dipping through the background; the child wrote: {}",
        term.since(mark)
    );

    for (key, to, sock) in [(b"j", other, &b_sock), (b"k", front, &a_sock)] {
        term.pump_until(Duration::from_millis(300), |_| false);
        open(&mut term, key, to);
        let entered = term.output.len();
        term.type_bytes(b"\r");
        assert!(
            term.pump_until(Duration::from_secs(5), |out| painted(out, entered)
                .is_some()
                && !floating(out)),
            "Enter did not bring {to:?} up; the child wrote: {}",
            term.since(entered)
        );
        let upto = painted(&term.output, entered).expect("painted");
        let shown: Vec<_> = presented(&term.output[..upto], 0)
            .into_iter()
            .filter(|(at, _)| *at > entered)
            .collect();
        assert!(
            shown.iter().all(|(_, row)| row.contains(to)),
            "something else showed as {to:?} came up: {shown:#?}"
        );
        assert!(row_on_screen(&term.output[..upto], 0).contains(to));
        assert!(
            find(&term.output[entered..], b"\x1b[?1003l\x1b[?1002h").is_some(),
            "its mouse reporting was not put back; the child wrote: {}",
            term.since(entered)
        );
        reaches(&mut term, sock, to, 'x');
    }

    // Read together, so the switch comes before the dip has left the session
    // floated over.
    term.pump_until(Duration::from_millis(300), |_| false);
    float(&mut term);
    let entered = term.output.len();
    term.type_bytes(b"j\r");
    assert!(
        term.pump_until(Duration::from_secs(5), |out| painted(out, entered)
            .is_some()
            && !floating(out)),
        "j and Enter did not bring {other:?} up; the child wrote: {}",
        term.since(entered)
    );
    let upto = painted(&term.output, entered).expect("painted");
    assert!(
        dips(&term.output[..upto], entered, other),
        "{other:?} came up without dipping through the background; the child wrote: {}",
        term.since(entered)
    );
    let shown: Vec<_> = presented(&term.output[..upto], 0)
        .into_iter()
        .filter(|(at, _)| *at > entered)
        .skip_while(|(_, row)| !row.contains(other))
        .collect();
    assert!(
        !shown.is_empty() && shown.iter().all(|(_, row)| row.contains(other)),
        "something else showed once {other:?} was up: {shown:#?}"
    );
    reaches(&mut term, &b_sock, other, 'y');
    assert_eq!(detach(&mut term), Some(0), "the child did not detach");
}

/// A terminal whose frames are several times what a pty holds (about 14 KB
/// on Linux), with [`BUSY_LUA`] drawing.
const BIG: (u16, u16) = (100, 300);

/// Every cell of a window on the left half changes digit, and so colour,
/// every 5 ms, as a busy `:terminal` might: about 90 KB a frame at [`BIG`].
/// With smaller or slower frames the client is often free to read the paste
/// even from a relay that blocks.
const BUSY_LUA: &str = r#"
-- The paste's target; its swap file would outlive the killed server.
vim.bo.swapfile = false
local busy = vim.api.nvim_create_buf(false, true)
vim.api.nvim_open_win(busy, false, { split = 'left', win = 0 })
vim.api.nvim_buf_call(busy, function()
  for d = 0, 9 do
    vim.cmd(('syntax match Digit%d /%d/'):format(d, d))
    -- gui colours too, for 'termguicolors'.
    vim.cmd(('highlight Digit%d ctermfg=%d ctermbg=%d guifg=#%02x%02x00 guibg=#0000%02x')
      :format(d, 160 + d * 7, 16 + d * 3, d * 25, 255 - d * 25, d * 13))
  end
end)
local tick = 0
vim.fn.timer_start(5, function()
  tick = tick + 1
  local lines = {}
  for r = 1, vim.o.lines do
    local row = {}
    for c = 1, vim.o.columns do
      row[c] = string.char(48 + (r * 3 + c * 7 + tick) % 10)
    end
    lines[r] = table.concat(row)
  end
  vim.api.nvim_buf_set_lines(busy, 0, -1, false, lines)
end, { ['repeat'] = -1 })
"#;

/// About a megabyte of distinct lines of code.
fn paste_lines() -> Vec<String> {
    let mut lines = Vec::new();
    let mut bytes = 0;
    while bytes < 1 << 20 {
        let i = lines.len();
        let line = format!(
            "static int v{i} = f({i}, 'c', \"s{i}\") * {}; /* {}*/",
            i % 97,
            "w ".repeat(i % 23)
        );
        bytes += line.len() + 1;
        lines.push(line);
    }
    lines
}

fn adler32(text: &[u8]) -> u64 {
    let (mut a, mut b) = (1u64, 0u64);
    for &byte in text {
        a = (a + u64::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    b << 16 | a
}

/// The current buffer's line count and, once it has `lines`, its text's
/// [`adler32`].
fn pasted(sock: &Path, lines: usize) -> (u64, Option<u64>) {
    let got = common::lua(
        sock,
        &format!(
            r#"local n = vim.api.nvim_buf_line_count(0)
if n ~= {lines} then return {{ n }} end
local text = table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, true), '\n')
local a, b = 1, 0
for i = 1, #text do
  a = (a + text:byte(i)) % 65521
  b = (b + a) % 65521
end
return {{ n, b * 65536 + a }}"#
        ),
    );
    (got[0].as_u64().unwrap_or(0), got[1].as_u64())
}

/// Regression: a client writing a frame reads no input, and the relay blocked
/// writing it the paste, so neither moved again and nothing could detach.
fn a_large_paste_arrives_while_the_screen_keeps_changing(tag: &str, own: bool) {
    require_nvim!();
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    common::lua(&sock, BUSY_LUA);

    let mut term = Terminal::spawn_child(
        &sock,
        &session.id,
        Protocol::Kitty,
        Asks {
            own,
            size: Some(BIG),
            ..Asks::default()
        },
    );
    assert!(
        term.pump_until(Duration::from_secs(15), |out| find(out, DA1).is_some()),
        "the client never started; the child wrote: {}",
        term.since(0)
    );
    nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
        .expect("connect")
        .input("i")
        .expect("start Insert mode");
    assert!(
        common::wait_until(Duration::from_secs(10), || {
            common::mode(&sock).is_some_and(|m| m.mode == "i")
        }),
        "the editor never went into Insert mode"
    );
    // Let the frames come at full size and rate.
    term.pump_until(Duration::from_secs(1), |_| false);

    let lines = paste_lines();
    let text = lines.join("\n");
    let want = adler32(text.as_bytes());
    let mut paste = b"\x1b[200~".to_vec();
    paste.extend_from_slice(text.as_bytes());
    paste.extend_from_slice(b"\x1b[201~");
    // From a thread, as a relay that stops reading leaves the write waiting.
    let mut keyboard = common::reader(&term.pty);
    let (typed_tx, typed) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = typed_tx.send(keyboard.write_all(&paste).is_ok());
    });

    // Generous: the client takes input only between frames, and nvmux's own
    // takes up to about 2 s for the paste here.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut got = pasted(&sock, lines.len());
    while got != (lines.len() as u64, Some(want)) && Instant::now() < deadline {
        term.drop_output_for(Duration::from_millis(100));
        got = pasted(&sock, lines.len());
    }
    assert_eq!(
        got,
        (lines.len() as u64, Some(want)),
        "the paste of {} lines did not arrive whole (line count, checksum)",
        lines.len()
    );
    // Typing more is safe only once the terminal took the whole paste.
    assert_eq!(
        typed.recv_timeout(Duration::from_secs(5)),
        Ok(true),
        "the relay left the paste's end unread"
    );
    // Stop drawing, so the detach is quick.
    common::lua(&sock, "vim.fn.timer_stopall()");
    assert_eq!(
        detach(&mut term),
        Some(0),
        "the relay did not survive the paste"
    );
}

#[test]
fn a_large_paste_arrives_while_the_screen_keeps_changing_under_neovims_client() {
    a_large_paste_arrives_while_the_screen_keeps_changing("paste-nvim", false);
}

#[test]
fn a_large_paste_arrives_while_the_screen_keeps_changing_under_its_own_client() {
    a_large_paste_arrives_while_the_screen_keeps_changing("paste-own", true);
}
