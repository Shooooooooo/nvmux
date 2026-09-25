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
//! Three tests here are not about a spelling at all. The rows the prefix puts
//! up ([`nvmux::hint`]) are written over a live editor's first and last rows
//! and taken off them again, with the editor dimmed behind them where there is
//! a shadow of its screen to dim — and a command that leaves the session
//! carries its fade out on from the dimmed screen. None of that can be shown
//! to work anywhere but here: it needs a real client painting a real screen to
//! cover, and the shadow's copy of that screen to dim and to put back.
//!
//! Nor are the last two. With `[client] per_session` a client is parked while
//! something else has the front, and when it comes back nvmux paints the
//! screen from its own copy of it and asks the server for its repaint on top —
//! which only a real client on a real pty, and a real server made too busy to
//! answer, can show.

#[macro_use]
mod common;

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::Scratch;
use nvmux::transport::Transport;
use portable_pty::{CommandBuilder, PtySize};

/// The session socket and id the child attaches to.
const CHILD_SOCK: &str = "NVMUX_TEST_RELAY_SOCK";
const CHILD_ID: &str = "NVMUX_TEST_RELAY_ID";
/// Set on a child that must keep a shadow of the session's screen — which is
/// what the prefix veils the session with, and hands the screen back from. A
/// shadow needs a palette and the fade on, and nothing here queries a real
/// terminal, so the colours are handed to the child rather than asked for.
const CHILD_PALETTE: &str = "NVMUX_TEST_RELAY_PALETTE";
/// Set on a child that keeps its client (`[client] per_session`): where
/// `<prefix> Space` would open the picker it parks the client instead, for
/// [`PARKED_FOR`], and then takes it back and relays it again.
const CHILD_KEEP: &str = "NVMUX_TEST_RELAY_KEEP";
/// What a child's client announces itself with, where it does: the name the
/// attach notice's box shows. Unset, it announces nothing.
const CHILD_ANNOUNCE: &str = "NVMUX_TEST_RELAY_ANNOUNCE";

/// How long a keeping child leaves its client parked: long enough for the
/// parent to have made the server busy before the client comes back.
const PARKED_FOR: Duration = Duration::from_secs(1);

/// What a keeping child writes once its client is parked, so the parent knows
/// the relay has let go of the terminal: an OSC no terminal acts on, which
/// the screen the tests read is none the worse for.
const PARKED: &[u8] = b"\x1b]9999;parked\x07";

/// The rows and columns of the pty every [`Terminal`] opens. Named because the
/// prefix's whole geometry is "the first row and the last", and a test that
/// asks what is on them has to agree with the child about which rows those are.
const ROWS: u16 = 40;
const COLS: u16 = 120;

/// The sessions the child's relay is told it can switch to: another one, and
/// the one under test, second — so the prefix has a sessions row to draw.
/// Their names are the child's to say; the relay only shows them.
const LISTED: [(u32, &str); 2] = [(1, "api"), (2, "web")];

/// What the prefix puts along the top for [`LISTED`], the session under test
/// marked as the one in front.
const SESSIONS_ROW: &str = "1 api  ▸2 web";

