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
//! Two tests here are not about a spelling at all. The hint bar the prefix puts
//! up ([`nvmux::hint`]) is written over a live editor's last row and taken off
//! it again, and neither half can be shown to work anywhere but here: it needs a
//! real client painting a real screen to cover, and the shadow's copy of that
//! screen to put back.
//!
//! And the picker, opened by `<prefix> Space` over a session the window was
//! resized under, has to land on a screen the size of the window. Whether it
//! does is up to the terminal as much as to nvmux — Windows Terminal sizes a
//! second alternate screen by its main screen, which a resize under the first
//! leaves as it was (see [`WindowsTerminal`]) — so this is the one place it can
//! be seen: a real client, the real relay, the real picker, and a terminal that
//! does what that one does.

#[macro_use]
mod common;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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
/// Set on a child that goes on from `<prefix> Space` to the picker rather than
/// ending there, to the runtime directory the picker lists its sessions from.
const CHILD_PICKER: &str = "NVMUX_TEST_RELAY_PICKER";

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
/// The cursor position report ratatui asks for as a screen opens, and an
/// answer. Where the cursor is does not matter to any test here; that the
/// question is answered does, or the screen gives up on its terminal.
const CPR: &[u8] = b"\x1b[6n";
const CPR_REPLY: &[u8] = b"\x1b[1;1R";
/// What the client sends to turn `modifyOtherKeys` on.
const XTERM_PUSH: &[u8] = b"\x1b[>4;2m";

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

    let picker = std::env::var_os(CHILD_PICKER).map(PathBuf::from);

    // An empty notice announces nothing: these tests are about the prefix
    // reaching the machine, and a box drawn over the screen would be noise in
    // the stream the parent is reading.
    let attachment = nvmux::pty::spawn(&id, Path::new(&sock), "").expect("attach");
    let code = match (nvmux::pty::relay(attachment, 0, &mut |_| {}), picker) {
        (Ok((nvmux::pty::Outcome::Detached, _)), _) => 0,
        (Ok((nvmux::pty::Outcome::ToPicker, Some(held))), Some(dir)) => {
            open_the_picker(held, &id, dir)
        }
        (Ok((other, _)), _) => {
            eprintln!("relay ended with {other:?}");
            3
        }
        (Err(e), _) => {
            eprintln!("relay failed: {e}");
            4
        }
    };
    std::process::exit(code);
}

/// The rest of a child told to go on to the picker: open it over the session
/// the relay left, the way `session_loop` does, and say through the exit code
/// how it ended. A quit is the one way out a test asks for.
fn open_the_picker(held: nvmux::pty::Attachment, id: &str, dir: PathBuf) -> i32 {
    let transport =
        nvmux::transport::local::LocalTransport::with_dir(dir).expect("build transport");
    let code = match nvmux::ui::run(&transport, None, Some(id), true) {
        Ok(nvmux::ui::Outcome::Quit) => 0,
        Ok(other) => {
            eprintln!("the picker ended with {other:?}");
            5
        }
        Err(e) => {
            eprintln!("the picker failed: {e}");
            6
        }
    };
    // Retired as `session_loop` retires a held client on a quit.
    held.terminate();
    code
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
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// The exit code, once the child has been seen to exit; it must not be
    /// signalled or waited for again after that.
    exited: Option<u32>,
    writer: Box<dyn Write + Send>,
    /// Everything the child has written, in order.
    output: Vec<u8>,
    /// How far `output` has been scanned for queries.
    answered: usize,
    /// Every [`Terminal::resize`], as how much of `output` had arrived when it
    /// happened and the rows and columns it left — so a replay can put each one
    /// back at its place in the stream.
    resizes: Vec<(usize, u16, u16)>,
    incoming: mpsc::Receiver<Vec<u8>>,
    // Held so the pty outlives the child, and resized through; dropped last.
    master: Box<dyn portable_pty::MasterPty>,
}

impl Terminal {
    /// Re-run this test binary as `relay_child` on a fresh pty, with no palette
    /// and so no fade — what every test about a spelling wants.
    fn spawn(sock: &Path, id: &str, protocol: Protocol) -> Self {
        Self::spawn_with(sock, id, protocol, false, None)
    }

