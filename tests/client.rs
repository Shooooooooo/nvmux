//! End-to-end: nvmux's own client (`[client] ui = "nvmux"`, see
//! `nvmux::client`) drawing a real session on a real pty.
//!
//! The client runs the way nvmux runs it — `nvmux --client <sock>`, the
//! binary these tests are built with, on a pty of its own — and the test plays
//! the terminal: it answers what the client asks as it starts, keeps the screen
//! the client draws in a terminal emulator of its own, as `src/shadow.rs`
//! does, and types. Skipped without a usable `nvim`, like the other suites.
//!
//! What needs a real server most is a client coming after another. A UI
//! attaching with `ext_multigrid` to a session another UI has drawn is sent
//! none of its windows — not where they are, nor anything on them — and the
//! client has to have Neovim draw them again; and while a UI that keeps every
//! window on grid 1 is attached, the client has to do without windows of its
//! own. Both are questions about what Neovim does, which only Neovim answers.

#[macro_use]
mod common;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use common::Scratch;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use nvmux::fade::SYNC_END;
use nvmux::transport::Transport;
use pty_process::blocking::{Command, Pty};
use rmpv::Value;

const ROWS: u16 = 24;
const COLS: u16 = 80;

/// The client's DA1, which ends what it asks as it starts, and a VT220's
/// answer; the kitty keyboard query, and "supported, no flags set".
const DA1: &[u8] = b"\x1b[c";
const DA1_REPLY: &[u8] = b"\x1b[?62;22c";
/// The answer of a terminal that takes OSC 52, which says so with a 52 among
/// its attributes, as kitty and others do.
const DA1_REPLY_OSC52: &[u8] = b"\x1b[?62;22;52c";
const KITTY_QUERY: &[u8] = b"\x1b[?u";
const KITTY_REPLY: &[u8] = b"\x1b[?0u";

/// The terminal a client draws on.
struct Terminal {
    child: std::process::Child,
    /// The exit code, once the child has been seen to exit; it must not be
    /// signalled or waited for again after that.
    exited: Option<u32>,
    /// The client has been hung up, and what it asks from then on goes
    /// unanswered: it does not wait for an answer on its way out, and one
    /// written once it has gone fails on macOS. Neovim asks the terminal
    /// something whenever a UI attaches, a refresh's among them, so a
    /// question can come as the client goes.
    hung_up: bool,
    /// The pty's master, which what is typed goes in through. Held so the pty
    /// outlives the child, which `Drop` reaps before the fields go.
    pty: Pty,
    /// Everything the client has written, in order, and the screen it makes.
    output: Vec<u8>,
    parser: vt100::Parser,
    /// How far into `output` the parser has had: all of it but the start of
    /// a character the last read ended part way through (see [`whole`]).
    fed: usize,
    /// How far `output` has been scanned for queries.
    answered: usize,
    /// What this terminal answers a DA1 with.
    da1: &'static [u8],
    /// Its size: columns, rows.
    size: (u16, u16),
    incoming: mpsc::Receiver<Vec<u8>>,
}

impl Terminal {
    /// `nvmux --client <sock>` on a fresh pty, with the config at `config`.
    fn spawn(sock: &Path, config: &Path) -> Self {
        Self::spawn_answering(sock, config, DA1_REPLY)
    }

    /// [`Terminal::spawn`], on a terminal that answers DA1 with `da1`.
    fn spawn_answering(sock: &Path, config: &Path, da1: &'static [u8]) -> Self {
        Self::spawn_sized(sock, config, da1, (COLS, ROWS))
    }

    /// [`Terminal::spawn_answering`], on a terminal `size` big: columns,
    /// rows.
    fn spawn_sized(sock: &Path, config: &Path, da1: &'static [u8], size: (u16, u16)) -> Self {
        Self::start(sock, config, None, da1, size)
    }

    /// [`Terminal::spawn`], told what nvmux tells a client it starts, which
    /// it goes by rather than the config: the settings to draw with, and
    /// whether the session is on another machine.
    fn spawn_told(sock: &Path, config: &Path, told: &nvmux::config::Drawing) -> Self {
        Self::start(sock, config, Some(told), DA1_REPLY, (COLS, ROWS))
    }

    fn start(
        sock: &Path,
        config: &Path,
        told: Option<&nvmux::config::Drawing>,
        da1: &'static [u8],
        size: (u16, u16),
    ) -> Self {
        let (pty, pts) = pty_process::blocking::open().expect("open a pty");
        pty.resize(pty_process::Size::new(size.1, size.0))
            .expect("size the pty");
        let cmd = Command::new(env!("CARGO_BIN_EXE_nvmux"))
            .arg("--client")
            .arg(sock)
            .env("NVMUX_CONFIG", config)
            .env("TERM", "xterm-256color")
            // The colours a parent nvmux would hand down: none, so the client
            // falls back on its own, whatever the environment running the
            // tests.
            .env_remove(nvmux::client::PALETTE_ENV);
        let cmd = match told {
            Some(drawing) => cmd.env(nvmux::client::SETTINGS_ENV, drawing.to_env()),
            None => cmd.env_remove(nvmux::client::SETTINGS_ENV),
        };
        // Takes the slave, or the master would never see EOF once the child
        // exits.
        let child = cmd.spawn(pts).expect("spawn the client");

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
            child,
            exited: None,
            hung_up: false,
            pty,
            output: Vec::new(),
            parser: vt100::Parser::new(size.1, size.0, 0),
            fed: 0,
            answered: 0,
            da1,
            size,
            incoming,
        }
    }

    /// Take what the client draws for up to `within`, answering its questions
    /// as a terminal would, until `done` holds of the screen. Says whether it
    /// did.
    fn pump_until(&mut self, within: Duration, done: impl Fn(&str) -> bool) -> bool {
        self.pump(within, |t| done(&t.text()))
    }

    /// [`Terminal::pump_until`], until the client has written `needle`.
    fn pump_until_written(&mut self, within: Duration, needle: &[u8]) -> bool {
        self.pump(within, |t| find(&t.output, needle).is_some())
    }

    fn pump(&mut self, within: Duration, done: impl Fn(&Self) -> bool) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if done(self) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            match self.incoming.recv_timeout(left) {
                Ok(bytes) => {
                    self.output.extend_from_slice(&bytes);
                    self.feed();
                    self.answer();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return done(self),
            }
        }
    }

    /// Give the parser what has come that it has not had, as far as it is
    /// whole characters.
    fn feed(&mut self) {
        let end = self.fed + whole(&self.output[self.fed..]);
        self.parser.process(&self.output[self.fed..end]);
        self.fed = end;
    }

    /// Take what the client draws for up to `within`, answering its questions
    /// as a terminal would, and hand `each` the screen as every frame leaves
    /// it — not as a read happens to, part way through one — until it says
    /// that is enough. Says whether it did.
    fn frames(&mut self, within: Duration, mut each: impl FnMut(&vt100::Screen) -> bool) -> bool {
        let deadline = Instant::now() + within;
        let enough = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break false;
            }
            match self.incoming.recv_timeout(left) {
                Ok(bytes) => {
                    self.output.extend_from_slice(&bytes);
                    self.answer();
                    let mut enough = false;
                    // Up to the end of each frame, which is never part way
                    // through a character.
                    while let Some(i) = find(&self.output[self.fed..], SYNC_END) {
                        let end = self.fed + i + SYNC_END.len();
                        self.parser.process(&self.output[self.fed..end]);
                        self.fed = end;
                        enough |= each(self.parser.screen());
                    }
                    if enough {
                        break true;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break false,
            }
        };
        self.feed();
        enough
    }

    /// Answer every query not yet answered, in the order they were asked. A
    /// query may straddle two reads, so the scan starts a little before where
    /// it left off and counts only matches that end past it.
    fn answer(&mut self) {
        if self.hung_up {
            self.answered = self.output.len();
            return;
        }
        let from = self.answered.saturating_sub(8);
        let mut replies = Vec::new();
        for (query, reply) in [(KITTY_QUERY, KITTY_REPLY), (DA1, self.da1)] {
            let mut at = from;
            while let Some(i) = find(&self.output[at..], query).map(|i| at + i) {
                if i + query.len() > self.answered {
                    replies.push((i, reply));
                }
                at = i + query.len();
            }
        }
        replies.sort_by_key(|&(i, _)| i);
        for (_, reply) in replies {
            self.type_bytes(reply);
        }
        self.answered = self.output.len();
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        self.pty.write_all(bytes).expect("write to the pty");
        self.pty.flush().expect("flush the pty");
    }

    /// The screen as text, a row to a line, trailing blanks trimmed.
    fn text(&self) -> String {
        let screen = self.parser.screen();
        (0..self.size.1)
            .map(|row| {
                let line: String = (0..self.size.0)
                    .map(|col| match screen.cell(row, col) {
                        Some(cell) if cell.has_contents() => cell.contents(),
                        _ => " ",
                    })
                    .collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Hang the client up as nvmux retires one, and say how it exited.
    fn hang_up(&mut self) -> Option<u32> {
        self.signal(Signal::SIGHUP)
    }

    /// Kill the client outright, as nvmux kills one still there two seconds
    /// after it was hung up, or as a crash ends one: it does nothing more on
    /// its way out. Says whether it is gone.
    fn kill(&mut self) -> bool {
        self.signal(Signal::SIGKILL).is_some()
    }

    /// Send the client `signal`, and say how it exited.
    fn signal(&mut self, signal: Signal) -> Option<u32> {
        let pid = self.child.id();
        self.hung_up = true;
        nix::sys::signal::kill(Pid::from_raw(pid as i32), signal).expect("signal the client");
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.exited.is_none() && Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exited = Some(common::exit_code(status));
                break;
            }
            self.pump_until(Duration::from_millis(50), |_| false);
        }
        // What it wrote on the way out.
        self.pump_until(Duration::from_millis(200), |_| false);
        self.exited
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

/// How much of `bytes` is whole characters: all of it but the start of a
/// character it ends part way through, which waits for the rest. `vte`, under
/// `vt100`, finishes such a character with up to three bytes of what comes
/// next, and should that run on into the start of another, drops whatever
/// came between the two: a read that ends inside the `é` of `é ─`, as reads
/// of a pty on macOS do, loses the space and draws the rest of the row a cell
/// to the left of where the client put it.
fn whole(bytes: &[u8]) -> usize {
    let n = bytes.len();
    // The last character's first byte, at most four back.
    for back in 1..=n.min(4) {
        let b = bytes[n - back];
        if b & 0xc0 == 0x80 {
            continue;
        }
        let len = match b {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 1,
        };
        return if back < len { n - back } else { n };
    }
    n
}

/// A session of its own for a test, a connection to it, and a config file
/// with nothing in it — the defaults, whatever the machine's own config says.
fn session(
    scratch: &Scratch,
    tag: &str,
) -> (
    PathBuf,
    nvmux::rpc::Client<std::os::unix::net::UnixStream>,
    PathBuf,
) {
    let t = scratch.transport();
    let session = t
        .create_session(&common::unique(tag), &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("socket path");
    let rpc = nvmux::rpc::Client::connect(&sock, Duration::from_secs(5)).expect("connect");
    let config = scratch.0.join("config.toml");
    std::fs::write(&config, "").expect("write the config");
    (sock, rpc, config)
}

/// The `ext_multigrid` of every UI attached, in Neovim's order.
fn multigrid_uis(rpc: &mut nvmux::rpc::Client<std::os::unix::net::UnixStream>) -> Vec<bool> {
    let v = rpc
        .eval("map(nvim_list_uis(), 'get(v:val, \"ext_multigrid\", v:false)')")
        .expect("list the UIs");
    v.as_array()
        .map(|a| a.iter().map(|b| b.as_bool() == Some(true)).collect())
        .unwrap_or_default()
}

/// A client draws the session and types into it, and hung up it hands the
/// terminal back.
#[test]
fn the_client_draws_a_session_and_types_into_it() {
    require_nvim!();
    let scratch = Scratch::new("client-draws");
    let (sock, mut rpc, config) = session(&scratch, "client-draws");
    let marker = "drawn by nvmux's own client";
    // No swap file: tests run at once, each with an unnamed buffer.
    rpc.command(&format!(
        "setlocal noswapfile | call setline(1, \"{marker}\")"
    ))
    .expect("setline");

    let mut term = Terminal::spawn(&sock, &config);
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.starts_with(marker)),
        "the session was never drawn:\n{}",
        term.text()
    );
    assert_eq!(multigrid_uis(&mut rpc), vec![true], "windows of their own");

    term.type_bytes(b"A, and typed in\x1b");
    let typed = format!("{marker}, and typed in");
    assert!(
        common::wait_until(Duration::from_secs(5), || {
            rpc.eval("getline(1)").ok().as_ref().and_then(Value::as_str) == Some(typed.as_str())
        }),
        "the keys never reached the editor"
    );
    assert!(
        term.pump_until(Duration::from_secs(5), |s| s.starts_with(&typed)),
        "what was typed was never drawn:\n{}",
        term.text()
    );

    assert_eq!(term.hang_up(), Some(0), "the client's exit");
    assert!(
        term.output.ends_with(b"\x1b[?1049l"),
        "the terminal was not handed back off the alternate screen"
    );
}

/// What nvmux tells its client to show — a card, in the client's input (see
/// `nvmux::card`) — is drawn in the margin over the editor, and is typed into
/// nothing. The editor clearing its screen and drawing it again draws the card
/// again with it. When its time is up its words go, and what they covered is
/// the editor's again; its sign stays, at the edge, until it is let go of.
#[test]
fn a_card_is_drawn_over_the_editor_and_outlives_its_repaints() {
    require_nvim!();
    let scratch = Scratch::new("client-card");
    let (sock, mut rpc, config) = session(&scratch, "client-card");
    let cols = usize::from(COLS);
    let under = "u".repeat(cols - 1);
    rpc.command(&format!(
        "setlocal noswapfile | call setline(1, ['first', '{under}', '{under}'])"
    ))
    .expect("setline");

    let mut term = Terminal::spawn(&sock, &config);
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.lines().nth(1)
            == Some(under.as_str())),
        "the session was never drawn:\n{}",
        term.text()
    );

    let life = Duration::from_millis(2500);
    let card = nvmux::card::Card {
        life_ms: life.as_millis() as u64,
        fade_ms: 0,
        ..nvmux::card::Card::news(
            1,
            nvmux::card::Kind::Finished,
            "api-server",
            "cargo test: exited with code 0",
            3,
            "Ctrl-Space ⏎ go there",
        )
    };
    let shown = Instant::now();
    term.type_bytes(&nvmux::card::encode(&nvmux::card::Command::Show(card)));
    // Flush right, two columns in: the last of them is past the editor's
    // text, and blank.
    let head = "✓ api-server  3s";
    let body = "cargo test: exited with code 0";
    let carded = |s: &str| {
        let rows: Vec<&str> = s.lines().collect();
        rows.get(1)
            .is_some_and(|r| r.ends_with(&format!("{head}u")))
            && rows
                .get(2)
                .is_some_and(|r| r.ends_with(&format!("{body}u")))
    };
    assert!(
        term.pump_until(Duration::from_secs(5), carded),
        "the card was never drawn:\n{}",
        term.text()
    );
    let row = term
        .text()
        .lines()
        .nth(1)
        .map(str::to_string)
        .unwrap_or_default();
    assert_eq!(
        row,
        format!("{}{head}u", "u".repeat(cols - 2 - head.chars().count())),
        "the card is not in the margin"
    );
    assert_eq!(
        rpc.eval("[getline(1, '$'), mode()]")
            .expect("ask the editor"),
        Value::Array(vec![
            Value::Array(vec![
                Value::from("first"),
                Value::from(under.as_str()),
                Value::from(under.as_str()),
            ]),
            Value::from("n"),
        ]),
        "the card reached the editor as keys"
    );

    // The editor clears its screen and draws it all again: the card with it.
    rpc.command("mode").expect(":mode");
    term.pump_until(Duration::from_millis(500), |_| false);
    assert!(
        carded(&term.text()),
        "a repaint took the card off the screen:\n{}",
        term.text()
    );

    let marked = format!("{}✓u", "u".repeat(cols - 3));
    let left = life.saturating_sub(shown.elapsed()) + Duration::from_secs(3);
    assert!(
        term.pump_until(left, |s| {
            let rows: Vec<&str> = s.lines().collect();
            rows.get(1) == Some(&marked.as_str()) && rows.get(2) == Some(&under.as_str())
        }),
        "the words never went, or did not leave the sign:\n{}",
        term.text()
    );
    term.type_bytes(&nvmux::card::encode(&nvmux::card::Command::Drop { id: 1 }));
    assert!(
        term.pump_until(Duration::from_secs(3), |s| s.lines().nth(1)
            == Some(under.as_str())),
        "the mark never went, or did not give the editor's text back:\n{}",
        term.text()
    );
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