/// The colours a child with a shadow is handed (see [`CHILD_PALETTE`]): light
/// grey text on black, and black for every ANSI colour.
fn child_palette() -> nvmux::palette::Palette {
    nvmux::palette::Palette {
        fg: nvmux::palette::Rgb(200, 200, 200),
        bg: nvmux::palette::Rgb(0, 0, 0),
        ansi: [nvmux::palette::Rgb(0, 0, 0); 16],
    }
}

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
    // The defaults, but with a command wait no scheduler hiccup can beat:
    // the gap the parent leaves between the prefix and its command is meant
    // to clear the sequence wait, not to race this one.
    let mut settings = nvmux::config::with_prefix(nvmux::keys::PREFIX);
    settings.keys.timeout_ms = 10_000;
    let keep = std::env::var_os(CHILD_KEEP).is_some();
    settings.client.per_session = keep;
    nvmux::config::init(settings);

    // Without this the fade is off — `palette::get` is `None`, so `fade::enabled`
    // is false — and there is no shadow of the session's screen. The tests about
    // spellings want it that way: no fade means no held first paint and no
    // frames, so what reaches the parent is the client's own bytes and nothing
    // else. The tests of the prefix's rows need the shadow, and say so.
    if std::env::var_os(CHILD_PALETTE).is_some() {
        nvmux::palette::init(Some(child_palette()));
    }

    // An empty notice announces nothing: most of these tests are about the
    // prefix reaching the machine, and a box drawn over the screen would be
    // noise in the stream the parent is reading. The one about the box asks.
    let announce = std::env::var(CHILD_ANNOUNCE).unwrap_or_default();
    let mut pool = nvmux::pool::Pool::configured();
    let mut attachment = nvmux::pty::spawn(&id, Path::new(&sock), &announce).expect("attach");
    let listing = nvmux::hint::Listing::new(LISTED.map(|(n, name)| (n, name.to_string())), Some(2));
    let code = loop {
        match nvmux::pty::relay(attachment, &listing, &mut |_| {}) {
            Ok((nvmux::pty::Outcome::Detached, _)) => break 0,
            // The picker's place, in a child that keeps its client: parked for
            // as long as a picker might have been up, then taken back.
            Ok((nvmux::pty::Outcome::ToPicker, held)) if keep => {
                assert!(pool.set_aside(held).is_none(), "the client was not parked");
                let mut out = std::io::stdout();
                let _ = out.write_all(PARKED);
                let _ = out.flush();
                std::thread::sleep(PARKED_FOR);
                attachment = match pool.take(&id) {
                    nvmux::pool::Taken::Kept(back) => *back,
                    other => panic!("the parked client did not come back: {other:?}"),
                };
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
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// The exit code, once the child has been seen to exit; it must not be
    /// signalled or waited for again after that.
    exited: Option<u32>,
    writer: Box<dyn Write + Send>,
    /// Everything the child has written, in order.
    output: Vec<u8>,
    /// How far `output` has been scanned for queries.
    answered: usize,
    incoming: mpsc::Receiver<Vec<u8>>,
    // Held so the pty outlives the child; dropped last.
    _master: Box<dyn portable_pty::MasterPty>,
}

impl Terminal {
    /// Re-run this test binary as `relay_child` on a fresh pty, with no palette
    /// and so no fade — what every test about a spelling wants.
    fn spawn(sock: &Path, id: &str, protocol: Protocol) -> Self {
        Self::spawn_with(sock, id, protocol, false)
    }

    /// [`Terminal::spawn`], with `shadow` asking the child for the palette that
    /// gives it a shadow of the session's screen (see [`CHILD_PALETTE`]).
    fn spawn_with(sock: &Path, id: &str, protocol: Protocol, shadow: bool) -> Self {
        Self::spawn_child(sock, id, protocol, shadow, false, false, "")
    }

    /// A child that keeps its client (see [`CHILD_KEEP`]), on a terminal that
    /// reports its size in band if `in_band`.
    fn spawn_keeping(sock: &Path, id: &str, in_band: bool) -> Self {
        Self::spawn_child(sock, id, Protocol::Kitty, false, true, in_band, "")
    }

    /// A child with a shadow whose client announces itself as `label` (see
    /// [`CHILD_ANNOUNCE`]), as a client does that a switch has just started.
    fn spawn_announcing(sock: &Path, id: &str, label: &str) -> Self {
        Self::spawn_child(sock, id, Protocol::Kitty, true, false, false, label)
    }

    fn spawn_child(
        sock: &Path,
        id: &str,
        protocol: Protocol,
        shadow: bool,
        keep: bool,
        in_band: bool,
        announce: &str,
    ) -> Self {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let mut cmd = CommandBuilder::new(std::env::current_exe().expect("current exe"));
        // `--nocapture`, or the child's diagnostics would sit in libtest's
        // capture buffer when it exits, and never reach this pty.
        cmd.args(["--ignored", "--exact", "relay_child", "--nocapture"]);
        cmd.env(CHILD_SOCK, sock);
        cmd.env(CHILD_ID, id);
        // What Windows Terminal, and most others, set. Not that it matters
        // for the kitty query, which is sent whatever the terminal claims to
        // be; but the `modifyOtherKeys` fallback is withheld from a terminal
        // that says it is an old VTE.
        cmd.env("TERM", "xterm-256color");
        cmd.env_remove("VTE_VERSION");
        // `NO_COLOR` forces the fade off whatever the config says, and with it
        // the shadow: removed so the child's screen does not depend on the
        // environment whoever ran `cargo test` happened to have.
        cmd.env_remove("NO_COLOR");
        if shadow {
            cmd.env(CHILD_PALETTE, "1");
        }
        if keep {
            cmd.env(CHILD_KEEP, "1");
        }
        if !announce.is_empty() {
            cmd.env(CHILD_ANNOUNCE, announce);
        }
        let child = pair.slave.spawn_command(cmd).expect("spawn relay child");
        // Or the master would never see EOF once the child exits.
        drop(pair.slave);

        let writer = pair.master.take_writer().expect("writer");
        let mut reader = pair.master.try_clone_reader().expect("reader");
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
            writer,
            output: Vec::new(),
            answered: 0,
            incoming,
            _master: pair.master,
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
            self.type_bytes(reply);
        }
        self.answered = self.output.len();
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write to pty");
        self.writer.flush().expect("flush pty");
    }

    /// The child's exit code, if it exits within `within`.
    fn exit_code(&mut self, within: Duration) -> Option<u32> {
        let deadline = Instant::now() + within;
        while self.exited.is_none() && Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exited = Some(status.exit_code());
                break;
            }
            // Keep answering: the client's own exit waits on a DA1 reply.
            self.pump_until(Duration::from_millis(50), |_| false);
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
            let _ = self.child.kill();
            let _ = self.child.wait();
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

/// The screen the child's output describes, as a terminal would show it.
///
/// The byte stream cannot answer questions about cells. nvmux's writes, the
/// client's paints and the fade's frames all cross the same wire in whatever
/// order the relay managed, and "the row is gone" or "the text is dimmed" is a
/// statement about cells rather than about bytes — so the tests keep a parser,
/// as `src/shadow.rs` keeps one, and read the screen off it.
fn screen_of(out: &[u8]) -> vt100::Parser {
    let mut parser = vt100::Parser::new(ROWS, COLS, 0);
    parser.process(out);
    parser
}

/// One row of a screen, trimmed.
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

/// One row of the screen the child's output describes.
fn row_on_screen(out: &[u8], row: u16) -> String {
    row_of(screen_of(out).screen(), row)
}

/// The colour a cell's text is drawn in, as the child's palette resolves it:
/// what a fade starts from, and what a dimmed cell is measured against.
fn colour_of(screen: &vt100::Screen, (row, col): (u16, u16)) -> nvmux::palette::Rgb {
    let palette = child_palette();
    match screen.cell(row, col).map(vt100::Cell::fgcolor) {
        None | Some(vt100::Color::Default) => palette.fg,
        Some(vt100::Color::Idx(n)) => palette.index(n),
        Some(vt100::Color::Rgb(r, g, b)) => nvmux::palette::Rgb(r, g, b),
    }
}

/// The colour of the cell at `at` on the screen the child's output describes.
fn colour_on_screen(out: &[u8], at: (u16, u16)) -> nvmux::palette::Rgb {
    colour_of(screen_of(out).screen(), at)
}

/// What the session under test has on its first few lines: text of the
/// editor's own between the prefix's two rows, to watch the veil on.
const TEXT: &str = "text the prefix veils";

/// A cell of [`TEXT`]: the first letter of the third row, clear of both of the
/// prefix's rows.
const TEXT_AT: (u16, u16) = (2, 0);

/// A session with [`TEXT`] on its first lines: the socket a client attaches to
/// it through, and its id.
fn session_with_text(scratch: &Scratch, tag: &str) -> (std::path::PathBuf, String) {
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let mut rpc = nvmux::rpc::Client::connect(&sock, Duration::from_secs(3)).expect("connect");
    // No swap file: these run at once, each with an unnamed buffer, and would
    // otherwise race for the same one.
    rpc.command(&format!(
        "setlocal noswapfile | call setline(1, repeat(['{TEXT}'], 5))"
    ))
    .expect("setline");
    (sock, session.id)
}

/// A relay child attached to a session with [`TEXT`] on it, whose client has
/// painted it. `shadow` is [`Terminal::spawn_with`]'s.
fn attached_with_text(scratch: &Scratch, tag: &str, shadow: bool) -> Terminal {
    let (sock, id) = session_with_text(scratch, tag);
    let mut term = Terminal::spawn_with(&sock, &id, Protocol::Kitty, shadow);
    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            row_on_screen(out, TEXT_AT.0) == TEXT
        }),
        "the session never painted its text; got: {}",
        term.since(0)
    );
    // With the fade on, the first paint is held back and dissolved in, and the
    // text is in the dissolve's colours until the held paint is written after
    // it. Let all of that be over before anything is measured.
    term.pump_until(Duration::from_millis(500), |_| false);
    term
}

