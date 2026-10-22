//! End-to-end: the prefix must reach the prefix machine however the terminal
//! spells it, through a real `nvim --remote-ui` client.
//!
//! The client asks the terminal for the kitty keyboard protocol when it
//! starts, or failing that for xterm's `modifyOtherKeys`, and nvmux relays
//! that negotiation untouched — so a terminal that grants the first (Windows
//! Terminal from 1.25, kitty, Ghostty, ...) then sends `Ctrl-Space` as
//! `CSI 32 ; 5 u`, and one that grants the second (xterm, WezTerm) as
//! `CSI 27 ; 5 ; 32 ~`, instead of the byte `NUL`. Until `keyseq`, either
//! went straight through to the editor: `<prefix> d` reached it as the chord
//! and a pending delete, and nobody could detach.
//!
//! The relay talks to its terminal on fds 0 and 1, so it has to run in a
//! process of its own. Each test spawns this very test binary again, on a pty,
//! to run [`relay_child`], and plays the terminal on the master side: it
//! answers the client's queries the way a terminal with the protocol under
//! test does, waits for the client to turn the protocol on, and types.
//! Skipped without a usable `nvim`, like the other suites.
//!
//! One test here is not about a spelling at all. The hint bar the prefix puts up
//! ([`nvmux::hint`]) is written over a live editor's last row and taken off it
//! again, and neither half can be shown to work anywhere but here: it needs a
//! real client painting a real screen to cover, and the shadow's copy of that
//! screen to put back. Nor is the bar staying on that row while the editor
//! redraws it underneath — a statusline ticking over with a language server's
//! progress — which needs the same, and every moment in between as a terminal
//! could have shown it.
//!
//! Nor are the last two. With `[client] per_session` a client is parked while
//! something else has the front, and when it comes back nvmux paints the
//! screen from its own copy of it and asks the server for its repaint on top —
//! which only a real client on a real pty, and a real server made too busy to
//! answer, can show.
//!
//! Nor is a client started ahead of its first visit (`[client] lazy = false`),
//! whose startup questions reach the terminal only on that visit, and have to
//! be answered and acted on then: which only a real client can show.

#[macro_use]
mod common;

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::Scratch;
use nvmux::transport::Transport;
use pty_process::blocking::{Command, Pty};

/// The session socket and id the child attaches to.
const CHILD_SOCK: &str = "NVMUX_TEST_RELAY_SOCK";
const CHILD_ID: &str = "NVMUX_TEST_RELAY_ID";
/// Set on a child that must keep a shadow of the session's screen — which is
/// what the hint bar puts the row it covered back from. A shadow needs a palette
/// and the fade on, and nothing here queries a real terminal, so the colours are
/// handed to the child rather than asked for.
const CHILD_PALETTE: &str = "NVMUX_TEST_RELAY_PALETTE";
/// The grey that palette has for text: what a session's text and a card's
/// come up to.
const PALETTE_FG: u8 = 200;
/// Set on a child that keeps its client (`[client] per_session`): where
/// `<prefix> Space` would open the picker it parks the client instead, for
/// [`PARKED_FOR`], and then takes it back and relays it again.
const CHILD_KEEP: &str = "NVMUX_TEST_RELAY_KEEP";

/// How long a keeping child leaves its client parked: long enough for the
/// parent to have made the server busy before the client comes back.
const PARKED_FOR: Duration = Duration::from_secs(1);

/// How long, in milliseconds, a keeping child leaves its client parked, where
/// it is not [`PARKED_FOR`].
const CHILD_PARKED_MS: &str = "NVMUX_TEST_RELAY_PARKED_MS";

/// Set on a child whose client is started ahead of its first visit
/// (`[client] lazy = false`): begun and handed to the pool, held there out of
/// sight for [`AHEAD_FOR`], then taken back and relayed for the first time.
const CHILD_AHEAD: &str = "NVMUX_TEST_RELAY_AHEAD";

/// How long a child starting its client ahead holds it before relaying it:
/// long enough for the parent to see the client attached to its server with
/// nothing yet on the terminal.
const AHEAD_FOR: Duration = Duration::from_secs(2);

/// What that child writes once its client has been started ahead.
const AHEAD: &[u8] = b"\x1b]9999;ahead\x07";

/// How long, in milliseconds, that child holds its client, where it is not
/// [`AHEAD_FOR`].
const CHILD_AHEAD_MS: &str = "NVMUX_TEST_RELAY_AHEAD_MS";

/// What the child's relay announces the session as — on a card nvmux's own
/// client draws (see [`nvmux::card`]), and Neovim's draws none — on its first
/// attach and, kept, on every return to the front: as a switch would. Unset,
/// it announces nothing, which is what the tests about the prefix want.
const CHILD_LABEL: &str = "NVMUX_TEST_RELAY_LABEL";

/// Set on a child whose client is nvmux's own — this crate's binary, run with
/// `--client` — rather than Neovim's: the client that draws cards (see
/// [`nvmux::card`]).
const CHILD_OWN: &str = "NVMUX_TEST_RELAY_OWN_CLIENT";

/// Set on a child to post a card on the board, for its own session, a moment
/// into its first relay — as the notifier does — that goes to the session
/// with the id this names, or with `buf:<n>` to buffer `n` of its own.
const CHILD_POST: &str = "NVMUX_TEST_RELAY_POST";

/// Set on a child whose relay is told, on a switch, that the switch lands with
/// a card naming this — as `main` tells it of the session it goes to.
const CHILD_LANDS: &str = "NVMUX_TEST_RELAY_LANDS";

/// Set on a child whose switch lands with a card ([`CHILD_LANDS`]) to have
/// that card a step along its row, as `<prefix> n` makes it: from `aa` to the
/// session it names, with twelve `b`s next and `cc` going off the far end.
const CHILD_STEPS: &str = "NVMUX_TEST_RELAY_STEPS";

/// Set on a child to move commands to other keys: the body of a `[keys.bind]`
/// table, read as the config file's would be.
const CHILD_BIND: &str = "NVMUX_TEST_RELAY_BIND";

/// How long into the first relay that card is posted: after the first paint.
const POST_AFTER: Duration = Duration::from_millis(1500);

/// Set on a child that asks its relay to say when the session has painted
/// (see `nvmux::pty::Attachment::tell_when_painted`), and writes [`PAINTED`]
/// when told — or [`UNPAINTED`] if it is let go of untold.
const CHILD_PAINTED: &str = "NVMUX_TEST_RELAY_PAINTED";