/// A session waiting for a key nvmux may not press for the user — a
/// half-typed `g`, which over RPC is also what a crashed callback leaves — is
/// drawn once the user presses one. Neovim holds the client's attach behind
/// the key; the client sends keys from the start all the same, and types
/// nothing of its own. The relay's half is in `tests/local_lifecycle.rs`.
#[test]
fn a_session_waiting_for_a_key_is_drawn_once_one_is_pressed() {
    require_nvim!();
    let scratch = Scratch::new("client-blocked");
    let (sock, mut rpc, config) = session(&scratch, "client-blocked");
    let marker = "waiting for a key";
    rpc.command(&format!(
        "setlocal noswapfile | call setline(1, \"{marker}\")"
    ))
    .expect("setline");
    common::block_on_a_key(&sock);

    let mut term = Terminal::spawn(&sock, &config);
    // Time enough to attach, were the session answering.
    term.pump_until(Duration::from_secs(1), |_| false);
    let mode = common::mode(&sock).expect("the mode is a fast call");
    assert!(
        mode.blocking && mode.mode == "n",
        "the client typed into a state it cannot identify: {mode:?}"
    );

    term.type_bytes(b"\x1b");
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.starts_with(marker)),
        "the session was never drawn:\n{}",
        term.text()
    );
    assert_eq!(multigrid_uis(&mut rpc), vec![true], "windows of their own");
}

/// A second client is shown everything the first was — the windows, a float
/// with its border, and a tab page it has never seen when it is first shown —
/// though Neovim sends a UI attaching after another none of them.
#[test]
fn a_client_after_another_is_shown_every_window() {
    require_nvim!();
    let scratch = Scratch::new("client-after");
    let (sock, mut rpc, config) = session(&scratch, "client-after");
    rpc.command("setlocal noswapfile | call setline(1, 'in both windows')")
        .expect("setline");
    rpc.command("vsplit").expect("vsplit");
    rpc.command(
        "lua local b = vim.api.nvim_create_buf(false, true) \
         vim.api.nvim_buf_set_lines(b, 0, -1, false, { 'a float, bordered' }) \
         vim.api.nvim_open_win(b, false, { relative = 'editor', row = 8, col = 30, \
         width = 18, height = 1, border = 'single' })",
    )
    .expect("a float");
    rpc.command("tabnew | setlocal noswapfile | call setline(1, 'the second tab page') | tabprev")
        .expect("a second tab page");
    let whole = |s: &str| {
        s.lines()
            .nth(1)
            .is_some_and(|row| row.matches("in both windows").count() == 2)
            && s.contains("│a float, bordered │")
            && s.contains("┌──────────────────┐")
    };

    let mut first = Terminal::spawn(&sock, &config);
    assert!(
        first.pump_until(Duration::from_secs(15), whole),
        "the first client never drew it all:\n{}",
        first.text()
    );
    assert_eq!(first.hang_up(), Some(0));
    assert!(
        common::wait_until(Duration::from_secs(5), || rpc.list_uis().ok() == Some(0)),
        "the first client stayed attached"
    );

    let mut second = Terminal::spawn(&sock, &config);
    assert!(
        second.pump_until(Duration::from_secs(15), whole),
        "the second client never drew it all:\n{}",
        second.text()
    );
    assert_eq!(
        multigrid_uis(&mut rpc),
        vec![true],
        "and with windows of their own"
    );
    second.type_bytes(b"gt");
    assert!(
        second.pump_until(Duration::from_secs(10), |s| {
            s.lines().nth(1) == Some("the second tab page")
        }),
        "the tab page it had never seen was never drawn:\n{}",
        second.text()
    );
    second.type_bytes(b"gT");
    assert!(
        second.pump_until(Duration::from_secs(10), whole),
        "the first tab page did not come back whole:\n{}",
        second.text()
    );
}

/// Take what the client draws until it has drawn nothing for `quiet`, for up
/// to `within`. Says whether it went quiet.
fn settle(term: &mut Terminal, quiet: Duration, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        let before = term.output.len();
        term.pump_until(quiet, |_| false);
        if term.output.len() == before {
            return true;
        }
    }
    false
}

/// With `[client] lean_output` on, a client draws a session as it changes —
/// scrolled, typed into, in colours that change along the line, with
/// characters two cells wide — into the very screen a client drawing it
/// afresh with it off shows: every cell, with its colours and attributes.
#[test]
fn a_lean_client_draws_the_screen_a_fresh_one_does() {
    require_nvim!();
    let scratch = Scratch::new("client-lean");
    let (sock, mut rpc, config) = session(&scratch, "client-lean");
    rpc.command(
        "setlocal noswapfile number cursorline | \
         call setline(1, map(range(1, 300), \
         {_, n -> printf('%d 漢字 é ─ %s', n, repeat('ab cd ', n % 9))})) | \
         call matchadd('Error', 'ab')",
    )
    .expect("setline");
    std::fs::write(&config, "[client]\nlean_output = true\n").expect("write the config");
    let rows = |t: &Terminal| {
        t.parser
            .screen()
            .rows_formatted(0, COLS)
            .collect::<Vec<_>>()
    };

    let mut lean = Terminal::spawn(&sock, &config);
    assert!(
        lean.pump_until(Duration::from_secs(15), |s| s.contains("é ─ ab cd")),
        "never drawn:\n{}",
        lean.text()
    );
    for keys in [&b"\x04"[..], b"jjjj", b"A typed here\x1b", b"\x06", b"\x15"] {
        lean.type_bytes(keys);
        assert!(
            settle(
                &mut lean,
                Duration::from_millis(400),
                Duration::from_secs(10)
            ),
            "never settled after {keys:?}"
        );
    }
    assert!(
        find(&lean.output, b"\x1b[r").is_some(),
        "no frame of the lean client's moved down by LF"
    );
    let (drawn, text) = (rows(&lean), lean.text());
    assert_eq!(lean.hang_up(), Some(0));
    assert!(
        common::wait_until(Duration::from_secs(5), || rpc.list_uis().ok() == Some(0)),
        "the lean client stayed attached"
    );

    std::fs::write(&config, "[client]\nlean_output = false\n").expect("write the config");
    let mut fresh = Terminal::spawn(&sock, &config);
    assert!(
        fresh.pump_until(Duration::from_secs(15), |s| s.contains("é ─ ab cd")),
        "never drawn afresh:\n{}",
        fresh.text()
    );
    assert!(settle(
        &mut fresh,
        Duration::from_millis(400),
        Duration::from_secs(10)
    ));
    assert!(
        drawn == rows(&fresh),
        "the lean client's screen:\n{text}\nis not the fresh one's:\n{}",
        fresh.text()
    );
}

/// What a client writes is drawn as it would be in one read, however the
/// reads of it fall: one that ends part way through a character waits for
/// the rest of it (see [`whole`]).
#[test]
fn reads_that_cut_a_character_in_two_draw_it_whole() {
    let written = "漢字 é ─ ab".as_bytes();
    let screen = |reads: &[&[u8]]| {
        let mut parser = vt100::Parser::new(1, 20, 0);
        let (mut output, mut fed) = (Vec::new(), 0);
        for read in reads {
            output.extend_from_slice(read);
            let end = fed + whole(&output[fed..]);
            parser.process(&output[fed..end]);
            fed = end;
        }
        parser.screen().rows_formatted(0, 20).collect::<Vec<_>>()
    };
    let at_once = screen(&[written]);
    for k in 1..written.len() {
        assert_eq!(
            screen(&[&written[..k], &written[k..]]),
            at_once,
            "read in two at byte {k}"
        );
    }
}

/// A yank to `+` reaches the terminal's clipboard as OSC 52. Neovim turns its
/// OSC 52 clipboard on only for a terminal it has asked whether it takes it,
/// and asks only through a UI that says it is on a tty, as the client does;
/// what it then writes to the terminal reaches it the same way.
#[test]
fn a_yank_reaches_the_terminals_clipboard() {
    require_nvim!();
    let scratch = Scratch::new("client-osc52");
    let (sock, mut rpc, config) = session(&scratch, "client-osc52");
    rpc.command("setlocal noswapfile | call setline(1, 'to the clipboard')")
        .expect("setline");

    let mut term = Terminal::spawn_answering(&sock, &config, DA1_REPLY_OSC52);
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s
            .starts_with("to the clipboard")),
        "never drawn:\n{}",
        term.text()
    );
    // Asked through the client, and answered through it.
    let detected = |rpc: &mut nvmux::rpc::Client<std::os::unix::net::UnixStream>| {
        rpc.eval("get(get(g:, 'termfeatures', {}), 'osc52', v:false)")
            .ok()
            .and_then(|v| v.as_bool())
            == Some(true)
    };
    assert!(
        common::wait_until(Duration::from_secs(5), || {
            term.pump_until(Duration::from_millis(20), |_| false);
            detected(&mut rpc)
        }),
        "Neovim never found the terminal takes OSC 52"
    );

    // The provider asked for by name: one a machine has installed — pbcopy,
    // xclip — comes first otherwise, and this is about the terminal's.
    rpc.command("let g:clipboard = 'osc52' | normal! \"+yy")
        .expect("yank to +");
    // "to the clipboard\n", in base64.
    assert!(
        term.pump_until_written(
            Duration::from_secs(5),
            b"\x1b]52;c;dG8gdGhlIGNsaXBib2FyZAo="
        ),
        "the yank never reached the terminal"
    );
}

/// While a UI that keeps every window on grid 1 is attached, the client does
/// without windows of its own, and draws everything all the same; once it has
/// gone, the client has them again.
#[test]
fn a_ui_without_windows_of_its_own_is_made_room_for() {
    require_nvim!();
    let scratch = Scratch::new("client-foreign");
    let (sock, mut rpc, config) = session(&scratch, "client-foreign");
    rpc.command("setlocal noswapfile | call setline(1, 'under a float')")
        .expect("setline");
    rpc.command(
        "lua local b = vim.api.nvim_create_buf(false, true) \
         vim.api.nvim_buf_set_lines(b, 0, -1, false, { 'over it' }) \
         vim.api.nvim_open_win(b, false, { relative = 'editor', row = 4, col = 10, \
         width = 7, height = 1, border = 'single' })",
    )
    .expect("a float");
    let whole = |s: &str| s.starts_with("under a float") && s.contains("│over it│");

    let mut term = Terminal::spawn(&sock, &config);
    assert!(
        term.pump_until(Duration::from_secs(15), whole),
        "never drawn:\n{}",
        term.text()
    );
    assert_eq!(multigrid_uis(&mut rpc), vec![true]);

    // A UI of no nvmux's: attached, and never read, which Neovim does not
    // mind for as long as this lasts.
    let mut other = nvmux::rpc::Client::connect(&sock, Duration::from_secs(5)).expect("connect");
    other
        .call(
            "nvim_ui_attach",
            vec![
                Value::from(i64::from(COLS)),
                Value::from(i64::from(ROWS)),
                Value::Map(vec![(Value::from("ext_linegrid"), Value::from(true))]),
            ],
        )
        .expect("attach a UI without ext_multigrid");
    assert!(
        common::wait_until(Duration::from_secs(5), || {
            multigrid_uis(&mut rpc) == vec![false, false]
        }),
        "the client kept ext_multigrid: {:?}",
        multigrid_uis(&mut rpc)
    );
    assert!(
        term.pump_until(Duration::from_secs(5), whole),
        "not drawn whole on grid 1:\n{}",
        term.text()
    );

    drop(other);
    assert!(
        common::wait_until(Duration::from_secs(5), || multigrid_uis(&mut rpc)
            == vec![true]),
        "the client did not take ext_multigrid back: {:?}",
        multigrid_uis(&mut rpc)
    );
    rpc.command("call setline(1, 'under a float, still')")
        .expect("setline");
    assert!(
        term.pump_until(Duration::from_secs(5), |s| {
            s.starts_with("under a float, still") && s.contains("│over it│")
        }),
        "not drawn whole after:\n{}",
        term.text()
    );
}

/// A split flies in the way animate.nvim draws it: with `'splitright'` the
/// new window starts as a sliver at the right edge, and its separator comes
/// left frame by frame to where Neovim put it.
#[test]
fn a_split_flies_in_from_its_side() {
    require_nvim!();
    let scratch = Scratch::new("client-split");
    let (sock, mut rpc, config) = session(&scratch, "client-split");
    rpc.command("setlocal noswapfile | set splitright | call setline(1, 'split me')")
        .expect("setline");

    let mut term = Terminal::spawn(&sock, &config);
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.starts_with("split me")),
        "never drawn:\n{}",
        term.text()
    );
    // The agent the client leaves in the editor has answered by now.
    term.pump_until(Duration::from_millis(300), |_| false);
    let from = term.output.len();
    rpc.command("vsplit").expect("vsplit");
    let col = rpc
        .eval("win_screenpos(winnr())[1]")
        .expect("where the new window is")
        .as_i64()
        .expect("a column");
    // The separator is the column left of the new window, 0-based.
    let landed = (col - 2) as u16;
    let sep_at = |screen: &vt100::Screen| {
        (0..COLS).find(|c| {
            screen
                .cell(0, *c)
                .is_some_and(|cell| cell.contents() == "│")
        })
    };
    assert!(
        term.pump(Duration::from_secs(5), |t| sep_at(t.parser.screen())
            == Some(landed)),
        "the separator never landed at {landed}:\n{}",
        term.text()
    );
    term.pump_until(Duration::from_millis(300), |_| false);

    // Every frame drawn on the way, in order: where the separator was.
    let mut replay = vt100::Parser::new(ROWS, COLS, 0);
    replay.process(&term.output[..from]);
    let end = b"\x1b[?2026l";
    let mut at = from;
    let mut seen: Vec<u16> = Vec::new();
    while let Some(i) = find(&term.output[at..], end) {
        replay.process(&term.output[at..at + i + end.len()]);
        at += i + end.len();
        if let Some(c) = sep_at(replay.screen()) {
            if seen.last() != Some(&c) {
                seen.push(c);
            }
        }
    }
    assert_eq!(seen.last(), Some(&landed), "{seen:?}");
    assert!(
        seen.first().is_some_and(|c| *c > landed + 10),
        "it did not start at the right edge: {seen:?}"
    );
    assert!(
        seen.windows(2).all(|w| w[0] > w[1]),
        "it did not come left frame by frame: {seen:?}"
    );
}

/// A socket that passes everything to `to` and back, `delay` late each way:
/// a slow link to draw a session over. Every connection made to it gets one
/// of its own to `to`, as an ssh forward would.
fn slow_link(dir: &Path, to: &Path, delay: Duration) -> PathBuf {
    counted_link(dir, to, delay).0
}