/// The prefix puts its rows up over a live editor — the sessions along the
/// first row, the keys along the last — and the next key takes them off again
/// with the editor's own rows underneath: the two halves of the feature,
/// neither of which is visible from a unit test.
///
/// `<prefix> <prefix>` is the dismissal to test on, because it is one of the
/// three that leave the relay *running*: the picker, a detach and a switch all
/// end it, and there the screen is dissolved or cleared on the way out and
/// would clear the rows whether or not anything had taken them off. Here
/// nothing else touches them, so if they are still up, they stay.
///
/// `shadow` picks which way the screen is put back, and both have to work.
/// With a shadow, the session is veiled behind the rows — its text dimmed to
/// half — and handed back painted from the shadow's copy, and the server's
/// repaint on top of it. With the fade off, and so no shadow, nothing is dimmed
/// and the rows are blanked, and only the server's repaint puts the rows back.
fn the_rows_go_up_and_come_down(tag: &str, shadow: bool) {
    require_nvim!();
    let scratch = Scratch::new(tag);
    let mut term = attached_with_text(&scratch, tag, shadow);
    let top = row_on_screen(&term.output, 0);
    let bottom = row_on_screen(&term.output, ROWS - 1);
    let drawn = colour_on_screen(&term.output, TEXT_AT);
    let bg = child_palette().bg;
    // Dimmed to half with a veil, and left alone without one.
    let behind = if shadow { drawn.lerp(bg, 0.5) } else { drawn };

    // The prefix alone. The child's `timeout_ms` is ten seconds, so the rows
    // have as long as they need to find a lull, and a veil to finish rising.
    term.type_bytes(&[nvmux::keys::PREFIX]);
    let mark = term.output.len();
    assert!(
        term.pump_until(Duration::from_secs(10), |out| {
            row_on_screen(out, ROWS - 1).contains("␣ picker")
                && row_on_screen(out, 0) == SESSIONS_ROW
                && colour_on_screen(out, TEXT_AT) == behind
        }),
        "the prefix did not put its rows up over the session as it should: the first row \
         says {:?}, the last {:?}, and the text is in {:?} where {behind:?} was wanted; the \
         child wrote: {}",
        row_on_screen(&term.output, 0),
        row_on_screen(&term.output, ROWS - 1),
        colour_on_screen(&term.output, TEXT_AT),
        term.since(mark)
    );
    assert!(
        row_on_screen(&term.output, ROWS - 1).contains("d detach"),
        "the row is not the key list: {:?}",
        row_on_screen(&term.output, ROWS - 1)
    );
    assert_eq!(
        row_on_screen(&term.output, TEXT_AT.0),
        TEXT,
        "the session's own text is not all still there behind the rows"
    );

    // A second prefix is a literal: Neovim gets the byte and the relay carries
    // on, so the rows have to take themselves off.
    let mark = term.output.len();
    term.type_bytes(&[nvmux::keys::PREFIX]);
    assert!(
        term.pump_until(Duration::from_secs(10), |out| {
            row_on_screen(out, 0) == top
                && row_on_screen(out, ROWS - 1) == bottom
                && colour_on_screen(out, TEXT_AT) == drawn
        }),
        "the session's own screen did not come back after the prefix resolved: the first \
         row says {:?}, the last {:?}, and the text is in {:?}; the child wrote: {}",
        row_on_screen(&term.output, 0),
        row_on_screen(&term.output, ROWS - 1),
        colour_on_screen(&term.output, TEXT_AT),
        term.since(mark)
    );

    // And the relay really is still running: the prefix still detaches.
    term.type_bytes(&[nvmux::keys::PREFIX]);
    term.pump_until(Duration::from_millis(150), |_| false);
    term.type_bytes(b"d");
    assert_eq!(
        term.exit_code(Duration::from_secs(10)),
        Some(0),
        "the relay did not survive the rows; the child wrote: {}",
        term.since(mark)
    );
}