/// What that child writes when told, and when let go of untold.
const PAINTED: &[u8] = b"\x1b]9999;painted\x07";
const UNPAINTED: &[u8] = b"\x1b]9999;unpainted\x07";

/// What a posted card says, so that the parent can find it on the screen.
const POSTED: &str = "posted for the session in front";

/// The exit code of a child whose relay ended in a switch to a session by
/// its id, which only `<prefix> Enter` on a card asks for.
const SWITCHED_BY_CARD: i32 = 5;

/// The fade a child draws, both ways, in milliseconds, where the default is
/// not what the test wants (see [`LONG_FADE_MS`]). Only a child with a
/// palette (see [`CHILD_PALETTE`]) has a fade to draw.
const CHILD_FADE_MS: &str = "NVMUX_TEST_RELAY_FADE_MS";

/// What a keeping child writes once its client is parked, so the parent knows
/// the relay has let go of the terminal: an OSC no terminal acts on, which
/// the screen the tests read is none the worse for.
const PARKED: &[u8] = b"\x1b]9999;parked\x07";

/// The rows and columns of the pty every [`Terminal`] opens. Named because the
/// hint bar's whole geometry is "the last row", and a test that asks what is on
/// it has to agree with the child about which row that is.
const ROWS: u16 = 40;
const COLS: u16 = 120;

/// The client's kitty keyboard query and DA1, and the terminal's answers. The
/// kitty reply says "supported, no flags set yet"; the DA1 reply is a
/// VT220's. A terminal without the kitty protocol answers only the DA1, and
/// the client then falls back to `modifyOtherKeys`.
const KITTY_QUERY: &[u8] = b"\x1b[?u";
const KITTY_REPLY: &[u8] = b"\x1b[?0u";
const DA1: &[u8] = b"\x1b[c";
const DA1_REPLY: &[u8] = b"\x1b[?62;22c";
/// What the client sends to turn `modifyOtherKeys` on.
const XTERM_PUSH: &[u8] = b"\x1b[>4;2m";
/// The client asking whether the terminal reports its size in band (DEC mode
/// 2048), and a terminal that does saying so — supported, and not yet set.
const IN_BAND_QUERY: &[u8] = b"\x1b[?2048$p";
const IN_BAND_REPLY: &[u8] = b"\x1b[?2048;2$y";

/// The child half: attach to the session named in the environment and relay
/// until the prefix machine ends it, then say how through the exit code.
///
/// Ignored so `cargo test` does not run it on its own; the parent runs it with
/// `--ignored --exact`. Without the environment it does nothing, so a stray
/// `--ignored` run is harmless.
#[test]
#[ignore = "the child half of the relay tests; run by them, not by hand"]
fn relay_child() {
    let (Ok(sock), Ok(id)) = (std::env::var(CHILD_SOCK), std::env::var(CHILD_ID)) else {
        return;
    };
    // The defaults, but with a command wait no scheduler hiccup can beat —
    // the gap the parent leaves between the prefix and its command is meant
    // to clear the sequence wait, not to race this one — and the client kept
    // only in a child that asks for it (CHILD_KEEP): the tests about spellings
    // and the hint row are about a relay that never parks.
    let mut settings = nvmux::config::with_prefix(nvmux::keys::PREFIX);
    settings.keys.timeout_ms = 10_000;
    // Neovim's own client, which these tests are about: it is the one that
    // negotiates the keyboard with the terminal through the relay, and the
    // default's own client is `nvmux --client`, which this binary is not.
    settings.client.ui = nvmux::config::Ui::Nvim;
    let own = std::env::var_os(CHILD_OWN).is_some();
    if own {
        settings.client.ui = nvmux::config::Ui::Nvmux;
    }
    let keep = std::env::var_os(CHILD_KEEP).is_some();
    settings.client.per_session = keep;
    // The picker full screen, which ends the relay: where these tests park a
    // kept client, and what a moved picker key is seen to do. Floating, it
    // would stay in the relay, over the session.
    settings.style.float = false;
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

    // Without this the fade is off — `palette::get` is `None`, so `fade::enabled`
    // is false — and there is no shadow of the session's screen. The tests about
    // spellings want it that way: no fade means no held first paint and no
    // frames, so what reaches the parent is the client's own bytes and nothing
    // else. The bar test needs the shadow, and says so.
    if std::env::var_os(CHILD_PALETTE).is_some() {
        nvmux::palette::init(Some(nvmux::palette::Palette {
            fg: nvmux::palette::Rgb(PALETTE_FG, PALETTE_FG, PALETTE_FG),
            bg: nvmux::palette::Rgb(0, 0, 0),
            ansi: [nvmux::palette::Rgb(0, 0, 0); 16],
        }));
    }

    // An empty notice announces nothing, and is the one most of these tests
    // want: they are about the prefix reaching the machine, and a message in
    // the editor would be noise in the stream the parent is reading.
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
            // The picker's place, in a child that keeps its client: parked for
            // as long as a picker might have been up, then taken back.
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
                // Back in place of another session's, as far as the notice is
                // concerned: what `main` asks of a kept client that is.
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

/// A client started as `main` starts one ahead, held by the pool for
/// [`AHEAD_FOR`] — or [`CHILD_AHEAD_MS`] — and then taken back, never yet
/// relayed, with its probe answered, which a local server does in well under
/// that. nvmux's own if `own`.
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

/// Which keyboard protocol the fake terminal admits to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    /// Answers the kitty query, so the client pushes the kitty protocol.
    Kitty,
    /// Ignores it, so the client falls back to `modifyOtherKeys`.
    Xterm,
}

/// The terminal side of a relay running in a child process.
struct Terminal {
    protocol: Protocol,
    /// Whether this terminal claims in-band size reports (DEC mode 2048),
    /// which a client then turns on and takes its size from instead of from
    /// `SIGWINCH`.
    in_band: bool,
    child: std::process::Child,
    /// The exit code, once the child has been seen to exit; it must not be
    /// signalled or waited for again after that.
    exited: Option<u32>,
    /// The pty's master, which what is typed goes in through and the
    /// terminal's size is set on. Held so the pty outlives the child, which
    /// `Drop` reaps before the fields go.
    pty: Pty,
    /// Everything the child has written, in order.
    output: Vec<u8>,
    /// How far `output` has been scanned for queries.
    answered: usize,
    incoming: mpsc::Receiver<Vec<u8>>,
}