/// [`slow_link`], and what it carries (see [`Link`]).
fn counted_link(dir: &Path, to: &Path, delay: Duration) -> (PathBuf, Link) {
    use std::os::unix::net::{UnixListener, UnixStream};
    /// One direction of connection `conn`, back from `to` or not: read as it
    /// comes, written `delay` after.
    fn carry(
        mut from: UnixStream,
        mut to: UnixStream,
        count: Arc<AtomicUsize>,
        (trace, conn, back): (Trace, usize, bool),
    ) {
        let delay = trace.delay;
        let (tx, rx) = mpsc::channel::<(Instant, Vec<u8>)>();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(n @ 1..) = from.read(&mut buf) {
                count.fetch_add(n, Ordering::Relaxed);
                if tx
                    .send((Instant::now() + delay, buf[..n].to_vec()))
                    .is_err()
                {
                    return;
                }
            }
        });
        std::thread::spawn(move || {
            for (at, bytes) in rx {
                std::thread::sleep(at.saturating_duration_since(Instant::now()));
                if to.write_all(&bytes).is_err() {
                    return;
                }
                trace.carried.lock().expect("the trace").push(Carried {
                    conn,
                    back,
                    read: at - delay,
                    written: Instant::now(),
                    bytes,
                });
            }
            let _ = to.shutdown(std::net::Shutdown::Write);
        });
    }
    std::fs::create_dir_all(dir).expect("the link's directory");
    let path = dir.join("slow.sock");
    let listener = UnixListener::bind(&path).expect("bind the slow link");
    let to = to.to_path_buf();
    let link = Link {
        back: Arc::new(AtomicUsize::new(0)),
        trace: Trace {
            delay,
            carried: Arc::default(),
        },
    };
    let (count, trace) = (Arc::clone(&link.back), link.trace.clone());
    std::thread::spawn(move || {
        for (conn, near) in listener.incoming().enumerate() {
            let (Ok(near), Ok(far)) = (near, UnixStream::connect(&to)) else {
                return;
            };
            let (Ok(near2), Ok(far2)) = (near.try_clone(), far.try_clone()) else {
                return;
            };
            carry(
                near,
                far,
                Arc::new(AtomicUsize::new(0)),
                (trace.clone(), conn, false),
            );
            carry(far2, near2, Arc::clone(&count), (trace.clone(), conn, true));
        }
    });
    (path, link)
}

/// What a test knows of a slow link it made: how many bytes it has carried
/// back from the session, and everything it carried either way.
struct Link {
    back: Arc<AtomicUsize>,
    trace: Trace,
}

/// What a slow link passed on, each way, `delay` late: for a test that times
/// what a client draws to say, when it is late, where the time went.
#[derive(Clone)]
struct Trace {
    delay: Duration,
    carried: Arc<std::sync::Mutex<Vec<Carried>>>,
}

/// A line number from Neovim's side of a slow link, with when the link read
/// it and wrote it on (see [`Trace::views_and_lines`]).
type Said = (Instant, Instant, u32);

/// One read a slow link passed on.
struct Carried {
    /// Which of the link's connections, counted from nought.
    conn: usize,
    /// From the session to the client, or the other way.
    back: bool,
    /// When it was read, and when written on.
    read: Instant,
    written: Instant,
    bytes: Vec<u8>,
}

impl Trace {
    /// The messages passed one way, each with when the link read the last
    /// of it and when it wrote that on; every connection's, in the order
    /// they were written.
    fn messages(&self, back: bool) -> Vec<(Instant, Instant, Value)> {
        let carried = self.carried.lock().expect("the trace");
        let mut pending: std::collections::HashMap<usize, Vec<u8>> = Default::default();
        let mut out = Vec::new();
        for c in carried.iter().filter(|c| c.back == back) {
            let bytes = pending.entry(c.conn).or_default();
            bytes.extend_from_slice(&c.bytes);
            let mut rest = &bytes[..];
            loop {
                let mut tried = rest;
                match rmpv::decode::read_value(&mut tried) {
                    Ok(value) => {
                        rest = tried;
                        out.push((c.read, c.written, value));
                    }
                    // A message the read ends in the middle of waits for the
                    // next.
                    Err(_) => break,
                }
            }
            let used = bytes.len() - rest.len();
            bytes.drain(..used);
        }
        out
    }

    /// What came back of a run of pages, each with when the link read it and
    /// wrote it on: the top line of each view Neovim drew, and how far down
    /// each push of the agent's lines went.
    fn views_and_lines(&self) -> (Vec<Said>, Vec<Said>) {
        let (mut views, mut lines) = (Vec::new(), Vec::new());
        for (read, written, v) in self.messages(true) {
            match notification(&v) {
                Some(("redraw", events)) => {
                    for event in events.iter().filter_map(Value::as_array) {
                        if event.first().and_then(Value::as_str) != Some("win_viewport") {
                            continue;
                        }
                        for args in event[1..].iter().filter_map(Value::as_array) {
                            if let Some(top) = args.get(2).and_then(Value::as_u64) {
                                views.push((read, written, top as u32 + 1));
                            }
                        }
                    }
                }
                Some(("nvmux_client", [kind, _, _, _, first, sent, ..]))
                    if kind.as_str() == Some("lines") =>
                {
                    let end =
                        first.as_u64().unwrap_or(0) as usize + sent.as_array().map_or(0, Vec::len);
                    lines.push((read, written, end as u32));
                }
                _ => {}
            }
        }
        (views, lines)
    }

    /// The most the link was late passing anything on, each way: to the
    /// session, and back from it.
    fn lateness(&self) -> (Duration, Duration) {
        let carried = self.carried.lock().expect("the trace");
        let late = |back: bool| {
            carried
                .iter()
                .filter(|c| c.back == back)
                .map(|c| (c.written - c.read).saturating_sub(self.delay))
                .max()
                .unwrap_or_default()
        };
        (late(false), late(true))
    }
}

/// A notification's method and arguments.
fn notification(v: &Value) -> Option<(&str, &[Value])> {
    let v = v.as_array()?;
    if v.first()?.as_u64()? != 2 {
        return None;
    }
    Some((v.get(1)?.as_str()?, v.get(2)?.as_array()?))
}

/// The number of the line at the top of a screen of `line N`s.
fn top_line(screen: &str) -> Option<u32> {
    screen
        .lines()
        .next()?
        .strip_prefix("line ")?
        .trim()
        .parse()
        .ok()
}

/// Over a slow link, a turn of the wheel scrolls the window before Neovim
/// could have heard of it; Neovim scrolls the same way, and when its own
/// scroll lands, nothing on the screen moves back.
#[test]
fn a_turn_of_the_wheel_over_a_slow_link_scrolls_before_neovim_answers() {
    require_nvim!();
    let scratch = Scratch::new("client-predict");
    let (sock, mut rpc, config) = session(&scratch, "client-predict");
    std::fs::write(&config, PREDICT_ON).expect("write the config");
    rpc.command("setlocal noswapfile | call setline(1, map(range(1, 200), '\"line \" . v:val'))")
        .expect("setline");
    // A second each way and back: Neovim's answer to the wheel cannot be on
    // the screen sooner than that.
    let delay = Duration::from_millis(500);
    let slow = slow_link(&scratch.0.join("link"), &sock, delay);

    let mut term = Terminal::spawn(&slow, &config);
    assert!(
        term.pump_until(Duration::from_secs(20), |s| top_line(s) == Some(1)),
        "never drawn:\n{}",
        term.text()
    );
    // The agent's lines take a round trip or two after the first paint.
    term.pump_until(Duration::from_secs(4), |_| false);

    let from = term.output.len();
    let turned = Instant::now();
    term.type_bytes(b"\x1b[<65;10;5M");
    // Three lines down: the bottom row is one the window never showed, which
    // only the agent's lines can draw.
    assert!(
        term.pump_until(Duration::from_millis(800), |s| {
            top_line(s) == Some(4) && s.lines().nth(21) == Some("line 25")
        }),
        "the wheel waited for Neovim:\n{}",
        term.text()
    );
    assert!(turned.elapsed() < 2 * delay, "drawn too late to tell");

    assert!(
        common::wait_until(Duration::from_secs(5), || {
            rpc.eval("line('w0')").ok().and_then(|v| v.as_i64()) == Some(4)
        }),
        "Neovim scrolled elsewhere"
    );
    // Its scroll, on its way back over the link.
    term.pump_until(Duration::from_secs(2), |_| false);
    assert_eq!(top_line(&term.text()), Some(4), "{}", term.text());

    // An edit above the view: the agent sends the lines again, as they are
    // now, and the next turn of the wheel is drawn from those.
    rpc.command("call append(0, 'line 0')").expect("append");
    assert!(
        term.pump_until(Duration::from_secs(5), |s| top_line(s) == Some(3)),
        "the edit was never drawn:\n{}",
        term.text()
    );
    term.pump_until(Duration::from_secs(2), |_| false);
    term.type_bytes(b"\x1b[<65;10;5M");
    assert!(
        term.pump_until(Duration::from_millis(800), |s| {
            top_line(s) == Some(6) && s.lines().nth(21) == Some("line 27")
        }),
        "the wheel waited for Neovim after an edit:\n{}",
        term.text()
    );
    term.pump_until(Duration::from_secs(2), |_| false);
    assert_eq!(top_line(&term.text()), Some(6), "{}", term.text());

    // Every frame drawn since the first turn, in order: down, and never back
    // but for the edit.
    let mut replay = vt100::Parser::new(ROWS, COLS, 0);
    replay.process(&term.output[..from]);
    let end = b"\x1b[?2026l";
    let mut at = from;
    let mut seen: Vec<u32> = Vec::new();
    while let Some(i) = find(&term.output[at..], end) {
        replay.process(&term.output[at..at + i + end.len()]);
        at += i + end.len();
        let text = replay.screen().contents();
        if let Some(n) = top_line(&text) {
            if seen.last() != Some(&n) {
                seen.push(n);
            }
        }
    }
    assert_eq!(seen.last(), Some(&6), "{seen:?}");
    let back: Vec<&[u32]> = seen.windows(2).filter(|w| w[0] > w[1]).collect();
    assert_eq!(
        back,
        [&[4, 3][..]],
        "it moved back but for the edit: {seen:?}"
    );
}

/// The rows of the window and where the cursor is: what a prediction must
/// get right.
fn window(term: &Terminal) -> (Vec<String>, (u16, u16)) {
    let rows = term.text().lines().take(22).map(String::from).collect();
    (rows, term.parser.screen().cursor_position())
}

/// How late the link to Neovim is each way in the tests of scrolls drawn
/// ahead of it.
const SLOW: Duration = Duration::from_millis(250);

/// A config that has a client predict wherever the link is slow enough, as
/// it does over ssh unless told otherwise: the sessions of these tests are on
/// this machine, and only a link of the test's own makes them slow.
const PREDICT_ON: &str = "[client]\npredict = \"on\"\n";

/// A session whose one window shows the text `setline` puts there, drawn by
/// a client over a slow link, told to predict over it, scrolls jumping
/// rather than sliding so that what a prediction shows can be read off the
/// screen at once.
///
/// With it, the link (see [`Link`]).
fn drawn_slowly(
    scratch: &Scratch,
    tag: &str,
    setline: &str,
) -> (
    nvmux::rpc::Client<std::os::unix::net::UnixStream>,
    Terminal,
    Link,
) {
    let (sock, mut rpc, config) = session(scratch, tag);
    rpc.command(&format!("setlocal noswapfile | {setline}"))
        .expect("setline");
    std::fs::write(
        &config,
        format!("{PREDICT_ON}\n[effects.scroll]\nenabled = false\n"),
    )
    .expect("config");
    let (slow, link) = counted_link(&scratch.0.join("link"), &sock, SLOW);

    let mut term = Terminal::spawn(&slow, &config);
    assert!(
        term.pump_until(Duration::from_secs(20), |s| s.contains("line 1")),
        "never drawn:\n{}",
        term.text()
    );
    // The agent's lines, and the answer to the attach's fence.
    term.pump_until(Duration::from_secs(3), |_| false);
    (rpc, term, link)
}

/// Type `keys`, a scroll the client predicts: it is on the screen before
/// Neovim could have heard of it, and when Neovim's own frame lands, every
/// row of the window and the cursor are where the prediction had them.
fn predicted(term: &mut Terminal, name: &str, keys: &[u8]) {
    let before = window(term);
    term.type_bytes(keys);
    // Within one way of the link: Neovim has not even heard of it yet.
    assert!(
        term.pump(SLOW, |t| window(t) != before),
        "{name} waited for Neovim:\n{}",
        term.text()
    );
    // The rest of the frame, should it have come in two reads.
    term.pump_until(Duration::from_millis(30), |_| false);
    let predicted = window(term);
    // Neovim's own, there and back.
    term.pump_until(4 * SLOW, |_| false);
    assert_eq!(
        predicted,
        window(term),
        "{name}: predicted, then drawn by Neovim"
    );
}

/// Type `keys`, which the client leaves to Neovim, and wait for it to draw
/// them.
fn typed(term: &mut Terminal, keys: &[u8]) {
    term.type_bytes(keys);
    term.pump_until(4 * SLOW, |_| false);
}

/// Every kind of scroll the client predicts, typed or turned over a slow
/// link to a real Neovim, made as Neovim makes it.
#[test]
fn every_scroll_predicted_is_the_one_neovim_makes() {
    require_nvim!();
    let scratch = Scratch::new("client-scrolls");
    // Indented by none to three blanks, so that a cursor sent to its line's
    // first non-blank has somewhere to go.
    let (mut rpc, mut term, _) = drawn_slowly(
        &scratch,
        "client-scrolls",
        "call setline(1, map(range(1, 200), 'repeat(\" \", v:val % 4) . \"line \" . v:val'))",
    );
    for (name, keys) in [
        ("<C-e>", &b"\x05"[..]),
        ("3<C-e>", b"3\x05"),
        ("<C-y>", b"\x19"),
        ("<C-d>", b"\x04"),
        ("<C-u>", b"\x15"),
        ("<C-f>", b"\x06"),
        ("<C-b>", b"\x02"),
        ("<PageDown>", b"\x1b[6~"),
        ("<S-Up>", b"\x1b[1;2A"),
        ("zt", b"zt"),
        ("zz", b"zz"),
        ("zb", b"zb"),
        ("z<CR>", b"z\r"),
        ("z-", b"z-"),
        ("the wheel with Shift", b"\x1b[<69;10;5M"),
        ("the wheel", b"\x1b[<65;10;5M"),
        ("10<C-d>", b"10\x04"),
        ("<C-d> by the new 'scroll'", b"\x04"),
    ] {
        predicted(&mut term, name, keys);
    }

    // With a count, the line to put at the top, middle or bottom.
    for (name, keys) in [
        ("45zt", &b"45zt"[..]),
        ("30zz", b"30zz"),
        ("60zb", b"60zb"),
        ("50z<CR>", b"50z\r"),
        ("40z.", b"40z."),
        ("70z-", b"70z-"),
    ] {
        predicted(&mut term, name, keys);
    }

    // Further than a window.
    for (name, keys) in [
        ("2<C-f>", &b"2\x06"[..]),
        ("30<C-e>", b"30\x05"),
        ("2<C-b>", b"2\x02"),
        ("25<C-y>", b"25\x19"),
        ("2<PageDown>", b"2\x1b[6~"),
    ] {
        predicted(&mut term, name, keys);
    }

    // In Insert mode, where the cursor may sit past the end of its line.
    typed(&mut term, b"A");
    for (name, keys) in [
        ("<PageDown> in Insert mode", &b"\x1b[6~"[..]),
        ("<S-Up> in Insert mode", b"\x1b[1;2A"),
        ("<S-Down> in Insert mode", b"\x1b[1;2B"),
        ("<PageUp> in Insert mode", b"\x1b[5~"),
        ("<C-x><C-e>", b"\x18\x05"),
        ("<C-x><C-y>", b"\x18\x19"),
        ("the wheel in Insert mode", b"\x1b[<65;10;5M"),
        ("the wheel with Shift in Insert mode", b"\x1b[<68;10;5M"),
    ] {
        predicted(&mut term, name, keys);
    }
    typed(&mut term, b"\x1b");

    // With 'scrolloff', and at the end of the buffer.
    rpc.command("set scrolloff=3").expect("scrolloff");
    term.pump_until(4 * SLOW, |_| false);
    for (name, keys) in [
        ("<C-e>, the cursor kept from the top", &b"\x05"[..]),
        ("<C-f> with 'scrolloff'", b"\x06"),
        ("<C-b> with 'scrolloff'", b"\x02"),
        ("zt with 'scrolloff'", b"zt"),
        ("zb with 'scrolloff'", b"zb"),
        ("90zt with 'scrolloff'", b"90zt"),
    ] {
        predicted(&mut term, name, keys);
    }
    typed(&mut term, b"G");
    for (name, keys) in [
        ("<C-e> past the end", &b"\x05"[..]),
        ("zt on the last line", b"zt"),
        ("<C-b> from the end", b"\x02"),
        ("2<C-f> to the end", b"2\x06"),
    ] {
        predicted(&mut term, name, keys);
    }
}

