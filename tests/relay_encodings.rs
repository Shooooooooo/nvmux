//! End-to-end: the prefix must reach the prefix machine however the terminal
//! spells it, through a real `nvim --remote-ui` client.
//!
//! The client asks the terminal for the kitty keyboard protocol when it
//! starts, or failing that for xterm's `modifyOtherKeys`, and nvmux relays
//! that negotiation untouched — so a terminal that grants the first (Windows
//! Terminal from 1.25, kitty, Ghostty, ...) then sends `Ctrl-t` as
//! `CSI 116 ; 5 u`, and one that grants the second (xterm, WezTerm) as
//! `CSI 27 ; 5 ; 116 ~`, instead of the byte 0x14. Until `keyseq`, either
//! went straight through to the editor: `<prefix> d` was a tag-stack pop
//! (`E73: Tag stack empty`) and a pending delete, and nobody could detach.
//!
//! The relay talks to its terminal on fds 0 and 1, so it has to run in a
//! process of its own. Each test spawns this very test binary again, on a pty,
//! to run [`relay_child`], and plays the terminal on the master side: it
//! answers the client's queries the way a terminal with the protocol under
//! test does, waits for the client to turn the protocol on, and types.
//! Skipped without a usable `nvim`, like the other suites.

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

    let attachment = nvmux::pty::spawn(&id, Path::new(&sock)).expect("attach");
    let code = match nvmux::pty::relay(attachment, 0) {
        Ok((nvmux::pty::Outcome::Detached, _)) => 0,
        Ok((other, _)) => {
            eprintln!("relay ended with {other:?}");
            3
        }
        Err(e) => {
            eprintln!("relay failed: {e}");
            4
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
    /// Re-run this test binary as `relay_child` on a fresh pty.
    fn spawn(sock: &Path, id: &str, protocol: Protocol) -> Self {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: 40,
                cols: 120,
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
        for (query, reply) in [(KITTY_QUERY, KITTY_REPLY), (DA1, DA1_REPLY)] {
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
    let session = t.create_session(&common::unique(tag)).expect("create");
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
        term.type_bytes(b"\x1b[116;5:3u");
    }
    // A human gap, well past the sequence wait.
    term.pump_until(Duration::from_millis(150), |_| false);
    term.type_bytes(b"d");

    let code = term.exit_code(Duration::from_secs(10));
    assert_eq!(
        code,
        Some(0),
        "expected a detach (exit 0) after {prefix:?} d; the child {}; output since the push: {}",
        match code {
            Some(c) => format!("exited with {c}"),
            None => "is still attached".to_string(),
        },
        term.since(mark)
    );
    assert!(
        find(&term.output[mark..], b"E73").is_none(),
        "the prefix reached the editor as Ctrl-t: {}",
        term.since(mark)
    );
}

/// The bug as reported: Windows Terminal 1.25 with the kitty keyboard
/// protocol, where the prefix arrives as `CSI 116 ; 5 u`.
#[test]
fn a_kitty_encoded_prefix_detaches_through_a_real_client() {
    detaches_with("kitty", Protocol::Kitty, b"\x1b[116;5u");
}

/// A terminal without the kitty protocol, where the client falls back to
/// `modifyOtherKeys` and the prefix arrives as `CSI 27 ; 5 ; 116 ~`.
#[test]
fn an_xterm_encoded_prefix_detaches_through_a_real_client() {
    detaches_with("xterm", Protocol::Xterm, b"\x1b[27;5;116~");
}

/// And the control byte still works, in a terminal that speaks the protocol
/// too: Neovim accepts both, and so must the machine.
#[test]
fn the_control_byte_still_detaches_through_a_real_client() {
    detaches_with("byte", Protocol::Kitty, b"\x14");
}

#[test]
fn the_push_is_matched_by_shape() {
    assert_eq!(kitty_push_flags(b"\x1b[?2004h\x1b[>1u\x1b[?1004h"), Some(1));
    assert_eq!(kitty_push_flags(b"\x1b[>3u"), Some(3));
    // The terminal's own reply, the xterm push, and a pop are not pushes.
    assert_eq!(kitty_push_flags(b"\x1b[?0u\x1b[>4;2m\x1b[<u"), None);
    assert_eq!(kitty_push_flags(b"\x1b[>"), None);
}