/// What a [`Terminal`]'s child is asked for, past the session and the
/// protocol: each is one of the `CHILD_*` switches, and none is the default.
#[derive(Default)]
struct Asks {
    /// [`CHILD_PALETTE`].
    shadow: bool,
    /// [`CHILD_KEEP`].
    keep: bool,
    /// Whether the terminal reports its size in band (DEC mode 2048).
    in_band: bool,
    /// [`CHILD_AHEAD`].
    ahead: bool,
    /// [`CHILD_AHEAD_MS`].
    ahead_ms: Option<u64>,
    /// [`CHILD_PARKED_MS`].
    parked_ms: Option<u64>,
    /// [`CHILD_LABEL`].
    label: Option<&'static str>,
    /// [`CHILD_FADE_MS`].
    fade_ms: Option<u64>,
    /// [`CHILD_OWN`].
    own: bool,
    /// [`CHILD_POST`]: where the posted card goes.
    post: Option<&'static str>,
    /// [`CHILD_LANDS`]: what a switch lands with.
    lands: Option<&'static str>,
    /// [`CHILD_STEPS`].
    steps: bool,
    /// [`CHILD_BIND`]: the commands moved, as a `[keys.bind]` table says it.
    bind: Option<&'static str>,
    /// [`CHILD_PAINTED`].
    painted: bool,
}

impl Terminal {
    /// Re-run this test binary as `relay_child` on a fresh pty, with no palette
    /// and so no fade, and a client that is not kept — what every test about a
    /// spelling wants.
    fn spawn(sock: &Path, id: &str, protocol: Protocol) -> Self {
        Self::spawn_with(sock, id, protocol, false)
    }

    /// [`Terminal::spawn`], with `shadow` asking the child for the palette that
    /// gives it a shadow of the session's screen (see [`CHILD_PALETTE`]).
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

    /// A child that keeps its client (see [`CHILD_KEEP`]), on a terminal that
    /// reports its size in band if `in_band`, and with the fade on if `fade`.
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

    /// A child whose client is started ahead of its first visit (see
    /// [`CHILD_AHEAD`]) and kept, as `[client] lazy = false` leaves it with
    /// the defaults, with the fade on if `fade`.
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