/// Lines long and short, and the cursor at the end of a short one, come to
/// from column 40 of a long one: each scroll that takes it to another line
/// puts it in column 40 of a line long enough, as Neovim does — where it
/// wants to be, not where it is — and after `$`, at the end of each line,
/// however long.
#[test]
fn a_scroll_takes_the_cursor_to_the_column_it_wants() {
    require_nvim!();
    let scratch = Scratch::new("client-want");
    let (_rpc, mut term, _) = drawn_slowly(
        &scratch,
        "client-want",
        "call setline(1, map(range(1, 200), \
         'v:val % 3 ? \"line \" . v:val . \" \" . repeat(\"abcdefghij\", 6) : \"line \" . v:val'))",
    );
    // Down from column 40 of line 1 to the end of line 3.
    typed(&mut term, b"40l");
    typed(&mut term, b"jj");
    for (name, keys) in [
        ("<C-d> from the end of a short line", &b"\x04"[..]),
        ("3<C-e> to the end of one again", b"3\x05"),
        ("<C-e> out to column 40", b"\x05"),
    ] {
        predicted(&mut term, name, keys);
    }
    typed(&mut term, b"$");
    for (name, keys) in [
        ("<C-e> after $", &b"\x05"[..]),
        ("<C-e> after $, to a short line", b"\x05"),
        ("<C-e> after $, to a long one", b"\x05"),
    ] {
        predicted(&mut term, name, keys);
    }
}

/// A window that does not wrap, scrolled sideways over a slow link to a real
/// Neovim: each scroll is on the screen before Neovim could have heard of
/// it, and is the one Neovim makes.
#[test]
fn every_scroll_sideways_predicted_is_the_one_neovim_makes() {
    require_nvim!();
    let scratch = Scratch::new("client-sideways");
    // Lines of every length, from shorter than the window to twice as wide,
    // their columns told apart by the digits in them.
    let (mut rpc, mut term, _) = drawn_slowly(
        &scratch,
        "client-sideways",
        "setlocal nowrap | call setline(1, map(range(1, 200), \
         'repeat(\" \", v:val % 4) . \"line \" . v:val . \" \" . \
         repeat(\"0123456789\", (v:val * 7) % 17)'))",
    );
    for (name, keys) in [
        ("zl", &b"zl"[..]),
        ("5zl", b"5zl"),
        ("zL", b"zL"),
        ("zH", b"zH"),
        ("zh", b"zh"),
        ("<Right> after z", b"z\x1b[C"),
    ] {
        predicted(&mut term, name, keys);
    }
    typed(&mut term, b"40l");
    for (name, keys) in [
        ("zs", &b"zs"[..]),
        ("ze", b"ze"),
        ("the wheel right", b"\x1b[<67;10;5M"),
        ("the wheel left", b"\x1b[<66;10;5M"),
        ("the wheel right with Shift", b"\x1b[<71;10;5M"),
        ("the wheel left with Shift", b"\x1b[<70;10;5M"),
        ("the wheel right again", b"\x1b[<67;10;5M"),
    ] {
        predicted(&mut term, name, keys);
    }

    // Past the end of the cursor's line, which sends the cursor to the
    // longest line in view.
    typed(&mut term, b"0j");
    for (name, keys) in [
        (
            "the wheel right, twice with Shift",
            &b"\x1b[<71;10;5M\x1b[<71;10;5M"[..],
        ),
        ("the wheel left with Shift, there", b"\x1b[<70;10;5M"),
    ] {
        predicted(&mut term, name, keys);
    }

    // Kept columns from the edges.
    rpc.command("set sidescrolloff=5").expect("sidescrolloff");
    typed(&mut term, b"0");
    typed(&mut term, b"30l");
    for (name, keys) in [
        ("zs with 'sidescrolloff'", &b"zs"[..]),
        ("ze with 'sidescrolloff'", b"ze"),
        ("zl with 'sidescrolloff'", b"10zl"),
    ] {
        predicted(&mut term, name, keys);
    }

    // In Insert mode.
    typed(&mut term, b"0i");
    for (name, keys) in [
        ("the wheel right in Insert mode", &b"\x1b[<67;10;5M"[..]),
        ("the wheel left in Insert mode", b"\x1b[<66;10;5M"),
    ] {
        predicted(&mut term, name, keys);
    }
    typed(&mut term, b"\x1b");
}

/// Lines of characters two cells wide in a window that does not wrap, every
/// other one a column further in, so that the window's edges cut one in two
/// on every other row: each scroll, sideways or down, is drawn ahead of
/// Neovim with a `>` where one does not fit at the right and a `<` where the
/// view starts in the middle of one, as Neovim draws them; and the cursor,
/// scroll after scroll, on the character that covers the column it wants,
/// which on every other row starts a column before it, as Neovim puts it —
/// and nothing moves when Neovim's own rows come.
#[test]
fn scrolls_over_wide_characters_cut_at_the_edges_are_the_ones_neovim_makes() {
    require_nvim!();
    let scratch = Scratch::new("client-wide");
    let (_rpc, mut term, _) = drawn_slowly(
        &scratch,
        "client-wide",
        "setlocal nowrap | call setline(1, map(range(1, 200), \
         'repeat(\" \", v:val % 2) . \"line \" . v:val . \" \" . repeat(\"日本\", 50)'))",
    );
    for (name, keys) in [
        ("zL into them", &b"zL"[..]),
        ("zl", b"zl"),
        ("zh", b"zh"),
        ("the wheel right", b"\x1b[<67;10;5M"),
        ("the wheel left", b"\x1b[<66;10;5M"),
        ("<C-f> among them", b"\x06"),
        ("<C-e>", b"\x05"),
        ("<C-b>", b"\x02"),
    ] {
        predicted(&mut term, name, keys);
    }
}

/// Lines long and short in a window that does not wrap, scrolled sideways:
/// a scroll that lands the cursor out of view moves the view sideways to
/// show it, as Neovim does — as far as it is out of view, or centred on it
/// from half the view away and more — and is drawn so ahead of Neovim.
#[test]
fn a_scroll_landing_out_of_view_moves_the_view_sideways_as_neovim_does() {
    require_nvim!();
    let scratch = Scratch::new("client-aside");
    let (_rpc, mut term, _) = drawn_slowly(
        &scratch,
        "client-aside",
        "setlocal nowrap | call setline(1, map(range(1, 200), \
         'v:val % 3 ? \"line \" . v:val . \" \" . repeat(\"abcdefghij\", 12) : \"line \" . v:val'))",
    );
    for (name, keys) in [
        ("zL", &b"zL"[..]),
        ("2<C-e> to a short line, out of view", b"2\x05"),
        ("<C-e> to a long one, in view", b"\x05"),
    ] {
        predicted(&mut term, name, keys);
    }
    // The cursor wants the end of every line from here on.
    typed(&mut term, b"$");
    for (name, keys) in [
        ("<C-e> after $, in view", &b"\x05"[..]),
        ("<C-e> after $ to a short line, far out of view", b"\x05"),
        ("<C-e> after $ to the end of a long line, far out", b"\x05"),
    ] {
        predicted(&mut term, name, keys);
    }
}

/// A scroll that lands the cursor on a character two cells wide that the
/// view's left edge cuts in two: Neovim moves the view a column to show it
/// whole, and the client draws it so ahead of Neovim.
#[test]
fn a_scroll_onto_a_wide_character_the_edge_cuts_moves_the_view_a_column() {
    require_nvim!();
    let scratch = Scratch::new("client-wide-cut");
    let (_rpc, mut term, _) = drawn_slowly(
        &scratch,
        "client-wide-cut",
        "setlocal nowrap | call setline(1, map(range(1, 200), \
         'repeat(\" \", v:val % 2) . \"line \" . v:val . \" \" . repeat(\"日本\", 50)'))",
    );
    for (name, keys) in [
        ("zL into them", &b"zL"[..]),
        ("<C-f> onto one the left edge cuts", b"\x06"),
    ] {
        predicted(&mut term, name, keys);
    }
}

/// The first line of the `k`th page down a window of `line N`s 24 rows
/// tall: a page is the window less two lines, 20.
fn page(k: usize) -> u32 {
    1 + 20 * k as u32
}

/// Type `presses` pages, `gap` apart, into a window of `line N`s drawn over
/// `link`: when each was typed and first on the screen, after making sure
/// the view never went back and ended where Neovim's did. Each goes a gap
/// after the one before, however late that one was: a test held up on a busy
/// machine goes on from there, as a key held down repeats, rather than make
/// up the time with pages typed all at once, faster than any key repeats and
/// further ahead than the lines kept go.
fn pages(
    rpc: &mut nvmux::rpc::Client<std::os::unix::net::UnixStream>,
    term: &mut Terminal,
    link: &Link,
    gap: Duration,
    presses: usize,
) -> Paging {
    let t0 = Instant::now();
    let mut typed = Vec::new();
    let mut drawn: Vec<Option<Instant>> = vec![None; presses];
    let mut tops = Vec::new();
    let mut watch = |term: &mut Terminal, until: Instant, drawn: &mut Vec<Option<Instant>>| loop {
        let now = Instant::now();
        if let Some(top) = top_line(&term.text()) {
            if tops.last() != Some(&top) {
                tops.push(top);
            }
            for (k, at) in drawn.iter_mut().enumerate() {
                if at.is_none() && top >= page(k + 1) {
                    *at = Some(now);
                }
            }
        }
        if now >= until {
            return tops.clone();
        }
        let last = tops.last().copied();
        term.pump(until - now, |t| top_line(&t.text()) != last);
    };
    for _ in 0..presses {
        let at = Instant::now();
        typed.push(at);
        term.type_bytes(b"\x06");
        watch(term, at + gap, &mut drawn);
    }
    let tops = watch(term, Instant::now() + 4 * SLOW, &mut drawn);
    let run = Paging {
        t0,
        typed,
        drawn,
        trace: link.trace.clone(),
    };
    assert!(
        tops.windows(2).all(|w| w[0] < w[1]),
        "it went back: {tops:?}\n{}",
        run.timeline(presses)
    );
    assert_eq!(
        tops.last(),
        Some(&page(presses)),
        "{tops:?}\n{}",
        run.timeline(presses)
    );
    assert_eq!(
        rpc.eval("line('w0')").ok().and_then(|v| v.as_u64()),
        Some(u64::from(page(presses))),
        "Neovim went elsewhere\n{}",
        run.timeline(presses)
    );
    run
}

/// A run of pages typed (see [`pages`]): when each was typed and first on
/// the screen, and what the link carried meanwhile.
struct Paging {
    t0: Instant,
    typed: Vec<Instant>,
    drawn: Vec<Option<Instant>>,
    trace: Trace,
}

impl Paging {
    /// How long after its key each page was first on the screen.
    fn late(&self) -> Vec<Option<Duration>> {
        self.typed
            .iter()
            .zip(&self.drawn)
            .map(|(typed, drawn)| drawn.map(|d| d - *typed))
            .collect()
    }

