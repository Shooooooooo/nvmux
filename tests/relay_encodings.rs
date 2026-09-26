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
//! screen to put back.
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
/// what the hint bar puts the row it covered back from. A shadow needs a palette
/// and the fade on, and nothing here queries a real terminal, so the colours are
/// handed to the child rather than asked for.
const CHILD_PALETTE: &str = "NVMUX_TEST_RELAY_PALETTE";
/// Set on a child that keeps its client (`[client] per_session`): where
/// `<prefix> Space` would open the picker it parks the client instead, for
/// [`PARKED_FOR`], and then takes it back and relays it again.
const CHILD_KEEP: &str = "NVMUX_TEST_RELAY_KEEP";

/// How long a keeping child leaves its client parked: long enough for the
/// parent to have made the server busy before the client comes back.
const PARKED_FOR: Duration = Duration::from_secs(1);

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
    let keep = std::env::var_os(CHILD_KEEP).is_some();
    settings.client.per_session = keep;
    nvmux::config::init(settings);

    // Without this the fade is off — `palette::get` is `None`, so `fade::enabled`
    // is false — and there is no shadow of the session's screen. The tests about
    // spellings want it that way: no fade means no held first paint and no
    // frames, so what reaches the parent is the client's own bytes and nothing
    // else. The bar test needs the shadow, and says so.
    if std::env::var_os(CHILD_PALETTE).is_some() {
        nvmux::palette::init(Some(nvmux::palette::Palette {
            fg: nvmux::palette::Rgb(200, 200, 200),
            bg: nvmux::palette::Rgb(0, 0, 0),
            ansi: [nvmux::palette::Rgb(0, 0, 0); 16],
        }));
    }

    // An empty notice announces nothing: these tests are about the prefix
    // reaching the machine, and a box drawn over the screen would be noise in
    // the stream the parent is reading.
    let mut pool = nvmux::pool::Pool::configured();
    let mut attachment = nvmux::pty::spawn(&id, Path::new(&sock), "").expect("attach");
    assert_eq!(
        attachment.is_kept(),
        keep,
        "[client] per_session = {keep} was not honoured"
    );
    let code = loop {
        match nvmux::pty::relay(attachment, 0, &mut |_| {}) {
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
    /// and so no fade, and a client that is not kept — what every test about a
    /// spelling wants.
    fn spawn(sock: &Path, id: &str, protocol: Protocol) -> Self {
        Self::spawn_with(sock, id, protocol, false)
    }

    /// [`Terminal::spawn`], with `shadow` asking the child for the palette that
    /// gives it a shadow of the session's screen (see [`CHILD_PALETTE`]).
    fn spawn_with(sock: &Path, id: &str, protocol: Protocol, shadow: bool) -> Self {
        Self::spawn_child(sock, id, protocol, shadow, false, false)
    }

    /// A child that keeps its client (see [`CHILD_KEEP`]), on a terminal that
    /// reports its size in band if `in_band`, and with the fade on if `fade`.
    fn spawn_keeping(sock: &Path, id: &str, in_band: bool, fade: bool) -> Self {
        Self::spawn_child(sock, id, Protocol::Kitty, fade, true, in_band)
    }

    fn spawn_child(
        sock: &Path,
        id: &str,
        protocol: Protocol,
        shadow: bool,
        keep: bool,
        in_band: bool,
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
    let screen = parser.screen();
    (0..COLS)
        .map(|col| match screen.cell(row, col) {
            Some(cell) if cell.has_contents() => cell.contents(),
            _ => " ",
        })
        .collect::<String>()
        .trim()
        .to_string()
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

/// And with the fade off there is no shadow, so the bar blanks its row and the
/// relay asks the server for what was under it — the one path on which taking
/// the bar down costs a round trip, and the one a `NO_COLOR` user is on.
#[test]
fn the_hint_row_comes_down_through_the_server_without_a_shadow() {
    the_hint_row_goes_up_and_comes_down("hint-no-shadow", false);
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