/// The ordinary path: a shadow of the session's screen, so the session is
/// veiled behind the rows and painted back from the shadow's copy.
#[test]
fn a_prefix_veils_the_session_behind_its_rows_and_the_next_key_hands_it_back() {
    the_rows_go_up_and_come_down("hint", true);
}

/// And with the fade off there is no shadow, so nothing is veiled, the rows
/// are blanked, and the relay asks the server for what was under them — the
/// one path on which the screen comes back only from the server, and the one a
/// `NO_COLOR` user is on.
#[test]
fn without_a_shadow_the_rows_come_down_through_the_server() {
    the_rows_go_up_and_come_down("hint-no-shadow", false);
}

/// A command that leaves the session from under the veil carries the fade out
/// on from the veil. Read frame by frame from the keypress: at no frame is the
/// session's text brighter than the veil left it — the fade does not bring the
/// session back to full colour only to dissolve it again — and the rows stay
/// on it all the way down, which ends in the background — with nothing of the
/// session showing through their blanks on the way: the editor's own row
/// threaded through the names of sessions reads as corruption, not as a fade.
#[test]
fn leaving_from_under_the_veil_carries_the_fade_on_from_it() {
    require_nvim!();
    let scratch = Scratch::new("veil-leave");
    let mut term = attached_with_text(&scratch, "veil-leave", true);
    let bg = child_palette().bg;
    let veiled = colour_on_screen(&term.output, TEXT_AT).lerp(bg, 0.5);

    term.type_bytes(&[nvmux::keys::PREFIX]);
    assert!(
        term.pump_until(Duration::from_secs(10), |out| {
            colour_on_screen(out, TEXT_AT) == veiled && row_on_screen(out, 0) == SESSIONS_ROW
        }),
        "the veil never rose: the text is in {:?}; the child wrote: {}",
        colour_on_screen(&term.output, TEXT_AT),
        term.since(0)
    );
    // The text's row is not the veil's last: the rest of that frame may still
    // be on its way. Whatever arrives from here on has to be the fade out.
    term.pump_until(Duration::from_millis(300), |_| false);

    // `<prefix> Space`, the picker — which a child that does not keep its
    // client answers by ending the relay, fade and all, and exiting with 3.
    let mark = term.output.len();
    term.type_bytes(b" ");
    assert_eq!(
        term.exit_code(Duration::from_secs(10)),
        Some(3),
        "the relay did not leave for the picker; the child wrote: {}",
        term.since(mark)
    );

    // The sessions row, as the prefix centres it on the first row.
    let left = (usize::from(COLS) - SESSIONS_ROW.chars().count()) / 2;
    let glyph_at = |col: usize| {
        col.checked_sub(left)
            .and_then(|i| SESSIONS_ROW.chars().nth(i))
            .filter(|c| *c != ' ')
    };
    let brighter =
        |a: nvmux::palette::Rgb, b: nvmux::palette::Rgb| a.0 > b.0 || a.1 > b.1 || a.2 > b.2;

    let mut parser = screen_of(&term.output[..mark]);
    let mut rest = &term.output[mark..];
    let mut frames = Vec::new();
    while let Some(end) = find(rest, nvmux::fade::SYNC_END) {
        let (frame, after) = rest.split_at(end + nvmux::fade::SYNC_END.len());
        parser.process(frame);
        rest = after;
        let n = frames.len() + 1;
        let screen = parser.screen();
        let text = colour_of(screen, TEXT_AT);
        assert!(
            !brighter(text, veiled),
            "frame {n} of the fade out brought the session back up to {text:?}, past the \
             veil's {veiled:?}"
        );
        for col in 0..COLS {
            let cell = screen.cell(0, col).expect("a cell");
            match glyph_at(usize::from(col)) {
                Some(glyph) => assert_eq!(
                    cell.contents(),
                    glyph.to_string(),
                    "the sessions row left the veil before the fade did, at frame {n}"
                ),
                None => assert!(
                    !cell.has_contents() || colour_of(screen, (0, col)) == bg,
                    "frame {n}: the session shows through the sessions row at column {col}: \
                     {:?}",
                    cell.contents()
                ),
            }
        }
        frames.push(text);
    }
    assert!(
        frames.len() >= 2,
        "the session was not faded out, it was cut: {frames:?}; the child wrote: {}",
        term.since(mark)
    );
    assert_eq!(
        frames.last(),
        Some(&bg),
        "the fade out did not end in the background: {frames:?}"
    );
}