    /// [`Terminal::spawn`], with `shadow` asking the child for the palette that
    /// gives it a shadow of the session's screen (see [`CHILD_PALETTE`]), and
    /// `picker` the runtime directory of a child that goes on to the picker
    /// (see [`CHILD_PICKER`]).
    fn spawn_with(
        sock: &Path,
        id: &str,
        protocol: Protocol,
        shadow: bool,
        picker: Option<&Path>,
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
        if let Some(dir) = picker {
            cmd.env(CHILD_PICKER, dir);
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
            child,
            exited: None,
            writer,
            output: Vec::new(),
            answered: 0,
            resizes: Vec::new(),
            incoming,
            master: pair.master,
        }
    }

    /// The window changing size under the child, as a user maximising it
    /// would: the pty is resized, and the child told so by the kernel.
    ///
    /// Where in the stream it happened is noted as whatever has arrived by
    /// now, so the caller should [`Terminal::settle`] first — a byte still in
    /// flight would otherwise be replayed as if written after it.
    fn resize(&mut self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize the pty");
        self.resizes.push((self.output.len(), rows, cols));
    }

    /// Relay output, answering queries, until the child has written nothing
    /// for `quiet` — or for at most `within`, as a bound on a child that never
    /// stops.
    fn settle(&mut self, quiet: Duration, within: Duration) {
        let deadline = Instant::now() + within;
        loop {
            let before = self.output.len();
            self.pump_until(quiet, |_| false);
            if self.output.len() == before || Instant::now() >= deadline {
                return;
            }
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
            (CPR, CPR_REPLY),
        ] {
            if query == KITTY_QUERY && self.protocol == Protocol::Xterm {
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

    let mut term = Terminal::spawn_with(&sock, &session.id, Protocol::Kitty, shadow, None);

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

/// The rows and columns the window grows to under the session, in the test
/// about the picker. Wide enough that the picker's hint row, centred in it,
/// runs one column past the edge of a screen still [`COLS`] wide — its `t` of
/// `q quit`, which is where it broke in the photographs the report came with.
const GROWN_ROWS: u16 = 50;
const GROWN_COLS: u16 = 173;

const ENTER_ALT: &[u8] = b"\x1b[?1049h";
const LEAVE_ALT: &[u8] = b"\x1b[?1049l";

/// The screen the child's output makes in Windows Terminal — as far as the
/// sizes of its two screens go, which is where it parts from `vt100`.
///
/// Two things, both read off its source (`TerminalCore`: `UserResize`,
/// `UseAlternateScreenBuffer`). A resize while the alternate screen is up
/// resizes only that one, and puts the main screen's off until the main screen
/// is shown again. And `?1049h` builds a *new* alternate screen whenever it
/// arrives, whether or not one is already up, at the size of the main screen.
///
/// Between them, a terminal resized under an alternate screen and then asked
/// for one again gets one the size the window used to be, while the window
/// goes on showing all of itself. Everything drawn
/// for the size the pty reports is cut down to the old one: rows past its last
/// are drawn on its last, and a row that runs past its last column wraps —
/// which on its last row scrolls the whole screen up a line.
struct WindowsTerminal {
    main: vt100::Parser,
    alt: Option<vt100::Parser>,
    /// The window's size: what the pty says, and what the main screen becomes
    /// when it is next shown.
    window: (u16, u16),
}

impl WindowsTerminal {
    /// Everything `term` has received, replayed from the first byte, with each
    /// of its resizes put back where it happened.
    fn replay(term: &Terminal) -> Self {
        let mut wt = Self {
            main: vt100::Parser::new(ROWS, COLS, 0),
            alt: None,
            window: (ROWS, COLS),
        };
        let mut from = 0;
        for &(at, rows, cols) in &term.resizes {
            wt.process(&term.output[from..at]);
            wt.resize(rows, cols);
            from = at;
        }
        wt.process(&term.output[from..]);
        wt
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.window = (rows, cols);
        match &mut self.alt {
            Some(alt) => alt.screen_mut().set_size(rows, cols),
            None => self.main.screen_mut().set_size(rows, cols),
        }
    }

    /// `bytes`, with the two switches taken out and done here rather than by
    /// `vt100`, whose own alternate screen is always its main screen's size.
    fn process(&mut self, mut bytes: &[u8]) {
        loop {
            let next = [ENTER_ALT, LEAVE_ALT]
                .into_iter()
                .filter_map(|switch| find(bytes, switch).map(|at| (at, switch)))
                .min_by_key(|&(at, _)| at);
            let Some((at, switch)) = next else {
                self.shown().process(bytes);
                return;
            };
            self.shown().process(&bytes[..at]);
            if switch == ENTER_ALT {
                let (rows, cols) = self.main.screen().size();
                self.alt = Some(vt100::Parser::new(rows, cols, 0));
            } else {
                self.alt = None;
                let (rows, cols) = self.window;
                self.main.screen_mut().set_size(rows, cols);
            }
            bytes = &bytes[at + switch.len()..];
        }
    }

    fn shown(&mut self) -> &mut vt100::Parser {
        self.alt.as_mut().unwrap_or(&mut self.main)
    }

    /// Every row of the window as it shows: the screen that is up, and nothing
    /// past its edges where it is smaller than the window.
    fn rows(&self) -> Vec<String> {
        let screen = self.alt.as_ref().unwrap_or(&self.main).screen();
        (0..self.window.0)
            .map(|row| {
                (0..self.window.1)
                    .map(|col| match screen.cell(row, col) {
                        Some(cell) if cell.has_contents() => cell.contents(),
                        _ => " ",
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }
}

/// The report: a window resized while a session was up — maximised, in the
/// photographs — and then `<prefix> Space`, and the picker came up in the top
/// left of the window at the size it used to be, a copy of its hint row one
/// line higher for every frame of its fade in, and the `t` of `q quit` wrapped
/// onto column 0 of the line below each.
///
/// The picker has to fill the window it is drawn for: its hint row on the
/// window's last row, once, and the session named once.
#[test]
fn the_picker_fills_a_window_resized_under_the_session_it_opens_over() {
    require_nvim!();
    let scratch = Scratch::new("grown");
    let t = scratch.transport();
    let name = common::unique("grown");
    let session = t
        .create_session(&name, &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");

    // With the palette, so the fade is on: every frame of the picker's fade in
    // is drawn in full, which is what turned one misplaced row into a smear.
    let mut term = Terminal::spawn_with(
        &sock,
        &session.id,
        Protocol::Kitty,
        true,
        Some(scratch.0.as_path()),
    );
    assert!(
        term.pump_until(Duration::from_secs(15), |out| {
            kitty_push_flags(out).is_some()
        }),
        "the client never started; got: {}",
        term.since(0)
    );
    let quiet = Duration::from_millis(300);
    term.settle(quiet, Duration::from_secs(5));

    // The window grows under the session, and the client repaints to fit it.
    term.resize(GROWN_ROWS, GROWN_COLS);
    term.settle(quiet, Duration::from_secs(5));

    let mark = term.output.len();
    term.type_bytes(&[nvmux::keys::PREFIX]);
    term.pump_until(Duration::from_millis(150), |_| false);
    term.type_bytes(b" ");
    // `quit` rather than `q quit`: the picker's hint row is drawn a word at a
    // time, with a colour or a cursor move between each, and only its own row
    // says `quit` — the prefix's bar says `detach`.
    assert!(
        term.pump_until(Duration::from_secs(10), |out| {
            find(&out[mark..], b"quit").is_some()
        }),
        "no picker after <prefix> Space; the child wrote: {}",
        term.since(mark)
    );
    // The rest of the fade in, which is a tenth of a second at the defaults:
    // not a settle, since an idle picker never goes quiet — it hides the
    // cursor again at every tick. Its first frame is already enough to be
    // wrong in; the rest is what makes a wrong one look like the report.
    term.pump_until(Duration::from_millis(500), |_| false);

    let rows = WindowsTerminal::replay(&term).rows();
    let screen = rows
        .iter()
        .enumerate()
        .map(|(i, row)| format!("{i:>3}|{row}"))
        .collect::<Vec<_>>()
        .join("\n");
    let hints: Vec<usize> = (0..rows.len())
        .filter(|&i| rows[i].contains("↑↓ move"))
        .collect();
    assert_eq!(
        hints,
        [usize::from(GROWN_ROWS) - 1],
        "the picker's hint row belongs on the window's last row, once:\n{screen}"
    );
    assert!(
        rows[usize::from(GROWN_ROWS) - 1].ends_with("q quit"),
        "the hint row lost its end:\n{screen}"
    );
    let named = rows.iter().filter(|row| row.contains(&name)).count();
    assert_eq!(named, 1, "the session is listed once:\n{screen}");

    term.type_bytes(b"q");
    assert_eq!(
        term.exit_code(Duration::from_secs(10)),
        Some(0),
        "the picker did not quit; the child wrote: {}",
        term.since(mark)
    );
}