    /// When Neovim's own frame first showing each page reached the client,
    /// as the link wrote it on: what a page waiting for Neovim would have
    /// waited for. The last may be on its way yet, and is waited for as long
    /// as it takes the link to come back.
    fn frames(&self) -> Vec<Option<Instant>> {
        let deadline = Instant::now() + 4 * self.trace.delay;
        loop {
            let (views, _) = self.trace.views_and_lines();
            let frames: Vec<Option<Instant>> = (1..=self.typed.len())
                .map(|k| views.iter().find(|v| v.2 >= page(k)).map(|v| v.1))
                .collect();
            if frames.iter().all(Option::is_some) || Instant::now() >= deadline {
                return frames;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Where the time went, page by page about the `around`th, for a test
    /// to say when a page is late: when it was typed; when its key left the
    /// client and reached Neovim; when Neovim's view got to it and reached
    /// the client, and the lines it shows; and when it was drawn. Each in
    /// ms from the first page typed, as the link read it and wrote it on.
    fn timeline(&self, around: usize) -> String {
        // What was still on its way when a page was found late — Neovim's
        // frame that lands it, perhaps — given the time to come: the link
        // goes on keeping what it carries meanwhile.
        std::thread::sleep(4 * self.trace.delay);
        let ms = |t: Instant| match t.checked_duration_since(self.t0) {
            Some(d) => d.as_millis().to_string(),
            None => format!("-{}", (self.t0 - t).as_millis()),
        };
        let way = |e: Option<&(Instant, Instant, u32)>| {
            e.map_or_else(|| "-".into(), |(r, w, _)| format!("{}→{}", ms(*r), ms(*w)))
        };
        // Each `<C-f>` the client sent; where Neovim's view got to; and how
        // far down the lines it sent went.
        let mut keys = Vec::new();
        for (read, written, v) in self.trace.messages(false) {
            if let Some(("nvim_input", [keys_typed, ..])) = notification(&v) {
                let n = keys_typed
                    .as_str()
                    .map_or(0, |k| k.to_lowercase().matches("<c-f>").count());
                keys.extend(std::iter::repeat_n((read, written, 0), n));
            }
        }
        let (views, lines) = self.trace.views_and_lines();
        let n = self.typed.len();
        let shown = if n <= 40 {
            1..=n
        } else {
            around.saturating_sub(10).max(1)..=(around + 10).min(n)
        };
        let (to, back) = self.trace.lateness();
        let mut out = format!(
            "where the time went, in ms from the first page typed, as the link read and \
             wrote it on ({} ms each way, at worst {} ms late to Neovim and {} ms back):",
            self.trace.delay.as_millis(),
            to.as_millis(),
            back.as_millis()
        );
        for k in shown {
            let top = page(k);
            out += &format!(
                "\n  page {k}: typed {}, key {}, Neovim's view {}, its lines {}, drawn {}",
                ms(self.typed[k - 1]),
                way(keys.get(k - 1)),
                way(views.iter().find(|v| v.2 >= top)),
                // The window's 22 lines from the page's first.
                way(lines.iter().find(|l| l.2 >= top + 21)),
                self.drawn[k - 1].map_or_else(|| "never".into(), ms)
            );
        }
        out
    }
}

/// Pages typed faster than a slow link answers: each is on the screen before
/// Neovim could have heard of it, the lines it shows sent before it was
/// typed.
#[test]
fn pages_typed_faster_than_the_link_answers_keep_up() {
    require_nvim!();
    let scratch = Scratch::new("client-burst");
    let (mut rpc, mut term, link) = drawn_slowly(
        &scratch,
        "client-burst",
        "call setline(1, map(range(1, 2000), '\"line \" . v:val'))",
    );
    let run = pages(&mut rpc, &mut term, &link, Duration::from_millis(100), 10);
    for (k, late) in run.late().iter().enumerate() {
        assert!(
            late.is_some_and(|l| l < SLOW),
            "page {} waited: {late:?}\n{}",
            k + 1,
            run.timeline(k + 1)
        );
    }
}

/// Pages typed faster still, further ahead of Neovim than the lines it sends
/// reach: those past them wait for their lines, which come as Neovim's view
/// moves on, and not for Neovim — none as long as its own frame would take.
#[test]
fn pages_typed_further_ahead_than_lines_go_wait_only_for_them() {
    require_nvim!();
    let scratch = Scratch::new("client-run");
    let (mut rpc, mut term, link) = drawn_slowly(
        &scratch,
        "client-run",
        "call setline(1, map(range(1, 2000), '\"line \" . v:val'))",
    );
    // A round trip of this link types fifty pages, more than the most
    // windows of lines it keeps either way (twenty-four, some twenty-seven
    // pages); thirty-two are as far ahead as a view may go.
    let run = pages(&mut rpc, &mut term, &link, Duration::from_millis(10), 32);
    // Its own frame takes the key there and the frame back, which on a busy
    // machine is longer than the link's delay says: each page is drawn
    // before the frame that shows it has come.
    let frames = run.frames();
    for (k, (late, frame)) in run.late().into_iter().zip(frames).enumerate() {
        let own = frame.map(|f| f - run.typed[k]);
        assert!(
            late.zip(own).is_some_and(|(late, own)| late < own),
            "page {} waited for Neovim: {late:?}, its own frame taking {own:?}\n{}",
            k + 1,
            run.timeline(k + 1)
        );
    }
}

/// The same pages typed faster than Neovim scrolls, as on a busy machine:
/// Neovim takes the keys typed ahead before anything scheduled, and the
/// lines still come as its view moves, not once it has caught up with the
/// last of them. Its first frame, which lands the pages, comes no sooner —
/// later than the first page would be given up on, did the client not hear
/// that Neovim is still at work on them: the view never goes back.
#[test]
fn pages_typed_faster_than_neovim_scrolls_bring_their_lines_as_it_goes() {
    require_nvim!();
    let scratch = Scratch::new("client-behind");
    // Each scroll takes Neovim this long, and a page is typed every 10 ms:
    // its 28 scrolls put its frame past two round trips of the link and a
    // quarter of a second after the first page. It sleeps rather than spins,
    // so as not to take a core from the client and the link on a machine
    // with few; and notes when it made each, once asked to.
    const SCROLL: Duration = Duration::from_millis(30);
    let (mut rpc, mut term, link) = drawn_slowly(
        &scratch,
        "client-behind",
        &format!(
            "call setline(1, map(range(1, 2000), '\"line \" . v:val')) | \
             autocmd WinScrolled * lua vim.uv.sleep({}) \
             table.insert(scrolled_at or {{}}, vim.uv.hrtime())",
            SCROLL.as_millis()
        ),
    );
    rpc.command("lua scrolled_at = {}")
        .expect("note the scrolls");
    // Two pages past the lines in hand, whose lines come with Neovim's second
    // view, or without a push at each scroll only once it has made all 28.
    // Those past them would wait as well for its views further on, each
    // further behind the keys than the last, and further still on a busy
    // machine. A page is late halfway between the two, in scrolls as long as
    // Neovim's were here: on a busy machine, longer than it sleeps.
    let run = pages(&mut rpc, &mut term, &link, Duration::from_millis(10), 28);
    let scroll = rpc
        .eval("luaeval('(scrolled_at[#scrolled_at] - scrolled_at[1]) / (#scrolled_at - 1)')")
        .ok()
        .and_then(|ns| ns.as_f64().or_else(|| ns.as_u64().map(|ns| ns as f64)))
        .map(|ns| Duration::from_nanos(ns as u64))
        .expect("how long a scroll took Neovim");
    let within = SLOW + 15 * scroll;
    for (k, late) in run.late().iter().enumerate() {
        assert!(
            late.is_some_and(|l| l < within),
            "page {} waited for Neovim: {late:?}, a scroll taking it {scroll:?}\n{}",
            k + 1,
            run.timeline(k + 1)
        );
    }
    // Neovim's frame, which may come after `pages` stops watching, and as
    // long again as a page would wait for one: nothing moves.
    let top = top_line(&term.text());
    assert!(
        !term.pump_until(8 * SLOW, |s| top_line(s) != top),
        "it moved once Neovim drew:\n{}",
        term.text()
    );
}

/// A page Neovim does not turn — a key it throws away, which nothing the
/// client can see says it will — is given up on in time, and the view goes
/// back to Neovim's: typed among pages Neovim is still at work on, as long
/// after Neovim has caught up as one typed then would be, and no sooner;
/// the pages around it kept.
#[test]
fn a_page_neovim_does_not_turn_is_given_up_in_time() {
    require_nvim!();
    let scratch = Scratch::new("client-unturned");
    // Neovim behind on pages typed every 10 ms, its frame past when the
    // first would be given up on, as in the test before.
    const SCROLL: Duration = Duration::from_millis(30);
    let (mut rpc, mut term, _) = drawn_slowly(
        &scratch,
        "client-unturned",
        &format!(
            "call setline(1, map(range(1, 2000), '\"line \" . v:val')) | \
             autocmd WinScrolled * lua vim.uv.sleep({})",
            SCROLL.as_millis()
        ),
    );
    // The fifth of them thrown away.
    rpc.command(
        "lua local n = 0 vim.on_key(function(key) \
         if key == '\\6' then n = n + 1 if n == 5 then return '' end end end)",
    )
    .expect("on_key");
    let t0 = Instant::now();
    let mut seen: Vec<(u32, Instant)> = Vec::new();
    // The view's top line each time it changes, and when, till `until` or
    // till the view goes back.
    let mut watch = |term: &mut Terminal, until: Instant| loop {
        let now = Instant::now();
        let mut last = seen.last().map(|&(top, _)| top);
        if let Some(top) = top_line(&term.text()).filter(|&top| Some(top) != last) {
            seen.push((top, now));
            last = Some(top);
        }
        if now >= until || seen.windows(2).any(|w| w[1].0 < w[0].0) {
            return;
        }
        term.pump(until - now, |t| top_line(&t.text()) != last);
    };
    for k in 1..=28 {
        term.type_bytes(b"\x06");
        watch(&mut term, t0 + Duration::from_millis(10) * k);
    }
    // Neovim's view once it has taken them all, and when that was: asked once
    // they have all reached it, it answers only then, taking keys typed
    // before anything asked of it. On a busy machine that is later than its
    // sleeps alone would make it, so it is not worked out from them.
    watch(&mut term, Instant::now() + SLOW + Duration::from_millis(50));
    let asked = std::thread::spawn(move || {
        let w0 = rpc.eval("line('w0')").map(|v| v.as_u64());
        (w0.map_err(|e| e.to_string()), Instant::now())
    });
    watch(&mut term, t0 + Duration::from_secs(20));
    let (w0, caught_up) = asked.join().expect("asked Neovim");
    let caught = caught_up - t0;
    let when: Vec<(u32, Duration)> = seen.iter().map(|&(top, at)| (top, at - t0)).collect();
    let tops: Vec<u32> = seen.iter().map(|&(top, _)| top).collect();
    let back: Vec<&[u32]> = tops.windows(2).filter(|w| w[0] > w[1]).collect();
    assert_eq!(
        back,
        [&[561, 541][..]],
        "not back by that page alone, Neovim at {w0:?} once caught up at {caught:?}: {when:?}"
    );
    assert_eq!(w0, Ok(Some(541)), "Neovim went elsewhere");
    // No sooner than Neovim's frame, a way of the link after it caught up,
    // and as long after that as a page typed then would wait — two round
    // trips and a quarter of a second, give or take — or twice that on a
    // busy machine.
    let at = seen.last().map(|&(_, at)| at).expect("seen");
    let wait = 4 * SLOW + Duration::from_millis(250);
    assert!(
        at > caught_up + SLOW && at < caught_up + SLOW + 2 * wait,
        "given up on {:?} after Neovim caught up at {caught:?}: {when:?}",
        at.saturating_duration_since(caught_up)
    );
}

/// A run of pages longer than the lines the client keeps lets go of those
/// furthest behind rather than start again from two windows: every page of
/// it is on the screen before Neovim could have heard of it. And edits after
/// it, among the lines kept, are drawn as Neovim draws them.
#[test]
fn a_long_run_of_pages_lets_go_of_the_lines_behind() {
    require_nvim!();
    let scratch = Scratch::new("client-long-run");
    let (mut rpc, mut term, link) = drawn_slowly(
        &scratch,
        "client-long-run",
        "call setline(1, map(range(1, 6000), '\"line \" . v:val'))",
    );
    // Past the 4000 lines the client keeps at most, before the end.
    let run = pages(&mut rpc, &mut term, &link, Duration::from_millis(30), 200);
    for (k, late) in run.late().iter().enumerate() {
        assert!(
            late.is_some_and(|l| l < SLOW),
            "page {} waited: {late:?}\n{}",
            k + 1,
            run.timeline(k + 1)
        );
    }
    // Above the view, among the lines kept, and in it.
    rpc.command("call setline(3990, 'edited 3990') | call append(4005, ['new a', 'new b'])")
        .expect("edit");
    term.pump_until(4 * SLOW, |_| false);
    predicted(&mut term, "<C-b> over lines edited", b"\x02");
    predicted(&mut term, "<C-f> back", b"\x06");
    predicted(&mut term, "<C-f> past them", b"\x06");
}

/// An edit reaches the client as the lines it changed, not as every line the
/// client keeps; and pages scrolled over lines edited, by keys or otherwise,
/// above the view, in it and below it, are drawn as Neovim draws them.
#[test]
fn edits_reach_the_client_as_what_they_changed() {
    require_nvim!();
    let scratch = Scratch::new("client-edits");
    // Long enough lines that sending them all again shows.
    let (mut rpc, mut term, link) = drawn_slowly(
        &scratch,
        "client-edits",
        "call setline(1, map(range(1, 600), \
         '\"line \" . v:val . \" \" . repeat(\"abcdefghij\", 6)'))",
    );
    let before = link.back.load(Ordering::Relaxed);
    typed(&mut term, b"x");
    let cost = link.back.load(Ordering::Relaxed) - before;
    assert!(cost < 2000, "an edit of a character cost {cost} bytes");

    // Below the view, as a formatter or a language server would: the
    // cursor stays where it is.
    rpc.command(
        "call setline(30, 'edited 30') | call nvim_buf_set_lines(0, 39, 40, v:true, []) | \
         call append(50, ['new a', 'new b']) | call setline(25, 'edited 25')",
    )
    .expect("edit below");
    term.pump_until(4 * SLOW, |_| false);
    predicted(&mut term, "<C-f> over lines edited", b"\x06");
    predicted(&mut term, "<C-f> again", b"\x06");
    // In the view, by keys: a line deleted, one put back below, a new one.
    typed(&mut term, b"ddjpoopened");
    typed(&mut term, b"\x1b");
    predicted(&mut term, "<C-b> over lines typed", b"\x02");
    predicted(&mut term, "<C-f> back", b"\x06");
    // Above the view: every line after moves.
    rpc.command(
        "call nvim_buf_set_lines(0, 0, 3, v:true, ['top a', 'top b', 'top c', 'top d', 'top e'])",
    )
    .expect("edit above");
    term.pump_until(4 * SLOW, |_| false);
    predicted(&mut term, "<C-b> after lines above changed", b"\x02");
    predicted(&mut term, "<C-b> to the top", b"\x02");
    // A substitute over a run of lines in view and out of it.
    typed(&mut term, b":%s/line 4/LINE 4/\r");
    for (name, keys) in [
        ("<C-f> after :s", &b"\x06"[..]),
        ("<C-f> again after :s", b"\x06"),
        ("<C-f> past them", b"\x06"),
    ] {
        predicted(&mut term, name, keys);
    }
}

/// The colours of the window's rows, cell by cell: what a prediction must
/// get right as well, now that it draws the editor's syntax colours.
fn colours(term: &Terminal) -> Vec<Vec<(vt100::Color, vt100::Color, bool, bool)>> {
    let screen = term.parser.screen();
    (0..22)
        .map(|r| {
            (0..COLS)
                .map(|c| {
                    screen.cell(r, c).map_or(
                        (vt100::Color::Default, vt100::Color::Default, false, false),
                        |x| (x.fgcolor(), x.bgcolor(), x.bold(), x.italic()),
                    )
                })
                .collect()
        })
        .collect()
}

/// Rows a scroll uncovers ahead of Neovim wear the colours the editor's
/// syntax highlighting gives them — lines edited as well, by the API or by
/// keys: when Neovim's own rows land, not a cell of the window changes, text
/// or colour.
#[test]
fn predicted_rows_wear_the_colours_neovim_gives_them() {
    require_nvim!();
    let scratch = Scratch::new("client-colours");
    let (mut rpc, mut term, _) = drawn_slowly(
        &scratch,
        "client-colours",
        "call setline(1, map(range(1, 600), \
         '\"local v\" . v:val . \" = \" . v:val . \" -- line \" . v:val')) | \
         set termguicolors | lua vim.treesitter.start(0, 'lua')",
    );
    let step = |term: &mut Terminal, name: &str, keys: &[u8]| {
        let before = window(term);
        term.type_bytes(keys);
        assert!(
            term.pump(SLOW, |t| window(t) != before),
            "{name} waited for Neovim:\n{}",
            term.text()
        );
        term.pump_until(Duration::from_millis(30), |_| false);
        let predicted = (window(term), colours(term));
        // Coloured, not plain: a keyword, a number and a comment apart, on
        // all but the lines edited whole into a comment or a string.
        let coloured = predicted
            .1
            .iter()
            .filter(|row| {
                let mut kinds: Vec<_> = row
                    .iter()
                    .take(20)
                    .map(|(fg, _, bold, _)| (*fg, *bold))
                    .collect();
                kinds.dedup();
                kinds.len() > 1
            })
            .count();
        assert!(coloured >= 19, "{name}: {coloured} rows of 22 coloured");
        term.pump_until(4 * SLOW, |_| false);
        let landed = (window(term), colours(term));
        assert_eq!(predicted.0, landed.0, "{name}: the text moved");
        for (r, (p, l)) in predicted.1.iter().zip(&landed.1).enumerate() {
            assert_eq!(p, l, "{name}: row {r} changed colour when Neovim's landed");
        }
    };
    for (name, keys) in [
        ("<C-f>", &b"\x06"[..]),
        ("<C-f> again", b"\x06"),
        ("<C-b>", b"\x02"),
        ("the wheel", b"\x1b[<65;10;5M"),
        ("3<C-e>", b"3\x05"),
    ] {
        step(&mut term, name, keys);
    }
    // Below the view, as a formatter would: a line made a comment, one a
    // string, the cursor where it was.
    rpc.command(
        "call nvim_buf_set_lines(0, 60, 62, v:true, \
         ['-- local v61 = 61 -- line 61', 'local s = [[line 62]] -- 62'])",
    )
    .expect("edit below");
    term.pump_until(4 * SLOW, |_| false);
    step(&mut term, "<C-f> over lines edited", b"\x06");
    // In the view, by keys: a comment opened below the cursor.
    typed(&mut term, b"o-- typed, local v = 1");
    typed(&mut term, b"\x1b");
    step(&mut term, "<C-f> past a line typed", b"\x06");
    step(&mut term, "<C-b> back over it", b"\x02");
}

/// Rows a scroll uncovers ahead of Neovim show what Neovim shows as
/// something it is not — a control character as `^X`, a tab as `^I` under
/// `'list'` with no `tab:` for it, an unprintable character as `<xx>` — in
/// the colours Neovim does, `SpecialKey`'s, and a tab `'list'` shows with
/// its `tab:` in `Whitespace`'s: when Neovim's own rows land, not a cell of
/// the window changes, text or colour.
#[test]
fn predicted_rows_show_what_neovim_shows_as_something_else_in_its_colours() {
    require_nvim!();
    let scratch = Scratch::new("client-special");
    let (mut rpc, mut term, _) = drawn_slowly(
        &scratch,
        "client-special",
        "call setline(1, map(range(1, 200), \
         '\"line \" . v:val . \" \\x01 \\x7f \\t \\u0080 end\"')) | \
         set termguicolors list listchars=nbsp:+",
    );
    let step = |term: &mut Terminal, name: &str, keys: &[u8]| {
        let before = window(term);
        term.type_bytes(keys);
        assert!(
            term.pump(SLOW, |t| window(t) != before),
            "{name} waited for Neovim:\n{}",
            term.text()
        );
        term.pump_until(Duration::from_millis(30), |_| false);
        let predicted = (window(term), colours(term));
        // Not one colour: `^A`, `^?` and `<80>` in another, on every row.
        let two = predicted
            .1
            .iter()
            .filter(|row| {
                let mut fgs: Vec<_> = row.iter().map(|(fg, ..)| *fg).collect();
                fgs.dedup();
                fgs.len() > 1
            })
            .count();
        assert_eq!(two, 22, "{name}: {two} rows of 22 in more than one colour");
        term.pump_until(4 * SLOW, |_| false);
        let landed = (window(term), colours(term));
        assert_eq!(predicted.0, landed.0, "{name}: the text moved");
        for (r, (p, l)) in predicted.1.iter().zip(&landed.1).enumerate() {
            assert_eq!(p, l, "{name}: row {r} changed colour when Neovim's landed");
        }
    };
    for (name, keys) in [
        ("<C-f>", &b"\x06"[..]),
        ("<C-b>", b"\x02"),
        ("the wheel", b"\x1b[<65;10;5M"),
        ("3<C-e>", b"3\x05"),
    ] {
        step(&mut term, name, keys);
    }
    // The tab shown with 'listchars' now, as `>---`.
    rpc.command("set listchars=tab:>-,nbsp:+")
        .expect("listchars");
    term.pump_until(4 * SLOW, |_| false);
    for (name, keys) in [
        ("<C-f> with tab:>-", &b"\x06"[..]),
        ("<C-b> with tab:>-", b"\x02"),
    ] {
        step(&mut term, name, keys);
    }
}

/// Rows a scroll uncovers or moves sideways ahead of Neovim show what
/// `'listchars'` draws as Neovim draws it: blanks — spaces alone and in runs,
/// before a line's text and after it, no-break spaces, tabs before the text
/// and in it — and the line's end, and the edges of a view that does not
/// wrap: the last column where the line goes on, the first where the view is
/// scrolled sideways. With `'linebreak'`, the spaces before a line's text are
/// in `Whitespace` though `leadtab` is all there is for them. When Neovim's
/// own rows land, not a cell of the window changes, text or colour.
#[test]
fn predicted_rows_show_listchars_as_neovim_draws_them() {
    require_nvim!();
    let scratch = Scratch::new("client-listchars");
    // Four kinds of line, the fourth 77 columns wide: as wide as a window
    // that wraps fits, its end and all, and wider than one with a gutter.
    let (mut rpc, mut term, _) = drawn_slowly(
        &scratch,
        "client-listchars",
        r"lua local t = {} for i = 1, 200 do local k = i % 4 t[i] = k == 0 and ('  line ' .. i .. '  with  runs   ') or k == 1 and ('\t\tline ' .. i .. '\tend ') or k == 2 and (' \194\160line ' .. i .. '\226\128\175x ') or ('    line ' .. string.format('%-3d', i) .. ' ' .. string.rep('ab  ', 16)) end vim.api.nvim_buf_set_lines(0, 0, -1, false, t)",
    );
    rpc.command(
        "set termguicolors list \
         listchars=tab:>-,leadtab:\\|.,space:.,multispace:ab,lead:_,leadmultispace:12,\
         trail:~,nbsp:+,eol:$,extends:>,precedes:< \
         | hi Whitespace guifg=#5f87ff | hi NonText guifg=#87d75f",
    )
    .expect("listchars");
    term.pump_until(4 * SLOW, |_| false);
    let step = |term: &mut Terminal, name: &str, keys: &[u8]| {
        let before = window(term);
        term.type_bytes(keys);
        assert!(
            term.pump(SLOW, |t| window(t) != before),
            "{name} waited for Neovim:\n{}",
            term.text()
        );
        term.pump_until(Duration::from_millis(30), |_| false);
        let predicted = (window(term), colours(term));
        term.pump_until(4 * SLOW, |_| false);
        let landed = (window(term), colours(term));
        assert_eq!(predicted.0, landed.0, "{name}: the text moved");
        for (r, (p, l)) in predicted.1.iter().zip(&landed.1).enumerate() {
            assert_eq!(p, l, "{name}: row {r} changed colour when Neovim's landed");
        }
        landed.0 .0
    };
    for (name, keys) in [
        ("<C-f>", &b"\x06"[..]),
        ("<C-b>", b"\x02"),
        ("the wheel", b"\x1b[<65;10;5M"),
        ("3<C-e>", b"3\x05"),
        ("<C-y>", b"\x19"),
    ] {
        step(&mut term, name, keys);
    }
    // Without wrapping, with a gutter, on a line as wide as any: from column
    // 1, down and back; and sideways, past the end of the shorter lines.
    rpc.command("set nowrap number | call cursor(39, 1)")
        .expect("nowrap");
    term.pump_until(4 * SLOW, |_| false);
    let mut seen = Vec::new();
    for (name, keys) in [
        ("zl", &b"zl"[..]),
        ("<C-f> from column 1", b"\x06"),
        ("<C-b> from column 1", b"\x02"),
    ] {
        seen.extend(step(&mut term, name, keys));
    }
    rpc.command("call winrestview({'lnum': 39, 'col': 1, 'leftcol': 1})")
        .expect("view");
    term.pump_until(4 * SLOW, |_| false);
    for (name, keys) in [("zL", &b"zL"[..]), ("zH", b"zH"), ("zh", b"zh")] {
        seen.extend(step(&mut term, name, keys));
    }
    // Neovim drew both edges: `>` in the last column, `<` in the first after
    // the gutter.
    for (what, col, mark) in [("extends", COLS - 1, '>'), ("precedes", 4, '<')] {
        assert!(
            seen.iter()
                .any(|row| row.chars().nth(col.into()) == Some(mark)),
            "no {what} drawn: {seen:#?}"
        );
    }
    rpc.command("set listchars=tab:>-,leadtab:\\|.,eol:$ linebreak wrap nonumber")
        .expect("linebreak");
    term.pump_until(4 * SLOW, |_| false);
    for (name, keys) in [
        ("<C-f> with linebreak", &b"\x06"[..]),
        ("<C-b> with linebreak", b"\x02"),
    ] {
        step(&mut term, name, keys);
    }
}

/// The frames the client wrote from `from` on: each hides the cursor first.
fn frames_since(term: &Terminal, from: usize) -> usize {
    term.output[from..]
        .windows(HIDE_CURSOR.len())
        .filter(|w| *w == HIDE_CURSOR)
        .count()
}

const HIDE_CURSOR: &[u8] = b"\x1b[?25l";

/// A client nvmux has put aside (see `nvmux::card::Command::Away`) draws a
/// session that changes all the while about once a second, where in front it
/// draws every change it can; and told it is back in front, it draws as it
/// did at once.
#[test]
fn a_client_away_draws_a_busy_session_at_a_crawl() {
    require_nvim!();
    let scratch = Scratch::new("client-away");
    let (sock, mut rpc, config) = session(&scratch, "client-away");
    rpc.command("setlocal noswapfile | call setline(1, 'ready')")
        .expect("setline");
    let mut term = Terminal::spawn(&sock, &config);
    assert!(
        term.pump_until(Duration::from_secs(10), |s| s.contains("ready")),
        "the session never painted: {}",
        term.text()
    );
    // The first line, changed every 20 ms for as long as the test runs.
    rpc.command(
        "lua vim.g.ticks = 0; vim.fn.timer_start(20, function() \
         vim.g.ticks = vim.g.ticks + 1; \
         vim.api.nvim_buf_set_lines(0, 0, 1, false, { 'tick ' .. vim.g.ticks }) \
         end, { ['repeat'] = -1 })",
    )
    .expect("start the ticks");
    let mark = term.output.len();
    term.pump_until(Duration::from_secs(1), |_| false);
    let front = frames_since(&term, mark);
    assert!(front >= 10, "in front, {front} frames in a second of ticks");

    term.type_bytes(&nvmux::card::encode(&nvmux::card::Command::Away));
    // What was on its way as the client was told.
    term.pump_until(Duration::from_millis(100), |_| false);
    let mark = term.output.len();
    term.pump_until(Duration::from_secs(3), |_| false);
    let away = frames_since(&term, mark);
    assert!(
        (2..=5).contains(&away),
        "away, {away} frames in three seconds of ticks"
    );

    term.type_bytes(&nvmux::card::encode(&nvmux::card::Command::Clear));
    let mark = term.output.len();
    term.pump_until(Duration::from_secs(1), |_| false);
    let back = frames_since(&term, mark);
    assert!(
        back >= 10,
        "back in front, {back} frames in a second of ticks"
    );
    let ticks = rpc
        .eval("g:ticks")
        .ok()
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let shown: u64 = term
        .text()
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("tick "))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    assert!(
        ticks.saturating_sub(shown) < 10,
        "back in front, the screen says tick {shown} of {ticks}"
    );
}

/// A C file highlighted by tree-sitter, its macros each a tree injected into
/// it, drawn over a slow link: once it is all drawn and nothing is typed,
/// Neovim falls quiet. Every parse of other rows than the last remakes or
/// lets go of the injected trees of both, and says so; the agent's parse of
/// the rows it colours and the highlighter's of the view must not keep each
/// other going, an editor busy for as long as the client is there.
#[test]
fn a_session_left_alone_falls_quiet_whatever_highlights_it() {
    require_nvim!();
    let scratch = Scratch::new("client-quiet");
    let (_rpc, mut term, link) = drawn_slowly(
        &scratch,
        "client-quiet",
        "call setline(1, map(range(1, 600), 'v:val % 3 \
         ? \"int v\" . v:val . \" = \" . v:val . \"; // line \" . v:val \
         : \"#define M\" . v:val . \" (\" . v:val . \" + 1) // line \" . v:val')) | \
         set filetype=c | lua vim.treesitter.start(0, 'c')",
    );
    // What a first push leaves to a moment later, and its colours.
    term.pump_until(4 * SLOW, |_| false);
    let before = link.back.load(Ordering::Relaxed);
    term.pump_until(Duration::from_secs(3), |_| false);
    let sent = link.back.load(Ordering::Relaxed) - before;
    assert!(
        sent < 4096,
        "Neovim sent {sent} bytes in 3 s with nothing typed and nothing to draw"
    );
}

/// A markdown file's lua code block, a long comment opened at its top by
/// keys typed one at a time: the rows of the block below the view, which the
/// client holds, are a comment in Neovim, and a page down predicted ahead of
/// it shows them as Neovim does. Only the code block's own tree, injected
/// into the markdown, says those rows changed, and it is parsed after the
/// markdown's: by the agent's push of the line edited, or by a later slice
/// of the highlighter's parse.
#[test]
fn a_comment_typed_into_a_code_block_colours_the_rows_held_below_it() {
    require_nvim!();
    let scratch = Scratch::new("client-md-block");
    let (_rpc, mut term, _) = drawn_slowly(
        &scratch,
        "client-md-block",
        "call setline(1, map(range(1, 360), 'v:val % 6 == 1 ? \"## line \" . v:val \
         : v:val % 6 == 3 ? \"Some *text* with `code` and **bold**, line \" . v:val \
         : v:val % 6 == 5 ? \"- item \" . v:val : \"\"')) | \
         call append(60, ['```lua'] + map(range(1, 80), 'v:val == 50 \
         ? \"x = 1 -- ]] closes\" : \"local v\" . v:val . \" = \" . v:val . \" -- code \" . v:val') \
         + ['```']) | \
         set termguicolors filetype=markdown | lua vim.treesitter.start(0, 'markdown')",
    );
    typed(&mut term, b":62\r");
    term.pump_until(Duration::from_secs(2), |_| false);
    // As typed: each key a change of its own, Neovim's redraw after it.
    for k in [b"O", b"-", b"-", b"[", b"["] {
        term.type_bytes(k);
        term.pump_until(Duration::from_millis(300), |_| false);
    }
    typed(&mut term, b"\x1b");
    term.pump_until(Duration::from_secs(2), |_| false);
    let before = window(&term);
    term.type_bytes(b"\x06");
    assert!(
        term.pump(SLOW, |t| window(t) != before),
        "<C-f> waited for Neovim:\n{}",
        term.text()
    );
    term.pump_until(Duration::from_millis(30), |_| false);
    let predicted = (window(&term), colours(&term));
    term.pump_until(4 * SLOW, |_| false);
    let landed = (window(&term), colours(&term));
    assert_eq!(predicted.0, landed.0, "the text moved");
    let mut wrong = Vec::new();
    for (r, (p, l)) in predicted.1.iter().zip(&landed.1).enumerate() {
        if p != l {
            wrong.push((r, landed.0 .0[r].clone()));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} predicted rows changed colour when Neovim's landed: {wrong:#?}",
        wrong.len()
    );
}

/// What the client's agent sends, counted by kind in the editor as it goes:
/// a `:lua` command, the last of a line of them.
const COUNT_SENT: &str = "lua local send = vim.rpcnotify; _G.nvmux_sent = {}; \
     vim.rpcnotify = function(chan, method, kind, ...) \
       if method == 'nvmux_client' then \
         _G.nvmux_sent[kind] = (_G.nvmux_sent[kind] or 0) + 1 \
       end \
       return send(chan, method, kind, ...) \
     end";

/// How many of each kind the agent has sent: see [`COUNT_SENT`].
fn sent_by_kind(
    rpc: &mut nvmux::rpc::Client<std::os::unix::net::UnixStream>,
) -> std::collections::HashMap<String, i64> {
    let sent = rpc.eval("luaeval('_G.nvmux_sent')").expect("the count");
    sent.as_map()
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| Some((k.as_str()?.to_string(), v.as_i64()?)))
        .collect()
}

/// What predictions are drawn from — the lines either side of a view, their
/// colours, how a window scrolls — is sent only over a link slow enough to
/// predict over. A session on this machine scrolled page by page is sent
/// none of it, its client told to predict wherever the link is; the same
/// over a slow link is.
#[test]
fn only_a_link_slow_enough_to_predict_over_is_sent_lines() {
    require_nvim!();
    let setline =
        format!("call setline(1, map(range(1, 600), '\"line \" . v:val')) | {COUNT_SENT}");
    let pages = |term: &mut Terminal| {
        for _ in 0..3 {
            typed(term, b"\x06");
        }
    };
    let predictions = ["lines", "spans", "shape", "keep", "splice"];

    let fast = Scratch::new("client-fastlines");
    let (sock, mut rpc, config) = session(&fast, "client-fastlines");
    std::fs::write(&config, PREDICT_ON).expect("write the config");
    rpc.command(&format!("setlocal noswapfile | {setline}"))
        .expect("setline");
    let mut term = Terminal::spawn(&sock, &config);
    assert!(
        term.pump_until(Duration::from_secs(20), |s| s.contains("line 1")),
        "never drawn:\n{}",
        term.text()
    );
    pages(&mut term);
    assert!(
        top_line(&term.text()).is_some_and(|top| top > 40),
        "not paged down:\n{}",
        term.text()
    );
    let sent = sent_by_kind(&mut rpc);
    for kind in predictions {
        assert_eq!(
            sent.get(kind),
            None,
            "a fast link was sent {kind}: {sent:?}"
        );
    }

    let slow = Scratch::new("client-slowlines");
    let (mut rpc, mut term, _) = drawn_slowly(&slow, "client-slowlines", &setline);
    pages(&mut term);
    let sent = sent_by_kind(&mut rpc);
    assert!(
        sent.get("lines").is_some_and(|&n| n > 0) && sent.get("shape").is_some_and(|&n| n > 0),
        "a slow link was not sent its lines: {sent:?}"
    );
}

/// How many autocommands the client's agent has in the editor, and how many
/// of them wait to be told how far either side of a view to send lines —
/// which only an agent left to predict with does.
const AGENT_AUTOCMDS: &str = "local all = vim.tbl_filter(function(a) \
       return vim.startswith(a.group_name or '', 'nvmux_client_') \
     end, vim.api.nvim_get_autocmds({})) \
     local reach = vim.tbl_filter(function(a) \
       return vim.startswith(a.pattern or '', 'nvmux_client_reach_') \
     end, all) \
     return { #all, #reach }";

/// See [`AGENT_AUTOCMDS`].
fn agent_autocmds(rpc: &mut nvmux::rpc::Client<std::os::unix::net::UnixStream>) -> (i64, i64) {
    let v = rpc
        .call(
            "nvim_exec_lua",
            vec![AGENT_AUTOCMDS.into(), Value::Array(vec![])],
        )
        .expect("ask the editor");
    let n = |i: usize| {
        v.as_array()
            .and_then(|a| a.get(i))
            .and_then(Value::as_i64)
            .unwrap_or(0)
    };
    (n(0), n(1))
}

/// Scrolls are predicted for a session over ssh unless told otherwise. A
/// client that nvmux says nothing to of where its session is — on this
/// machine — leaves its agent in the editor with nothing to predict with,
/// `predict` at its default; told `"on"`, it leaves it the means, as it does
/// at the default for a session it is told is on another machine.
#[test]
fn scrolls_are_predicted_for_a_session_over_ssh_unless_told() {
    require_nvim!();
    let over_ssh = nvmux::config::Drawing {
        remote: true,
        ..nvmux::config::Settings::default().drawing()
    };
    for (tag, config, told, predicting) in [
        ("client-auto-here", "", None, false),
        ("client-on-here", PREDICT_ON, None, true),
        ("client-auto-there", "", Some(&over_ssh), true),
    ] {
        let scratch = Scratch::new(tag);
        let (sock, mut rpc, path) = session(&scratch, tag);
        std::fs::write(&path, config).expect("write the config");
        rpc.command("setlocal noswapfile | call setline(1, 'drawn by the client')")
            .expect("setline");
        let mut term = match told {
            Some(drawing) => Terminal::spawn_told(&sock, &path, drawing),
            None => Terminal::spawn(&sock, &path),
        };
        let drawn = |s: &str| s.contains("drawn by the client");
        assert!(
            term.pump_until(Duration::from_secs(20), drawn),
            "{tag}: never drawn:\n{}",
            term.text()
        );
        assert!(
            common::wait_until(Duration::from_secs(5), || {
                term.pump_until(Duration::from_millis(20), |_| false);
                agent_autocmds(&mut rpc).0 > 0
            }),
            "{tag}: no agent in the editor"
        );
        assert_eq!(agent_autocmds(&mut rpc).1 > 0, predicting, "{tag}");
        assert_eq!(term.hang_up(), Some(0), "{tag}: the client's exit");
    }
}

/// A config for a wide terminal: every window's text centred, as wide as
/// what it shows wants, or else 60 columns of it. A session in one window,
/// its status line its own colour, and a terminal 160 columns wide.
fn wide(
    scratch: &Scratch,
    tag: &str,
) -> (
    Terminal,
    nvmux::rpc::Client<std::os::unix::net::UnixStream>,
    PathBuf,
) {
    wide_with(scratch, tag, "smart")
}

/// [`wide`], centred the way `centre` says.
fn wide_with(
    scratch: &Scratch,
    tag: &str,
    centre: &str,
) -> (
    Terminal,
    nvmux::rpc::Client<std::os::unix::net::UnixStream>,
    PathBuf,
) {
    let (sock, mut rpc, config) = session(scratch, tag);
    std::fs::write(
        &config,
        format!("[client]\ncentre = \"{centre}\"\nwindow_width = 60\n"),
    )
    .expect("write the config");
    rpc.command(
        "setlocal noswapfile | set laststatus=2 termguicolors \
         | hi StatusLine guibg=#123456 | hi StatusLineNC guibg=#123456 \
         | call setline(1, ['the left window'])",
    )
    .expect("set the session up");
    let term = Terminal::spawn_sized(&sock, &config, DA1_REPLY, (160, 12));
    (term, rpc, sock)
}

/// What the editor says of itself: `'columns'`, and each window's width.
fn widths(rpc: &mut nvmux::rpc::Client<std::os::unix::net::UnixStream>) -> Value {
    rpc.eval("[&columns, map(range(1, winnr('$')), 'winwidth(v:val)')]")
        .expect("ask the editor")
}

fn columns(n: i64, wins: &[i64]) -> Value {
    Value::Array(vec![
        n.into(),
        Value::Array(wins.iter().map(|w| Value::from(*w)).collect()),
    ])
}

/// Each window's text centred: Neovim has the whole terminal, and a window
/// wider than the text gets has it that wide in its middle, its status line's
/// words over it; the client gives the windows their width back as it goes.
#[test]
fn a_wide_window_has_its_text_centred_in_it() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-text");
    let (mut term, mut rpc, _) = wide(&scratch, "client-centre-text");
    // One window, the whole terminal wide, its text in the middle.
    let alone = format!("{}the left window", " ".repeat(50));
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.starts_with(&alone)),
        "the text is not in the middle of its window:\n{}",
        term.text()
    );
    // Split, two windows of about 80, each with its text 60 in its middle.
    rpc.command("vsplit").expect("vsplit");
    let own = |rpc: &mut nvmux::rpc::Client<std::os::unix::net::UnixStream>| -> Vec<usize> {
        rpc.eval("map(nvim_tabpage_list_wins(0), 'nvim_win_get_width(v:val)')")
            .expect("ask the editor")
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_u64)
                    .map(|w| w as usize)
                    .collect()
            })
            .unwrap_or_default()
    };
    assert!(
        common::wait_until(Duration::from_secs(5), || own(&mut rpc).len() == 2),
        "no split"
    );
    let w = own(&mut rpc);
    let (a, b) = ((w[0] - 60) / 2, w[0] + 1 + (w[1] - 60) / 2);
    let laid = |s: &str| {
        let first: Vec<char> = s.lines().next().unwrap_or("").chars().collect();
        let at = |n: usize| first.iter().skip(n).take(15).collect::<String>();
        at(a) == "the left window" && at(b) == "the left window"
    };
    assert!(
        term.pump_until(Duration::from_secs(5), laid),
        "the text is not in the middle of each window ({w:?}):\n{}",
        term.text()
    );
    assert_eq!(
        widths(&mut rpc),
        columns(160, &[60, 60]),
        "the text 60 wide"
    );
    let over = format!("{}[No Name]", " ".repeat(a));
    assert!(
        term.pump_until(Duration::from_secs(5), |s| s
            .lines()
            .nth(10)
            .is_some_and(|l| l.starts_with(&over))),
        "the status line's words are not over the text:\n{}",
        term.text()
    );

    assert_eq!(term.hang_up(), Some(0), "the client's exit");
    assert!(
        common::wait_until(Duration::from_secs(5), || {
            let w = widths(&mut rpc);
            w.as_array()
                .and_then(|a| a.get(1))
                .and_then(Value::as_array)
                .is_some_and(|ws| ws.iter().all(|w| w.as_i64().unwrap_or(0) > 60))
        }),
        "the windows were not given their width back: {}",
        widths(&mut rpc)
    );
}