/// The attach notice's box can be up when the prefix is pressed — a switch,
/// and the prefix at once. The veil repaints the whole screen from the shadow,
/// which has never held the box, so the box goes under it; the relay lets the
/// notice go rather than have it paint itself over the veil; and the screen
/// the veil hands back when it lifts has no box in it either — nor does
/// anything put one back afterwards.
#[test]
fn a_veil_takes_the_attach_notice_with_it() {
    require_nvim!();
    let scratch = Scratch::new("veil-notice");
    let (sock, id) = session_with_text(&scratch, "veil-notice");
    let mut term = Terminal::spawn_announcing(&sock, &id, "web");
    let boxed = |out: &[u8]| screen_of(out).screen().contents().contains('╭');
    assert!(
        term.pump_until(Duration::from_secs(15), boxed),
        "the notice never went up; the child wrote: {}",
        term.since(0)
    );
    // The notice goes up over a screen the client has painted, so the text is
    // in its own colours by now.
    let drawn = colour_on_screen(&term.output, TEXT_AT);
    let veiled = drawn.lerp(child_palette().bg, 0.5);

    term.type_bytes(&[nvmux::keys::PREFIX]);
    let mark = term.output.len();
    assert!(
        term.pump_until(Duration::from_secs(10), |out| {
            row_on_screen(out, 0) == SESSIONS_ROW
                && colour_on_screen(out, TEXT_AT) == veiled
                && !boxed(out)
        }),
        "the veil did not take the box: it is {}, the text is in {:?}; the child wrote: {}",
        if boxed(&term.output) {
            "still up"
        } else {
            "gone"
        },
        colour_on_screen(&term.output, TEXT_AT),
        term.since(mark)
    );

    // A literal prefix, and the veil lifts onto a screen with no box on it.
    let mark = term.output.len();
    term.type_bytes(&[nvmux::keys::PREFIX]);
    assert!(
        term.pump_until(Duration::from_secs(10), |out| {
            row_on_screen(out, 0) == TEXT && colour_on_screen(out, TEXT_AT) == drawn
        }),
        "the session did not come back: the first row says {:?}, the text is in {:?}; the \
         child wrote: {}",
        row_on_screen(&term.output, 0),
        colour_on_screen(&term.output, TEXT_AT),
        term.since(mark)
    );
    // Longer than the notice would have lived: nothing brings the box back.
    term.pump_until(Duration::from_millis(1500), |_| false);
    assert!(
        !boxed(&term.output),
        "the box came back after the veil lifted; the child wrote: {}",
        term.since(mark)
    );

    term.type_bytes(&[nvmux::keys::PREFIX]);
    term.pump_until(Duration::from_millis(150), |_| false);
    term.type_bytes(b"d");
    assert_eq!(
        term.exit_code(Duration::from_secs(10)),
        Some(0),
        "the relay did not survive the notice and the veil; the child wrote: {}",
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
fn a_kept_clients_screen_comes_back(tag: &str, in_band: bool) {
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

    let mut term = Terminal::spawn_keeping(&sock, &session.id, in_band);
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
        term._master
            .resize(PtySize {
                rows: ROWS,
                cols: narrower,
                pixel_width: 0,
                pixel_height: 0,
            })
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
    a_kept_clients_screen_comes_back("kept-pty", false);
}

/// A client that takes its size in band, and ignores the signal (kitty,
/// ghostty, foot): told the new one with a report instead.
#[test]
fn a_kept_clients_screen_comes_back_on_an_in_band_terminal() {
    a_kept_clients_screen_comes_back("kept-in-band", true);
}