    /// A child that announces the session as `label` (see [`CHILD_LABEL`]),
    /// and keeps its client if `keep`.
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
        } = asks;
        let (pty, pts) = pty_process::blocking::open().expect("open a pty");
        pty.resize(pty_process::Size::new(ROWS, COLS))
            .expect("size the pty");

        let mut cmd = Command::new(std::env::current_exe().expect("current exe"))
            // `--nocapture`, or the child's diagnostics would sit in libtest's
            // capture buffer when it exits, and never reach this pty.
            .args(["--ignored", "--exact", "relay_child", "--nocapture"])
            .env(CHILD_SOCK, sock)
            .env(CHILD_ID, id)
            // What Windows Terminal, and most others, set. Not that it matters
            // for the kitty query, which is sent whatever the terminal claims
            // to be; but the `modifyOtherKeys` fallback is withheld from a
            // terminal that says it is an old VTE.
            .env("TERM", "xterm-256color")
            .env_remove("VTE_VERSION")
            // `NO_COLOR` forces the fade off whatever the config says, and with
            // it the shadow: removed so the child's screen does not depend on
            // the environment whoever ran `cargo test` happened to have.
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
        // Takes the slave, or the master would never see EOF once the child
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

    /// Relay output for up to `within`, answering the client's queries as the
    /// terminal would, until `done` holds over everything received so far.
    /// Returns whether it did.
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

    /// Answer every query in the output not yet answered. A query may straddle
    /// two reads, so the scan restarts a little before where it left off and
    /// counts only matches that end past it.
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
        // In the order the client asked, which is the order it expects.
        replies.sort_by_key(|&(i, _)| i);
        for (_, reply) in replies {
            self.reply(reply);
        }
        self.answered = self.output.len();
    }

    /// Answer the child as its terminal would, and not at all once it has
    /// gone: what it asks on its way out it does not wait for, and may be
    /// read only after, when a write to the pty fails on macOS.
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

    fn type_bytes(&mut self, bytes: &[u8]) {
        self.pty.write_all(bytes).expect("write to pty");
        self.pty.flush().expect("flush pty");
    }

    /// The child's exit code, if it exits within `within`, with all it wrote
    /// on its way out read.
    fn exit_code(&mut self, within: Duration) -> Option<u32> {
        let deadline = Instant::now() + within;
        while self.exited.is_none() && Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exited = Some(common::exit_code(status));
                break;
            }
            // Keep answering: the client's own exit waits on a DA1 reply.
            self.pump_until(Duration::from_millis(50), |_| false);
        }
        // Its last frames can still be on their way from the pty when it is
        // seen to have gone: read on to the end of what it wrote, which the
        // reader comes to once nothing has the pty's slave open.
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
        // A child already reaped must not be signalled: its pid may be
        // someone else's by now.
        if self.exited.is_none() {
            common::hang_up_and_reap(&mut self.child);
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The flags of the client's kitty keyboard push, `CSI > flags u`, if it has
/// sent one. Neovim 0.10 and 0.11 push 1, 0.12 pushes 3; matched by shape so
/// the test follows whichever Neovim is installed.
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

/// Attach a real client to a real session, let it negotiate `protocol`, then
/// type the prefix spelled as `prefix` followed by `d`, and expect a detach —
/// with nothing reaching the editor on the way.
fn detaches_with(tag: &str, protocol: Protocol, prefix: &[u8]) {
    require_nvim!();
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");

    let mut term = Terminal::spawn(&sock, &session.id, protocol);

    // The client only spells keys the new way once it has turned the
    // protocol on, and it only does that after our replies reach it —
    // through nvmux, in both directions.
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
        // A client that asked for release reports gets one for the prefix,
        // which must not count as its command.
        term.type_bytes(b"\x1b[32;5:3u");
    }
    // A human gap, well past the sequence wait.
    term.pump_until(Duration::from_millis(150), |_| false);
    term.type_bytes(b"d");

    // The detach is the whole signal, and a leak has nowhere else to show: a
    // prefix that reached the editor would leave `d` pending there as an
    // operator — silent on screen — and no detach would ever happen.
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

/// The bug as reported: Windows Terminal 1.25 with the kitty keyboard
/// protocol, where the prefix arrives as `CSI 32 ; 5 u`.
#[test]
fn a_kitty_encoded_prefix_detaches_through_a_real_client() {
    detaches_with("kitty", Protocol::Kitty, b"\x1b[32;5u");
}

/// A terminal without the kitty protocol, where the client falls back to
/// `modifyOtherKeys` and the prefix arrives as `CSI 27 ; 5 ; 32 ~`.
#[test]
fn an_xterm_encoded_prefix_detaches_through_a_real_client() {
    detaches_with("xterm", Protocol::Xterm, b"\x1b[27;5;32~");
}

/// And the control byte still works, in a terminal that speaks the protocol
/// too: Neovim accepts both, and so must the machine.
#[test]
fn the_control_byte_still_detaches_through_a_real_client() {
    detaches_with("byte", Protocol::Kitty, b"\x00");
}

#[test]
fn the_push_is_matched_by_shape() {
    assert_eq!(kitty_push_flags(b"\x1b[?2004h\x1b[>1u\x1b[?1004h"), Some(1));
    assert_eq!(kitty_push_flags(b"\x1b[>3u"), Some(3));
    // The terminal's own reply, the xterm push, and a pop are not pushes.
    assert_eq!(kitty_push_flags(b"\x1b[?0u\x1b[>4;2m\x1b[<u"), None);
    assert_eq!(kitty_push_flags(b"\x1b[>"), None);
}

/// One row of the screen the child's output describes, as a terminal would show
/// it.
///
/// The byte stream cannot answer this question. nvmux's writes, the client's
/// paints and the fade's frames all cross the same wire in whatever order the
/// relay managed, and "the bar is gone" is a statement about cells rather than
/// about bytes — so the test keeps a parser, as `src/shadow.rs` keeps one, and
/// reads the row off it.
fn row_on_screen(out: &[u8], row: u16) -> String {
    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    parser.process(out);
    row_of(parser.screen(), row)
}

/// One row of a parsed screen, as text, trimmed.
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

/// What a terminal could have put on the glass of `row`, at every moment it
/// could have put anything there: the end of each synchronized update, and
/// every byte of what is written outside one. A run of moments that show the
/// same thing is one, at the first of them, and each comes with how far into
/// `out` it is.
///
/// [`row_on_screen`] reads the end of the stream, where a row wiped by every
/// frame of the session's and put back after each looks exactly like one that
/// was never touched. What the user sees is everything in between, because a
/// terminal presents on a clock of its own: outside an update it may show any
/// prefix of the stream, and inside one only the whole of it.
fn presented(out: &[u8], row: u16) -> Vec<(usize, String)> {
    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    let mut moments: Vec<(usize, String)> = Vec::new();
    let mut held = false;
    for (i, byte) in out.iter().enumerate() {
        parser.process(std::slice::from_ref(byte));
        // Held as a mode, as terminals hold it: a second begin inside an
        // update changes nothing, and the first end lets it all go.
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

/// The prefix puts the key row up over a live editor, and the next key takes it
/// off again with the editor's own row underneath it — the two halves of the
/// feature, neither of which is visible from a unit test.
///
/// `<prefix> <prefix>` is the dismissal to test on, because it is one of the
/// three that leave the relay *running*: the picker, a detach and a switch all
/// end it, and there the screen is dissolved or cleared on the way out and would
/// clear the bar whether or not anything had taken it off. Here nothing else
/// touches the row, so if the bar is still on it, it stays.
///
/// `shadow` picks which way the row is put back, and both have to work: from the
/// shadow's copy of the screen, which is local and exact, or — with the fade off,
/// and so no shadow — by blanking the row and asking the server to repaint it,
/// the way the attach notice always leaves.
fn the_hint_row_goes_up_and_comes_down(tag: &str, shadow: bool) {
    require_nvim!();
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    // No 'showcmd': the literal prefix below reaches Neovim, which shows the
    // key it was given there — `<20>`, for the space in Ctrl-Space — on the
    // very row this reads, until the key is done with. Whether that frame is
    // read before the bar is down or after is the runner's timing, not the
    // relay's.
    nvmux::rpc::Client::connect(&sock, Duration::from_secs(3))
        .expect("connect")
        .command("set noshowcmd")
        .expect("noshowcmd");

    let mut term = Terminal::spawn_with(&sock, &session.id, Protocol::Kitty, shadow);

    // The client is up and painting once it has turned the protocol on.
    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            kitty_push_flags(out).is_some()
        }),
        "the client never started; got: {}",
        term.since(0)
    );
    // Let the first paint land: with the fade on it is held back and dissolved
    // in, so the screen the bar covers does not exist until that is over.
    term.pump_until(Duration::from_secs(2), |out| {
        !row_on_screen(out, 0).is_empty()
    });
    let under = row_on_screen(&term.output, ROWS - 1);

    // The prefix alone. The child's `timeout_ms` is ten seconds, so the bar has
    // as long as it needs to find a lull.
    term.type_bytes(&[nvmux::keys::PREFIX]);
    let mark = term.output.len();
    assert!(
        term.pump_until(Duration::from_secs(10), |out| {
            row_on_screen(out, ROWS - 1).contains("␣ picker")
        }),
        "the prefix put no hint row on row {ROWS}; it says {:?}, and the child wrote: {}",
        row_on_screen(&term.output, ROWS - 1),
        term.since(mark)
    );
    assert!(
        row_on_screen(&term.output, ROWS - 1).contains("d detach"),
        "the row is not the key list: {:?}",
        row_on_screen(&term.output, ROWS - 1)
    );

    // A second prefix is a literal: Neovim gets the byte and the relay carries
    // on, so the bar has to take itself off.
    let mark = term.output.len();
    term.type_bytes(&[nvmux::keys::PREFIX]);
    assert!(
        term.pump_until(Duration::from_secs(10), |out| {
            !row_on_screen(out, ROWS - 1).contains("picker")
        }),
        "the hint row is still on row {ROWS} after the prefix resolved: {:?}; the child wrote: {}",
        row_on_screen(&term.output, ROWS - 1),
        term.since(mark)
    );
    assert_eq!(
        row_on_screen(&term.output, ROWS - 1),
        under,
        "the editor's own row did not come back"
    );

    // And the relay really is still running: the prefix still detaches.
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

/// The ordinary path: a shadow of the session's screen, so the row goes back
/// exactly and nothing is asked of the server.
#[test]
fn a_prefix_raises_the_hint_row_and_the_next_key_takes_it_off() {
    the_hint_row_goes_up_and_comes_down("hint", true);
}

/// One row of the screen the child's output describes, the terminal made
/// `rows` tall `at` that far into it, as the test made it.
fn row_on_resized_screen(out: &[u8], at: usize, rows: u16, row: u16) -> String {
    let at = at.min(out.len());
    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    parser.process(&out[..at]);
    parser.screen_mut().set_size(rows, COLS);
    parser.process(&out[at..]);
    row_of(parser.screen(), row)
}

/// The terminal made taller while a session is attached: the hint row the
/// prefix puts up goes on its new last row. The relay reads the terminal's
/// size when it starts and again on each `SIGWINCH`, not on every pass, so a
/// size not read again would put the bar on the old last row, now rows above
/// the bottom.
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
    // No 'showcmd', as for the bar above: the key Neovim shows would be on
    // the row this reads.
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

    // Four rows taller, while attached, and the editor redrawn for it.
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

    // And the relay goes on: a literal prefix takes the bar down, and the
    // prefix still detaches.
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

/// And with the fade off there is no shadow, so the bar blanks its row and the
/// relay asks the server for what was under it — the one path on which taking
/// the bar down costs a round trip, and the one a `NO_COLOR` user is on.
#[test]
fn the_hint_row_comes_down_through_the_server_without_a_shadow() {
    the_hint_row_goes_up_and_comes_down("hint-no-shadow", false);
}

/// How often the editor redraws its last row in
/// [`the_hint_row_stays_over_a_last_row_that_keeps_changing`]: about as often
/// as a language server reports progress while it indexes, and far enough
/// apart for a lull between two of them.
const TICK_MS: u64 = 50;

/// A last row the editor keeps redrawing — a statusline with a language
/// server's progress in it — does not show through the bar while the prefix
/// waits: at every moment the terminal could have presented, from the bar
/// going up to the key that takes it down, the row is the bar, whole.
///
/// The row is a statusline with no command line under it (`cmdheight=0`),
/// one letter the whole way across and the next letter every [`TICK_MS`], so
/// every cell of the bar is drawn over on every tick. A letter rather than a
/// digit, because a `%{}` that comes to nothing but digits is read as a
/// number, and a row of them is one far too big to show. The ticks leave lulls,
/// so a bar that waits for one goes up between them — which is how it used to
/// be lost: wiped by the next tick, and put back at the lull after it.
///
/// `own` is whose client draws the session. Neovim's brackets nothing here,
/// since this terminal never answers its `?2026` query, so the bar goes into
/// a synchronized update nvmux opens around each of the client's writes;
/// nvmux's own brackets every frame, and the bar goes in front of the
/// frame's close.
fn the_hint_row_stays_over_a_last_row_that_keeps_changing(tag: &str, own: bool) {
    require_nvim!();
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let mut editor = nvmux::rpc::Client::connect(&sock, Duration::from_secs(3)).expect("connect");
    // No 'showcmd', for the reason the test above gives.
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
    // One letter across the whole row: the statusline, as the editor has it.
    let ticking = |row: &str| {
        row.len() == usize::from(COLS)
            && row
                .bytes()
                .all(|b| b.is_ascii_lowercase() && row.starts_with(b as char))
    };
    // And not before the client's first paint has been let through. With the
    // fade on, the row is on the screen first in the fade's frames, drawn from
    // the shadow while the relay is still dissolving the paint in and reading
    // no keys; the DA1 in the paint goes by only after them. Waiting on the
    // row alone, the ticks were started and the prefix pressed in the middle
    // of the fade, and the prefix was not read until the fade was over.
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
    let after = ticks();
    // What the relay wrote before the key that takes the bar down: all of it
    // was written with the bar wanted up.
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

    // A second prefix is a literal, and takes the bar down; the editor's row,
    // still ticking, is what is left.
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

/// Neovim's own client, whose writes nvmux has to hold together with the bar.
#[test]
fn the_hint_row_stays_over_a_last_row_that_keeps_changing_under_neovims_client() {
    the_hint_row_stays_over_a_last_row_that_keeps_changing("hint-tick", false);
}

/// nvmux's own client, the default, which brackets its frames itself.
#[test]
fn the_hint_row_stays_over_a_last_row_that_keeps_changing_under_its_own_client() {
    the_hint_row_stays_over_a_last_row_that_keeps_changing("hint-tick-own", true);
}

/// A command `[keys.bind]` moves runs on its new key through a real client, and
/// the row the prefix puts up names that key: the machine and the bar both
/// follow the config rather than the table's own keys.
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

    // The prefix as the kitty protocol spells it, which is how this terminal
    // sends it now that the client has asked.
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

/// What a resumed relay writes before anything of the client's: back onto the
/// alternate screen, and cleared. After it, whatever is on the screen is what
/// the client has drawn since.
const RESUMED: &[u8] = b"\x1b[?1049h\x1b[2J";

/// With `[client] per_session`, a client that comes back to the front has
/// its screen put straight back from the copy kept while it was parked, the
/// server's own repaint asked for on top of it, and — if the terminal changed
/// size meanwhile — the new size told to it: by the pty, or by an in-band
/// report for a client on a terminal that sends those (`in_band`: DEC mode
/// 2048, which such a client takes its size from instead of from `SIGWINCH`,
/// and which the reports sent while it was parked never reached).
///
/// Three returns. At the same size, where nothing but the server's repaint
/// clears the screen — so the one that comes was asked for. After the
/// terminal was made narrower, where the server has to end up at the new
/// size. And with the server spinning in Lua for the whole of the wait,
/// unable to answer a repaint or anything else, where the screen has to come
/// back all the same: from the kept copy, which is what the user looks at
/// until the server is free.
///
/// With the fade on (`fade`), the copy is the fade's shadow and the screen is
/// dissolved back in from it rather than painted straight back.
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
    // No swap file: two of these run at once, each with an unnamed buffer,
    // and would otherwise race for the same one.
    rpc.command(&format!(
        "setlocal noswapfile | call setline(1, '{marker}')"
    ))
    .expect("setline");

    let mut term = Terminal::spawn_keeping(&sock, &session.id, in_band, fade);
    // Painted, and — on a terminal that offers them — taking its size in
    // band, which the client turns on once the terminal's answers are in.
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
    // A client is parked only once the terminal has answered its DA1. With
    // the fade, the marker is on the screen in the fade's frames before the
    // client's first paint, the DA1 in it, is let through: so until the DA1
    // has gone by, and a moment for the answer to reach the client.
    assert!(
        term.pump_until(Duration::from_secs(5), |out| find(out, DA1).is_some()),
        "the client never asked its DA1; got: {}",
        term.since(0)
    );
    term.pump_until(Duration::from_millis(300), |_| false);

    // Away to the picker's place — parked, in the child — and back, at the
    // same size, until the output after the resume has cleared the screen.
    let after = away_and_back(&mut term, |_| {});
    assert!(
        term.pump_until(Duration::from_secs(5), |out| {
            find(&out[after.min(out.len())..], b"\x1b[2J").is_some()
        }),
        "no repaint came after the client came back; the child wrote: {}",
        term.since(after)
    );
    assert!(
        row_on_screen(&term.output, 0).contains(marker),
        "the repainted screen is not the session's: {:?}",
        row_on_screen(&term.output, 0)
    );
    // Its mouse reporting was put back from what it told the terminal —
    // Neovim's default `'mouse'` turns on button tracking — after the resume
    // took the screen.
    assert!(
        find(&term.output[after..], b"\x1b[?1002h").is_some(),
        "the client's mouse reporting was not put back; the child wrote: {}",
        term.since(after)
    );

    // Away again, the terminal made narrower while the client is parked, and
    // back: the server has to be told the new size — through the pty for a
    // client that listens to it, and, for one that takes its size in band and
    // ignores the pty, only by the report nvmux writes it.
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

    // Away again, and this time the server made too busy to answer anything
    // for longer than the rest of this waits.
    term.pump_until(Duration::from_millis(300), |_| false);
    let mark = term.output.len();
    term.type_bytes(&[nvmux::keys::PREFIX, b' ']);
    // Written `4 > …` rather than `… < 4`: the input is key notation, where
    // a `<` would start a key name and take the `<CR>` with it.
    rpc.input(":lua local t = os.clock() while 4 > os.clock() - t do end<CR>")
        .expect("make the server busy");
    let came_back = term.pump_until(Duration::from_secs(3), |out| {
        let since = &out[mark.min(out.len())..];
        find(since, RESUMED).is_some() && row_on_screen(out, 0).contains(marker)
    });
    // Asked after the screen came back, so it says the server was busy then.
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

    // And it is a live relay again: the prefix still detaches.
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

/// `<prefix> Space` to a keeping child, `meanwhile` done once the child has
/// parked its client, and the child's resume waited for. Returns where in the
/// output the resumed relay's own bytes begin.
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

/// A client that takes its size from the pty, and so is told a new one by
/// the resize alone.
#[test]
fn a_kept_clients_screen_comes_back_through_the_pty() {
    a_kept_clients_screen_comes_back("kept-pty", false, false);
}

/// A client that takes its size in band, and ignores the signal (kitty,
/// ghostty, foot): told the new one with a report instead.
#[test]
fn a_kept_clients_screen_comes_back_on_an_in_band_terminal() {
    a_kept_clients_screen_comes_back("kept-in-band", true, false);
}

/// With the fade on, as it is wherever the terminal answers the colour
/// query: dissolved back in from the shadow, the pen and cursor then handed
/// back to the client.
#[test]
fn a_kept_clients_screen_dissolves_back_in() {
    a_kept_clients_screen_comes_back("kept-fade", false, true);
}

/// The fade [`a_kept_clients_repaint_is_asked_under_its_fade_in`] has its
/// child draw, both ways: as long as a fade is allowed to be, so that "while
/// the fade in is still being drawn" is a second wide.
const LONG_FADE_MS: u64 = 2_000;

/// A kept client's repaint is asked as its screen starts to dissolve back in,
/// not once it has: the server redraws underneath frames nvmux draws from its
/// copy, so the exact screen, which the copy stands in for until it comes, is
/// there as the fade ends rather than a whole redraw after it.
///
/// Seen from the server, which counts the windows it redraws. Idle, its
/// client parked, it redraws none; half way through the fade in, the count
/// has moved while nothing the client drew has reached the terminal yet — the
/// repaint, which clears it, comes after the fade.
#[test]
fn a_kept_clients_repaint_is_asked_under_its_fade_in() {
    require_nvim!();
    let tag = "kept-early";
    let scratch = Scratch::new(tag);
    let t = scratch.transport();
    // Clean, so nothing of a config redraws on a timer while the client is
    // parked and the count is meant to be still.
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
    // Parked only once the terminal has answered the client's DA1.
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

    // And it lands once the fade is over, with the session's screen.
    assert!(
        term.pump_until(Duration::from_secs(5), repainted),
        "no repaint came after the fade in; the child wrote: {}",
        term.since(after)
    );
    assert!(
        row_on_screen(&term.output, 0).contains(marker),
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

/// `[client] lazy = false`, end to end. A client started ahead of its first
/// visit is a UI of its session from the start, and says nothing to the
/// terminal until that visit; then it says everything, its startup questions
/// first, which the terminal answers late and the client acts on as though
/// they had come at once: it draws the session's screen and turns the kitty
/// protocol on, leaves the front and comes back as a kept client, and a
/// prefix spelled that way detaches.
///
/// With the fade on (`fade`), what it kept goes through the hold, as a fresh
/// client's first paint does, and is dissolved in.
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
    // Attached to its server, while the child still holds it.
    assert!(
        common::wait_until(AHEAD_FOR / 2, || {
            nvmux::rpc::Client::connect(&sock, Duration::from_secs(2))
                .and_then(|mut c| c.list_uis())
                .is_ok_and(|n| n == 1)
        }),
        "the client started ahead never attached to its session"
    );
    // And not a word of it on the terminal.
    assert!(
        !term.pump_until(Duration::from_millis(200), |out| {
            find(out, KITTY_QUERY).is_some()
        }),
        "the client's questions reached the terminal before its first visit: {}",
        term.since(0)
    );

    // The visit: its questions asked, answered, and acted on.
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

    // Its questions answered, it is a kept client like any other from here:
    // parked, and painted straight back.
    term.pump_until(Duration::from_millis(300), |_| false);
    let after = away_and_back(&mut term, |_| {});
    assert!(
        term.pump_until(Duration::from_secs(5), |out| {
            find(&out[after.min(out.len())..], b"\x1b[2J").is_some()
        }),
        "no repaint came after the client came back; the child wrote: {}",
        term.since(after)
    );
    assert!(
        row_on_screen(&term.output, 0).contains(marker),
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

/// A relay says when its session has painted, once, and only after the
/// session's screen has reached the terminal — with the first paint held and
/// dissolved in if `fade`, and relayed as it comes if not. It is what the
/// clients a dropped link took wait for before they are started again behind
/// the session in front (`[client] lazy` off): each has its session's screen
/// sent over the same link.
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
    // Once: nothing else is said, however long the session goes on.
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

/// A client started ahead of a session that
/// draws all the while — a build running in a `:terminal`, a log — is kept to
/// its first visit. Away, it draws a frame a second rather than every change
/// (see `nvmux::card::Command::Away`), and what it writes for the terminal
/// stays far under what would have it retired as having said too much (see
/// `nvmux::pty::AHEAD_MAX`). And on that visit the session is drawn as it is
/// by then, not as it was the last time it was drawn away.
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
    // Every row of the screen rewritten every 5 ms: a frame of several
    // kilobytes each time, a megabyte in well under the time it is held.
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
    // And kept as it is from then on, as in front: never a second behind,
    // which is two hundred ticks. Neovim asked first, and the screen read
    // after, so that the time the reading takes — megabytes of frames by
    // then, on a slow machine — is not counted against the client.
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

/// A client started ahead, drawing at a crawl out of sight, was sized for
/// the terminal as it was then. Made shorter since,
/// the terminal has the client resized on the first visit, and the paint that
/// visit holds and dissolves in is drawn for the size it has now — not the old
/// screen cut short. The client is told it is back in front before it is
/// resized: away, its redraw would come at the crawl it draws at there (see
/// `nvmux::card::Command::Away`), after the held paint had gone out without
/// it.
///
/// Held for a time that is no whole number of seconds: away, a client draws
/// a frame a second at most, counted from its first, and a visit on the
/// second finds it free to draw whatever it is told then.
///
/// And the session draws all the while, a line rewritten every 10 ms, so
/// that a client left at the crawl has drawn within the last second whenever
/// the visit comes, and its redraw for the size waits: the paint goes out
/// without it. A client back in front draws it at once, and what it draws as
/// the session goes keeps the held paint waiting the longer. Neovim's own
/// redraw for the size may still come after the paint on a slow machine,
/// which is why the first frame is held only to being drawn for the size.
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
    // The first frame the session is on: the fade's, all of it there at once.
    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            row_on_resized_screen(out, at, rows, 0).starts_with("tick ")
        }),
        "the session's screen never came up; the child wrote: {}",
        term.since(at)
    );
    // Drawn for the new size: the session's status line above its last row,
    // or, where Neovim has yet to redraw for the size, the client's own frame
    // for it — its window running on to the last row, or ending two rows
    // above it, over the rows Neovim has still to draw. Not the old screen cut
    // short: its window's `~` rows running on to the row above the last, and
    // the rows below that — its status line, its command line — all written
    // onto the last.
    let above = row_on_resized_screen(&term.output, at, rows, rows - 2);
    let last = row_on_resized_screen(&term.output, at, rows, rows - 1);
    assert!(
        !(above == "~" && last != "~"),
        "the first frame is the old screen cut short: {above:?} over {last:?}, and the child wrote: {}",
        term.since(at)
    );
    // And the status line where it goes, soon after if not at once.
    assert!(
        term.pump_until(Duration::from_secs(5), |out| {
            row_on_resized_screen(out, at, rows, rows - 2).contains("[No Name]")
        }),
        "no status line above the last row: {:?}",
        row_on_resized_screen(&term.output, at, rows, rows - 2)
    );
    assert_eq!(detach(&mut term), Some(0), "the child did not detach");
}

/// The session's `:messages`.
fn messages(sock: &Path) -> String {
    nvmux::rpc::Client::connect(sock, Duration::from_secs(3))
        .and_then(|mut c| c.eval("execute('messages')"))
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// `<prefix> d`, as the kitty protocol spells the prefix, and the child's exit
/// waited for.
fn detach(term: &mut Terminal) -> Option<u32> {
    prefix_then(term, b"d")
}

/// The prefix as the kitty protocol spells it, then `key`, and the child's
/// exit waited for.
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

/// A child relaying nvmux's own client, which says the session is `label`.
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

/// The rows of the screen the output makes, as text.
fn screen_rows(out: &[u8]) -> Vec<String> {
    (0..ROWS).map(|row| row_on_screen(out, row)).collect()
}

/// Whether a card saying `label` is in the margin, set flush right on the
/// row under the top one.
fn carded(out: &[u8], label: &str) -> bool {
    row_on_screen(out, 1).ends_with(label)
}

/// Where a switch landed is a card nvmux's own client draws, in the margin:
/// nothing is said to the editor, which keeps nothing of it.
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

/// A detach hands the shell back a clean screen, with nothing on it of what
/// the relay told the client. With the fade off, a fresh client is told where
/// it landed the moment its relay starts — before it has made its pty raw, on
/// the primary screen the relay has just cleared, which is the one a detach
/// goes back to — and a pty that echoed the card left it there, as hex.
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
    // What the child wrote last can still be on its way when its exit is
    // seen.
    term.pump_until(Duration::from_millis(200), |_| false);
    let left: Vec<String> = screen_rows(&term.output)
        .into_iter()
        .filter(|row| !row.trim().is_empty())
        .collect();
    assert!(left.is_empty(), "the detach left the shell this: {left:#?}");
}

/// A kept client draws at a crawl while it is
/// parked: a change its session makes just after it went waits a second to be
/// drawn. Brought back sooner, with no card — as from the picker or the help
/// full screen — it draws that change on its way back, and the relay waits
/// for it: the screen dissolves in as the session is now, not as it was last
/// drawn, to jump to the present once the fade is over.
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
    // Long enough since its last frame that the client draws the one it goes
    // with at once, and then nothing for a second.
    term.pump_until(Duration::from_millis(1500), |_| false);

    let mark = term.output.len();
    term.type_bytes(&[nvmux::keys::PREFIX, b' ']);
    assert!(
        term.pump_until(Duration::from_secs(5), |out| {
            find(&out[mark.min(out.len())..], PARKED).is_some()
        }),
        "the child never parked its client"
    );
    rpc.command("call setline(1, 'as it is now')")
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
    assert!(
        row.contains("as it is now"),
        "a quarter into the fade in, the screen coming back is {row:?}"
    );
    assert_eq!(detach(&mut term), Some(0), "the child did not detach");
}