/// What a client centring each window's text leaves the next to attach: the
/// windows' grids, given their width back as it went, and holding what was
/// drawn on them narrower unless Neovim drew them again before the client
/// was gone. Of a grid it kept, Neovim sends the next client only the cells
/// that differ from what it holds: none, or those of a line that wrapped
/// narrower and what that moved. A client centring nothing draws every
/// window whole all the same.
#[test]
fn the_next_client_draws_whole_what_a_centring_one_left() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-left");
    let (mut term, mut rpc, sock) = wide(&scratch, "client-centre-left");
    // Split once it is drawn: split before the client attaches, the split is
    // laid out for Neovim's own width, and the client's widens the right.
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.contains("the left window")),
        "never drawn:\n{}",
        term.text()
    );
    rpc.command(
        "set filetype=rust number textwidth=0 | syntax on \
         | call setline(1, ['fn clean() {', \
             '    // A thousand at a time, so a backlog never locks the table.', \
             '    let mut deleted = 0;', '    loop {', '        deleted += 1;', \
             '    }', '}']) \
         | vsplit",
    )
    .expect("set the session up");
    assert!(
        common::wait_until(Duration::from_secs(5), || {
            term.pump_until(Duration::from_millis(20), |_| false);
            widths(&mut rpc)
                .as_array()
                .and_then(|a| a.get(1))
                .and_then(Value::as_array)
                .is_some_and(|ws| ws.iter().all(|w| w.as_i64() == Some(60)))
        }),
        "never narrowed: {}\n{}",
        widths(&mut rpc),
        term.text()
    );
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s
            .matches("let mut deleted")
            .count()
            == 2),
        "never drawn:\n{}",
        term.text()
    );
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
    assert!(
        common::wait_until(Duration::from_secs(5), || {
            let w = widths(&mut rpc);
            w.as_array()
                .and_then(|a| a.get(1))
                .and_then(Value::as_array)
                .is_some_and(|ws| ws.iter().all(|w| w.as_i64().unwrap_or(0) > 60))
        }),
        "the windows were not given their width back: {}",
        widths(&mut rpc)
    );

    let config = scratch.0.join("config.toml");
    std::fs::write(&config, "[client]\ncentre = \"off\"\n").expect("write the config");
    let mut term = Terminal::spawn_sized(&sock, &config, DA1_REPLY, (160, 12));
    let whole = |s: &str| {
        let lines: Vec<&str> = s.lines().collect();
        [
            "fn clean() {",
            "so a backlog never locks the table.",
            "let mut deleted = 0;",
            "loop {",
            "deleted += 1;",
        ]
        .iter()
        .enumerate()
        .all(|(row, text)| lines.get(row).is_some_and(|l| l.matches(text).count() == 2))
    };
    assert!(
        term.pump_until(Duration::from_secs(15), whole),
        "not every window drawn whole:\n{}",
        term.text()
    );
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

/// What a client centring each window's text leaves the next to attach when
/// it is killed outright — by nvmux, still there two seconds after it was
/// hung up, or by a crash — rather than let go: the windows asked to be
/// narrower, and narrower for every UI, for Neovim keeps a grid as narrow as
/// it was asked to be until a UI asks for it whole. The next client with
/// windows of their own asks for every one whole that it does not narrow
/// itself: one centring nothing, for every one.
#[test]
fn the_next_client_gives_back_the_width_a_killed_one_took() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-kill");
    let (mut term, mut rpc, sock) = wide(&scratch, "client-centre-kill");
    // Split once it is drawn: split before the client attaches, the split is
    // laid out for Neovim's own width, and the client's widens the right.
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.contains("the left window")),
        "never drawn:\n{}",
        term.text()
    );
    rpc.command("vsplit").expect("vsplit");
    let narrow = columns(160, &[60, 60]);
    assert!(
        until_widths(&mut term, &mut rpc, &narrow),
        "never narrowed: {}\n{}",
        widths(&mut rpc),
        term.text()
    );
    assert!(term.kill(), "the client outlived a SIGKILL");
    assert!(
        common::wait_until(Duration::from_secs(5), || rpc.list_uis().ok() == Some(0)),
        "the killed client stayed attached"
    );
    assert_eq!(
        widths(&mut rpc),
        narrow,
        "left narrow, with no UI to see it"
    );

    let config = scratch.0.join("config.toml");
    std::fs::write(&config, "[client]\ncentre = \"off\"\n").expect("write the config");
    let mut term = Terminal::spawn_sized(&sock, &config, DA1_REPLY, (160, 12));
    let whole = |w: &Value| -> Option<Vec<usize>> {
        let ws: Vec<usize> = w
            .as_array()?
            .get(1)?
            .as_array()?
            .iter()
            .filter_map(Value::as_u64)
            .map(|w| w as usize)
            .collect();
        (ws.len() == 2 && ws.iter().all(|w| *w > 60)).then_some(ws)
    };
    let mut given = None;
    assert!(
        common::wait_until(Duration::from_secs(15), || {
            term.pump_until(Duration::from_millis(20), |_| false);
            given = whole(&widths(&mut rpc));
            given.is_some()
        }),
        "the windows were not given their width back: {}\n{}",
        widths(&mut rpc),
        term.text()
    );
    // And drawn as wide: each window's text at its left edge, not in its
    // middle as it is on a grid narrower than the window.
    let w = given.unwrap_or_default();
    let laid = |s: &str| {
        let first: Vec<char> = s.lines().next().unwrap_or("").chars().collect();
        let at = |n: usize| first.iter().skip(n).take(15).collect::<String>();
        at(0) == "the left window" && at(w[0] + 1) == "the left window"
    };
    assert!(
        term.pump_until(Duration::from_secs(5), laid),
        "the windows are not drawn whole ({w:?}):\n{}",
        term.text()
    );
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

/// Over a slow link, a window split off another, each window's text
/// centred, has its text in the middle of the window from the first frame
/// it shows in: the client cuts the grid to the width it asks for at once,
/// rather than drawing the window's text from its edge until Neovim's
/// narrower grid comes, a round trip later. On its way in, the new window's
/// text only ever moves left, to where it goes — never back right, as it
/// did when the narrower grid came.
#[test]
fn a_split_over_a_slow_link_is_centred_from_its_first_frame() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-slow");
    let (sock, mut rpc, config) = session(&scratch, "client-centre-slow");
    std::fs::write(
        &config,
        format!("{PREDICT_ON}centre = \"smart\"\nwindow_width = 60\n"),
    )
    .expect("write the config");
    rpc.command(
        "setlocal noswapfile | set splitright \
         | call setline(1, map(range(1, 10), '\"MARK \" . v:val'))",
    )
    .expect("set the session up");
    let slow = slow_link(&scratch.0.join("link"), &sock, Duration::from_millis(80));
    let mut term = Terminal::spawn_sized(&slow, &config, DA1_REPLY, (160, 12));
    assert!(
        term.pump_until(Duration::from_secs(20), |s| s
            .starts_with(&format!("{}MARK 1", " ".repeat(50)))),
        "never drawn centred:\n{}",
        term.text()
    );

    rpc.command("vsplit").expect("vsplit");
    // The column the new window's text starts at on the first row, each
    // time it moves, as it flies in and once it has landed.
    let right = |t: &Terminal| -> Option<usize> {
        let text = t.text();
        let row = text.lines().next()?;
        let (at, _) = row.match_indices("MARK").nth(1)?;
        Some(row[..at].chars().count())
    };
    let mut seen: Vec<usize> = Vec::new();
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        term.pump_until(Duration::from_millis(5), |_| false);
        if let Some(col) = right(&term).filter(|col| seen.last() != Some(col)) {
            seen.push(col);
        }
    }
    let goes = rpc
        .eval("[win_screenpos(2)[1] - 1, nvim_win_get_width(win_getid(2))]")
        .expect("where the new window is");
    let goes: Vec<i64> = goes
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let want = usize::try_from(goes[0] + (goes[1] - 60) / 2).expect("a column");
    assert_eq!(seen.last(), Some(&want), "landed in its middle: {seen:?}");
    assert!(
        seen.windows(2).all(|w| w[1] <= w[0]),
        "moved back right on its way in: {seen:?}"
    );
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

/// A UI that keeps every window on grid 1 sees the windows whole: the client
/// attaching again without `ext_multigrid` lets go of the windows it had
/// narrowed, and once that UI has gone and the client has windows of its own
/// again, narrows them again.
#[test]
fn a_ui_without_windows_of_its_own_sees_them_whole() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-ui");
    let (mut term, mut rpc, sock) = wide(&scratch, "client-centre-ui");
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.contains("the left window")),
        "never drawn:\n{}",
        term.text()
    );
    assert!(
        until_widths(&mut term, &mut rpc, &columns(160, &[60])),
        "never narrowed: {}",
        widths(&mut rpc)
    );

    // A UI of no nvmux's, never read: see
    // `a_ui_without_windows_of_its_own_is_made_room_for`.
    let mut other = nvmux::rpc::Client::connect(&sock, Duration::from_secs(5)).expect("connect");
    other
        .call(
            "nvim_ui_attach",
            vec![
                Value::from(160),
                Value::from(12),
                Value::Map(vec![(Value::from("ext_linegrid"), Value::from(true))]),
            ],
        )
        .expect("attach a UI without ext_multigrid");
    assert!(
        until_widths(&mut term, &mut rpc, &columns(160, &[160])),
        "the window was left narrow: {}",
        widths(&mut rpc)
    );

    drop(other);
    assert!(
        until_widths(&mut term, &mut rpc, &columns(160, &[60])),
        "not narrowed again: {}",
        widths(&mut rpc)
    );
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