/// A kept client coming back has the card drawn before its screen dissolves
/// in, and the card stands as the screen comes in: on it, at its full colour,
/// half way through the fade in, before anything the client draws after the
/// fade has arrived.
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
    // Parked only once the terminal has answered the client's DA1, and well
    // after the first card has gone.
    assert!(
        term.pump_until(Duration::from_secs(5), |out| find(out, DA1).is_some()),
        "the client never asked its DA1"
    );
    // Its life, and three of the fade's one way to go: see `client::cards`.
    let gone = nvmux::card::ARRIVAL_LIFE + Duration::from_millis(3 * LONG_FADE_MS / 2 + 500);
    term.pump_until(gone, |_| false);
    assert!(!carded(&term.output, label), "the first card never went");

    // Parked and brought back until the card comes in with the screen, six
    // times at most. The relay gives the client `CARD_WAIT` to draw it before
    // the screen dissolves in (see `nvmux::pty`), no longer: a client too busy
    // for that, as on a busy machine, has the screen come in without it and
    // the card after, which says nothing of how the card stands. Here it takes
    // the client under 3 ms; a CI runner held up for longer now and then —
    // its machine's, not the client's — had three returns in a row late. A
    // relay that never waits has every one late, and fails all the same.
    let mut late = 0;
    while late < 6 {
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
        // The screen from there on a frame at a time, as it came, rather than
        // as it is at moments of the test's own: a test reading behind the
        // fade, as one parsing the whole output again for every look does on a
        // slow machine, sees the card further into its life, which the client
        // times, than it means to look.
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
        // From the fade's first frame the card is on the screen as it is,
        // while the session comes in under it: a `~` far from it, say, still
        // dark. Gone before the client is parked again where it came after.
        let dark =
            matches!(tilde_first, vt100::Color::Rgb(..)) && bright(tilde_first) < PALETTE_FG / 2;
        if !dark {
            late += 1;
            term.pump_until(gone, |_| false);
            assert!(!carded(&term.output, label), "the card never went");
            continue;
        }
        // Half way through the fade in, as the screen has it: the `~` half as
        // bright as the text it comes up to — or the fade over before that.
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

/// A card posted for the session in front — the notifier's, here the child's
/// own — comes up in the margin, and `<prefix> Enter` goes to the session it
/// is about: the relay ends in a switch to it.
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

/// A card about a terminal of the session in front: `<prefix> Enter` shows
/// it, in the session, and the relay goes on.
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
    // A buffer no window shows, as a terminal off the screen is.
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

/// The foreground the cell at (`row`, `col`) of the screen the output makes
/// is drawn in.
fn fg_on_screen(out: &[u8], row: u16, col: u16) -> vt100::Color {
    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    parser.process(out);
    parser
        .screen()
        .cell(row, col)
        .map_or(vt100::Color::Default, vt100::Cell::fgcolor)
}

/// How bright a grey is, in the palette these tests hand the child (see
/// [`PALETTE_FG`]), which draws in greys: nothing for one of the terminal's
/// own colours.
fn bright(color: vt100::Color) -> u8 {
    match color {
        vt100::Color::Rgb(r, _, _) => r,
        _ => 0,
    }
}

/// The screen a terminal's output makes, taken in a frame at a time as the
/// output comes — to the end of each synchronized update, which is when a
/// terminal shows it — and never parsed again from the start, as
/// [`row_on_screen`] parses it: for a test that follows a fade frame by
/// frame, wherever its own reading has got to.
struct Frames {
    parser: vt100::Parser,
    /// How far into the output it has taken in.
    read: usize,
}

impl Frames {
    /// The screen as the output up to `from` leaves it.
    fn from(out: &[u8], from: usize) -> Self {
        let mut parser = vt100::Parser::new(ROWS, COLS, 0);
        parser.process(&out[..from]);
        Self { parser, read: from }
    }

    /// Take in `term`'s next frame, waiting until `until` for it to come.
    /// Says whether one did.
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

    /// The foreground the cell at (`row`, `col`) is drawn in.
    fn fg(&self, row: u16, col: u16) -> vt100::Color {
        self.parser
            .screen()
            .cell(row, col)
            .map_or(vt100::Color::Default, vt100::Cell::fgcolor)
    }
}

/// A switch lands with its card standing: the session's own client draws it
/// as the switch begins, and it stays at its full colour while the session
/// dissolves out from under it — and on the screen the relay hands over to
/// the next, with nothing else.
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
    term.pump_until(Duration::from_millis(500), |_| false);
    let text_was = fg_on_screen(&term.output, 1, 0);

    let flags = kitty_push_flags(&term.output).unwrap_or(0);
    term.type_bytes(b"\x1b[32;5u");
    if flags & 2 != 0 {
        term.type_bytes(b"\x1b[32;5:3u");
    }
    term.pump_until(Duration::from_millis(150), |_| false);
    term.type_bytes(b"3");
    // Flush right, two columns in: the first of the two is the line's last.
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
        fg_on_screen(&term.output, 1, 0),
        text_was,
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
    // The child's own word on how its relay ended aside, on its stderr.
    assert!(
        rows.iter()
            .enumerate()
            .all(|(r, row)| r == 1 || row.is_empty() || row.starts_with("relay ended")),
        "more than the card stayed: {rows:#?}"
    );
}

/// A switch a step along the row lands with the row on the move from the
/// key: the session's own client goes on drawing it as it moves, and the fade
/// out draws it as the client does, at its full colour, while the session
/// dissolves out from under it — so the row is further along by the end of
/// the fade than at its start, and the screen handed over has it as far
/// along as it came.
///
/// Read a frame at a time once the relay has ended, not as it runs: what the
/// test reads the screen with is slower than the fade.
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

    // Each frame from the switch on with the card in it: the column the
    // name it lands on starts at, and its colour; and whether the session's
    // screen is still under it, and the colour of its text.
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
        // The child's palette's text colour.
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

/// Neovim's own client has no way to be asked to draw a card, and is never
/// sent one: it would pass it on to the editor, as the terminal's answer to
/// something (`TermResponse`), for every plugin that listens for those.
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