/// A window showing a terminal keeps its whole width with each window's text
/// centred: the program in it is told its window's width. The window beside
/// it has its text centred as before.
#[test]
fn a_window_showing_a_terminal_keeps_its_width() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-term");
    let (mut term, mut rpc, _) = wide(&scratch, "client-centre-term");
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.contains("the left window")),
        "never drawn:\n{}",
        term.text()
    );
    // The new window, on the left and in front, the terminal.
    rpc.command("vsplit | terminal")
        .expect("a terminal beside it");
    assert!(
        common::wait_until(Duration::from_secs(5), || {
            term.pump_until(Duration::from_millis(20), |_| false);
            widths(&mut rpc) == columns(160, &[80, 60])
        }),
        "the terminal narrowed, or the text not: {}\n{}",
        widths(&mut rpc),
        term.text()
    );
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

/// Wait for the editor to say `want` of itself (see [`widths`]), drawing
/// what the client sends as it does.
fn until_widths(
    term: &mut Terminal,
    rpc: &mut nvmux::rpc::Client<std::os::unix::net::UnixStream>,
    want: &Value,
) -> bool {
    common::wait_until(Duration::from_secs(5), || {
        term.pump_until(Duration::from_millis(20), |_| false);
        widths(rpc) == *want
    })
}

/// Judged by what it shows, a window's text is as wide as its buffer's
/// `'textwidth'`; as wide as nine in ten of its lines where they are not
/// wrapped, up to the whole window; and `window_width` where they are.
#[test]
fn a_windows_text_is_as_wide_as_what_it_shows_wants() {
    require_nvim!();
    let scratch = Scratch::new("client-smart");
    let (mut term, mut rpc, _) = wide(&scratch, "client-smart");
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.contains("the left window")),
        "never drawn:\n{}",
        term.text()
    );
    for (command, want) in [
        ("setlocal textwidth=40", columns(160, &[40])),
        (
            "setlocal textwidth=0 nowrap | call setline(1, repeat([repeat('x', 100)], 9) + ['y'])",
            columns(160, &[100]),
        ),
        (
            "call setline(1, repeat([repeat('x', 200)], 10)) | setlocal nowrap",
            columns(160, &[160]),
        ),
        ("setlocal wrap", columns(160, &[60])),
    ] {
        rpc.command(command).expect(command);
        assert!(
            until_widths(&mut term, &mut rpc, &want),
            "after {command}: {} not {want}",
            widths(&mut rpc)
        );
    }
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

/// A window whose text is in its middle already, as a start screen lays
/// itself out for its window's whole width, keeps that width — judged by
/// what it shows or not: on a grid narrower, it would be centred twice
/// over, and so off to the right. Lines set in from the left but not to the
/// middle, and lines not set in at all, are narrowed as any are.
fn a_window_centred_already_keeps_its_width(tag: &str, centre: &str) {
    let scratch = Scratch::new(tag);
    let (mut term, mut rpc, _) = wide_with(&scratch, tag, centre);
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.contains("the left window")),
        "never drawn:\n{}",
        term.text()
    );
    // 40 columns of it 60 in on the 160, and a shorter line in its middle.
    let screen = format!("{}{}", " ".repeat(60), "x".repeat(40));
    for (command, want) in [
        (
            "setlocal wrap | call setline(1, \
             [repeat(' ', 60) .. repeat('x', 40), '', repeat(' ', 70) .. repeat('y', 20)])",
            columns(160, &[160]),
        ),
        (
            "setlocal wrap | call setline(1, [repeat(' ', 10) .. repeat('x', 40)])",
            columns(160, &[60]),
        ),
        (
            "setlocal wrap | call setline(1, \
             [repeat(' ', 60) .. repeat('x', 40), repeat('z', 10)])",
            columns(160, &[60]),
        ),
    ] {
        rpc.command(command).expect(command);
        assert!(
            until_widths(&mut term, &mut rpc, &want),
            "after {command}: {} not {want}",
            widths(&mut rpc)
        );
        if want == columns(160, &[160]) {
            assert!(
                term.pump_until(Duration::from_secs(5), |s| s.starts_with(&screen)),
                "not in the middle of the window:\n{}",
                term.text()
            );
        }
    }
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

#[test]
fn a_window_centred_already_keeps_its_width_judged_by_what_it_shows() {
    require_nvim!();
    a_window_centred_already_keeps_its_width("client-centred", "smart");
}

#[test]
fn a_window_centred_already_keeps_its_width_judged_by_nothing() {
    require_nvim!();
    a_window_centred_already_keeps_its_width("client-centred-fx", "fixed");
}

/// Its width judged as a window's text always is, a Markdown window's is as
/// wide as its tables and code blocks, which wrapping would break, and no
/// wider for its paragraphs, which it does not — where this Neovim has its
/// Markdown parser to say which lines are which.
#[test]
fn a_markdown_window_has_room_for_its_tables() {
    require_nvim!();
    let scratch = Scratch::new("client-smart-md");
    let (mut term, mut rpc, _) = wide(&scratch, "client-smart-md");
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.contains("the left window")),
        "never drawn:\n{}",
        term.text()
    );
    let parser = rpc
        .eval("luaeval('select(2, pcall(vim.treesitter.language.add, \"markdown\")) == true')")
        .expect("ask the editor");
    if parser != Value::Boolean(true) {
        eprintln!("skipping: this Neovim has no Markdown parser");
        return;
    }
    let paragraph = "words ".repeat(40);
    rpc.command(&format!(
        "call setline(1, ['# Notes', '', '{paragraph}']) | setlocal filetype=markdown"
    ))
    .expect("a paragraph");
    assert!(
        until_widths(&mut term, &mut rpc, &columns(160, &[60])),
        "a paragraph, wrapped: {}",
        widths(&mut rpc)
    );
    let row = |c: char| format!("| {} |", c.to_string().repeat(86));
    rpc.command(&format!(
        "call append('$', ['', '{}', '|{}|', '{}', '{}']) | setlocal textwidth=0",
        row('h'),
        "-".repeat(88),
        row('a'),
        row('b'),
    ))
    .expect("a table");
    assert!(
        until_widths(&mut term, &mut rpc, &columns(160, &[90])),
        "room for the table: {}",
        widths(&mut rpc)
    );
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

/// Judged as nothing but `window_width`, with `centre = "fixed"`: a
/// `'textwidth'` is the buffer's own business.
#[test]
fn widths_judged_by_nothing_are_window_width() {
    require_nvim!();
    let scratch = Scratch::new("client-fixed");
    let (mut term, mut rpc, _) = wide_with(&scratch, "client-fixed", "fixed");
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.contains("the left window")),
        "never drawn:\n{}",
        term.text()
    );
    rpc.command("setlocal textwidth=40").expect("a textwidth");
    assert!(
        until_widths(&mut term, &mut rpc, &columns(160, &[60])),
        "{}",
        widths(&mut rpc)
    );
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

/// With every effect off and nothing predicted, a window's text is centred
/// all the same: a grid narrower than its window is a grid of the window's
/// own, which the client asks for with nothing else wanting them.
#[test]
fn a_windows_text_is_centred_with_every_effect_off() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-bare");
    let (sock, mut rpc, config) = session(&scratch, "client-centre-bare");
    std::fs::write(
        &config,
        "[client]\ncentre = \"fixed\"\nwindow_width = 60\n\n[effects]\nenabled = false\n",
    )
    .expect("write the config");
    rpc.command("setlocal noswapfile | call setline(1, ['the left window'])")
        .expect("set the session up");
    let mut term = Terminal::spawn_sized(&sock, &config, DA1_REPLY, (160, 12));
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.contains("the left window")),
        "never drawn:\n{}",
        term.text()
    );
    assert!(
        until_widths(&mut term, &mut rpc, &columns(160, &[60])),
        "{}",
        widths(&mut rpc)
    );
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

/// Whether each of the first `rows` rows of a screen shows `line 1` on, a
/// line a row, in each of `windows` side by side.
fn lines_in(screen: &str, rows: usize, windows: usize) -> bool {
    let lines: Vec<&str> = screen.lines().collect();
    (0..rows).all(|r| {
        let text = format!("line {}", r + 1);
        lines.get(r).is_some_and(|l| {
            let shown: Vec<&str> = l.split('│').map(str::trim_end).collect();
            shown.len() == windows && shown.iter().all(|s| *s == text)
        })
    })
}

/// A session of eleven lines drawn by a client `first` big — columns, rows —
/// in `windows` side by side, and then by one `second` big: drawn whole by
/// the second.
fn drawn_again_at(tag: &str, windows: usize, first: (u16, u16), second: (u16, u16)) {
    let scratch = Scratch::new(tag);
    let (sock, mut rpc, config) = session(&scratch, tag);
    rpc.command("setlocal noswapfile | call setline(1, map(range(1, 11), '\"line \" . v:val'))")
        .expect("setline");
    // Every window drawn on a screen `size` big: as many rows of text as
    // there are lines, or all but its status line and the command line's.
    let drawn = |size: (u16, u16), s: &str| lines_in(s, usize::from(size.1 - 2).min(11), windows);

    let mut term = Terminal::spawn_sized(&sock, &config, DA1_REPLY, first);
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.starts_with("line 1")),
        "the first client never drew it:\n{}",
        term.text()
    );
    // Split once it is drawn: split before the client attaches, the split is
    // laid out for Neovim's own width, and the client's widens the right.
    for _ in 1..windows {
        rpc.command("vsplit").expect("vsplit");
    }
    assert!(
        term.pump_until(Duration::from_secs(5), |s| drawn(first, s)),
        "the first client never drew every window:\n{}",
        term.text()
    );
    assert_eq!(term.hang_up(), Some(0), "the first client's exit");
    assert!(
        common::wait_until(Duration::from_secs(5), || rpc.list_uis().ok() == Some(0)),
        "the first client stayed attached"
    );

    let mut term = Terminal::spawn_sized(&sock, &config, DA1_REPLY, second);
    assert!(
        term.pump_until(Duration::from_secs(15), |s| drawn(second, s)),
        "the second client never drew every window whole:\n{}",
        term.text()
    );
    assert_eq!(term.hang_up(), Some(0), "the second client's exit");
}

/// A client on a terminal taller than the last one to draw the session.
/// Neovim keeps each window's grid from one UI to the next, and a resize
/// keeps the rows it had as they were: of a window the client is sent its
/// new place and the rows it gained, and nothing of the rows above them,
/// which have not changed. It draws every row of every window all the same.
#[test]
fn a_client_taller_than_the_last_draws_every_window_whole() {
    require_nvim!();
    drawn_again_at("client-taller", 2, (160, 12), (160, 20));
}

/// The same on a terminal wider than the last: of every row the client is
/// sent only the columns the window gained, to the right of its text.
#[test]
fn a_client_wider_than_the_last_draws_its_window_whole() {
    require_nvim!();
    drawn_again_at("client-wider", 1, (160, 12), (200, 12));
}

/// A program for a `:terminal`: frame after frame, every cell in a background
/// whose red and green are its own row and column and whose blue is the
/// frame — but for the first ten columns, which keep a blue of 255 and never
/// change. Every other cell of every frame is a pair of colours Neovim has not
/// seen, and so a highlight of its own: its table of them fills in a few
/// dozen frames, and full, Neovim clears it and gives its ids out again, part
/// way through drawing a frame as often as not. The cells that never change
/// it does not draw again then, and they go on naming ids it has given out.
const REFILL_LUA: &str = r"
local w, h = 80, 22
local tty = vim.uv.new_tty(1, false)
if tty then
  local tw, th = tty:get_winsize()
  w, h = tw or w, th or h
end
io.write('\27[?25l\27[?7l')
for f = 0, 249 do
  local out = {}
  for r = 0, h - 1 do
    out[#out + 1] = ('\27[%d;1H'):format(r + 1)
    for c = 0, w - 1 do
      out[#out + 1] = ('\27[48;2;%d;%d;%dm '):format(r, c, c < 10 and 255 or f)
    end
  end
  io.write(table.concat(out))
  io.flush()
  vim.uv.sleep(15)
end
";

/// Every frame the client draws of a `:terminal` filling Neovim's table of
/// highlights again and again (see [`REFILL_LUA`]) has each of the program's
/// cells in the colours the program gave it — the cells that never change
/// among them. Neovim's own TUI writes a cell's colours as the cell comes;
/// this client draws once a flush is whole, and drew a cell Neovim had not
/// drawn again since giving out its id in whatever the id had come to mean:
/// a flash of another cell's colours, every time the table was cleared.
#[test]
fn a_terminal_that_fills_neovims_highlights_is_drawn_in_its_own_colours() {
    require_nvim!();
    let scratch = Scratch::new("client-refill");
    let (sock, mut rpc, config) = session(&scratch, "client-refill");
    // Nothing fading or sliding: every colour on the screen is the program's.
    std::fs::write(&config, "[effects]\nenabled = false\n").expect("config");
    let program = scratch.0.join("refill.lua");
    std::fs::write(&program, REFILL_LUA).expect("write the program");
    rpc.command("setlocal noswapfile | set termguicolors | call setline(1, 'before the program')")
        .expect("setline");
    // The editor's own background, which is no colour of the program's.
    let normal = rpc
        .eval("synIDattr(synIDtrans(hlID('Normal')), 'bg#')")
        .expect("Normal's background");
    let normal = normal
        .as_str()
        .and_then(|bg| u32::from_str_radix(bg.trim_start_matches('#'), 16).ok());
    let mut term = Terminal::spawn(&sock, &config);
    let drawn = |s: &str| s.contains("before the program");
    assert!(
        term.pump_until(Duration::from_secs(20), drawn),
        "never drawn:\n{}",
        term.text()
    );
    rpc.command(&format!("terminal nvim --clean -l {}", program.display()))
        .expect("start the program");
    let (mut frames, mut seen, mut wrong) = (0, std::collections::HashSet::new(), Vec::new());
    let finished = term.frames(Duration::from_secs(60), |screen| {
        frames += 1;
        // The window's rows: the status line and the command line below
        // them are the editor's.
        for r in 0..ROWS - 2 {
            for c in 0..COLS {
                let Some(vt100::Color::Rgb(red, green, blue)) =
                    screen.cell(r, c).map(|cell| cell.bgcolor())
                else {
                    continue;
                };
                let rgb = u32::from(red) << 16 | u32::from(green) << 8 | u32::from(blue);
                if Some(rgb) == normal {
                    continue;
                }
                if (u16::from(red), u16::from(green)) != (r, c) {
                    wrong.push((frames, r, c, (red, green, blue)));
                } else if blue != 255 {
                    seen.insert(blue);
                }
            }
        }
        seen.contains(&249)
    });
    assert!(
        finished,
        "the program never drew its last frame: {} of its frames drawn",
        seen.len()
    );
    assert!(
        wrong.is_empty(),
        "{} cells of {frames} frames drawn in another cell's colours, the first \
         (frame, row, column, colour): {:?}",
        wrong.len(),
        &wrong[..wrong.len().min(8)]
    );
    // A highlight a cell for 70 cells a row, 22 rows a frame: some 43
    // frames fill Neovim's table.
    assert!(
        seen.len() >= 100,
        "only {} of the program's 250 frames were drawn: too few to fill Neovim's table twice",
        seen.len()
    );
}
