//! End-to-end: nvmux's own client (`[client] ui = "nvmux"`) drawing a real
//! session on a real pty. The client runs as `nvmux --client <sock>`; the test
//! plays the terminal, answering its start-up queries, emulating the screen
//! as `src/shadow.rs` does, and typing. Skipped without a usable `nvim`.
//!
//! A real server matters most for a client following another: a UI attaching
//! with `ext_multigrid` to a session another UI drew is sent none of its
//! windows, so the client must have Neovim redraw them; and while a UI that
//! keeps every window on grid 1 is attached, the client must do without
//! windows of its own.

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

/// The client's DA1 (its last start-up query) and a VT220's answer; the kitty
/// keyboard query and "supported, no flags set".
const DA1: &[u8] = b"\x1b[c";
const DA1_REPLY: &[u8] = b"\x1b[?62;22c";
/// A DA1 answer advertising OSC 52 (attribute 52), as kitty and others send.
const DA1_REPLY_OSC52: &[u8] = b"\x1b[?62;22;52c";
const KITTY_QUERY: &[u8] = b"\x1b[?u";
const KITTY_REPLY: &[u8] = b"\x1b[?0u";

struct Terminal {
    child: std::process::Child,
    /// Set once the child is seen to exit; it must not be signalled or waited
    /// for after that.
    exited: Option<u32>,
    /// Once hung up, queries go unanswered: the client does not wait for
    /// answers on its way out, and a write after it exits fails on macOS.
    /// Neovim queries the terminal whenever a UI attaches, so one can arrive
    /// as the client exits.
    hung_up: bool,
    /// The master. Held so the pty outlives the child, which `Drop` reaps
    /// first.
    pty: Pty,
    output: Vec<u8>,
    parser: vt100::Parser,
    /// How far into `output` the parser has read: all but a trailing partial
    /// character (see [`whole`]).
    fed: usize,
    /// How far `output` has been scanned for queries.
    answered: usize,
    da1: &'static [u8],
    /// Columns, rows.
    size: (u16, u16),
    incoming: mpsc::Receiver<Vec<u8>>,
}

impl Terminal {
    /// `nvmux --client <sock>` on a fresh pty, with the config at `config`.
    fn spawn(sock: &Path, config: &Path) -> Self {
        Self::spawn_answering(sock, config, DA1_REPLY)
    }

    /// [`Terminal::spawn`], answering DA1 with `da1`.
    fn spawn_answering(sock: &Path, config: &Path, da1: &'static [u8]) -> Self {
        Self::spawn_sized(sock, config, da1, (COLS, ROWS))
    }

    /// [`Terminal::spawn_answering`]; `size` is columns, rows.
    fn spawn_sized(sock: &Path, config: &Path, da1: &'static [u8], size: (u16, u16)) -> Self {
        Self::start(sock, config, None, da1, size)
    }

    /// [`Terminal::spawn`], given the drawing settings nvmux passes a client
    /// it starts (used instead of the config), including whether the session
    /// is remote.
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
            // No palette from a parent nvmux, so the client uses its own
            // whatever the test environment has.
            .env_remove(nvmux::client::PALETTE_ENV);
        let cmd = match told {
            Some(drawing) => cmd.env(nvmux::client::SETTINGS_ENV, drawing.to_env()),
            None => cmd.env_remove(nvmux::client::SETTINGS_ENV),
        };
        // Takes the slave so the master sees EOF when the child exits.
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

    /// Read what the client draws for up to `within`, answering its queries,
    /// until `done` holds of the screen. Returns whether it did.
    fn pump_until(&mut self, within: Duration, done: impl Fn(&str) -> bool) -> bool {
        self.pump(within, |t| done(&t.text()))
    }

    /// [`Terminal::pump_until`], until the client has written `needle`.
    fn pump_until_written(&mut self, within: Duration, needle: &[u8]) -> bool {
        self.pump(within, |t| find(&t.output, needle).is_some())
    }

    fn pump(&mut self, within: Duration, done: impl Fn(&Self) -> bool) -> bool {
        let deadline = Instant::now() + within;
        // Feed what frames() left of an unfinished frame.
        self.feed();
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

    /// Feed the parser new output, up to the last whole character.
    fn feed(&mut self) {
        let end = self.fed + whole(&self.output[self.fed..]);
        self.parser.process(&self.output[self.fed..end]);
        self.fed = end;
    }

    /// Like [`Terminal::pump_until`], but calls `each` with the screen at the
    /// end of every frame (`SYNC_END`) until it returns true. Returns whether
    /// it did. A trailing partial frame is left unparsed, so the screen is
    /// never half a frame.
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
                    // A frame never ends inside a character.
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
        enough
    }

    /// Answer new queries in order. A query may straddle two reads, so the
    /// scan starts 8 bytes back and counts only matches ending past it.
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

    /// Every cell, row by row: its text (a blank if none, as for an erased
    /// cell), colours and attributes.
    fn cells(&self) -> Vec<String> {
        let screen = self.parser.screen();
        let mut cells = Vec::new();
        for row in 0..self.size.1 {
            for col in 0..self.size.0 {
                let cell = screen.cell(row, col).expect("on the screen");
                let text = if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                };
                cells.push(format!(
                    "{text}{:?}{:?}{}{}{}{}{}{}",
                    cell.fgcolor(),
                    cell.bgcolor(),
                    cell.bold(),
                    cell.dim(),
                    cell.italic(),
                    cell.underline(),
                    cell.inverse(),
                    cell.is_wide()
                ));
            }
        }
        cells
    }

    /// SIGHUP the client, as nvmux retires one; returns its exit code.
    fn hang_up(&mut self) -> Option<u32> {
        self.signal(Signal::SIGHUP)
    }

    /// SIGKILL the client, as nvmux does to one still there 2 s after a hang
    /// up, or as a crash would. Returns whether it is gone.
    fn kill(&mut self) -> bool {
        self.signal(Signal::SIGKILL).is_some()
    }

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
        // Collect its last output.
        self.pump_until(Duration::from_millis(200), |_| false);
        self.exited
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

/// The length of `bytes` minus any trailing partial UTF-8 character. `vte`
/// (under `vt100`) completes a partial character with up to three following
/// bytes, and if those start another, drops what lay between: a read ending
/// inside the `é` of `é ─` (as pty reads on macOS do) loses the space and
/// shifts the rest of the row a cell left.
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

/// A session for one test, a connection to it, and an empty config file, so
/// the defaults apply whatever the machine's config says.
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

/// Hung up, the client leaves the alternate screen.
#[test]
fn the_client_draws_a_session_and_types_into_it() {
    require_nvim!();
    let scratch = Scratch::new("client-draws");
    let (sock, mut rpc, config) = session(&scratch, "client-draws");
    let marker = "drawn by nvmux's own client";
    // No swap file: tests run in parallel, each with an unnamed buffer.
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

/// A card (`nvmux::card`), sent in the client's input, is drawn in the margin
/// over the editor and never reaches it as keys. It survives repaints. When
/// its life ends its words go and its sign stays at the edge until dropped.
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
    // Inset two columns from the right; the outer one is past the text, so
    // blank.
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

    // `:mode` repaints everything; the card must come back with it.
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

/// The session waits on a half-typed `g` (also what a crashed RPC callback
/// leaves). Neovim holds the attach until a key arrives; the client forwards
/// keys from the start but types none itself. The relay's side is in
/// `tests/local_lifecycle.rs`.
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
    // Long enough to attach, were the session answering.
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

/// Neovim sends a UI attaching after another none of the windows; the second
/// client must still show the split, a bordered float, and an unseen tab page.
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

/// Read until the client is quiet for `quiet`, for up to `within`. Returns
/// whether it went quiet.
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

/// After scrolls, typing, colours changing along the line and wide
/// characters, every cell (colours and attributes included) matches a fresh
/// client's. Cells the terminal blanked while scrolling must equal spaces.
#[test]
fn a_client_draws_the_screen_a_fresh_one_does() {
    require_nvim!();
    let scratch = Scratch::new("client-afresh");
    let (sock, mut rpc, config) = session(&scratch, "client-afresh");
    rpc.command(
        "setlocal noswapfile number cursorline | \
         call setline(1, map(range(1, 300), \
         {_, n -> printf('%d 漢字 é ─ %s', n, repeat('ab cd ', n % 9))})) | \
         call matchadd('Error', 'ab')",
    )
    .expect("setline");

    let mut first = Terminal::spawn(&sock, &config);
    assert!(
        first.pump_until(Duration::from_secs(15), |s| s.contains("é ─ ab cd")),
        "never drawn:\n{}",
        first.text()
    );
    for keys in [&b"\x04"[..], b"jjjj", b"A typed here\x1b", b"\x06", b"\x15"] {
        first.type_bytes(keys);
        assert!(
            settle(
                &mut first,
                Duration::from_millis(400),
                Duration::from_secs(10)
            ),
            "never settled after {keys:?}"
        );
    }
    assert!(
        find(&first.output, b"\x1b[r").is_some(),
        "no frame of the first client's moved down by LF"
    );
    assert!(
        find(&first.output, b"M\x1b[r").is_some(),
        "no rows of the first client's moved by the terminal"
    );
    let (drawn, text) = (first.cells(), first.text());
    assert_eq!(first.hang_up(), Some(0));
    assert!(
        common::wait_until(Duration::from_secs(5), || rpc.list_uis().ok() == Some(0)),
        "the first client stayed attached"
    );

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
        drawn == fresh.cells(),
        "the first client's screen:\n{text}\nis not the fresh one's:\n{}",
        fresh.text()
    );
}

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

/// A yank to `+` reaches the terminal as OSC 52. Neovim enables that only
/// after asking the terminal, and asks only through a UI that says it is on a
/// tty, as the client does.
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
    // Asked and answered through the client.
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

    // Named explicitly: an installed provider (pbcopy, xclip) would win
    // otherwise.
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

/// While a UI without `ext_multigrid` is attached, the client draws
/// everything on grid 1, and takes its own windows back once that UI goes.
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

    // A foreign UI, never read from; Neovim tolerates that for this long.
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

/// As animate.nvim draws it: with `'splitright'` the new window starts as a
/// sliver at the right edge and its separator moves left frame by frame.
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
    // Let the client's in-editor agent answer first.
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

    // The separator's column in every frame, in order.
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

/// A socket relaying to `to` with `delay` added each way. Each connection
/// gets its own to `to`, as an ssh forward would.
fn slow_link(dir: &Path, to: &Path, delay: Duration) -> PathBuf {
    counted_link(dir, to, delay).0
}

/// [`slow_link`], and what it carries (see [`Link`]).
fn counted_link(dir: &Path, to: &Path, delay: Duration) -> (PathBuf, Link) {
    use std::os::unix::net::{UnixListener, UnixStream};
    /// One direction of connection `conn`: read as it comes, written `delay`
    /// later.
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

struct Link {
    /// Bytes carried back from the session.
    back: Arc<AtomicUsize>,
    trace: Trace,
}

/// Everything a slow link passed on, for timing tests to say where time went.
#[derive(Clone)]
struct Trace {
    delay: Duration,
    carried: Arc<std::sync::Mutex<Vec<Carried>>>,
}

/// A line number from Neovim's side, with when the link read and wrote it
/// (see [`Trace::views_and_lines`]).
type Said = (Instant, Instant, u32);

/// One read a slow link passed on.
struct Carried {
    /// The link's connection index, from 0.
    conn: usize,
    /// Session to client.
    back: bool,
    read: Instant,
    written: Instant,
    bytes: Vec<u8>,
}

impl Trace {
    /// Messages passed one way on every connection, in write order, each with
    /// when its last part was read and written.
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
                    // Incomplete: wait for the next read.
                    Err(_) => break,
                }
            }
            let used = bytes.len() - rest.len();
            bytes.drain(..used);
        }
        out
    }

    /// The top line of each view Neovim drew, and the end line of each push
    /// of the agent's lines, with read and write times.
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

    /// The agent's acks (`nvmux::client::predict::Ack`) with write times:
    /// `[count, tail, mode, win, buf, tick, line_count, lnum, col, want, top,
    /// left]`.
    fn acks(&self) -> Vec<(Instant, Vec<Value>)> {
        self.messages(true)
            .into_iter()
            .filter_map(|(_, written, v)| match notification(&v) {
                Some(("nvmux_client", [kind, rest @ ..])) if kind.as_str() == Some("ack") => {
                    Some((written, rest.to_vec()))
                }
                _ => None,
            })
            .collect()
    }

    /// Keys the client sent, counted as Neovim's notation does: a character,
    /// or a name in `<…>`.
    fn inputs(&self) -> usize {
        self.messages(false)
            .iter()
            .filter_map(|(_, _, v)| match notification(v) {
                Some(("nvim_input", [keys])) => keys.as_str(),
                _ => None,
            })
            .map(|keys| nvmux::client::predict::tokens(keys).count())
            .sum()
    }

    /// The link's worst extra lateness beyond `delay`: to the session, and
    /// back.
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

fn notification(v: &Value) -> Option<(&str, &[Value])> {
    let v = v.as_array()?;
    if v.first()?.as_u64()? != 2 {
        return None;
    }
    Some((v.get(1)?.as_str()?, v.get(2)?.as_array()?))
}

/// `N` from the top row of a screen of `line N`s.
fn top_line(screen: &str) -> Option<u32> {
    screen
        .lines()
        .next()?
        .strip_prefix("line ")?
        .trim()
        .parse()
        .ok()
}

/// When Neovim's own scroll lands, nothing on the screen moves back.
#[test]
fn a_turn_of_the_wheel_over_a_slow_link_scrolls_before_neovim_answers() {
    require_nvim!();
    let scratch = Scratch::new("client-predict");
    let (sock, mut rpc, config) = session(&scratch, "client-predict");
    rpc.command("setlocal noswapfile | call setline(1, map(range(1, 200), '\"line \" . v:val'))")
        .expect("setline");
    // A 1 s round trip: Neovim's answer cannot be on screen sooner.
    let delay = Duration::from_millis(500);
    let slow = slow_link(&scratch.0.join("link"), &sock, delay);

    let scrolls = format!("[client]\n{SCROLLS}");
    let mut term = Terminal::spawn_told(&slow, &config, &over_ssh(&scrolls));
    assert!(
        term.pump_until(Duration::from_secs(20), |s| top_line(s) == Some(1)),
        "never drawn:\n{}",
        term.text()
    );
    // The agent's lines arrive a round trip or two after the first paint.
    term.pump_until(Duration::from_secs(4), |_| false);

    let from = term.output.len();
    let turned = Instant::now();
    term.type_bytes(b"\x1b[<65;10;5M");
    // Three lines down: the bottom row was never shown, so only the agent's
    // lines can draw it.
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
    // Let Neovim's scroll arrive over the link.
    term.pump_until(Duration::from_secs(2), |_| false);
    assert_eq!(top_line(&term.text()), Some(4), "{}", term.text());

    // An edit above the view: the agent resends the lines, and the next wheel
    // turn is drawn from those.
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

    // Every frame since the first turn: only down, except for the edit.
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

/// The window's rows and the cursor: what a prediction must get right.
fn window(term: &Terminal) -> (Vec<String>, (u16, u16)) {
    let rows = term.text().lines().take(22).map(String::from).collect();
    (rows, term.parser.screen().cursor_position())
}

/// One-way link delay for the prediction tests.
const SLOW: Duration = Duration::from_millis(250);

/// The drawing settings for a remote session, from `settings`. A client
/// predicts only for remote sessions; told this, it ignores its config file.
fn over_ssh(settings: &str) -> nvmux::config::Drawing {
    let settings: nvmux::config::Settings = toml::from_str(settings).expect("the settings");
    nvmux::config::Drawing {
        remote: true,
        ..settings.drawing()
    }
}

/// A session with `setline`'s text, drawn over a slow link by a client that
/// predicts scrolls only ([`SCROLLS`]), with the scroll effect off so a
/// prediction can be read off the screen at once. Returns the link too.
fn drawn_slowly(
    scratch: &Scratch,
    tag: &str,
    setline: &str,
) -> (
    nvmux::rpc::Client<std::os::unix::net::UnixStream>,
    Terminal,
    Link,
) {
    drawn_slowly_with(scratch, tag, setline, SCROLLS)
}

/// [`drawn_slowly`], with `client` among the client's settings.
fn drawn_slowly_with(
    scratch: &Scratch,
    tag: &str,
    setline: &str,
    client: &str,
) -> (
    nvmux::rpc::Client<std::os::unix::net::UnixStream>,
    Terminal,
    Link,
) {
    let settings = format!("[client]\n{client}\n[effects.scroll]\nenabled = false\n");
    drawn_slowly_as(scratch, tag, setline, &settings)
}

/// [`drawn_slowly`] with the whole client config `settings` (the scroll
/// effect as it says), told the session is remote ([`over_ssh`]).
fn drawn_slowly_as(
    scratch: &Scratch,
    tag: &str,
    setline: &str,
    settings: &str,
) -> (
    nvmux::rpc::Client<std::os::unix::net::UnixStream>,
    Terminal,
    Link,
) {
    let (sock, mut rpc, config) = session(scratch, tag);
    rpc.command(&format!("setlocal noswapfile | {setline}"))
        .expect("setline");
    std::fs::write(&config, settings).expect("config");
    let (slow, link) = counted_link(&scratch.0.join("link"), &sock, SLOW);

    let mut term = Terminal::spawn_told(&slow, &config, &over_ssh(settings));
    assert!(
        term.pump_until(Duration::from_secs(20), |s| s.contains("line 1")),
        "never drawn:\n{}",
        term.text()
    );
    // Wait for the agent's lines and the answer to the attach's fence.
    term.pump_until(Duration::from_secs(3), |_| false);
    (rpc, term, link)
}

/// Type `keys`, a scroll the client predicts, and check that Neovim's own
/// frame matches the prediction: every row of the window and the cursor.
fn predicted(term: &mut Terminal, name: &str, keys: &[u8]) {
    let predicted = sight(&ahead(term, name, keys));
    // Neovim's frame, after a round trip.
    let landed = last_frame(term, 4 * SLOW);
    assert_eq!(
        predicted,
        sight(&landed),
        "{name}: predicted, then drawn by Neovim"
    );
}

/// Type `keys`, which the client draws ahead of Neovim, and return the
/// screen after the first frame that changed the window (plus any drawn right
/// after), within one link delay, before Neovim has the keys. Whole frames
/// only: a busy machine delivers frames in pieces.
fn ahead(term: &mut Terminal, name: &str, keys: &[u8]) -> vt100::Screen {
    let before = window(term);
    term.type_bytes(keys);
    let mut drawn = None;
    term.frames(SLOW, |screen| {
        if drawn.is_some() || sight(screen) != before {
            drawn = Some(screen.clone());
        }
        drawn.is_some()
    });
    let Some(mut drawn) = drawn else {
        panic!("{name} waited for Neovim:\n{}", term.text());
    };
    term.frames(Duration::from_millis(30), |screen| {
        drawn = screen.clone();
        false
    });
    drawn
}

/// The screen after the last whole frame within `within`, or as it was if
/// none came.
fn last_frame(term: &mut Terminal, within: Duration) -> vt100::Screen {
    let mut last = term.parser.screen().clone();
    term.frames(within, |screen| {
        last = screen.clone();
        false
    });
    last
}

/// Type `keys` the client leaves to Neovim, and wait for the result.
fn typed(term: &mut Terminal, keys: &[u8]) {
    term.type_bytes(keys);
    term.pump_until(4 * SLOW, |_| false);
}

#[test]
fn every_scroll_predicted_is_the_one_neovim_makes() {
    require_nvim!();
    let scratch = Scratch::new("client-scrolls");
    // Indented 0-3 blanks, so moves to the first non-blank are visible.
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

    // A count names the line to put at the top, middle or bottom.
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

/// With motions predicted, the agent acks each time Neovim waits for a key
/// with all typed keys taken: the count taken and the cursor. Digits count as
/// taken, a mapping's keys count as typed, and nothing is acked while a key
/// may still start a mapping. The acked cursor is Neovim's.
#[test]
fn the_agent_acks_each_key_neovim_takes() {
    require_nvim!();
    let scratch = Scratch::new("client-acks");
    let (mut rpc, mut term, link) = drawn_slowly_with(
        &scratch,
        "client-acks",
        "call setline(1, map(range(1, 200), '\"line \" . v:val'))",
        MOTIONS,
    );
    // Long enough that `j` still waits when `k` arrives over the link.
    rpc.command("inoremap jk <Esc> | set timeoutlen=5000")
        .expect("the mapping");
    let count = |link: &Link| {
        link.trace
            .acks()
            .last()
            .and_then(|(_, a)| a.first().and_then(Value::as_u64))
            .expect("an ack")
    };
    // Where the last ack has the cursor, and where Neovim has it.
    let check = |rpc: &mut nvmux::rpc::Client<std::os::unix::net::UnixStream>, link: &Link| {
        let acks = link.trace.acks();
        let (_, a) = acks.last().expect("an ack");
        let at = (a[7].as_i64().expect("lnum"), a[8].as_i64().expect("col"));
        let cursor = rpc
            .call("nvim_win_get_cursor", vec![0.into()])
            .expect("the cursor");
        let cursor = cursor.as_array().expect("line and column");
        let there = (
            cursor[0].as_i64().expect("line") - 1,
            cursor[1].as_i64().expect("column"),
        );
        assert_eq!(at, there, "the ack's cursor is Neovim's");
        a[2].as_str().expect("mode").to_string()
    };
    let from = (count(&link), link.trace.inputs());
    // Counted from what the link has carried so far.
    let mut taken = |term: &mut Terminal, keys: &[u8], by: u64, mode: &str| {
        let was = count(&link);
        typed(term, keys);
        assert_eq!(count(&link), was + by, "{keys:?}");
        assert_eq!(check(&mut rpc, &link), mode, "{keys:?}");
    };
    taken(&mut term, b"3jl", 3, "n");
    taken(&mut term, b"i", 1, "i");
    // `j` waits for a possible `jk`: not taken, so no ack.
    let heard = link.trace.acks().len();
    term.type_bytes(b"j");
    term.pump_until(6 * SLOW, |_| false);
    assert_eq!(link.trace.acks().len(), heard, "nothing while `j` waits");
    taken(&mut term, b"k", 2, "n");
    taken(&mut term, b"\x1b", 1, "n");
    // As many keys taken as the client sent.
    let to = (count(&link), link.trace.inputs());
    assert_eq!(to.0 - from.0, (to.1 - from.1) as u64);
}

/// Starting at the end of a short line, reached from column 40 of a long
/// one, a scroll puts the cursor in column 40 of a long enough line (its
/// wanted column, as Neovim does); after `$`, at the end of each line.
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

#[test]
fn every_scroll_sideways_predicted_is_the_one_neovim_makes() {
    require_nvim!();
    let scratch = Scratch::new("client-sideways");
    // Lines from shorter than the window to twice as wide, with digits to
    // tell columns apart.
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

    // Past the end of the cursor's line, which moves the cursor to the
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

    typed(&mut term, b"0i");
    for (name, keys) in [
        ("the wheel right in Insert mode", &b"\x1b[<67;10;5M"[..]),
        ("the wheel left in Insert mode", b"\x1b[<66;10;5M"),
    ] {
        predicted(&mut term, name, keys);
    }
    typed(&mut term, b"\x1b");
}

/// Wide characters in a nowrap window, every other line offset a column so
/// the edges cut one in two on alternate rows. Predictions must match
/// Neovim's `>` (cut at the right) and `<` (cut at the left), and put the
/// cursor on the character covering its wanted column.
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

/// In a nowrap window, a scroll landing the cursor out of view shifts the
/// view sideways as Neovim does: just enough, or centred on the cursor when
/// half a view or more away.
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
    // From here the cursor wants the end of every line.
    typed(&mut term, b"$");
    for (name, keys) in [
        ("<C-e> after $, in view", &b"\x05"[..]),
        ("<C-e> after $ to a short line, far out of view", b"\x05"),
        ("<C-e> after $ to the end of a long line, far out", b"\x05"),
    ] {
        predicted(&mut term, name, keys);
    }
}

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

/// The first line of the `k`th page down: a page is the 22-line window less
/// two, 20.
fn page(k: usize) -> u32 {
    1 + 20 * k as u32
}

/// Type `presses` `<C-f>`s `gap` apart and record when each was typed and
/// first drawn, asserting the view never went back and ended where Neovim's
/// did. Each press is `gap` after the previous one, however late that was,
/// as with key repeat: catching up with a burst would outrun the kept lines.
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

/// A run of [`pages`].
struct Paging {
    t0: Instant,
    typed: Vec<Instant>,
    drawn: Vec<Option<Instant>>,
    trace: Trace,
}

impl Paging {
    /// How long after its key each page was first drawn.
    fn late(&self) -> Vec<Option<Duration>> {
        self.typed
            .iter()
            .zip(&self.drawn)
            .map(|(typed, drawn)| drawn.map(|d| d - *typed))
            .collect()
    }

    /// When Neovim's first frame showing each page reached the client: what
    /// a non-predicted page would have waited for. Waits up to 4 link delays
    /// for the last.
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

    /// Per-page timings around page `around`, for failure messages: typed,
    /// key to Neovim, Neovim's view and lines back, drawn. In ms from the
    /// first page, as the link read and wrote each.
    fn timeline(&self, around: usize) -> String {
        // Let in-flight traffic (perhaps the frame landing the page) arrive.
        std::thread::sleep(4 * self.trace.delay);
        let ms = |t: Instant| match t.checked_duration_since(self.t0) {
            Some(d) => d.as_millis().to_string(),
            None => format!("-{}", (self.t0 - t).as_millis()),
        };
        let way = |e: Option<&(Instant, Instant, u32)>| {
            e.map_or_else(|| "-".into(), |(r, w, _)| format!("{}→{}", ms(*r), ms(*w)))
        };
        // Each `<C-f>` sent, Neovim's views, and its lines' reach.
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

/// Each page is drawn before Neovim could have the key, from lines sent
/// before it was typed.
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

/// Pages beyond the sent lines wait for lines, which come as Neovim's view
/// moves, but never as long as Neovim's own frame would take.
#[test]
fn pages_typed_further_ahead_than_lines_go_wait_only_for_them() {
    require_nvim!();
    let scratch = Scratch::new("client-run");
    let (mut rpc, mut term, link) = drawn_slowly(
        &scratch,
        "client-run",
        "call setline(1, map(range(1, 2000), '\"line \" . v:val'))",
    );
    // A round trip types 50 pages, more than the kept lines reach (24
    // windows each way, about 27 pages); 32 is as far ahead as a view goes.
    let run = pages(&mut rpc, &mut term, &link, Duration::from_millis(10), 32);
    // Compare with the measured frame times, which on a busy machine exceed
    // the link's delay.
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

/// As on a busy machine: Neovim takes typed-ahead keys before anything
/// scheduled, yet lines still come as its view moves, not once it catches
/// up. Its first frame comes later than the first page would be given up on,
/// unless the client hears Neovim is still working: the view never goes back.
#[test]
fn pages_typed_faster_than_neovim_scrolls_bring_their_lines_as_it_goes() {
    require_nvim!();
    let scratch = Scratch::new("client-behind");
    // Each scroll takes Neovim this long; with a page every 10 ms, its 28
    // scrolls put its frame past two link round trips, 250 ms after the
    // first page. Sleeps rather than spins to spare cores on small machines,
    // and records each scroll's time once asked to.
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
    // The two pages past the lines in hand get theirs with Neovim's second
    // view if lines are pushed at each scroll, else only after all 28; later
    // pages wait for later views. The limit is halfway between, in scrolls as
    // long as Neovim's measured ones (longer than its sleep on a busy
    // machine).
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
    // Neovim's frame may land after `pages` stops watching; nothing must move.
    let top = top_line(&term.text());
    assert!(
        !term.pump_until(8 * SLOW, |s| top_line(s) != top),
        "it moved once Neovim drew:\n{}",
        term.text()
    );
}

/// A `<C-f>` Neovim discards (nothing the client sees says so) is given up
/// and the view goes back to Neovim's, keeping the pages around it. Typed
/// among pages Neovim is still working on, it is given up as long after
/// Neovim catches up as one typed then would be, and no sooner.
#[test]
fn a_page_neovim_does_not_turn_is_given_up_in_time() {
    require_nvim!();
    let scratch = Scratch::new("client-unturned");
    // As in the previous test: Neovim falls behind pages typed every 10 ms,
    // its frame later than the first page would be given up.
    const SCROLL: Duration = Duration::from_millis(30);
    let (mut rpc, mut term, link) = drawn_slowly(
        &scratch,
        "client-unturned",
        &format!(
            "call setline(1, map(range(1, 2000), '\"line \" . v:val')) | \
             autocmd WinScrolled * lua vim.uv.sleep({})",
            SCROLL.as_millis()
        ),
    );
    // Discard the fifth.
    rpc.command(
        "lua local n = 0 vim.on_key(function(key) \
         if key == '\\6' then n = n + 1 if n == 5 then return '' end end end)",
    )
    .expect("on_key");
    let t0 = Instant::now();
    let mut seen: Vec<(u32, Instant)> = Vec::new();
    // Record the top line at each change, until `until` or the view goes
    // back.
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
    let sent = link.trace.inputs();
    for k in 1..=28 {
        term.type_bytes(b"\x06");
        watch(&mut term, t0 + Duration::from_millis(10) * k);
    }
    // Ask for Neovim's view once the link has delivered every key (possibly
    // later than one delay after typing). Neovim takes typed keys before
    // requests, so the answer's time is when it caught up; on a busy machine
    // that is later than its sleeps alone predict.
    let reached = Instant::now() + Duration::from_secs(10);
    while link.trace.inputs() < sent + 28 && Instant::now() < reached {
        watch(&mut term, Instant::now() + Duration::from_millis(10));
        // `watch` returns at once if the view has gone back.
        std::thread::sleep(Duration::from_millis(1));
    }
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
    // After Neovim's frame (a link delay after it caught up), and within
    // twice the wait of a page typed then (two round trips, 250 ms and a lost
    // segment, roughly), for a busy machine.
    let at = seen.last().map(|&(_, at)| at).expect("seen");
    let wait = 4 * SLOW + Duration::from_millis(450);
    assert!(
        at > caught_up + SLOW && at < caught_up + SLOW + 2 * wait,
        "given up on {:?} after Neovim caught up at {caught:?}: {when:?}",
        at.saturating_duration_since(caught_up)
    );
}

/// With motions predicted, the client goes by the agent's acks; pages must
/// still be drawn before Neovim could have the keys.
#[test]
fn pages_typed_faster_than_the_link_answers_keep_up_told_the_motions() {
    require_nvim!();
    let scratch = Scratch::new("client-burst-ack");
    let (mut rpc, mut term, link) = drawn_slowly_with(
        &scratch,
        "client-burst-ack",
        "call setline(1, map(range(1, 2000), '\"line \" . v:val'))",
        MOTIONS,
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

/// [`a_page_neovim_does_not_turn_is_given_up_in_time`] with motions
/// predicted: acks arrive with the frames, so the page is given up at the
/// first frame that should have turned it (not 2+ round trips later), the
/// view going back by that page alone and redoing the rest to where Neovim
/// ends.
#[test]
fn a_page_neovim_does_not_turn_is_given_up_at_its_ack() {
    require_nvim!();
    let scratch = Scratch::new("client-unturn-ack");
    const SCROLL: Duration = Duration::from_millis(30);
    let (mut rpc, mut term, link) = drawn_slowly_with(
        &scratch,
        "client-unturn-ack",
        &format!(
            "call setline(1, map(range(1, 2000), '\"line \" . v:val')) | \
             autocmd WinScrolled * lua vim.uv.sleep({})",
            SCROLL.as_millis()
        ),
        MOTIONS,
    );
    rpc.command(
        "lua local n = 0 vim.on_key(function(key) \
         if key == '\\6' then n = n + 1 if n == 5 then return '' end end end)",
    )
    .expect("on_key");
    term.pump_until(4 * SLOW, |_| false);
    // The agent's count before the pages. The discarded fifth key counts as
    // taken.
    let count = |a: &Vec<Value>| a.first().and_then(Value::as_u64).expect("a count");
    let before = link
        .trace
        .acks()
        .last()
        .map(|(_, a)| count(a))
        .expect("an ack");
    let t0 = Instant::now();
    let mut seen: Vec<(u32, Instant)> = Vec::new();
    let mut watch = |term: &mut Terminal, until: Instant| loop {
        let now = Instant::now();
        let last = seen.last().map(|&(top, _)| top);
        if let Some(top) = top_line(&term.text()).filter(|&top| Some(top) != last) {
            seen.push((top, now));
        }
        if now >= until {
            return;
        }
        let last = seen.last().map(|&(top, _)| top);
        term.pump(until - now, |t| top_line(&t.text()) != last);
    };
    for k in 1..=28 {
        term.type_bytes(b"\x06");
        watch(&mut term, t0 + Duration::from_millis(10) * k);
    }
    let w0 = |rpc: &mut nvmux::rpc::Client<std::os::unix::net::UnixStream>| {
        rpc.eval("line('w0')").ok().and_then(|v| v.as_u64())
    };
    watch(&mut term, Instant::now() + 6 * SLOW);
    assert!(
        common::wait_until(Duration::from_secs(10), || w0(&mut rpc) == Some(541)),
        "Neovim went elsewhere: {:?}",
        w0(&mut rpc)
    );
    watch(&mut term, Instant::now() + 6 * SLOW);
    let when: Vec<(u32, Duration)> = seen.iter().map(|&(top, at)| (top, at - t0)).collect();
    let tops: Vec<u32> = seen.iter().map(|&(top, _)| top).collect();
    let back: Vec<usize> = (1..seen.len()).filter(|&i| tops[i] < tops[i - 1]).collect();
    assert_eq!(back.len(), 1, "back but once: {when:?}");
    let i = back[0];
    assert_eq!(tops[i - 1] - tops[i], 20, "by that page alone: {when:?}");
    assert_eq!(tops.last(), Some(&541), "where Neovim ends: {when:?}");
    // The view goes back with the first ack covering the fifth page, no
    // sooner, and within a link delay of it: slack for a busy machine, still
    // well short of the 2+ round trips it would take without acks.
    let acked = link
        .trace
        .acks()
        .into_iter()
        .find(|(_, a)| count(a) >= before + 5)
        .map(|(written, _)| written)
        .expect("an ack covering the fifth page");
    let gone = seen[i].1;
    assert!(
        gone >= acked && gone < acked + SLOW,
        "back {:?} after the ack that covered it: {when:?}",
        gone.checked_duration_since(acked)
            .map_or_else(|| format!("-{:?}", acked - gone), |d| format!("{d:?}"))
    );
}

const NOTHING: &str = "\
    predict_scrolls = \"off\"\n\
    predict_motions = \"off\"\n\
    predict_typing = \"off\"\n";

/// Scrolls only; motions and typing default to `"auto"`.
const SCROLLS: &str = "predict_motions = \"off\"\npredict_typing = \"off\"\n";

/// Scrolls and motions, not typing.
const MOTIONS: &str = "predict_motions = \"auto\"\npredict_typing = \"off\"\n";

/// Lines with tabs, CJK, an emoji, a combining mark, nothing, only blanks,
/// and brackets for MatchParen: what line motions step over. `line 1` is
/// what [`drawn_slowly_with`] waits for.
const MOVING: &str = r#"call setline(1, ["\tone\ttwo three", "日本語のテキスト", "smile \U1F600 here", "café au lait", "", "    ", "  (a[b]c) end", "the quick brown fox jumps over the lazy dog and on", "line 1"])"#;

#[test]
fn horizontal_motions_typed_over_a_slow_link_show_at_once() {
    require_nvim!();
    let scratch = Scratch::new("client-motions");
    let (_rpc, mut term, _) = drawn_slowly_with(&scratch, "client-motions", MOVING, MOTIONS);
    term.pump_until(Duration::from_secs(4), |s| s.contains("one"));
    // `\tone\ttwo three`, from its tab.
    for (name, keys) in [
        ("l", &b"l"[..]),
        ("h", b"h"),
        ("$", b"$"),
        ("0", b"0"),
        ("^", b"^"),
        ("5l", b"5l"),
        ("2h", b"2h"),
        ("<End>", b"\x1b[F"),
        ("<Home>", b"\x1b[H"),
        ("12|", b"12|"),
        ("g_", b"g_"),
    ] {
        predicted(&mut term, name, keys);
    }
    // The CJK, from its end.
    typed(&mut term, b"j");
    for (name, keys) in [("0", &b"0"[..]), ("3l", b"3l"), ("h", b"h"), ("$", b"$")] {
        predicted(&mut term, name, keys);
    }
    // The emoji (two cells), then a letter with a combining mark (one).
    typed(&mut term, b"j");
    for (name, keys) in [("^", &b"^"[..]), ("7l", b"7l"), ("h", b"h"), ("$", b"$")] {
        predicted(&mut term, name, keys);
    }
    typed(&mut term, b"j");
    for (name, keys) in [
        ("0", &b"0"[..]),
        ("4l", b"4l"),
        ("h", b"h"),
        ("<Space>", b" "),
    ] {
        predicted(&mut term, name, keys);
    }
    // Blanks only, and brackets.
    typed(&mut term, b"jj");
    for (name, keys) in [("0", &b"0"[..]), ("$", b"$")] {
        predicted(&mut term, name, keys);
    }
    typed(&mut term, b"j");
    for (name, keys) in [
        ("^", &b"^"[..]),
        ("l", b"l"),
        ("$", b"$"),
        ("<BS>", b"\x7f"),
    ] {
        predicted(&mut term, name, keys);
    }
}

#[test]
fn motions_by_words_show_at_once() {
    require_nvim!();
    let scratch = Scratch::new("client-words-at");
    let (_rpc, mut term, _) = drawn_slowly_with(&scratch, "client-words-at", MOVING, MOTIONS);
    term.pump_until(Duration::from_secs(4), |s| s.contains("one"));
    for (name, keys) in [
        ("w", &b"w"[..]),
        ("w", b"w"),
        ("e", b"e"),
        ("b", b"b"),
        ("ge", b"ge"),
        ("3w", b"3w"),
        ("2b", b"2b"),
        ("W", b"W"),
        ("E", b"E"),
        ("B", b"B"),
        ("gE", b"gE"),
        ("<S-Right>", b"\x1b[1;2C"),
        ("<C-Right>", b"\x1b[1;5C"),
        ("<S-Left>", b"\x1b[1;2D"),
        ("<C-Left>", b"\x1b[1;5D"),
        ("5w", b"5w"),
        ("4e", b"4e"),
        ("w", b"w"),
        ("2W", b"2W"),
        ("3b", b"3b"),
        ("6w", b"6w"),
        ("w", b"w"),
        ("w", b"w"),
        ("2ge", b"2ge"),
        ("8w", b"8w"),
        ("3e", b"3e"),
        ("4B", b"4B"),
    ] {
        predicted(&mut term, name, keys);
    }
}

#[test]
fn motions_to_a_character_show_at_once() {
    require_nvim!();
    let scratch = Scratch::new("client-finds-at");
    let (_rpc, mut term, _) = drawn_slowly_with(&scratch, "client-finds-at", MOVING, MOTIONS);
    term.pump_until(Duration::from_secs(4), |s| s.contains("one"));
    // `\tone\ttwo three`, from its tab.
    for (name, keys) in [
        ("fe", &b"fe"[..]),
        ("fe", b"fe"),
        ("Fo", b"Fo"),
        ("T<Tab>", b"T\t"),
        ("t<Space>", b"t "),
        ("F<Tab>", b"F\t"),
        ("2fe", b"2fe"),
        ("2Fo", b"2Fo"),
        ("th", b"th"),
    ] {
        predicted(&mut term, name, keys);
    }
    // The CJK, from its start.
    typed(&mut term, b"j0");
    for name in ["fテ", "Fの", "T日", "tト"] {
        predicted(&mut term, name, name.as_bytes());
    }
    // An emoji among letters.
    typed(&mut term, b"j0");
    for name in ["f😀", "fe", "T😀", "Fe", "t😀", "2fe"] {
        predicted(&mut term, name, name.as_bytes());
    }
    // `café`'s `é` is an `e` with a combining mark: `fe` finds it, and `T`
    // stops on it, as Neovim moves back to the character holding the byte
    // after the `e`.
    typed(&mut term, b"j0");
    for name in ["fe", "2fa", "Te", "tl", "F "] {
        predicted(&mut term, name, name.as_bytes());
    }
}

/// `}` and `{` with counts: to the empty line ending a paragraph, past the
/// last to the buffer's last character, and back to the first line's tab.
#[test]
fn motions_by_paragraphs_show_at_once() {
    require_nvim!();
    let scratch = Scratch::new("client-paras-at");
    let (_rpc, mut term, _) = drawn_slowly_with(&scratch, "client-paras-at", MOVING, MOTIONS);
    term.pump_until(Duration::from_secs(4), |s| s.contains("one"));
    for name in ["}", "}", "{", "{", "2}", "2{"] {
        predicted(&mut term, name, name.as_bytes());
    }
}

/// `H`, `M`, `L` with counts (past the lines shown, and from the bottom up
/// past the top), with `'number'` and `'relativenumber'`: to the wanted
/// column, then with `'startofline'` to the first non-blank, then scrolled
/// with `'scrolloff'` 3. The gutter must match too.
#[test]
fn motions_to_the_window_edges_show_at_once() {
    require_nvim!();
    let scratch = Scratch::new("client-hml-at");
    let setline = format!("set nu rnu | {MOVING}");
    let (mut rpc, mut term, _) = drawn_slowly_with(&scratch, "client-hml-at", &setline, MOTIONS);
    term.pump_until(Duration::from_secs(4), |s| s.contains("one"));
    for name in [
        "L", "H", "M", "3L", "$", "2H", "M", "50H", "50L", "Fo", "L", "M", "2L", "9L",
    ] {
        predicted(&mut term, name, name.as_bytes());
    }
    rpc.command("set startofline").expect("startofline");
    term.pump_until(4 * SLOW, |_| false);
    for name in ["L", "H", "M", "3H", "4L", "2H"] {
        predicted(&mut term, name, name.as_bytes());
    }
    rpc.command(&format!("set nosol so=3 | {SCROLLED} | 60 | normal! zt"))
        .expect("scrolled");
    term.pump_until(4 * SLOW, |_| false);
    for name in ["L", "H", "M", "5L", "2H", "M", "30L", "30H"] {
        predicted(&mut term, name, name.as_bytes());
    }
}

/// Jumps too far past the view's edge for a minimal scroll (`gg`/`G` with
/// counts, `j`/`k` past half the window, `}`/`{`), with `'number'`,
/// `'relativenumber'` and `'scrolloff'` 3: Neovim centres the cursor, or
/// pins the buffer's first or last line to the edge. Then again with
/// `'smoothscroll'` (centres a view moved down a row higher) and
/// `'scrolljump'` 5.
#[test]
fn long_jumps_show_at_once() {
    require_nvim!();
    let scratch = Scratch::new("client-jumps-at");
    let setline = format!("set nu rnu so=3 | {SCROLLED}");
    let (mut rpc, mut term, _) = drawn_slowly_with(&scratch, "client-jumps-at", &setline, MOTIONS);
    term.pump_until(Duration::from_secs(4), |s| s.contains("line 1"));
    // Lets the client time the link, so the agent sends lines far enough
    // either side of the view for all of these (see `Predictor::reach`).
    for _ in 0..3 {
        typed(&mut term, b"j");
        typed(&mut term, b"k");
    }
    term.pump_until(4 * SLOW, |_| false);
    let jumps = [
        "G", "gg", "100G", "50j", "30k", "G", "120gg", "40k", "15j", "15k", "}", "gg", "3}",
        "150G", "20j", "4{", "1G",
    ];
    for name in jumps {
        predicted(&mut term, name, name.as_bytes());
    }
    rpc.command("set smoothscroll scrolljump=5").expect("set");
    term.pump_until(4 * SLOW, |_| false);
    for name in jumps {
        predicted(&mut term, name, name.as_bytes());
    }
}

/// A mapped `f` (as jump plugins map it) is left to Neovim, as is `t`'s
/// target when `:lmap` may translate it (`'iminsert'` set while attached).
/// Here `f` is mapped to `F`, and `o` lmapped to `e`.
#[test]
fn a_motion_to_a_character_mapped_is_left_to_neovim() {
    require_nvim!();
    let scratch = Scratch::new("client-f-mapped");
    let setline = format!("nnoremap f F| {MOVING}");
    let (mut rpc, mut term, _) = drawn_slowly_with(&scratch, "client-f-mapped", &setline, MOTIONS);
    term.pump_until(Duration::from_secs(4), |s| s.contains("one"));
    typed(&mut term, b"$");
    left_to_neovim(&mut term, "fo, mapped to Fo", b"fo");
    predicted(&mut term, "te", b"te");
    rpc.command("lmap o e| setlocal iminsert=1")
        .expect("lmapped");
    term.pump_until(Duration::from_secs(2), |_| false);
    typed(&mut term, b"0");
    left_to_neovim(&mut term, "to, o taken for e", b"to");
}

/// With `'number'` and `'relativenumber'`: the gutter must match too.
#[test]
fn vertical_motions_show_at_once() {
    require_nvim!();
    let scratch = Scratch::new("client-vertical");
    let setline = format!("set nu rnu | {MOVING}");
    let (_rpc, mut term, _) = drawn_slowly_with(&scratch, "client-vertical", &setline, MOTIONS);
    term.pump_until(Duration::from_secs(4), |s| s.contains("one"));
    for (name, keys) in [
        ("j", &b"j"[..]),
        ("k", b"k"),
        ("3j", b"3j"),
        ("2k", b"2k"),
        ("+", b"+"),
        ("-", b"-"),
        ("<CR>", b"\r"),
        ("$", b"$"),
        ("_", b"_"),
        ("2$", b"2$"),
        ("3_", b"3_"),
        ("<Up>", b"\x1b[A"),
        ("<C-n>", b"\x0e"),
        ("<C-p>", b"\x10"),
        ("<Down>", b"\x1b[B"),
        ("<C-j>", b"\n"),
        ("2<BS>", b"2\x7f"),
        ("12<Space>", b"12 "),
        ("5k", b"5k"),
        ("2g_", b"2g_"),
    ] {
        predicted(&mut term, name, keys);
    }
}

/// LazyVim maps `j`/`k` to `gj`/`gk` without a count; on single-row lines
/// they behave alike, so they are predicted as `j`/`k`.
#[test]
fn jk_mapped_the_lazyvim_way_are_predicted() {
    require_nvim!();
    let scratch = Scratch::new("client-lazy-jk");
    let (mut rpc, mut term, _) = drawn_slowly_with(&scratch, "client-lazy-jk", MOVING, MOTIONS);
    for key in ["j", "k"] {
        rpc.command(&format!(
            "lua vim.keymap.set({{'n', 'x'}}, '{key}', \"v:count == 0 ? 'g{key}' : '{key}'\", \
             {{ expr = true, silent = true }})"
        ))
        .expect("mapped");
    }
    // The client reads mappings a second after Neovim last waited.
    term.pump_until(Duration::from_secs(2), |_| false);
    for (name, keys) in [
        ("j", &b"j"[..]),
        ("j", b"j"),
        ("k", b"k"),
        ("3j", b"3j"),
        ("k", b"k"),
    ] {
        predicted(&mut term, name, keys);
    }
}

const SCROLLING: &str = "call setline(1, map(range(1, 300), '\"line \" . v:val'))";

/// The cursor's buffer line (from 1) in a window of [`SCROLLING`]'s lines.
fn line_on(screen: &vt100::Screen) -> Option<u32> {
    let top = top_line(&screen.contents())?;
    Some(top + u32::from(screen.cursor_position().0))
}

/// `j` every 30 ms, from the top to well past the bottom, over a link slower
/// than that each way: each is drawn before Neovim has it, one line at a
/// time (Neovim's frames, arriving in bursts, would jump), ending where
/// Neovim's frames leave it.
#[test]
fn holding_j_past_the_bottom_keeps_up() {
    require_nvim!();
    let scratch = Scratch::new("client-held-j");
    let (mut rpc, mut term, _) = drawn_slowly_with(&scratch, "client-held-j", SCROLLING, MOTIONS);
    let start = line_on(term.parser.screen()).expect("the first line");
    let mut seen: Vec<(Instant, Option<u32>)> = vec![(Instant::now(), Some(start))];
    let mut typed_at = Vec::new();
    let mut note = |screen: &vt100::Screen| {
        let on = line_on(screen);
        if seen.last().map(|s| s.1) != Some(on) {
            seen.push((Instant::now(), on));
        }
        false
    };
    for k in 0..40 {
        let at = Instant::now();
        typed_at.push(at);
        term.type_bytes(b"j");
        // Wait for its line before the next `j`: a client delayed past
        // 30 ms (busy machine) would read both and draw one frame two lines on.
        let line = start + k + 1;
        term.frames(SLOW, |screen| {
            note(screen);
            line_on(screen) == Some(line)
        });
        term.frames(
            Duration::from_millis(30).saturating_sub(at.elapsed()),
            &mut note,
        );
    }
    term.frames(4 * SLOW, &mut note);
    let when: Vec<(Duration, Option<u32>)> = seen
        .iter()
        .map(|(t, l)| (t.saturating_duration_since(typed_at[0]), *l))
        .collect();
    let lines: Vec<u32> = seen.iter().filter_map(|s| s.1).collect();
    assert_eq!(lines.len(), seen.len(), "every sight a line: {when:?}");
    assert!(
        lines.windows(2).all(|w| w[1] == w[0] + 1),
        "a line at a time: {when:?}"
    );
    for (k, at) in typed_at.iter().enumerate() {
        let line = start + k as u32 + 1;
        let drawn = seen.iter().find(|s| s.1 == Some(line)).map(|s| s.0);
        assert!(
            drawn.is_some_and(|d| d < *at + SLOW),
            "the {}th `j` waited: {when:?}",
            k + 1
        );
    }
    let at = rpc
        .call("nvim_win_get_cursor", vec![0.into()])
        .expect("Neovim's cursor");
    let line = at.as_array().and_then(|a| a[0].as_u64()).expect("its line");
    assert_eq!(line, u64::from(start) + 40, "Neovim's");
    assert_eq!(line_on(term.parser.screen()), Some(start + 40));
    assert_eq!(top_line(&term.text()), Some(start + 40 - 21), "scrolled");
}

/// Type `keys` the client leaves to Neovim: nothing changes within one link
/// delay, and then Neovim's frame changes the window.
fn left_to_neovim(term: &mut Terminal, name: &str, keys: &[u8]) {
    let before = window(term);
    term.type_bytes(keys);
    assert!(
        !term.pump(SLOW, |t| window(t) != before),
        "{name} drawn before Neovim could have heard of it:\n{}",
        term.text()
    );
    term.pump_until(4 * SLOW, |_| false);
    assert_ne!(window(term), before, "{name}: Neovim's frame");
}

/// With `'cursorline'` and the default sliding scroll effect, every frame has
/// the bar under the cursor's row only, even while an earlier slide is still
/// settling.
#[test]
fn holding_j_past_the_bottom_as_the_view_slides_keeps_the_bar_under_the_cursor() {
    require_nvim!();
    let scratch = Scratch::new("client-slide-j");
    let setline = format!("set termguicolors cursorline | {SCROLLING}");
    let settings = format!("[client]\n{MOTIONS}");
    let (_rpc, mut term, _) = drawn_slowly_as(&scratch, "client-slide-j", &setline, &settings);
    typed(&mut term, b"L");
    let screen = term.parser.screen();
    let colours = colours_of(screen);
    let bar = colours[usize::from(cursor(screen).0)][40].1;
    assert_ne!(bar, colours[0][40].1, "a bar to be seen");
    let mut wrong: Vec<(usize, Vec<usize>)> = Vec::new();
    let mut check = |screen: &vt100::Screen| {
        let colours = colours_of(screen);
        let barred: Vec<usize> = (0..colours.len())
            .filter(|&r| colours[r][40].1 == bar)
            .collect();
        let at = usize::from(cursor(screen).0);
        if barred != [at] {
            wrong.push((at, barred));
        }
        false
    };
    for _ in 0..30 {
        term.type_bytes(b"j");
        term.frames(Duration::from_millis(30), &mut check);
    }
    term.frames(4 * SLOW, &mut check);
    assert!(
        wrong.is_empty(),
        "the cursor's row, and the bar's: {wrong:?}"
    );
}

/// A wheel turn after predicted motions starts from where they left the
/// cursor and view, not from Neovim's (not yet moved).
#[test]
fn turns_of_the_wheel_among_motions_show_nothing_neovim_never_drew() {
    require_nvim!();
    const DOWN: (&[u8], &str) = (b"\x1b[<65;20;5M", "<ScrollWheelDown>");
    const UP: (&[u8], &str) = (b"\x1b[<64;20;5M", "<ScrollWheelUp>");
    let mut keys: Vec<(Vec<u8>, String)> = vec![(b"21j".to_vec(), "21j".into())];
    for _ in 0..4 {
        for (bytes, name) in [
            (&b"j"[..], "j"),
            (b"j", "j"),
            (b"j", "j"),
            DOWN,
            (b"j", "j"),
            DOWN,
            DOWN,
            (b"k", "k"),
            (b"k", "k"),
            UP,
            (b"j", "j"),
            (b"j", "j"),
            UP,
            (b"k", "k"),
        ] {
            keys.push((bytes.to_vec(), name.into()));
        }
    }
    sights_are_neovims("client-wheel", SCROLLING, MOTIONS, &keys, Lcg(13));
}

/// Counted `j`/`k` past the window's edges: under half the window (9 of its
/// 22 rows) Neovim scrolls just far enough, beyond that it centres the
/// cursor.
#[test]
fn every_jump_predicted_lands_where_neovim_puts_it() {
    require_nvim!();
    let scratch = Scratch::new("client-jumps");
    let (_rpc, mut term, _) = drawn_slowly_with(&scratch, "client-jumps", SCROLLING, MOTIONS);
    predicted(&mut term, "21j, to the last row", b"21j");
    predicted(&mut term, "8j past it", b"8j");
    predicted(&mut term, "9j", b"9j");
    predicted(&mut term, "10j", b"10j");
    predicted(&mut term, "15j", b"15j");
    typed(&mut term, b"H");
    predicted(&mut term, "8k past the first row", b"8k");
    predicted(&mut term, "9k", b"9k");
    predicted(&mut term, "10k", b"10k");
    typed(&mut term, b"H");
    predicted(&mut term, "15k", b"15k");
    // With 'scrolloff', starting from the row it keeps the cursor on.
    typed(&mut term, b":set so=3\r");
    typed(&mut term, b"L");
    predicted(&mut term, "2j to 'scrolloff'", b"2j");
    predicted(&mut term, "j with 'scrolloff'", b"j");
    predicted(&mut term, "5+", b"5+");
    predicted(&mut term, "12j", b"12j");
}

/// Each prediction in a mixed run starts from where the previous one left
/// the cursor and view. With 'number', 'relativenumber' and 'scrolloff'.
#[test]
fn scrolls_and_motions_mixed_show_at_once() {
    require_nvim!();
    let scratch = Scratch::new("client-mixed");
    let setline = format!("set nu rnu so=2 | {SCROLLING}");
    let (_rpc, mut term, _) = drawn_slowly_with(&scratch, "client-mixed", &setline, MOTIONS);
    for (name, keys) in [
        ("jjjj<C-d>", &b"jjjj\x04"[..]),
        ("<C-d>jj", b"\x04jj"),
        ("j<C-e>j", b"j\x05j"),
        ("<C-e><C-e>k", b"\x05\x05k"),
        ("k<C-y>", b"k\x19"),
        ("<C-u>3k", b"\x153k"),
        ("2<C-e>j", b"2\x05j"),
        ("<C-f>j", b"\x06j"),
        ("<C-b>k", b"\x02k"),
        ("ztj", b"ztj"),
        ("zzk", b"zzk"),
        ("zbj", b"zbj"),
    ] {
        predicted(&mut term, name, keys);
    }
}

/// [`predicted`], checking every cell's colours too.
fn predicted_in_colour(term: &mut Terminal, name: &str, keys: &[u8]) {
    let drawn = ahead(term, name, keys);
    let predicted = (sight(&drawn), colours_of(&drawn));
    let landed = last_frame(term, 4 * SLOW);
    let landed = (sight(&landed), colours_of(&landed));
    assert_eq!(predicted.0, landed.0, "{name}: the text and the cursor");
    for (r, (p, l)) in predicted.1.iter().zip(&landed.1).enumerate() {
        assert_eq!(p, l, "{name}: row {r} changed colour when Neovim's landed");
    }
}

/// Pure scrolls (the wheel, Insert-mode pages) that move the cursor to
/// another line, with `'number'`, `'relativenumber'` and `'cursorline'`: the
/// gutter and bar must match Neovim's, colours included, on old and new rows.
#[test]
fn scrolls_alone_number_the_rows_and_lay_the_bar_as_neovim_does() {
    require_nvim!();
    let scratch = Scratch::new("client-alone");
    let setline = format!("set termguicolors nu rnu cursorline | {SCROLLING}");
    let (_rpc, mut term, _) = drawn_slowly_with(&scratch, "client-alone", &setline, MOTIONS);
    term.pump_until(Duration::from_secs(2), |_| false);
    // The wheel scrolls three lines, pushing the cursor along.
    predicted_in_colour(&mut term, "the wheel down", b"\x1b[<65;20;5M");
    predicted_in_colour(&mut term, "and again", b"\x1b[<65;20;5M");
    typed(&mut term, b"L");
    predicted_in_colour(&mut term, "the wheel up", b"\x1b[<64;20;5M");
    typed(&mut term, b"i");
    predicted_in_colour(&mut term, "<PageDown> in Insert mode", b"\x1b[6~");
    predicted_in_colour(&mut term, "<PageUp> in Insert mode", b"\x1b[5~");
    predicted_in_colour(&mut term, "<S-Down> in Insert mode", b"\x1b[1;2B");
}

const TYPING: &str = "predict_motions = \"auto\"\npredict_typing = \"auto\"\n";

/// Letters, blanks, an accented and a wide character, a burst faster than
/// the link, and `<BS>`, at a line's end and before a tab.
#[test]
fn typed_text_is_echoed_as_neovim_draws_it() {
    require_nvim!();
    echoed_as_neovim_draws_it("client-echo", "set nolist");
}

/// With `'list'` drawing blanks, the tab and the line's end.
#[test]
fn typed_text_is_echoed_as_neovim_draws_it_with_listchars() {
    require_nvim!();
    echoed_as_neovim_draws_it(
        "client-echo-list",
        "set list listchars=tab:>-,trail:-,eol:$",
    );
}

/// The body of [`typed_text_is_echoed_as_neovim_draws_it`], with its own
/// session set up by `setup` so the variants run in parallel.
fn echoed_as_neovim_draws_it(tag: &str, setup: &str) {
    let scratch = Scratch::new(tag);
    let setline = format!(r#"{setup} | call setline(1, ["first", "a\tb", "line 1"])"#);
    let (_rpc, mut term, _) = drawn_slowly_with(&scratch, tag, &setline, TYPING);
    typed(&mut term, b"A");
    for c in "hello wörld 日本 x".chars() {
        let key = c.to_string();
        predicted(&mut term, &format!("{tag}: {key}"), key.as_bytes());
    }
    predicted(&mut term, &format!("{tag}: a burst"), b"abc");
    for _ in 0..3 {
        predicted(&mut term, &format!("{tag}: <BS>"), b"\x7f");
    }
    // Before a tab, which shrinks with each.
    typed(&mut term, b"\x1b");
    typed(&mut term, b"j0a");
    for key in ["x", "y", " ", "z"] {
        predicted(
            &mut term,
            &format!("{tag}: {key} before a tab"),
            key.as_bytes(),
        );
    }
}

/// Insert-mode `<C-w>` and `<C-u>` with Neovim's default mappings: words,
/// stopping where Insert mode began and then going past it, then the line's
/// start; the indent kept with `'autoindent'`; after `o`. After an arrow,
/// which may move where Insert began, they wait for Neovim.
#[test]
fn words_and_lines_typed_out_show_at_once() {
    require_nvim!();
    let scratch = Scratch::new("client-rub");
    let setline = r#"call setline(1, ["first", "  indented", "line 1"])"#;
    let (_rpc, mut term, _) = drawn_slowly_with(&scratch, "client-rub", setline, TYPING);
    term.pump_until(Duration::from_secs(4), |s| s.contains("first"));
    typed(&mut term, b"A");
    predicted(&mut term, "typed", b" hello world");
    for k in 0..4 {
        predicted(&mut term, &format!("<C-w> {k}"), b"\x17");
    }
    // From now on Neovim's <C-w> mapping stops at the line's start, not
    // where Insert mode began.
    predicted(&mut term, "typed again", b"abcdefg");
    predicted(&mut term, "<C-w> where it stopped last", b"\x17");
    predicted(&mut term, "typed once more", b"x y");
    predicted(&mut term, "<C-u>", b"\x15");
    typed(&mut term, b"\x1bjA");
    predicted(&mut term, "typed after the indent", b" text");
    predicted(&mut term, "<C-u> back to where it began", b"\x15");
    predicted(&mut term, "<C-u> back to the indent", b"\x15");
    typed(&mut term, b"\x1bo");
    predicted(&mut term, "typed after o", b"one two");
    predicted(&mut term, "<C-w> after o", b"\x17");
    predicted(&mut term, "<C-u> after o", b"\x15");
    predicted(&mut term, "typed after o again", b"three four");
    typed(&mut term, b"\x1b[D");
    left_to_neovim(&mut term, "<C-w> after an arrow", b"\x17");
}

/// Typing over `'cursorline'`, `'list'`'s `trail` and `eol` glyphs and a
/// `'colorcolumn'`: no cell changes text or colour when Neovim's frame lands.
/// Reaching the colour column waits for Neovim.
#[test]
fn typed_text_wears_the_colours_neovim_draws_it_in() {
    require_nvim!();
    let scratch = Scratch::new("client-echo-colours");
    let setline = "set termguicolors cursorline list listchars=trail:-,eol:$ colorcolumn=12 | \
         hi Normal guifg=#dddddd guibg=#101010 | hi CursorLine guibg=#202060 | \
         hi ColorColumn guibg=#550000 | hi Whitespace guifg=#aa00aa | hi NonText guifg=#00aa00 | \
         call setline(1, ['foobar', 'line 1'])";
    let (_rpc, mut term, _) = drawn_slowly_with(&scratch, "client-echo-colours", setline, TYPING);
    typed(&mut term, b"A");
    for (name, key) in [
        ("a blank, drawn as trail", &b" "[..]),
        ("x after it", b"x"),
        ("<BS>", b"\x7f"),
        ("<BS> over the blank", b"\x7f"),
        ("a", b"a"),
        ("b", b"b"),
        ("c", b"c"),
        ("d, the line's end short of the column", b"d"),
    ] {
        let drawn = ahead(&mut term, name, key);
        let landed = last_frame(&mut term, 4 * SLOW);
        assert_eq!(sight(&drawn), sight(&landed), "{name}: the text");
        assert_eq!(
            colours_of(&drawn),
            colours_of(&landed),
            "{name}: the colours"
        );
    }
    waited(&mut term, "e, the line's end into the column", b"e");
}

/// Text typed while `<CR>`'s new line is in flight (it changes the line
/// count and so the window's shape) appears with the new line, at Neovim's
/// ack of `<CR>`, not at the ack of the next key.
#[test]
fn text_typed_while_a_new_line_is_on_its_way_appears_with_it() {
    require_nvim!();
    let scratch = Scratch::new("client-echo-cr");
    let setline = r#"call setline(1, ["first", "line 1"])"#;
    let (_rpc, mut term, _) = drawn_slowly_with(&scratch, "client-echo-cr", setline, TYPING);
    typed(&mut term, b"A");
    for key in [&b"\r"[..], b"a", b"b"] {
        term.type_bytes(key);
        term.pump_until(Duration::from_millis(50), |_| false);
    }
    let mut first = None;
    term.frames(4 * SLOW, |screen| {
        let rows: Vec<String> = screen.rows(0, COLS).take(3).collect();
        if first.is_none() && rows[2].trim_end() == "line 1" {
            first = Some(rows[1].trim_end().to_string());
        }
        false
    });
    assert_eq!(first.as_deref(), Some("ab"), "with the new line");
}

/// End-of-line virtual text moves with typing, predicted. Right-aligned,
/// window-column, or inline (left gravity) virtual text makes `A` and typing
/// on that line wait for Neovim.
#[test]
fn typing_beside_virtual_text_is_drawn_where_neovim_draws_it() {
    require_nvim!();
    let scratch = Scratch::new("client-echo-vt");
    let setline = r#"call setline(1, ["eol", "right", "column", "inline", "line 1"])"#;
    let (mut rpc, mut term, _) = drawn_slowly_with(&scratch, "client-echo-vt", setline, TYPING);
    rpc.command(
        "lua local ns, set = vim.api.nvim_create_namespace('t'), vim.api.nvim_buf_set_extmark \
         set(0, ns, 0, 0, { virt_text = { { 'E1', 'Error' } } }) \
         set(0, ns, 1, 0, { virt_text = { { 'RA', 'Error' } }, virt_text_pos = 'right_align' }) \
         set(0, ns, 2, 0, { virt_text = { { 'W', 'Error' } }, virt_text_win_col = 20 }) \
         set(0, ns, 3, 6, { virt_text = { { '<L>', 'Error' } }, virt_text_pos = 'inline', \
           right_gravity = false })",
    )
    .expect("virtual text");
    term.pump_until(4 * SLOW, |_| false);
    predicted(&mut term, "A before virtual text at the end", b"A");
    predicted(&mut term, "x", b"x");
    predicted(&mut term, "y", b"y");
    for line in ["right-aligned", "in a column", "inline"] {
        typed(&mut term, b"\x1b");
        typed(&mut term, b"j");
        waited(&mut term, &format!("A, {line}"), b"A");
        waited(&mut term, &format!("x, {line}"), b"x");
    }
}

/// Typing waits for Neovim under `'digraph'` (char, `<BS>`, char make one),
/// `'vartabstop'` (tabs not `'tabstop'` wide) and in a prompt buffer (text
/// goes after the prompt), and is predicted again once each is off.
#[test]
fn keys_typed_where_neovim_types_otherwise_wait_for_it() {
    require_nvim!();
    let scratch = Scratch::new("client-otherwise");
    let setline = r#"call setline(1, ["ab\tc", "line 1"])"#;
    let (mut rpc, mut term, _) = drawn_slowly_with(&scratch, "client-otherwise", setline, TYPING);
    let mut set = |term: &mut Terminal, option: &str| {
        rpc.command(option).expect("set");
        term.pump_until(4 * SLOW, |_| false);
    };
    typed(&mut term, b"A");
    predicted(&mut term, "x", b"x");
    set(&mut term, "set digraph");
    waited(&mut term, "y with 'digraph'", b"y");
    set(&mut term, "set nodigraph");
    predicted(&mut term, "y without", b"y");
    // First stop matches `'tabstop'`, so the line looks as the client would
    // lay it out until the tab passes it.
    typed(&mut term, b"\x1b");
    typed(&mut term, b"0l");
    set(&mut term, "setlocal vartabstop=8,4");
    waited(&mut term, "a with 'vartabstop'", b"a");
    waited(&mut term, "z before a tab of 'vartabstop'", b"z");
    typed(&mut term, b"\x1b");
    set(&mut term, "setlocal vartabstop=");
    predicted(&mut term, "a without", b"a");
    predicted(&mut term, "z before a tab without", b"z");
    typed(&mut term, b"\x1b");
    set(
        &mut term,
        "setlocal buftype=prompt | call prompt_setprompt(bufnr(), '> ')",
    );
    waited(&mut term, "i in a prompt buffer", b"i");
    waited(&mut term, "w in a prompt buffer", b"w");
}

/// Whether, within one link delay, the client wrote one of `shapes` (a
/// `DECSCUSR`, steady or blinking) after `from`.
fn shaped_within(term: &mut Terminal, from: usize, shapes: [&[u8]; 2]) -> bool {
    term.pump(SLOW, |t| {
        shapes
            .iter()
            .any(|shape| find(&t.output[from..], shape).is_some())
    })
}

/// `i` sets the bar cursor and `<Esc>` the block before Neovim has the key;
/// `a`, `A`, `I`, `<Esc>` and typing put it where Neovim does.
#[test]
fn the_cursor_takes_its_insert_shape_at_once() {
    require_nvim!();
    let scratch = Scratch::new("client-shape");
    let setline = r#"call setline(1, ["  hello world", "line 1"])"#;
    let (_rpc, mut term, _) = drawn_slowly_with(&scratch, "client-shape", setline, TYPING);
    let bar: [&[u8]; 2] = [b"\x1b[5 q", b"\x1b[6 q"];
    let block: [&[u8]; 2] = [b"\x1b[1 q", b"\x1b[2 q"];
    let from = term.output.len();
    term.type_bytes(b"i");
    assert!(shaped_within(&mut term, from, bar), "a bar at once");
    term.pump_until(4 * SLOW, |_| false);
    let from = term.output.len();
    term.type_bytes(b"\x1b");
    assert!(shaped_within(&mut term, from, block), "a block at once");
    term.pump_until(4 * SLOW, |_| false);
    for (name, keys) in [
        ("$a", &b"$a"[..]),
        ("x", b"x"),
        ("<Esc> after x", b"\x1b"),
        ("A", b"A"),
        ("<Esc> after A", b"\x1b"),
        ("I", b"I"),
        ("y", b"y"),
        ("<Esc> after y", b"\x1b"),
        ("j0a", b"j0a"),
        ("<Esc> after j0a", b"\x1b"),
    ] {
        predicted(&mut term, name, keys);
    }
}

/// Like [`left_to_neovim`], with its own messages.
fn waited(term: &mut Terminal, name: &str, keys: &[u8]) {
    let before = window(term);
    term.type_bytes(keys);
    assert!(
        !term.pump(SLOW, |t| window(t) != before),
        "{name} was drawn before Neovim had heard of it:\n{}",
        term.text()
    );
    term.pump_until(4 * SLOW, |_| false);
    assert_ne!(window(term), before, "{name}: never drawn by Neovim");
}

/// Keys Neovim does more with than insert (an autopair's `(`, a blank after
/// an abbreviation, a character past `'textwidth'`) wait; those around them
/// do not.
#[test]
fn refused_keys_wait_for_neovim() {
    require_nvim!();
    let scratch = Scratch::new("client-refused");
    let setline = r#"inoremap ( ()<Left> | iabbrev teh the | setlocal tw=20 | call setline(1, ["one two three four", "line 1"])"#;
    let (_rpc, mut term, _) = drawn_slowly_with(&scratch, "client-refused", setline, TYPING);
    typed(&mut term, b"A");
    predicted(&mut term, "a blank short of 'textwidth'", b" ");
    predicted(&mut term, "a character ending on it", b"f");
    waited(&mut term, "a character past it", b"i");
    waited(&mut term, "an autopair", b"(");
    predicted(&mut term, "a character between the pair", b"x");
    for key in ["t", "e", "h"] {
        predicted(&mut term, key, key.as_bytes());
    }
    waited(&mut term, "a blank after an abbreviation", b" ");
}

/// With an `InsertCharPre` turning `q` into `Q!`, typing is predicted but
/// not drawn until the buffer has proved itself, and `q`, seen typed
/// otherwise, must prove itself again. For a first such character after the
/// proof, see
/// [`a_character_typed_otherwise_after_the_proof_is_put_right_at_once`].
#[test]
fn insert_char_pre_shows_nothing_wrong_till_typing_has_proved_itself() {
    require_nvim!();
    let setline = r#"call setline(1, ["x", "line 1"]) | exe "autocmd InsertCharPre * if v:char ==# 'q' | let v:char = 'Q!' | endif""#;
    let mut keys: Vec<(Vec<u8>, String)> = vec![(b"A".to_vec(), "A".into())];
    for word in ["aq", "bq", "cdefghij", "q", "klmnop", "q"] {
        for c in word.chars() {
            keys.push((c.to_string().into_bytes(), c.to_string()));
        }
        keys.push((b" ".to_vec(), "<Space>".into()));
    }
    sights_are_neovims("client-charpre", setline, TYPING, &keys, Lcg(11));
}

/// After the buffer has proved itself, a first `q` (typed as `Q!` by
/// `InsertCharPre`) is drawn as typed, since the client cannot tell; Neovim's
/// ack corrects it within a round trip and it must prove itself again.
#[test]
fn a_character_typed_otherwise_after_the_proof_is_put_right_at_once() {
    require_nvim!();
    let scratch = Scratch::new("client-charpre-late");
    let setline = r#"call setline(1, ["x", "line 1"]) | exe "autocmd InsertCharPre * if v:char ==# 'q' | let v:char = 'Q!' | endif""#;
    let (_rpc, mut term, link) =
        drawn_slowly_with(&scratch, "client-charpre-late", setline, TYPING);
    typed(&mut term, b"A");
    for c in "abcdefghij".chars() {
        typed(&mut term, c.to_string().as_bytes());
    }
    // The agent's count before the q.
    let count = |a: &Vec<Value>| a.first().and_then(Value::as_u64).expect("a count");
    let before = link
        .trace
        .acks()
        .last()
        .map(|(_, a)| count(a))
        .expect("an ack");
    let drawn = ahead(&mut term, "q, proved", b"q");
    assert!(sight(&drawn).0[0].starts_with("xabcdefghijq"));
    // Corrected within a link delay of the ack covering the q, however late
    // that comes, and well before the 2+ round trips after which a
    // prediction is given up without one.
    let mut right = None;
    assert!(
        term.frames(Duration::from_secs(10), |screen| {
            let put = sight(screen).0[0].starts_with("xabcdefghijQ!");
            if put {
                right = Some(Instant::now());
            }
            put
        }),
        "put right:\n{}",
        term.text()
    );
    let right = right.expect("put right");
    // The link traces a write just after making it, and the client may draw
    // first.
    let covering = || {
        link.trace
            .acks()
            .into_iter()
            .find(|(_, a)| count(a) > before)
            .map(|(written, _)| written)
    };
    assert!(
        common::wait_until(Duration::from_secs(1), || covering().is_some()),
        "no ack covering the q"
    );
    let acked = covering().expect("an ack covering the q");
    assert!(
        right < acked + SLOW,
        "put right {:?} after the ack that covered the q",
        right.saturating_duration_since(acked)
    );
    // Take its trailing frames before the next key.
    last_frame(&mut term, SLOW);
    waited(&mut term, "q, proving itself again", b"q");
}

/// An `InsertLeave` trimming trailing blanks on every line (its `:%s` leaves
/// the cursor on the last line trimmed) makes `<Esc>` more than a cursor
/// move: `<Esc>` and the keys after it are not drawn until it has proved
/// itself.
#[test]
fn insert_leave_shows_nothing_wrong_till_leaving_has_proved_itself() {
    require_nvim!();
    let setline = r#"call setline(1, ["one", "two three   ", "four", "line 1"]) | autocmd InsertLeave * silent! keepjumps keeppatterns %s/\s\+$//e"#;
    let mut keys: Vec<(Vec<u8>, String)> = Vec::new();
    for _ in 0..6 {
        for (bytes, name) in [
            (&b"A"[..], "A"),
            (b"x", "x"),
            (b"\x1b", "<Esc>"),
            (b"j", "j"),
            (b"$", "$"),
            (b"k", "k"),
            (b"0", "0"),
        ] {
            keys.push((bytes.to_vec(), name.into()));
        }
    }
    for seed in [3, 4, 5] {
        sights_are_neovims(
            &format!("client-trim-{seed}"),
            setline,
            TYPING,
            &keys,
            Lcg(seed),
        );
    }
}

/// With a harmless `InsertLeave`, `<Esc>` is left to Neovim until eight have
/// been judged right, then predicted.
#[test]
fn esc_is_drawn_ahead_once_leaving_has_proved_itself() {
    require_nvim!();
    let scratch = Scratch::new("client-left");
    let setline = r#"call setline(1, ["x", "line 1"]) | autocmd InsertLeave * let g:left = get(g:, 'left', 0) + 1"#;
    let (_rpc, mut term, _) = drawn_slowly_with(&scratch, "client-left", setline, TYPING);
    for _ in 0..8 {
        typed(&mut term, b"A");
        waited(&mut term, "<Esc>, proving itself", b"\x1b");
    }
    typed(&mut term, b"A");
    predicted(&mut term, "<Esc>, proved", b"\x1b");
}

/// A trimming `InsertLeave` defined after `<Esc>` has been predicted nine
/// times (a late plugin, or by hand) sends the next `<Esc>` back to proving
/// itself, even though Insert mode was not re-entered: it must not be drawn
/// off the line the `:%s` leaves the cursor on.
#[test]
fn an_insert_leave_made_late_has_esc_prove_itself() {
    require_nvim!();
    let scratch = Scratch::new("client-late-leave");
    let setline = r#"call setline(1, ["one", "ab c", "line 1"])"#;
    let (mut rpc, mut term, _) = drawn_slowly_with(&scratch, "client-late-leave", setline, TYPING);
    for _ in 0..9 {
        typed(&mut term, b"A");
        predicted(&mut term, "<Esc>, with nothing to prove", b"\x1b");
    }
    rpc.command(r"autocmd InsertLeave * silent! keepjumps keeppatterns %s/\s\+$//e")
        .expect("autocmd");
    typed(&mut term, b"j$xk");
    // `<Esc>` before Neovim has the `A`, but after the `A` is drawn ahead
    // (a delayed client may take longer than a fixed wait).
    ahead(&mut term, "A", b"A");
    waited(&mut term, "<Esc>, proving itself", b"\x1b");
    assert_eq!(cursor(term.parser.screen()).0, 1, "on the line trimmed");
}

#[test]
fn random_typing_shows_nothing_neovim_never_drew() {
    require_nvim!();
    const KEYS: &[(&[u8], &str)] = &[
        (b" ", "<Space>"),
        (b",", ","),
        (b".", "."),
        (b"(", "("),
        (b"\x7f", "<BS>"),
        (b"\x7f", "<BS>"),
    ];
    let setline =
        r#"set list listchars=tab:>-,trail:-,eol:$ | call setline(1, ["\tend", "line 1"])"#;
    const OUT: &[(&[u8], &str)] = &[
        (b"j", "j"),
        (b"k", "k"),
        (b"h", "h"),
        (b"l", "l"),
        (b"0", "0"),
        (b"$", "$"),
    ];
    const IN: &[(&[u8], &str)] = &[(b"i", "i"), (b"a", "a"), (b"A", "A"), (b"I", "I")];
    let mut rng = Lcg(5);
    let mut keys: Vec<(Vec<u8>, String)> =
        vec![(b"0".to_vec(), "0".into()), (b"i".to_vec(), "i".into())];
    while keys.len() < 200 {
        let (bytes, name) = match rng.below(12) {
            0..=3 => KEYS[rng.below(KEYS.len() as u64) as usize],
            4 => {
                keys.push((b"\x1b".to_vec(), "<Esc>".into()));
                let (bytes, name) = OUT[rng.below(OUT.len() as u64) as usize];
                keys.push((bytes.to_vec(), name.into()));
                IN[rng.below(IN.len() as u64) as usize]
            }
            _ => {
                let c = (b'a' + rng.below(26) as u8) as char;
                keys.push((c.to_string().into_bytes(), c.to_string()));
                continue;
            }
        };
        keys.push((bytes.to_vec(), name.into()));
    }
    sights_are_neovims("client-tfuzz", setline, TYPING, &keys, rng);
}

fn cursor(screen: &vt100::Screen) -> (u16, u16) {
    screen.cursor_position()
}

/// `l` held faster than the link answers: each is drawn before Neovim has
/// it, one column at a time (Neovim's bursty frames would jump), ending
/// where Neovim's cursor does.
#[test]
fn motions_typed_faster_than_the_link_answers_keep_up() {
    require_nvim!();
    let scratch = Scratch::new("client-held-l");
    let (mut rpc, mut term, _) = drawn_slowly_with(&scratch, "client-held-l", MOVING, MOTIONS);
    typed(&mut term, b"8G0");
    let start = cursor(term.parser.screen());
    let mut seen: Vec<(Instant, (u16, u16))> = vec![(Instant::now(), start)];
    let mut typed_at = Vec::new();
    let mut note = |screen: &vt100::Screen| {
        if seen.last().map(|s| s.1) != Some(cursor(screen)) {
            seen.push((Instant::now(), cursor(screen)));
        }
    };
    for k in 0..20 {
        let at = Instant::now();
        typed_at.push(at);
        term.type_bytes(b"l");
        // As in holding_j_past_the_bottom_keeps_up.
        let col = start.1 + k + 1;
        term.frames(SLOW, |screen| {
            note(screen);
            cursor(screen).1 == col
        });
        term.frames(
            Duration::from_millis(30).saturating_sub(at.elapsed()),
            |screen| {
                note(screen);
                false
            },
        );
    }
    term.frames(4 * SLOW, |screen| {
        note(screen);
        false
    });
    let cols: Vec<i32> = seen.iter().map(|s| i32::from(s.1 .1)).collect();
    let when: Vec<(Duration, (u16, u16))> = seen
        .iter()
        .map(|(t, c)| (t.saturating_duration_since(typed_at[0]), *c))
        .collect();
    assert!(
        seen.iter().all(|s| s.1 .0 == start.0),
        "along the line: {when:?}"
    );
    assert!(
        cols.windows(2).all(|w| w[1] - w[0] == 1),
        "a column at a time: {when:?}"
    );
    for (k, at) in typed_at.iter().enumerate() {
        let col = start.1 + k as u16 + 1;
        let drawn = seen.iter().find(|s| s.1 .1 == col).map(|s| s.0);
        assert!(
            drawn.is_some_and(|d| d < *at + SLOW),
            "the {}th `l` waited: {when:?}",
            k + 1
        );
    }
    let at = rpc
        .call("nvim_win_get_cursor", vec![0.into()])
        .expect("Neovim's cursor");
    let byte = at
        .as_array()
        .and_then(|a| a[1].as_u64())
        .expect("its column");
    assert_eq!(byte, 20, "Neovim's");
    assert_eq!(cursor(term.parser.screen()), (start.0, start.1 + 20));
}

/// A motion mapped after attach (late plugin, or by hand) is noticed within
/// about a second of Neovim waiting for a key, and left to Neovim. Here `l`
/// is mapped to `h`.
#[test]
fn a_motion_mapped_after_the_attach_is_left_to_neovim() {
    require_nvim!();
    let scratch = Scratch::new("client-mapped-late");
    let (mut rpc, mut term, _) = drawn_slowly_with(&scratch, "client-mapped-late", MOVING, MOTIONS);
    typed(&mut term, b"8G8l");
    let start = cursor(term.parser.screen());
    rpc.command("nnoremap l h").expect("mapped");
    term.pump_until(Duration::from_secs(2), |_| false);
    term.type_bytes(b"l");
    let mut seen = vec![start];
    term.frames(4 * SLOW, |screen| {
        if seen.last() != Some(&cursor(screen)) {
            seen.push(cursor(screen));
        }
        false
    });
    assert_eq!(seen, [start, (start.0, start.1 - 1)], "as Neovim took it");
}

/// Blank inline virtual text (padding, as some plugins draw) on a line
/// ending in blanks is invisible on screen, but the agent sends Neovim's
/// measure of the line, so the client leaves its motions to Neovim.
#[test]
fn a_line_padded_with_blank_virtual_text_is_left_to_neovim() {
    require_nvim!();
    let scratch = Scratch::new("client-padded");
    let setup = "call setline(1, ['x     ', 'line 1'])";
    let (mut rpc, mut term, _) = drawn_slowly_with(&scratch, "client-padded", setup, MOTIONS);
    rpc.command(
        "lua vim.api.nvim_buf_set_extmark(0, vim.api.nvim_create_namespace('pad'), 0, 1, \
         { virt_text = { { '   ', 'Normal' } }, virt_text_pos = 'inline' })",
    )
    .expect("padded");
    typed(&mut term, b"gg0");
    let start = cursor(term.parser.screen());
    term.type_bytes(b"3l");
    let mut seen = vec![start];
    term.frames(4 * SLOW, |screen| {
        if seen.last() != Some(&cursor(screen)) {
            seen.push(cursor(screen));
        }
        false
    });
    assert_eq!(
        seen,
        [start, (start.0, start.1 + 6)],
        "byte 3, where Neovim draws it"
    );
}

/// The same, for a line the cursor moves onto: the agent measures every line
/// in view ending in blanks.
#[test]
fn a_line_moved_onto_padded_with_blank_virtual_text_is_left_to_neovim() {
    require_nvim!();
    let scratch = Scratch::new("client-padded-onto");
    let setup = "call setline(1, ['x     ', 'abcdefgh line 1'])";
    let (mut rpc, mut term, _) = drawn_slowly_with(&scratch, "client-padded-onto", setup, MOTIONS);
    rpc.command(
        "lua vim.api.nvim_buf_set_extmark(0, vim.api.nvim_create_namespace('pad'), 0, 1, \
         { virt_text = { { '   ', 'Normal' } }, virt_text_pos = 'inline' })",
    )
    .expect("padded");
    typed(&mut term, b"gg0j0lll");
    let start = cursor(term.parser.screen());
    term.type_bytes(b"k");
    let mut seen = vec![start];
    term.frames(4 * SLOW, |screen| {
        if seen.last() != Some(&cursor(screen)) {
            seen.push(cursor(screen));
        }
        false
    });
    assert_eq!(
        seen,
        [start, (start.0 - 1, start.1 + 1)],
        "byte 1, where Neovim draws it past the padding"
    );
}

/// A seeded, reproducible random number generator.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

/// The window's rows and the cursor.
type Sight = (Vec<String>, (u16, u16));

fn sight(screen: &vt100::Screen) -> Sight {
    let rows = (0..22)
        .map(|r| {
            (0..COLS)
                .map(|c| match screen.cell(r, c) {
                    Some(cell) if cell.has_contents() => cell.contents(),
                    _ => " ",
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    (rows, screen.cursor_position())
}

#[test]
fn random_motions_show_nothing_neovim_never_drew() {
    require_nvim!();
    // As the terminal sends them, and as Neovim names them.
    const KEYS: &[(&[u8], &str)] = &[
        (b"l", "l"),
        (b"h", "h"),
        (b"0", "0"),
        (b"$", "$"),
        (b"^", "^"),
        (b"g_", "g_"),
        (b"|", "|"),
        (b"\x1b[H", "<Home>"),
        (b"\x1b[F", "<End>"),
        (b" ", "<Space>"),
        (b"\x7f", "<BS>"),
        (b"j", "j"),
        (b"k", "k"),
    ];
    const COUNTED: &[&str] = &["l", "h", "|", "<Space>", "<BS>"];
    random_motions("client-fuzz", "", KEYS, COUNTED);
}

/// With counted motions to other lines, and `'number'`/`'relativenumber'`:
/// sights compare whole rows, so the gutter must match too.
#[test]
fn random_motions_to_other_lines_show_nothing_neovim_never_drew() {
    require_nvim!();
    const KEYS: &[(&[u8], &str)] = &[
        (b"l", "l"),
        (b"h", "h"),
        (b"$", "$"),
        (b"^", "^"),
        (b"g_", "g_"),
        (b" ", "<Space>"),
        (b"\x7f", "<BS>"),
        (b"j", "j"),
        (b"k", "k"),
        (b"j", "j"),
        (b"k", "k"),
        (b"\x1b[B", "<Down>"),
        (b"\x1b[A", "<Up>"),
        (b"\x0e", "<C-n>"),
        (b"\x10", "<C-p>"),
        (b"+", "+"),
        (b"-", "-"),
        (b"_", "_"),
        (b"\r", "<CR>"),
    ];
    const COUNTED: &[&str] = &[
        "l", "h", "$", "g_", "<Space>", "<BS>", "j", "k", "+", "-", "_",
    ];
    random_motions("client-fuzz-nu", "set nu rnu", KEYS, COUNTED);
}

#[test]
fn random_motions_by_words_show_nothing_neovim_never_drew() {
    require_nvim!();
    random_motions("client-fuzz-w", "set nu rnu", WORD_KEYS, WORD_COUNTED);
}

/// Word motions over [`SCROLLED`], scrolling the view past the window's edge.
#[test]
fn random_motions_by_words_past_the_edges_show_nothing_neovim_never_drew() {
    require_nvim!();
    let mut rng = Lcg(13);
    let keys = random_keys(&mut rng, WORD_KEYS, WORD_COUNTED);
    sights_compared("client-fuzz-ws", SCROLLED, MOTIONS, &keys, rng, false);
}

/// Some characters are on only some lines, so some counts exceed the matches.
#[test]
fn random_motions_to_a_character_show_nothing_neovim_never_drew() {
    require_nvim!();
    random_motions("client-fuzz-f", "set nu rnu", FIND_KEYS, FIND_COUNTED);
}

/// `}` and `{` with counts over [`SCROLLED`], where empty lines end
/// paragraphs and blank-only lines do not.
#[test]
fn random_motions_by_paragraphs_show_nothing_neovim_never_drew() {
    require_nvim!();
    const KEYS: &[(&[u8], &str)] = &[
        (b"}", "}"),
        (b"{", "{"),
        (b"}", "}"),
        (b"{", "{"),
        (b"j", "j"),
        (b"k", "k"),
        (b"$", "$"),
        (b"0", "0"),
    ];
    const COUNTED: &[&str] = &["}", "{", "j", "k"];
    let mut rng = Lcg(17);
    let keys = random_keys(&mut rng, KEYS, COUNTED);
    sights_compared("client-fuzz-p", SCROLLED, MOTIONS, &keys, rng, false);
}

/// `H`, `M`, `L` with counts (`M` ignores its count) among scrolls, with
/// `'scrolloff'` 3, which they respect except at the buffer's edges.
#[test]
fn random_motions_to_the_window_edges_show_nothing_neovim_never_drew() {
    require_nvim!();
    const KEYS: &[(&[u8], &str)] = &[
        (b"H", "H"),
        (b"M", "M"),
        (b"L", "L"),
        (b"H", "H"),
        (b"L", "L"),
        (b"j", "j"),
        (b"k", "k"),
        (b"$", "$"),
        (b"0", "0"),
        (b"\x05", "<C-e>"),
        (b"\x19", "<C-y>"),
        (b"\x04", "<C-d>"),
        (b"\x15", "<C-u>"),
    ];
    const COUNTED: &[&str] = &["H", "M", "L", "j", "k", "<C-e>", "<C-y>"];
    let mut rng = Lcg(19);
    let keys = random_keys(&mut rng, KEYS, COUNTED);
    let setline = format!("set nu rnu so=3 | {SCROLLED}");
    sights_compared("client-fuzz-hml", &setline, MOTIONS, &keys, rng, false);
}

/// Jumps too far for a minimal scroll (`gg`/`G` with counts, `j`/`k` past
/// half the window, `}`/`{`) with `'scrolloff'` 3: Neovim centres the cursor
/// or pins the buffer's first or last line to the edge.
#[test]
fn random_long_jumps_show_nothing_neovim_never_drew() {
    require_nvim!();
    const KEYS: &[(&[u8], &str)] = &[
        (b"gg", "gg"),
        (b"G", "G"),
        (b"j", "j"),
        (b"k", "k"),
        (b"25j", "25j"),
        (b"25k", "25k"),
        (b"14j", "14j"),
        (b"14k", "14k"),
        (b"60j", "60j"),
        (b"60k", "60k"),
        (b"}", "}"),
        (b"{", "{"),
        (b"$", "$"),
        (b"\x04", "<C-d>"),
        (b"\x15", "<C-u>"),
        (b"L", "L"),
    ];
    const COUNTED: &[&str] = &["gg", "G", "}", "{", "j", "k"];
    let mut rng = Lcg(23);
    let keys = random_keys(&mut rng, KEYS, COUNTED);
    let setline = format!("set nu rnu so=3 | {SCROLLED}");
    sights_compared("client-fuzz-jmp", &setline, MOTIONS, &keys, rng, false);
}

/// `f`, `t`, `F`, `T` for letters on several of [`MOVING`]'s lines, a blank,
/// a tab, wide characters, an emoji and an accented letter; plus `j`, `k`,
/// `0`, `$`.
const FIND_KEYS: &[(&[u8], &str)] = &[
    (b"fo", "fo"),
    (b"to", "to"),
    (b"Fo", "Fo"),
    (b"To", "To"),
    (b"fe", "fe"),
    (b"te", "te"),
    (b"Fe", "Fe"),
    (b"Te", "Te"),
    (b"f ", "f<Space>"),
    (b"T ", "T<Space>"),
    (b"f\t", "f<Tab>"),
    (b"F\t", "F<Tab>"),
    ("fの".as_bytes(), "fの"),
    ("T日".as_bytes(), "T日"),
    ("f😀".as_bytes(), "f😀"),
    ("Té".as_bytes(), "Té"),
    (b"j", "j"),
    (b"k", "k"),
    (b"0", "0"),
    (b"$", "$"),
];

const FIND_COUNTED: &[&str] = &["fo", "to", "Fo", "To", "fe", "Fe", "f<Space>", "j", "k"];

const WORD_KEYS: &[(&[u8], &str)] = &[
    (b"w", "w"),
    (b"w", "w"),
    (b"b", "b"),
    (b"b", "b"),
    (b"e", "e"),
    (b"ge", "ge"),
    (b"W", "W"),
    (b"B", "B"),
    (b"E", "E"),
    (b"gE", "gE"),
    (b"\x1b[1;2C", "<S-Right>"),
    (b"\x1b[1;5C", "<C-Right>"),
    (b"\x1b[1;2D", "<S-Left>"),
    (b"\x1b[1;5D", "<C-Left>"),
    (b"j", "j"),
    (b"k", "k"),
    (b"l", "l"),
    (b"$", "$"),
    (b"0", "0"),
];

const WORD_COUNTED: &[&str] = &["w", "b", "e", "ge", "W", "B", "E", "gE", "j", "k"];

/// [`random_keys`] from `alphabet` over [`MOVING`] plus `setup`, checked with
/// [`sights_are_neovims`].
fn random_motions(tag: &str, setup: &str, alphabet: &[(&[u8], &str)], counted: &[&str]) {
    let mut rng = Lcg(7);
    let keys = random_keys(&mut rng, alphabet, counted);
    let setline = match setup {
        "" => MOVING.to_string(),
        setup => format!("{setup} | {MOVING}"),
    };
    sights_are_neovims(tag, &setline, MOTIONS, &keys, rng);
}

/// 200 keys from `alphabet`, those in `counted` with a count a third of the
/// time, and the odd `x`; each as the terminal sends it and as Neovim names
/// it.
fn random_keys(
    rng: &mut Lcg,
    alphabet: &[(&[u8], &str)],
    counted: &[&str],
) -> Vec<(Vec<u8>, String)> {
    let mut keys: Vec<(Vec<u8>, String)> = Vec::new();
    while keys.len() < 200 {
        if rng.below(15) == 0 {
            keys.push((b"x".to_vec(), "x".into()));
            continue;
        }
        let (bytes, name) = alphabet[rng.below(alphabet.len() as u64) as usize];
        let count = if counted.contains(&name) && rng.below(3) == 0 {
            (1 + rng.below(9)).to_string()
        } else {
            String::new()
        };
        let mut b = count.clone().into_bytes();
        b.extend_from_slice(bytes);
        keys.push((b, format!("{count}{name}")));
    }
    keys
}

/// 200 lines to scroll over: `line N` with varying words, among tabs, wide
/// text, blank-only and empty lines.
const SCROLLED: &str = r#"call setline(1, map(range(1, 200), 'v:val % 9 == 4 ? "\tint x" . v:val . " = 0;" : v:val % 11 == 5 ? "" : v:val % 13 == 6 ? "日本語のテキスト " . v:val : v:val % 17 == 7 ? "    " : "line " . v:val . repeat(" word", v:val % 8)'))"#;

/// Random motions and `<C-e>`/`<C-y>`/`<C-d>`/`<C-u>` with counts over
/// [`SCROLLED`] plus `setup`, checked by [`sights_compared`].
fn random_scrolls(tag: &str, setup: &str, coloured: bool) {
    const KEYS: &[(&[u8], &str)] = &[
        (b"l", "l"),
        (b"h", "h"),
        (b"$", "$"),
        (b"0", "0"),
        (b"j", "j"),
        (b"k", "k"),
        (b"j", "j"),
        (b"k", "k"),
        (b"j", "j"),
        (b"+", "+"),
        (b"-", "-"),
        (b"\x05", "<C-e>"),
        (b"\x19", "<C-y>"),
        (b"\x04", "<C-d>"),
        (b"\x15", "<C-u>"),
    ];
    const COUNTED: &[&str] = &["l", "h", "j", "k", "+", "-", "<C-e>", "<C-y>"];
    let mut rng = Lcg(11);
    let keys = random_keys(&mut rng, KEYS, COUNTED);
    let setline = match setup {
        "" => SCROLLED.to_string(),
        setup => format!("{setup} | {SCROLLED}"),
    };
    sights_compared(tag, &setline, MOTIONS, &keys, rng, coloured);
}

#[test]
fn random_scrolls_and_motions_show_nothing_neovim_never_drew() {
    require_nvim!();
    random_scrolls("fz-scroll", "", false);
}

#[test]
fn random_scrolls_and_motions_number_the_rows_as_neovim_does() {
    require_nvim!();
    random_scrolls("fz-scroll-nu", "set nu rnu", false);
}

#[test]
fn random_scrolls_and_motions_draw_the_bar_as_neovim_does() {
    require_nvim!();
    random_scrolls(
        "fz-scroll-cul",
        "set termguicolors nu rnu cursorline scrolloff=3 | exe 'NoMatchParen'",
        true,
    );
}

/// `j` past the bottom and `k` past the top: a line with a sign or
/// virtual text (end-of-line or overlay) is never scrolled in without it.
#[test]
fn lines_with_signs_or_virtual_text_are_not_scrolled_into_view_without_them() {
    require_nvim!();
    let setline = format!(
        "set signcolumn=yes | {SCROLLING} | \
         exe 'lua local api, ns = vim.api, vim.api.nvim_create_namespace(\"t\") \
         vim.fn.sign_define(\"S\", {{ text = \">>\" }}) \
         for l = 24, 70, 5 do vim.fn.sign_place(0, \"g\", \"S\", \"%\", {{ lnum = l }}) end \
         for l = 26, 70, 5 do api.nvim_buf_set_extmark(0, ns, l, 0, \
         {{ virt_text = {{ {{ \"E: oops\", \"Error\" }} }} }}) end \
         for l = 28, 70, 5 do api.nvim_buf_set_extmark(0, ns, l, 0, \
         {{ virt_text = {{ {{ \"XX\", \"Error\" }} }}, virt_text_pos = \"overlay\" }}) end'"
    );
    let mut keys: Vec<(Vec<u8>, String)> = vec![(b"L".to_vec(), "L".into())];
    keys.extend((0..40).map(|_| (b"j".to_vec(), "j".to_string())));
    keys.push((b"H".to_vec(), "H".into()));
    keys.extend((0..30).map(|_| (b"k".to_vec(), "k".to_string())));
    sights_are_neovims("client-signs", &setline, MOTIONS, &keys, Lcg(9));
}

/// Type `keys` (terminal bytes, Neovim name) in `rng`-timed bursts over a
/// slow link to a client configured with `client`, in a session set up by
/// `setline`. Every sight it shows must be one Neovim drew for the keys so
/// far, in order, and it must end where Neovim does. The reference is the
/// same keys typed one at a time into a second session drawn by a client
/// that predicts nothing. MatchParen differs between the two, but only in
/// colour, which is not compared.
fn sights_are_neovims(
    tag: &str,
    setline: &str,
    client: &str,
    keys: &[(Vec<u8>, String)],
    rng: Lcg,
) {
    sights_compared(tag, setline, client, keys, rng, false);
}

/// [`sights_are_neovims`], comparing colours too if `coloured`; `setline`
/// must then disable MatchParen.
fn sights_compared(
    tag: &str,
    setline: &str,
    client: &str,
    keys: &[(Vec<u8>, String)],
    mut rng: Lcg,
    coloured: bool,
) {
    type Seen = (
        Sight,
        Option<Vec<Vec<(vt100::Color, vt100::Color, bool, bool)>>>,
    );
    let sight =
        |screen: &vt100::Screen| -> Seen { (sight(screen), coloured.then(|| colours_of(screen))) };
    let scratch = Scratch::new(tag);
    let (_rpc, mut term, _) = drawn_slowly_with(&scratch, tag, setline, client);
    // Bursts of 1-6 keys 30 ms apart, with pauses between.
    let mut seen: Vec<Seen> = vec![sight(term.parser.screen())];
    let mut at = 0;
    while at < keys.len() {
        let burst = (1 + rng.below(6) as usize).min(keys.len() - at);
        for (bytes, _) in &keys[at..at + burst] {
            term.type_bytes(bytes);
            term.frames(Duration::from_millis(30), |screen| {
                let s = sight(screen);
                if seen.last() != Some(&s) {
                    seen.push(s);
                }
                false
            });
        }
        at += burst;
        let pause = Duration::from_millis(50 + rng.below(650));
        term.frames(pause, |screen| {
            let s = sight(screen);
            if seen.last() != Some(&s) {
                seen.push(s);
            }
            false
        });
    }
    term.frames(6 * SLOW, |screen| {
        let s = sight(screen);
        if seen.last() != Some(&s) {
            seen.push(s);
        }
        false
    });

    // The reference: the same keys one at a time to another Neovim, without
    // MatchParen or a slow link, scroll effect off as above.
    let other = Scratch::new(&format!("{tag}-ref"));
    let (sock, mut rpc, config) = session(&other, &format!("{tag}-ref"));
    rpc.command(&format!("setlocal noswapfile | {setline} | NoMatchParen"))
        .expect("setline");
    std::fs::write(
        &config,
        format!("[client]\n{NOTHING}\n[effects.scroll]\nenabled = false\n"),
    )
    .expect("config");
    let mut reference = Terminal::spawn(&sock, &config);
    assert!(
        reference.pump_until(Duration::from_secs(20), |s| s.contains("line 1")),
        "never drawn:\n{}",
        reference.text()
    );
    // The screen once Neovim has drawn everything so far: the frame showing
    // a mark put in the status line (below the compared rows; the command
    // line is Insert mode's). Waiting for quiet would not do: a busy machine
    // can go quiet before anything is drawn.
    let drawn = |reference: &mut Terminal, rpc: &mut nvmux::rpc::Client<_>, mark: &str| {
        rpc.command(&format!("let &l:statusline = '{mark}'"))
            .expect("mark");
        let mut seen = None;
        reference.frames(Duration::from_secs(10), |screen| {
            let row = screen.contents_between(ROWS - 2, 0, ROWS - 2, COLS);
            if row.contains(mark) {
                seen = Some(sight(screen));
            }
            seen.is_some()
        });
        seen.unwrap_or_else(|| panic!("{mark} never drawn:\n{}", reference.text()))
    };
    let mut drew: Vec<Seen> = vec![drawn(&mut reference, &mut rpc, "drawn")];
    for (k, (_, name)) in keys.iter().enumerate() {
        rpc.call("nvim_input", vec![name.as_str().into()])
            .expect("input");
        // Neovim takes it before answering the next call.
        drew.push(drawn(&mut reference, &mut rpc, &format!("key {k} taken")));
    }

    // Each sight must be one Neovim drew, no earlier than the previous.
    let mut from = 0;
    for (i, s) in seen.iter().enumerate() {
        match drew[from..].iter().position(|d| d == s) {
            Some(k) => from += k,
            None => panic!(
                "sight {i} of {} is none Neovim drew from key {from} on ({:?}): \
                 cursor {:?}, rows\n{}\nNeovim's after that key: cursor {:?}, rows\n{}{}",
                seen.len(),
                keys.get(from).map(|k| &k.1),
                s.0 .1,
                s.0 .0.join("\n"),
                drew[from].0 .1,
                drew[from].0 .0.join("\n"),
                if s.0 == drew[from].0 {
                    "\n(in other colours)"
                } else {
                    ""
                },
            ),
        }
    }
    assert_eq!(seen.last(), drew.last(), "ends where Neovim does");
}

/// A run of pages longer than the kept lines drops those furthest behind
/// rather than restarting from two windows, so every page is still drawn
/// ahead. Edits among the kept lines afterwards are drawn as Neovim does.
#[test]
fn a_long_run_of_pages_lets_go_of_the_lines_behind() {
    require_nvim!();
    let scratch = Scratch::new("client-long-run");
    let (mut rpc, mut term, link) = drawn_slowly(
        &scratch,
        "client-long-run",
        "call setline(1, map(range(1, 6000), '\"line \" . v:val'))",
    );
    // Past the client's 4000-line maximum, short of the buffer's end.
    let run = pages(&mut rpc, &mut term, &link, Duration::from_millis(30), 200);
    for (k, late) in run.late().iter().enumerate() {
        assert!(
            late.is_some_and(|l| l < SLOW),
            "page {} waited: {late:?}\n{}",
            k + 1,
            run.timeline(k + 1)
        );
    }
    // Edits above the view (among the kept lines) and in it.
    rpc.command("call setline(3990, 'edited 3990') | call append(4005, ['new a', 'new b'])")
        .expect("edit");
    term.pump_until(4 * SLOW, |_| false);
    predicted(&mut term, "<C-b> over lines edited", b"\x02");
    predicted(&mut term, "<C-f> back", b"\x06");
    predicted(&mut term, "<C-f> past them", b"\x06");
}

/// An edit is sent as the changed lines only, not every kept line; and pages
/// over lines edited (by keys or the API, above, in and below the view) are
/// drawn as Neovim does.
#[test]
fn edits_reach_the_client_as_what_they_changed() {
    require_nvim!();
    let scratch = Scratch::new("client-edits");
    // Lines long enough that resending them all would show in the count.
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

    // Below the view, as a formatter or language server would; the cursor
    // stays put.
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
    // A substitute over lines in and out of view.
    typed(&mut term, b":%s/line 4/LINE 4/\r");
    for (name, keys) in [
        ("<C-f> after :s", &b"\x06"[..]),
        ("<C-f> again after :s", b"\x06"),
        ("<C-f> past them", b"\x06"),
    ] {
        predicted(&mut term, name, keys);
    }
}

/// The window's colours, cell by cell; predictions draw syntax colours too.
fn colours(term: &Terminal) -> Vec<Vec<(vt100::Color, vt100::Color, bool, bool)>> {
    colours_of(term.parser.screen())
}

fn colours_of(screen: &vt100::Screen) -> Vec<Vec<(vt100::Color, vt100::Color, bool, bool)>> {
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

/// Rows a predicted scroll uncovers get their syntax colours, including
/// lines edited by the API or keys: no cell changes when Neovim's rows land.
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
        let drawn = ahead(term, name, keys);
        let predicted = (sight(&drawn), colours_of(&drawn));
        // Several colours per row (keyword, number, comment), except lines
        // edited wholly into a comment or string.
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
        let landed = last_frame(term, 4 * SLOW);
        let landed = (sight(&landed), colours_of(&landed));
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
    // Below the view, as a formatter would: one line made a comment, one a
    // string.
    rpc.command(
        "call nvim_buf_set_lines(0, 60, 62, v:true, \
         ['-- local v61 = 61 -- line 61', 'local s = [[line 62]] -- 62'])",
    )
    .expect("edit below");
    term.pump_until(4 * SLOW, |_| false);
    step(&mut term, "<C-f> over lines edited", b"\x06");
    // In the view, by keys: a comment line opened below.
    typed(&mut term, b"o-- typed, local v = 1");
    typed(&mut term, b"\x1b");
    step(&mut term, "<C-f> past a line typed", b"\x06");
    step(&mut term, "<C-b> back over it", b"\x02");
}

/// With the default colorscheme's `'cursorline'` over syntax-coloured lines,
/// a predicted line motion moves the bar and its number at once; no cell
/// changes when Neovim's frame lands.
#[test]
fn cursorline_moves_with_the_cursor() {
    require_nvim!();
    let scratch = Scratch::new("client-cursorline");
    let (_rpc, mut term, _) = drawn_slowly_with(
        &scratch,
        "client-cursorline",
        "call setline(1, map(range(1, 30), \
         '\"local v\" . v:val . \" = \" . v:val . \" -- line \" . v:val')) | \
         set termguicolors cursorline nu rnu | lua vim.treesitter.start(0, 'lua')",
        MOTIONS,
    );
    term.pump_until(Duration::from_secs(2), |_| false);
    for (name, keys) in [
        ("j", &b"j"[..]),
        ("k", b"k"),
        ("3j", b"3j"),
        ("2k", b"2k"),
        ("+", b"+"),
    ] {
        let before = colours(&term);
        let drawn = ahead(&mut term, name, keys);
        let predicted = (sight(&drawn), colours_of(&drawn));
        assert_ne!(predicted.1, before, "{name}: the bar moved");
        let landed = last_frame(&mut term, 4 * SLOW);
        let landed = (sight(&landed), colours_of(&landed));
        assert_eq!(predicted.0, landed.0, "{name}: the text and the cursor");
        for (r, (p, l)) in predicted.1.iter().zip(&landed.1).enumerate() {
            assert_eq!(p, l, "{name}: row {r} changed colour when Neovim's landed");
        }
    }
}

/// A [`sight`] with its [`colours`].
type Coloured = (Sight, Vec<Vec<(vt100::Color, vt100::Color, bool, bool)>>);

/// Append each new coloured sight shown within `within` to `seen`.
fn watch(term: &mut Terminal, seen: &mut Vec<Coloured>, within: Duration) {
    if seen.is_empty() {
        let screen = term.parser.screen();
        seen.push((sight(screen), colours_of(screen)));
    }
    term.frames(within, |screen| {
        let now = (sight(screen), colours_of(screen));
        if seen.last() != Some(&now) {
            seen.push(now);
        }
        false
    });
}

/// A window-local highlight namespace, or `'winhighlight'` remapping `LineNr`
/// or `CursorLine` (as file trees do), draws the cursor line in groups the
/// client does not know, so `j` is left to Neovim: only Neovim's frames show.
/// A window namespace raises no event; it is found when Neovim next waits
/// for a key.
#[test]
fn a_window_with_highlights_of_its_own_leaves_its_motions_to_other_lines_to_neovim() {
    require_nvim!();
    let scratch = Scratch::new("client-winhl");
    let (mut rpc, mut term, _) = drawn_slowly_with(
        &scratch,
        "client-winhl",
        "call setline(1, map(range(1, 30), '\"line \" . v:val')) | \
         set termguicolors nu rnu | hi MyNr guifg=#0000ff | hi MyCul guibg=#550000",
        MOTIONS,
    );
    for (what, set) in [
        (
            "a namespace",
            "lua local ns = vim.api.nvim_create_namespace('own') \
             vim.api.nvim_set_hl(ns, 'LineNr', { fg = '#ff8800' }) \
             vim.api.nvim_set_hl(ns, 'LineNrAbove', { fg = '#ff0000' }) \
             vim.api.nvim_win_set_hl_ns(0, ns)",
        ),
        (
            "'winhighlight'",
            "lua vim.api.nvim_win_set_hl_ns(0, 0) vim.wo.winhighlight = 'LineNr:MyNr'",
        ),
        (
            "'winhighlight' and the bar",
            "setlocal cursorline winhighlight=CursorLine:MyCul",
        ),
    ] {
        rpc.command(set).expect(what);
        term.pump_until(Duration::from_secs(2), |_| false);
        let mut seen = Vec::new();
        term.type_bytes(b"j");
        watch(&mut term, &mut seen, 4 * SLOW);
        assert_eq!(seen.len(), 2, "Neovim's frames alone, {what}");
    }
}

/// With `concealcursor` empty, `j` off a line with concealed text is left to
/// Neovim, which re-conceals the line left; the client must not draw the
/// cursor moved beside it unconcealed.
#[test]
fn concealed_text_leaves_the_motions_to_other_lines_to_neovim() {
    require_nvim!();
    let scratch = Scratch::new("client-conceal");
    let (_rpc, mut term, _) = drawn_slowly_with(
        &scratch,
        "client-conceal",
        "call setline(1, ['a XXhiddenXX word', 'plain', 'line 1']) | \
         set conceallevel=2 concealcursor= | syntax match Hide /XX/ conceal",
        MOTIONS,
    );
    let mut seen = Vec::new();
    term.type_bytes(b"j");
    watch(&mut term, &mut seen, 4 * SLOW);
    assert_eq!(seen.len(), 2, "Neovim's frames alone");
    assert_eq!(seen[1].0 .0[0], "a hidden word", "the line left, concealed");
}

/// Autocommands toggling `'relativenumber'` and `'cursorline'` on Insert
/// enter/leave fire no OptionSet, yet the change reaches the client with
/// the ack of `<Esc>`: a `j` right after shows the cursor only with
/// Neovim's gutter and bar.
#[test]
fn options_autocommands_set_reach_the_client_before_the_motions_after() {
    require_nvim!();
    let scratch = Scratch::new("client-toggled");
    let (_rpc, mut term, _) = drawn_slowly_with(
        &scratch,
        "client-toggled",
        "call setline(1, map(range(1, 30), '\"line \" . v:val')) | \
         set termguicolors nu rnu cursorline updatetime=300 | \
         autocmd InsertEnter * set norelativenumber nocursorline | \
         autocmd InsertLeave * set relativenumber cursorline",
        MOTIONS,
    );
    // Long enough for CursorHoldI to report the Insert-mode window shape.
    typed(&mut term, b"i");
    term.pump_until(Duration::from_secs(1), |_| false);
    let mut seen = Vec::new();
    term.type_bytes(b"\x1b");
    watch(&mut term, &mut seen, Duration::from_millis(40));
    term.type_bytes(b"j");
    watch(&mut term, &mut seen, 4 * SLOW);
    let landed = seen.last().cloned().expect("a sight");
    assert_eq!(
        landed.0 .0[1].get(..4),
        Some("2   "),
        "Neovim's, renumbered"
    );
    for (i, s) in seen.iter().enumerate() {
        if s.0 .1 == landed.0 .1 {
            assert_eq!(
                s, &landed,
                "sight {i}: the cursor on its line beside another gutter or bar"
            );
        }
    }
}

/// Predicted rows show control characters as `^X`, a tab under `'list'`
/// without `tab:` as `^I`, and unprintables as `<xx>`, in `SpecialKey`; and a
/// tab with `tab:` in `Whitespace`. No cell changes when Neovim's rows land.
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
        let drawn = ahead(term, name, keys);
        let predicted = (sight(&drawn), colours_of(&drawn));
        // `^A`, `^?` and `<80>` in another colour on every row.
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
        let landed = last_frame(term, 4 * SLOW);
        let landed = (sight(&landed), colours_of(&landed));
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
    // Now the tab shows via 'listchars', as `>---`.
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

/// Predicted rows (scrolled down or sideways) draw `'listchars'` as Neovim
/// does: single and runs of spaces, leading and trailing, no-break spaces,
/// leading and inner tabs, the line's end, and `extends`/`precedes` in a
/// nowrap view. With `'linebreak'`, leading spaces are in `Whitespace` even
/// with only `leadtab` set. No cell changes when Neovim's rows land.
#[test]
fn predicted_rows_show_listchars_as_neovim_draws_them() {
    require_nvim!();
    let scratch = Scratch::new("client-listchars");
    // Four kinds of line; the fourth is 77 columns, which fits a wrapping
    // window with its end but not one with a gutter.
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
        let drawn = ahead(term, name, keys);
        let predicted = (sight(&drawn), colours_of(&drawn));
        let landed = last_frame(term, 4 * SLOW);
        let landed = (sight(&landed), colours_of(&landed));
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
    // Nowrap with a gutter, on a widest line: from column 1, down and back,
    // then sideways past the end of shorter lines.
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
    // Both edges were drawn: `>` in the last column, `<` in the first after
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

/// Frames written since `from`, counted by their leading cursor hide.
fn frames_since(term: &Terminal, from: usize) -> usize {
    term.output[from..]
        .windows(HIDE_CURSOR.len())
        .filter(|w| *w == HIDE_CURSOR)
        .count()
}

const HIDE_CURSOR: &[u8] = b"\x1b[?25l";

/// A client put aside (`nvmux::card::Command::Away`) draws a constantly
/// changing session about once a second; back in front, it draws every
/// change again at once.
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
    // Change the first line every 20 ms for the rest of the test.
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
    // Let frames already in flight arrive.
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
    // It shows the current tick (or later) well within the second a client
    // at a crawl takes. Neovim is asked first, so a test delay between the
    // two does not count against the client.
    let ticks = rpc
        .eval("g:ticks")
        .ok()
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let shown = |screen: &vt100::Screen| -> u64 {
        sight(screen).0[0]
            .strip_prefix("tick ")
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    };
    assert!(
        term.frames(Duration::from_millis(600), |screen| shown(screen) >= ticks),
        "back in front, the screen says tick {} of {ticks}",
        shown(term.parser.screen())
    );
}

/// A tree-sitter C file whose macros are injected trees: once drawn and idle,
/// Neovim goes quiet. Each parse of different rows remakes or drops the
/// injected trees of both and reports it, so the agent's parse and the
/// highlighter's must not keep triggering each other forever.
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
    // Let the deferred part of the first push and its colours arrive.
    term.pump_until(4 * SLOW, |_| false);
    let before = link.back.load(Ordering::Relaxed);
    term.pump_until(Duration::from_secs(3), |_| false);
    let sent = link.back.load(Ordering::Relaxed) - before;
    assert!(
        sent < 4096,
        "Neovim sent {sent} bytes in 3 s with nothing typed and nothing to draw"
    );
}

/// A long comment typed key by key at the top of a markdown file's lua block
/// turns the held rows below the view into a comment; a predicted page down
/// must colour them as Neovim does. Only the injected lua tree, parsed after
/// the markdown's (by the agent's push of the edited line, or a later slice
/// of the highlighter's parse), reports those rows changed.
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
    // One key at a time, each a separate change and redraw.
    for k in [b"O", b"-", b"-", b"[", b"["] {
        term.type_bytes(k);
        term.pump_until(Duration::from_millis(300), |_| false);
    }
    typed(&mut term, b"\x1b");
    term.pump_until(Duration::from_secs(2), |_| false);
    let drawn = ahead(&mut term, "<C-f>", b"\x06");
    let predicted = (sight(&drawn), colours_of(&drawn));
    let landed = last_frame(&mut term, 4 * SLOW);
    let landed = (sight(&landed), colours_of(&landed));
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

/// A `:lua` command (must come last on its line) counting the agent's
/// notifications by kind, in the editor.
const COUNT_SENT: &str = "lua local send = vim.rpcnotify; _G.nvmux_sent = {}; \
     vim.rpcnotify = function(chan, method, kind, ...) \
       if method == 'nvmux_client' then \
         _G.nvmux_sent[kind] = (_G.nvmux_sent[kind] or 0) + 1 \
       end \
       return send(chan, method, kind, ...) \
     end";

/// See [`COUNT_SENT`].
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

/// Prediction data (lines either side of the view, their colours, window
/// shape) goes to a remote session's client however fast the link, and none
/// to a local one on `"auto"`, even while paging.
#[test]
fn a_session_over_ssh_is_sent_lines_however_fast_the_link() {
    require_nvim!();
    let setline =
        format!("call setline(1, map(range(1, 600), '\"line \" . v:val')) | {COUNT_SENT}");
    let there = over_ssh("");
    for (tag, told) in [
        ("client-fastlines", Some(&there)),
        ("client-herelines", None),
    ] {
        let scratch = Scratch::new(tag);
        let (sock, mut rpc, config) = session(&scratch, tag);
        rpc.command(&format!("setlocal noswapfile | {setline}"))
            .expect("setline");
        let mut term = match told {
            Some(drawing) => Terminal::spawn_told(&sock, &config, drawing),
            None => Terminal::spawn(&sock, &config),
        };
        assert!(
            term.pump_until(Duration::from_secs(20), |s| s.contains("line 1")),
            "{tag}: never drawn:\n{}",
            term.text()
        );
        for _ in 0..3 {
            typed(&mut term, b"\x06");
        }
        assert!(
            top_line(&term.text()).is_some_and(|top| top > 40),
            "{tag}: not paged down:\n{}",
            term.text()
        );
        let sent = sent_by_kind(&mut rpc);
        if told.is_some() {
            assert!(
                sent.get("lines").is_some_and(|&n| n > 0)
                    && sent.get("shape").is_some_and(|&n| n > 0),
                "{tag}: a fast link over ssh was not sent its lines: {sent:?}"
            );
        } else {
            for kind in ["lines", "spans", "shape", "keep", "splice"] {
                assert_eq!(sent.get(kind), None, "{tag}: sent {kind}: {sent:?}");
            }
        }
    }
}

/// The agent's autocommand count, and how many listen for the line reach
/// (only an agent set up to predict has those).
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

/// `"auto"` predicts for remote sessions only. A local client (not told
/// otherwise) installs a predicting agent only if a setting is `"on"`; a
/// remote one does unless every setting is `"off"`.
#[test]
fn auto_predicts_over_ssh_and_on_here_as_well() {
    require_nvim!();
    let off = format!("[client]\n{NOTHING}");
    let on = "[client]\npredict_motions = \"on\"\n";
    let (there, there_off) = (over_ssh(""), over_ssh(&off));
    for (tag, config, told, predicting) in [
        ("client-here", "", None, false),
        ("client-on-here", on, None, true),
        ("client-there", "", Some(&there), true),
        ("client-off-there", off.as_str(), Some(&there_off), false),
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

/// A remote client, or a local one on `"on"`, scrolls at once on a wheel turn
/// while Neovim is busy for 1.5 s, and Neovim's scroll later lands where the
/// client is. A local one on `"auto"` waits for Neovim.
#[test]
fn a_scroll_is_drawn_before_neovim_takes_it_over_ssh_or_told_on() {
    require_nvim!();
    for (tag, client, ssh, ahead) in [
        ("client-ssh-scroll", "", true, true),
        (
            "client-on-scroll",
            "predict_scrolls = \"on\"\n",
            false,
            true,
        ),
        ("client-auto-scroll", "", false, false),
    ] {
        let scratch = Scratch::new(tag);
        let (sock, mut rpc, config) = session(&scratch, tag);
        let settings = format!("[client]\n{client}[effects.scroll]\nenabled = false\n");
        std::fs::write(&config, &settings).expect("write the config");
        rpc.command(
            "setlocal noswapfile | call setline(1, map(range(1, 200), '\"line \" . v:val'))",
        )
        .expect("setline");
        let mut term = if ssh {
            Terminal::spawn_told(&sock, &config, &over_ssh(&settings))
        } else {
            Terminal::spawn(&sock, &config)
        };
        assert!(
            term.pump_until(Duration::from_secs(20), |s| top_line(s) == Some(1)),
            "{tag}: never drawn:\n{}",
            term.text()
        );
        // The agent's lines arrive a moment after the first paint.
        term.pump_until(Duration::from_secs(2), |_| false);

        // Busy before the wheel turn arrives, which waits in typeahead.
        let busy = std::thread::spawn(move || rpc.command("lua vim.uv.sleep(1500)"));
        term.pump_until(Duration::from_millis(200), |_| false);
        term.type_bytes(b"\x1b[<65;10;5M");
        let early = term.pump_until(Duration::from_millis(600), |s| top_line(s) == Some(4));
        assert_eq!(
            early,
            ahead,
            "{tag}: drawn ahead of Neovim\n{}",
            term.text()
        );
        busy.join().expect("the busy call").expect("Neovim busy");
        assert!(
            term.pump_until(Duration::from_secs(5), |s| top_line(s) == Some(4)),
            "{tag}: Neovim's scroll never drawn:\n{}",
            term.text()
        );
        term.pump_until(Duration::from_secs(1), |_| false);
        assert_eq!(top_line(&term.text()), Some(4), "{tag}:\n{}", term.text());
        assert_eq!(term.hang_up(), Some(0), "{tag}: the client's exit");
    }
}

/// A 160-column terminal with `centre = "smart"` and `window_width = 60`,
/// on a one-window session whose status line has its own colour.
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

/// [`wide`] with the given `centre` mode.
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

/// `'columns'` and each window's width.
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

/// Neovim has the whole terminal width; a window wider than the text width
/// shows it centred, with the status line's words above it. On exit the
/// client restores the windows' widths.
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
    // Split into two of about 80, each with 60 of text centred.
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
    // Neovim may get the client's resize after this test's query, which
    // goes over another socket.
    assert!(
        until_widths(&mut term, &mut rpc, &columns(160, &[60, 60])),
        "the text not 60 wide: {:?}",
        widths(&mut rpc)
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

/// A centring client leaves grids restored to full width but holding what
/// was drawn narrower, unless Neovim redrew them before it left. Neovim sends
/// the next client only cells that differ (none, or a rewrapped line and what
/// it moved); a non-centring client must still draw every window whole.
#[test]
fn the_next_client_draws_whole_what_a_centring_one_left() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-left");
    let (mut term, mut rpc, sock) = wide(&scratch, "client-centre-left");
    // Split after drawing: a split made before the attach is laid out for
    // Neovim's own width, and the client's widens the right.
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

/// A centring client killed outright (by nvmux 2 s after a hang up, or a
/// crash) leaves windows narrowed for every UI: Neovim keeps a grid as narrow
/// as asked until a UI asks for it whole. The next multigrid client asks for
/// every window it does not narrow itself back whole.
#[test]
fn the_next_client_gives_back_the_width_a_killed_one_took() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-kill");
    let (mut term, mut rpc, sock) = wide(&scratch, "client-centre-kill");
    // Split after drawing, as above.
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
    // Drawn wide too: text at each window's left edge, not centred as on a
    // narrower grid.
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

/// Over a slow link, a new split's text is centred from its first frame: the
/// client cuts the grid to the requested width at once instead of waiting a
/// round trip for Neovim's narrower grid. Flying in, the text only moves
/// left, never back right (as it did when the narrower grid arrived).
#[test]
fn a_split_over_a_slow_link_is_centred_from_its_first_frame() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-slow");
    let (sock, mut rpc, config) = session(&scratch, "client-centre-slow");
    let settings = "[client]\ncentre = \"smart\"\nwindow_width = 60\n";
    rpc.command(
        "setlocal noswapfile | set splitright \
         | call setline(1, map(range(1, 10), '\"MARK \" . v:val'))",
    )
    .expect("set the session up");
    let slow = slow_link(&scratch.0.join("link"), &sock, Duration::from_millis(80));
    let told = over_ssh(settings);
    let mut term = Terminal::start(&slow, &config, Some(&told), DA1_REPLY, (160, 12));
    assert!(
        term.pump_until(Duration::from_secs(20), |s| s
            .starts_with(&format!("{}MARK 1", " ".repeat(50)))),
        "never drawn centred:\n{}",
        term.text()
    );

    rpc.command("vsplit").expect("vsplit");
    // The new window's text column on row 0, per whole frame: mid-frame the
    // left window's old text can still be beside its new copy.
    let right = |screen: &vt100::Screen| -> Option<usize> {
        let row: String = (0..160)
            .map(|c| match screen.cell(0, c) {
                Some(cell) if cell.has_contents() => cell.contents(),
                _ => " ",
            })
            .collect();
        let (at, _) = row.match_indices("MARK").nth(1)?;
        Some(row[..at].chars().count())
    };
    let mut seen: Vec<usize> = Vec::new();
    term.frames(Duration::from_secs(3), |screen| {
        if let Some(col) = right(screen).filter(|col| seen.last() != Some(col)) {
            seen.push(col);
        }
        false
    });
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

/// Closing the right window: the left one's centred text moves to the middle
/// of the full width as the closed window's ghost flies off (never jumping
/// there under the ghost), and the ghost's text leaves from where it was
/// drawn, centred.
#[test]
fn a_window_closed_beside_centred_text_takes_it_to_its_middle() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-close");
    let (mut term, mut rpc, _) = wide(&scratch, "client-centre-close");
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.contains("the left window")),
        "never drawn:\n{}",
        term.text()
    );
    rpc.command("set splitright | vsplit").expect("vsplit");
    let own = rpc
        .eval("map(nvim_tabpage_list_wins(0), 'nvim_win_get_width(v:val)')")
        .expect("ask the editor");
    let w: Vec<usize> = own
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_u64)
                .map(|w| w as usize)
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(w.len(), 2, "no split: {own:?}");
    let (left, right) = ((w[0] - 60) / 2, w[0] + 1 + (w[1] - 60) / 2);
    // Each window's text column on row 0.
    let starts = |row: &str| -> Vec<usize> {
        row.match_indices("the left window")
            .map(|(at, _)| row[..at].chars().count())
            .collect()
    };
    assert!(
        term.pump_until(Duration::from_secs(5), |s| {
            starts(s.lines().next().unwrap_or("")) == [left, right]
        }),
        "the text is not in the middle of each window ({w:?}):\n{}",
        term.text()
    );

    // Let the split finish.
    term.pump_until(Duration::from_millis(500), |_| false);

    rpc.command("close").expect("close the right window");
    // Text columns in every frame: the remaining window's, and the ghost's
    // while it shows.
    let mut seen: Vec<Vec<usize>> = Vec::new();
    term.frames(Duration::from_secs(2), |screen| {
        let row: String = (0..160)
            .map(|col| match screen.cell(0, col) {
                Some(cell) if cell.has_contents() => cell.contents(),
                _ => " ",
            })
            .collect();
        let now = starts(&row);
        if seen.last() != Some(&now) {
            seen.push(now);
        }
        false
    });
    let ghosted: Vec<&Vec<usize>> = seen.iter().filter(|s| s.len() == 2).collect();
    assert!(
        !ghosted.is_empty(),
        "no ghost of the closed window: {seen:?}"
    );
    assert!(
        ghosted.iter().all(|s| s[0] < 50),
        "there at once, under the ghost: {seen:?}"
    );
    assert!(
        ghosted.iter().all(|s| s[1] >= right),
        "the closed window's text drawn left of where it was: {seen:?}"
    );
    let held: Vec<usize> = seen.iter().filter_map(|s| s.first().copied()).collect();
    assert_eq!(held.last(), Some(&50), "landed in its middle: {seen:?}");
    assert!(
        held.windows(2).all(|w| w[0] <= w[1]),
        "moved back left on its way: {seen:?}"
    );
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

/// While a UI without `ext_multigrid` is attached, the client re-attaches
/// without it and releases its narrowed windows; once that UI goes, it
/// narrows them again.
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

    // A foreign UI, never read: see
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

/// A terminal window is not narrowed, so its program sees the window's full
/// width; the window beside it is still centred.
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
    // The new (left, current) window is the terminal.
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

/// End-of-line virtual text (a diagnostic) cut at the narrowed grid's edge
/// continues in the margin: all of it for a line about as wide as the grid,
/// after the last row for a wrapped line. It moves with its row, and the
/// client's autocommands are removed when it exits.
#[test]
fn a_diagnostic_cut_at_the_grids_edge_runs_on_in_the_margin() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-tails");
    let (mut term, mut rpc, _) = wide(&scratch, "client-centre-tails");
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.contains("the left window")),
        "never drawn:\n{}",
        term.text()
    );
    // Neither message fits on the 60-column grid beyond the leading blanks.
    rpc.command(
        "lua vim.diagnostic.config({ virtual_text = true, signs = false }) \
         vim.api.nvim_buf_set_lines(0, 0, -1, false, \
           { string.rep('x', 56), string.rep('y', 60) .. string.rep('z', 55) }) \
         local ns = vim.api.nvim_create_namespace('test') \
         vim.diagnostic.set(ns, 0, { \
           { lnum = 0, col = 0, message = 'the end of the first' }, \
           { lnum = 1, col = 0, message = 'the end of the second' } })",
    )
    .expect("diagnostics");
    let first = format!(
        "{}{}     ■ the end of the first",
        " ".repeat(50),
        "x".repeat(56)
    );
    let second = format!(
        "{}{}     ■ the end of the second",
        " ".repeat(50),
        "z".repeat(55)
    );
    let laid = |s: &str, from: usize| {
        let rows: Vec<&str> = s.lines().collect();
        rows.get(from).is_some_and(|l| *l == first)
            && rows.get(from + 2).is_some_and(|l| *l == second)
    };
    assert!(
        term.pump_until(Duration::from_secs(5), |s| laid(s, 0)),
        "not in the margin:\n{}",
        term.text()
    );
    rpc.command("call append(0, 'above')")
        .expect("a line above");
    assert!(
        term.pump_until(Duration::from_secs(5), |s| laid(s, 1)),
        "not moved with its row:\n{}",
        term.text()
    );
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
    let left = |rpc: &mut nvmux::rpc::Client<std::os::unix::net::UnixStream>| {
        rpc.eval(
            "luaeval('#vim.tbl_filter(function(a) \
               return (a.group_name or \"\"):find(\"^nvmux_tails_\") ~= nil end, \
               vim.api.nvim_get_autocmds({}))')",
        )
        .expect("ask the editor")
    };
    assert!(
        common::wait_until(Duration::from_secs(5), || left(&mut rpc) == Value::from(0)),
        "left in the editor: {}",
        left(&mut rpc)
    );
}

/// With concealed text, a line ends left of where the editor computes it, so
/// the margin continuation starts where the row shows the message.
#[test]
fn a_diagnostic_after_concealed_text_runs_on_from_where_neovim_drew_it() {
    require_nvim!();
    let scratch = Scratch::new("client-conceal");
    let (mut term, mut rpc, _) = wide(&scratch, "client-conceal");
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.contains("the left window")),
        "never drawn:\n{}",
        term.text()
    );
    // 55 characters, 35 shown; the cursor on the next line, so the first
    // stays concealed.
    rpc.command(
        "lua vim.diagnostic.config({ virtual_text = true, signs = false }) \
         vim.wo.conceallevel = 2 \
         vim.api.nvim_buf_set_lines(0, 0, -1, false, \
           { string.rep('a', 20) .. string.rep('z', 20) .. string.rep('b', 15), 'here' }) \
         local ns = vim.api.nvim_create_namespace('test') \
         vim.api.nvim_buf_set_extmark(0, ns, 0, 20, { end_col = 40, conceal = '' }) \
         vim.api.nvim_win_set_cursor(0, { 2, 0 }) \
         vim.diagnostic.set(ns, 0, { { lnum = 0, col = 0, \
           message = 'a message long enough to be cut at the edge' } })",
    )
    .expect("a diagnostic after concealed text");
    let whole = format!(
        "{}{}{}     ■ a message long enough to be cut at the edge",
        " ".repeat(50),
        "a".repeat(20),
        "b".repeat(15)
    );
    assert!(
        term.pump_until(Duration::from_secs(5), |s| s.starts_with(&whole)),
        "not in the margin from where it is:\n{}",
        term.text()
    );
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

/// Lines in a closed fold get no margin continuation: Neovim draws the fold
/// as its own text. A one-character or empty line (ending where a fold's
/// text would start) outside a fold still gets one.
#[test]
fn nothing_after_a_line_in_a_closed_fold_runs_on_in_the_margin() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-fold");
    let (mut term, mut rpc, _) = wide(&scratch, "client-centre-fold");
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.contains("the left window")),
        "never drawn:\n{}",
        term.text()
    );
    // Lines 4-6 in a closed fold, the last empty; the cursor after it.
    rpc.command(
        "lua vim.diagnostic.config({ virtual_text = true, signs = false }) \
         vim.wo.foldmethod = 'manual' \
         vim.api.nvim_buf_set_lines(0, 0, -1, false, \
           { '}', '', 'before', 'folded ' .. string.rep('f', 49), 'folded', '', 'after' }) \
         local ns = vim.api.nvim_create_namespace('test') \
         local long = 'a message long enough to run on past the edge of the grid' \
         vim.diagnostic.set(ns, 0, { \
           { lnum = 0, col = 0, message = 'a brace, ' .. long }, \
           { lnum = 1, col = 0, message = 'empty, ' .. long }, \
           { lnum = 3, col = 0, message = 'folded, ' .. long }, \
           { lnum = 5, col = 0, message = 'empty and folded, ' .. long } }) \
         vim.cmd('4,6fold') \
         vim.api.nvim_win_set_cursor(0, { 7, 0 })",
    )
    .expect("diagnostics, and a fold");
    let long = "a message long enough to run on past the edge of the grid";
    let brace = format!("{}}}     ■ a brace, {long}", " ".repeat(50));
    let empty = format!("{}     ■ empty, {long}", " ".repeat(50));
    let laid = |s: &str| {
        let rows: Vec<&str> = s.lines().collect();
        rows.len() > 4
            && rows[0] == brace
            && rows[1] == empty
            && rows[3].starts_with(&format!("{}+--", " ".repeat(50)))
            && rows[4].ends_with("after")
    };
    assert!(
        term.pump_until(Duration::from_secs(5), laid),
        "not as Neovim draws it:\n{}",
        term.text()
    );
    // Long enough for any continuation to arrive.
    term.pump_until(Duration::from_millis(500), |_| false);
    let text = term.text();
    let fold = text.lines().nth(3).unwrap_or_default();
    assert!(
        !fold.contains('■') && fold.chars().count() <= 110,
        "the fold ran on past the grid's edge:\n{text}"
    );
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

/// With `'autochdir'`, a predicted scroll beside a window with margin
/// continuations lands without moving back. Regression: the continuation
/// code once switched the current window to query the other, which under
/// `'autochdir'` made Neovim redraw and withhold the scroll positions at
/// every redraw, so the client gave up its prediction.
#[test]
fn a_scroll_beside_text_run_on_past_the_edge_lands_with_autochdir() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-acd");
    let (sock, mut rpc, config) = session(&scratch, "client-centre-acd");
    let settings = "[client]\ncentre = \"smart\"\nwindow_width = 50\n\n\
                    [effects.scroll]\nenabled = false\n";
    // A real file: 'autochdir' follows the current window's file.
    let file = scratch.0.join("text").join("lines.txt");
    std::fs::create_dir_all(file.parent().expect("a directory")).expect("the directory");
    rpc.command(&format!(
        "setlocal noswapfile | set autochdir \
         | call setline(1, map(range(1, 200), '\"line \" . v:val')) | write {}",
        file.display()
    ))
    .expect("set the session up");
    rpc.command(
        "lua vim.diagnostic.config({ virtual_text = true, signs = false }) \
         local ns = vim.api.nvim_create_namespace('test') \
         local all = {} \
         for i = 0, 199 do all[#all + 1] = { lnum = i, col = 0, \
           message = 'a message long enough to run on past the edge of the grid' } end \
         vim.diagnostic.set(ns, 0, all)",
    )
    .expect("diagnostics");
    let slow = slow_link(&scratch.0.join("link"), &sock, SLOW);
    let told = over_ssh(settings);
    let mut term = Terminal::start(&slow, &config, Some(&told), DA1_REPLY, (160, 24));
    assert!(
        term.pump_until(Duration::from_secs(20), |s| s.contains("line 1 ")),
        "never drawn:\n{}",
        term.text()
    );
    // The new window is left and current, each with 50 of about 80 columns
    // of text; then wait for the agent's lines.
    rpc.command("vsplit").expect("vsplit");
    term.pump_until(Duration::from_secs(4), |_| false);

    // The top line number of the left window.
    let top = |s: &vt100::Screen| -> Option<u32> {
        let row: String = (0..80)
            .map(|c| match s.cell(0, c) {
                Some(cell) if cell.has_contents() => cell.contents(),
                _ => " ",
            })
            .collect();
        let at = row.find("line ")?;
        row[at + 5..]
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()
    };
    let mut seen: Vec<u32> = Vec::new();
    for _ in 0..2 {
        term.type_bytes(b"\x1b[<65;20;5M");
        // Past when an unlanded prediction would be given up.
        term.frames(Duration::from_secs(2), |s| {
            if let Some(n) = top(s).filter(|n| seen.last() != Some(n)) {
                seen.push(n);
            }
            false
        });
    }
    assert_eq!(
        rpc.eval("line('w0')").expect("the window's top line"),
        Value::from(7),
        "Neovim scrolled elsewhere"
    );
    assert_eq!(seen.last(), Some(&7), "not landed: {seen:?}");
    assert!(seen.windows(2).all(|w| w[0] < w[1]), "moved back: {seen:?}");
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

/// A float on a centred window shifts in with the text, but only as far as
/// keeps it on screen: a diagnostic float at a long line's end, which Neovim
/// fits beside the grid, is drawn whole with its border.
#[test]
fn a_float_by_centred_text_stays_on_the_screen() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-float");
    let (mut term, mut rpc, _) = wide(&scratch, "client-centre-float");
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.contains("the left window")),
        "never drawn:\n{}",
        term.text()
    );
    rpc.command(
        "lua vim.diagnostic.config({ signs = false, float = { border = 'single' } }) \
         vim.api.nvim_buf_set_lines(0, 0, -1, false, { string.rep('x', 56) }) \
         vim.diagnostic.set(vim.api.nvim_create_namespace('test'), 0, \
           { { lnum = 0, col = 0, message = string.rep('m', 80) .. ' end' } }) \
         vim.api.nvim_win_set_cursor(0, { 1, 55 })",
    )
    .expect("a diagnostic");
    // Kept open despite the cursor move above.
    rpc.command("lua vim.diagnostic.open_float({ close_events = {} })")
        .expect("its float");
    assert!(
        term.pump_until(Duration::from_secs(5), |s| s
            .lines()
            .any(|l| l.ends_with(" end│"))),
        "the float is not on the screen whole:\n{}",
        term.text()
    );
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

/// Wait for [`widths`] to equal `want`, pumping the client meanwhile.
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

/// With `centre = "smart"`, the text width is `'textwidth'` if set; else, for
/// `nowrap`, wide enough for nine in ten lines (up to the whole window); else
/// `window_width`.
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

/// A window already centred (as start screens lay out) keeps its full width
/// in either mode: narrowed, it would be centred twice and sit off to the
/// right. Lines indented short of the middle, or not at all, are narrowed.
fn a_window_centred_already_keeps_its_width(tag: &str, centre: &str) {
    let scratch = Scratch::new(tag);
    let (mut term, mut rpc, _) = wide_with(&scratch, tag, centre);
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.contains("the left window")),
        "never drawn:\n{}",
        term.text()
    );
    // 40 columns at column 60 of 160, and a shorter line in the middle.
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

/// A Markdown window is as wide as its tables and code blocks (wrapping
/// breaks them), not its paragraphs. Needs Neovim's Markdown parser.
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

/// `centre = "fixed"` ignores `'textwidth'`.
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

/// Centring needs per-window grids, which the client must request even when
/// no effect or prediction wants them.
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

/// Whether row `r` (for `r < rows`) shows `line r+1` in each of `windows`
/// side-by-side windows.
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

/// Draw an 11-line session in `windows` splits with a client of size `first`
/// (columns, rows), then check a client of size `second` draws it whole.
fn drawn_again_at(tag: &str, windows: usize, first: (u16, u16), second: (u16, u16)) {
    let scratch = Scratch::new(tag);
    let (sock, mut rpc, config) = session(&scratch, tag);
    rpc.command("setlocal noswapfile | call setline(1, map(range(1, 11), '\"line \" . v:val'))")
        .expect("setline");
    // Rows of text: all 11 lines, or all rows but the status and command
    // lines.
    let drawn = |size: (u16, u16), s: &str| lines_in(s, usize::from(size.1 - 2).min(11), windows);

    let mut term = Terminal::spawn_sized(&sock, &config, DA1_REPLY, first);
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.starts_with("line 1")),
        "the first client never drew it:\n{}",
        term.text()
    );
    // Split after drawing, as in the centring tests.
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

/// Neovim keeps window grids between UIs, and on a resize sends only the
/// new position and gained rows, not the unchanged rows above. The client
/// must still draw every row.
#[test]
fn a_client_taller_than_the_last_draws_every_window_whole() {
    require_nvim!();
    drawn_again_at("client-taller", 2, (160, 12), (160, 20));
}

/// Wider: only the gained columns of each row are sent.
#[test]
fn a_client_wider_than_the_last_draws_its_window_whole() {
    require_nvim!();
    drawn_again_at("client-wider", 1, (160, 12), (200, 12));
}

/// A `:terminal` program drawing frames where each cell's background is
/// (row, column, frame), except the first ten columns, fixed at blue 255.
/// Every new cell needs a new highlight, so Neovim's table fills every few
/// dozen frames; Neovim then clears it and reuses ids, often mid-frame,
/// while the unchanged cells, not redrawn, keep naming reused ids.
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

/// Every frame of [`REFILL_LUA`] shows each cell, unchanged ones included, in
/// its own colours. Regression: Neovim's TUI writes colours as cells come,
/// but this client draws per flush, and drew cells not redrawn since an id
/// was reused in the id's new colours, flashing on every table clear.
#[test]
fn a_terminal_that_fills_neovims_highlights_is_drawn_in_its_own_colours() {
    require_nvim!();
    let scratch = Scratch::new("client-refill");
    let (sock, mut rpc, config) = session(&scratch, "client-refill");
    // No effects, so every colour on screen is the program's.
    std::fs::write(&config, "[effects]\nenabled = false\n").expect("config");
    let program = scratch.0.join("refill.lua");
    std::fs::write(&program, REFILL_LUA).expect("write the program");
    rpc.command("setlocal noswapfile | set termguicolors | call setline(1, 'before the program')")
        .expect("setline");
    // Normal's background, which the program never uses.
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
        // The window's rows, not the status and command lines.
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
    // 70 new highlights a row, 22 rows a frame: about 43 frames fill the
    // table.
    assert!(
        seen.len() >= 100,
        "only {} of the program's 250 frames were drawn: too few to fill Neovim's table twice",
        seen.len()
    );
}

/// Lines for word motions: words, punctuation, blanks and tabs, a blank-only
/// line, empty lines, CJK beside other text, an emoji, a combining mark, a
/// no-break space, and `-` (in `'iskeyword'` or not).
const WORDS: &[&str] = &[
    "fn main() {",
    "    let x_1 = foo.bar(2, \"three\");",
    "",
    "\ta\tb  c",
    "   ",
    "",
    "",
    "日本語のテキストと、カタカナ漢字",
    "smile 😀 here — there…",
    "café e\u{301}tude naïve",
    "no\u{a0}break --dash-ed-- end",
    "x",
    "  // trailing  ",
    "last word",
];

/// `(line, byte)` of every character start in `lines` (combining marks join
/// the previous character; an empty line gives byte 0), lines from 0.
fn firsts(lines: &[&str]) -> Vec<(i64, usize)> {
    lines
        .iter()
        .enumerate()
        .flat_map(|(l, t)| {
            let firsts: Vec<usize> = t
                .char_indices()
                .filter(|&(_, c)| !nvmux::client::predict::joins(c))
                .map(|(b, _)| b)
                .collect();
            let firsts = if firsts.is_empty() { vec![0] } else { firsts };
            firsts.into_iter().map(move |b| (l as i64, b))
        })
        .collect()
}

/// Neovim's cursor `(line, byte)` after each start × key × count, in that
/// order (see [`MOTIONS_LUA`]).
fn neovims_motions(
    rpc: &mut nvmux::rpc::Client<std::os::unix::net::UnixStream>,
    starts: &[(i64, usize)],
    keys: &[&str],
    counts: &[i64],
) -> Vec<(i64, usize)> {
    let pair = |&(l, b): &(i64, usize)| Value::Array(vec![l.into(), (b as i64).into()]);
    let args = Value::Array(vec![
        Value::Array(starts.iter().map(pair).collect()),
        Value::Array(keys.iter().map(|&k| Value::from(k)).collect()),
        Value::Array(counts.iter().map(|&n| Value::from(n)).collect()),
    ]);
    let got = rpc
        .call("nvim_exec_lua", vec![MOTIONS_LUA.into(), args])
        .expect("Neovim's motions");
    let got = got.as_array().expect("a list");
    assert_eq!(
        got.len(),
        starts.len() * keys.len() * counts.len(),
        "as many"
    );
    got.iter()
        .map(|p| {
            let p = p.as_array().expect("a pair");
            (
                p[0].as_i64().expect("line"),
                p[1].as_i64().expect("byte") as usize,
            )
        })
        .collect()
}

/// For each start × key × count, the cursor `(line, byte)` (lines from 0)
/// after feeding the keys as typed, unmapped (a trailing blank counts too),
/// with a character's bytes taken literally even if one is Neovim's
/// special-key lead byte.
const MOTIONS_LUA: &str = "local starts, keys, counts = ...
  local out = {}
  for _, s in ipairs(starts) do
    for _, k in ipairs(keys) do
      for _, n in ipairs(counts) do
        vim.api.nvim_win_set_cursor(0, { s[1] + 1, s[2] })
        vim.api.nvim_feedkeys(n .. k, 'nx', true)
        local c = vim.api.nvim_win_get_cursor(0)
        out[#out + 1] = { c[1] - 1, c[2] }
      end
    end
  end
  return out";

#[test]
fn motions_by_words_go_where_neovim_takes_them() {
    use nvmux::client::words::{self, Way};
    require_nvim!();
    let scratch = Scratch::new("client-words");
    let (_sock, mut rpc, _config) = session(&scratch, "client-words");
    let keys = [
        ("w", Way::Next, false),
        ("b", Way::Back, false),
        ("e", Way::End, false),
        ("ge", Way::BackEnd, false),
        ("W", Way::Next, true),
        ("B", Way::Back, true),
        ("E", Way::End, true),
        ("gE", Way::BackEnd, true),
    ];
    let counts = [1i64, 2, 5];
    let mut checked = 0;
    for (tail, isk) in [(None, ""), (Some(""), ""), (None, "-")] {
        let lines: Vec<&str> = WORDS.iter().copied().chain(tail).collect();
        let text = |l: i64| usize::try_from(l).ok().and_then(|l| lines.get(l)).copied();
        let set = Value::Array(lines.iter().map(|&l| Value::from(l)).collect());
        rpc.call(
            "nvim_buf_set_lines",
            vec![0.into(), 0.into(), (-1).into(), false.into(), set],
        )
        .expect("set the lines");
        rpc.command(&format!("setlocal noswapfile iskeyword& iskeyword+={isk}"))
            .expect("iskeyword");
        let classes = rpc
            .call(
                "nvim_exec_lua",
                vec![
                    "local t = {} for b = 0, 255 do \
                       t[#t + 1] = math.min(vim.fn.charclass(vim.fn.nr2char(b)), 2) end \
                     return table.concat(t)"
                        .into(),
                    Value::Array(vec![]),
                ],
            )
            .expect("the classes");
        let keyword: Vec<u8> = classes
            .as_str()
            .expect("a string")
            .bytes()
            .map(|b| b - b'0')
            .collect();
        let starts = firsts(&lines);
        let names: Vec<&str> = keys.iter().map(|(k, ..)| *k).collect();
        let mut theirs = neovims_motions(&mut rpc, &starts, &names, &counts).into_iter();
        let lines_of = words::Lines {
            text: &text,
            count: lines.len() as i64,
            keyword: &keyword,
        };
        for &from in &starts {
            for &(key, way, big) in &keys {
                for &n in &counts {
                    let neovims = theirs.next().expect("as many");
                    let ours = words::moved(way, big, n, from, &lines_of);
                    assert_eq!(
                        ours,
                        Some(neovims),
                        "{n}{key} from {from:?}, iskeyword+={isk:?}, after the last {tail:?}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 5000, "{checked}");
}

/// Lines for `f`/`t`: repeated letters, brackets, blanks and tabs, an empty
/// line, CJK, emoji, a letter both precomposed and with a combining mark, and
/// a blank-only line.
const FINDS: &[&str] = &[
    "foo(bar, baz) -> foo.bar",
    "\ta\tb  a\t",
    "",
    "日本語の本、日本",
    "x 😀 y 😀 z",
    "café e\u{301}te\u{301} e",
    "aaaa",
    "  ",
];

#[test]
fn motions_to_a_character_go_where_neovim_takes_them() {
    use nvmux::client::moves;
    require_nvim!();
    let scratch = Scratch::new("client-finds");
    let (_sock, mut rpc, _config) = session(&scratch, "client-finds");
    let set = Value::Array(FINDS.iter().map(|&l| Value::from(l)).collect());
    rpc.call(
        "nvim_buf_set_lines",
        vec![0.into(), 0.into(), (-1).into(), false.into(), set],
    )
    .expect("set the lines");
    rpc.command("setlocal noswapfile").expect("noswapfile");
    let sought = [
        'o', 'a', 'b', ' ', '\t', '本', '😀', 'e', 'é', '(', '.', 'Z',
    ];
    let keys: Vec<(String, char, bool, bool)> = [("f", false, false), ("t", true, false)]
        .into_iter()
        .chain([("F", false, true), ("T", true, true)])
        .flat_map(|(k, till, back)| sought.map(|c| (format!("{k}{c}"), c, till, back)))
        .collect();
    let counts = [1i64, 2, 3];
    let starts = firsts(FINDS);
    let names: Vec<&str> = keys.iter().map(|(k, ..)| k.as_str()).collect();
    let mut theirs = neovims_motions(&mut rpc, &starts, &names, &counts).into_iter();
    let mut checked = 0;
    for &(line, byte) in &starts {
        for (key, c, till, back) in &keys {
            for &n in &counts {
                let neovims = theirs.next().expect("as many");
                let text = FINDS[line as usize];
                let ours = moves::find(text, byte, *c, (*till, *back), n as usize)
                    .map_or((line, byte), |b| (line, b));
                assert_eq!(
                    ours,
                    neovims,
                    "{n}{key:?} from {:?} on {text:?}",
                    (line, byte)
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 5000, "{checked}");
}

/// Lines for `}`/`{`: paragraphs of one line and more, single and runs of
/// empty lines, a blank-only line (not a boundary), `.` lines that are and
/// are not `'paragraphs'`/`'sections'` macros (as in a stylesheet), and a
/// form feed.
const PARAS: &[&str] = &[
    "first paragraph",
    "goes on here",
    "",
    "",
    "   ",
    "second, after blanks alone",
    "\tand a tab",
    ".PP",
    "after a macro of 'paragraphs'",
    ".SH a section",
    ".container {",
    "  color: red;",
    "}",
    ".pp-header {",
    "\x0cform feed",
    "x",
    "",
    ".P",
    ".",
    "last line",
];

#[test]
fn motions_by_paragraphs_go_where_neovim_takes_them() {
    use nvmux::client::moves;
    require_nvim!();
    let scratch = Scratch::new("client-paras");
    let (_sock, mut rpc, _config) = session(&scratch, "client-paras");
    let counts = [1i64, 2, 3, 7];
    let mut checked = 0;
    for (tail, (paragraphs, sections)) in [
        (None, ("IPLPPPQPP TPHPLIPpLpItpplpipbp", "SHNHH HUnhsh")),
        (Some(""), ("IPLPPPQPP TPHPLIPpLpItpplpipbp", "SHNHH HUnhsh")),
        (None, ("", "")),
        (None, ("co", "P")),
    ] {
        let lines: Vec<&str> = PARAS.iter().copied().chain(tail).collect();
        let text = |l: i64| usize::try_from(l).ok().and_then(|l| lines.get(l)).copied();
        let set = Value::Array(lines.iter().map(|&l| Value::from(l)).collect());
        rpc.call(
            "nvim_buf_set_lines",
            vec![0.into(), 0.into(), (-1).into(), false.into(), set],
        )
        .expect("set the lines");
        rpc.command("setlocal noswapfile").expect("noswapfile");
        for (name, value) in [("paragraphs", paragraphs), ("sections", sections)] {
            rpc.call(
                "nvim_set_option_value",
                vec![name.into(), value.into(), Value::Map(vec![])],
            )
            .expect("set the option");
        }
        let starts: Vec<(i64, usize)> = (0..lines.len() as i64).map(|l| (l, 0)).collect();
        let keys = ["}", "{"];
        let mut theirs = neovims_motions(&mut rpc, &starts, &keys, &counts).into_iter();
        let macros = moves::Macros {
            sections,
            paragraphs,
        };
        for &(line, byte) in &starts {
            for key in keys {
                for &n in &counts {
                    let neovims = theirs.next().expect("as many");
                    let count = lines.len() as i64;
                    let ours = moves::paragraph(key == "{", n, line, count, &text, macros)
                        .expect("followed")
                        .unwrap_or((line, byte));
                    assert_eq!(
                        ours, neovims,
                        "{n}{key} from {line}, paragraphs={paragraphs:?} sections={sections:?}, \
                         after the last {tail:?}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 300, "{checked}");
}

/// Lines for `<C-w>`/`<C-u>`: words, punctuation, blanks and tabs, an
/// indent, CJK beside Latin, an emoji, and letters with and without a
/// combining mark.
const RUBS: &[&str] = &[
    "foo bar.baz  qux",
    "  indented, (a) b",
    "\tx = y;\t",
    "日本語のabc テキスト",
    "x 😀 y",
    "café e\u{301}te\u{301}",
];

/// For each case × key, the line and cursor byte after typing the key in
/// Insert mode. A case is `[line, start, typed]`: Insert entered at byte
/// `start` of `line`, then `typed` typed first.
const RUBS_LUA: &str = "local cases, keys = ...
  local out, got = {}, nil
  function _G.NvmuxRubbed()
    got = { vim.api.nvim_get_current_line(), vim.api.nvim_win_get_cursor(0)[2] }
    return ''
  end
  for _, c in ipairs(cases) do
    for _, k in ipairs(keys) do
      vim.api.nvim_buf_set_lines(0, 0, -1, false, { c[1] })
      local enter = 'i'
      if c[2] >= #c[1] and #c[1] > 0 then
        enter = 'A'
      else
        vim.api.nvim_win_set_cursor(0, { 1, c[2] })
      end
      got = nil
      local keys = enter .. c[3]:gsub('<', '<lt>') .. k .. '<C-r>=v:lua.NvmuxRubbed()<CR><Esc>'
      vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes(keys, true, false, true), 'x', false)
      out[#out + 1] = got
    end
  end
  return out";

#[test]
fn words_and_lines_come_out_as_neovim_takes_them_out() {
    use nvmux::client::typing::{self, Edit};
    require_nvim!();
    let scratch = Scratch::new("client-rubs");
    let (_sock, mut rpc, _config) = session(&scratch, "client-rubs");
    rpc.command("setlocal noswapfile nosmarttab")
        .expect("options");
    let classes = rpc
        .call(
            "nvim_exec_lua",
            vec![
                "local t = {} for b = 0, 255 do \
                   t[#t + 1] = math.min(vim.fn.charclass(vim.fn.nr2char(b)), 2) end \
                 return table.concat(t)"
                    .into(),
                Value::Array(vec![]),
            ],
        )
        .expect("the classes");
    let classes: Vec<u8> = classes
        .as_str()
        .expect("a string")
        .bytes()
        .map(|b| b - b'0')
        .collect();
    let mut checked = 0;
    for ai in [true, false] {
        rpc.command(if ai {
            "setlocal autoindent"
        } else {
            "setlocal noautoindent"
        })
        .expect("'autoindent'");
        for line in RUBS {
            let mut bounds: Vec<usize> = line
                .char_indices()
                .filter(|&(_, c)| !nvmux::client::predict::joins(c))
                .map(|(b, _)| b)
                .collect();
            bounds.push(line.len());
            // `(s, e)`: Insert entered at `s` of the line without `s..e`,
            // then `s..e` typed.
            let cases: Vec<(usize, usize)> = bounds
                .iter()
                .flat_map(|&s| {
                    bounds
                        .iter()
                        .filter(move |&&e| e >= s && e > 0)
                        .map(move |&e| (s, e))
                })
                .collect();
            let wire = Value::Array(
                cases
                    .iter()
                    .map(|&(s, e)| {
                        let short = format!("{}{}", &line[..s], &line[e..]);
                        Value::Array(vec![short.into(), (s as i64).into(), line[s..e].into()])
                    })
                    .collect(),
            );
            let keys = Value::Array(vec!["<C-w>".into(), "<C-u>".into()]);
            let got = rpc
                .call(
                    "nvim_exec_lua",
                    vec![RUBS_LUA.into(), Value::Array(vec![wire, keys])],
                )
                .expect("Neovim's");
            let mut theirs = got.as_array().expect("a list").iter().map(|p| {
                let p = p.as_array().expect("a pair");
                (
                    p[0].as_str().expect("the line").to_string(),
                    p[1].as_i64().expect("the byte") as usize,
                )
            });
            for &(s, e) in &cases {
                for key in [Edit::Word, Edit::Line] {
                    let neovims = theirs.next().expect("as many");
                    let ours = typing::rubbed(key, (line, e), Some(s), &classes, ai)
                        .expect("something to take out");
                    assert_eq!(
                        ours, neovims,
                        "{key:?} in {line:?}, begun at {s}, at {e}, 'autoindent' {ai}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 1500, "{checked}");
}

#[test]
fn random_typing_and_taking_out_shows_nothing_neovim_never_drew() {
    require_nvim!();
    const KEYS: &[(&[u8], &str)] = &[
        (b" ", "<Space>"),
        (b".", "."),
        (b"\x7f", "<BS>"),
        (b"\x17", "<C-w>"),
        (b"\x17", "<C-w>"),
        (b"\x15", "<C-u>"),
    ];
    const OUT: &[(&[u8], &str)] = &[(b"j", "j"), (b"k", "k"), (b"0", "0"), (b"$", "$")];
    const IN: &[(&[u8], &str)] = &[
        (b"i", "i"),
        (b"a", "a"),
        (b"A", "A"),
        (b"I", "I"),
        (b"o", "o"),
    ];
    let setline = r#"call setline(1, ["  some words here", "line 1", "x.y z"])"#;
    let mut rng = Lcg(23);
    let mut keys: Vec<(Vec<u8>, String)> = vec![(b"A".to_vec(), "A".into())];
    while keys.len() < 200 {
        let (bytes, name) = match rng.below(12) {
            0..=4 => KEYS[rng.below(KEYS.len() as u64) as usize],
            5 => {
                keys.push((b"\x1b".to_vec(), "<Esc>".into()));
                let (bytes, name) = OUT[rng.below(OUT.len() as u64) as usize];
                keys.push((bytes.to_vec(), name.into()));
                IN[rng.below(IN.len() as u64) as usize]
            }
            _ => {
                let c = (b'a' + rng.below(26) as u8) as char;
                keys.push((c.to_string().into_bytes(), c.to_string()));
                continue;
            }
        };
        keys.push((bytes.to_vec(), name.into()));
    }
    sights_are_neovims("client-rfuzz", setline, TYPING, &keys, rng);
}

/// For `H`/`M`/`L` in a `rows`-tall window over `count` lines: for each
/// `'scrolloff'` × top × key × count, the top line before, the cursor's line
/// after, and the top after (lines from 0). Element 0 is the window height.
const WINDOW_LUA: &str = "local rows, count, tops, sos, keys, counts = ...
  vim.o.laststatus, vim.o.cmdheight, vim.o.lines = 0, 1, rows + 1
  local lines = {}
  for i = 1, count do
    lines[i] = 'line ' .. i
  end
  vim.api.nvim_buf_set_lines(0, 0, -1, false, lines)
  local out = { vim.api.nvim_win_get_height(0) }
  for _, so in ipairs(sos) do
    vim.wo.scrolloff = so
    for _, top in ipairs(tops) do
      for _, k in ipairs(keys) do
        for _, n in ipairs(counts) do
          vim.fn.winrestview({ topline = top + 1,
            lnum = math.min(top + 1 + math.floor(rows / 2), count) })
          local was = vim.fn.line('w0') - 1
          vim.api.nvim_feedkeys(n .. k, 'nx', false)
          out[#out + 1] = { was, vim.fn.line('.') - 1, vim.fn.line('w0') - 1 }
        end
      end
    end
  end
  return out";

#[test]
fn window_motions_go_where_neovim_takes_them() {
    use nvmux::client::moves;
    use nvmux::client::predict::Edge;
    require_nvim!();
    let scratch = Scratch::new("client-hml");
    let (_sock, mut rpc, _config) = session(&scratch, "client-hml");
    rpc.command("setlocal noswapfile").expect("noswapfile");
    let keys = [("H", Edge::Top), ("M", Edge::Middle), ("L", Edge::Bottom)];
    let sos = [0i64, 1, 2, 3, 6, 30];
    let counts = [1i64, 2, 3, 7, 50];
    let mut checked = 0;
    for (rows, count) in [
        (4i64, 3i64),
        (4, 8),
        (5, 8),
        (5, 40),
        (9, 8),
        (9, 40),
        (22, 40),
    ] {
        let tops: Vec<i64> = (0..count)
            .filter(|t| count < 10 || t % 4 == 0 || *t > count - 4)
            .collect();
        let list = |v: &[i64]| Value::Array(v.iter().map(|&n| Value::from(n)).collect());
        let args = Value::Array(vec![
            rows.into(),
            count.into(),
            list(&tops),
            list(&sos),
            Value::Array(keys.iter().map(|(k, _)| Value::from(*k)).collect()),
            list(&counts),
        ]);
        let got = rpc
            .call("nvim_exec_lua", vec![WINDOW_LUA.into(), args])
            .expect("Neovim's");
        let got = got.as_array().expect("a list");
        assert_eq!(got[0].as_i64(), Some(rows), "the window's height");
        let mut theirs = got[1..].iter().map(|v| {
            let v = v.as_array().expect("three");
            [0, 1, 2].map(|i| v[i].as_i64().expect("a line"))
        });
        for &so in &sos {
            for _ in &tops {
                for &(key, edge) in &keys {
                    for &n in &counts {
                        let [top, line, after] = theirs.next().expect("as many");
                        // A `'scrolloff'` over half the window has its own
                        // rules; the client leaves all motions to Neovim.
                        if so > (rows - 1) / 2 {
                            continue;
                        }
                        assert_eq!(
                            after, top,
                            "{n}{key}: the view moved, {rows} rows over {count} lines, 'scrolloff' {so}"
                        );
                        let ours = moves::window(edge, n, top, (rows, so), count);
                        assert_eq!(
                            ours, line,
                            "{n}{key} from top {top}, {rows} rows over {count} lines, 'scrolloff' {so}"
                        );
                        checked += 1;
                    }
                }
            }
        }
    }
    assert!(checked > 2000, "{checked}");
}

/// The view after `G` to a line, in a `rows`-tall split (so `'scrolljump'`
/// can exceed its height) over `count` lines. For each `'scrolloff'` ×
/// `'scrolljump'` × top (cursor on the first line `'scrolloff'` allows) ×
/// target: the top before (Neovim may have moved it for `'scrolloff'`), the
/// top after, and the cursor's line after, lines from 0. Element 0 is the
/// window height.
const VIEWS_LUA: &str = "local rows, count, smooth, sos, jumps, tops, targets = ...
  vim.o.lines = 100
  if #vim.api.nvim_tabpage_list_wins(0) == 1 then
    vim.cmd('new')
    vim.bo.swapfile = false
  end
  vim.api.nvim_win_set_height(0, rows)
  local lines = {}
  for i = 1, count do
    lines[i] = 'line ' .. i
  end
  vim.api.nvim_buf_set_lines(0, 0, -1, false, lines)
  vim.wo.smoothscroll = smooth
  local out = { vim.api.nvim_win_get_height(0) }
  for _, so in ipairs(sos) do
    vim.wo.scrolloff = so
    for _, sj in ipairs(jumps) do
      vim.o.scrolljump = sj
      for _, top in ipairs(tops) do
        for _, to in ipairs(targets) do
          local start = math.min(top + (top > 0 and so or 0), count - 1)
          vim.fn.winrestview({ topline = top + 1, lnum = start + 1, col = 0 })
          local was = vim.fn.line('w0') - 1
          vim.cmd('normal! ' .. (to + 1) .. 'G')
          out[#out + 1] = { was, vim.fn.line('w0') - 1, vim.fn.line('.') - 1 }
        end
      end
    end
  end
  return out";

#[test]
fn views_go_where_neovim_takes_them() {
    use nvmux::client::moves::{self, Scrolling};
    require_nvim!();
    let scratch = Scratch::new("client-views");
    let (_sock, mut rpc, _config) = session(&scratch, "client-views");
    rpc.command("setlocal noswapfile").expect("noswapfile");
    let mut checked = 0;
    let mut cases = Vec::new();
    for rows in [4i64, 5, 6, 7, 9, 12, 21, 22, 23, 40] {
        let sos: Vec<i64> = if rows < 30 {
            (0..=(rows - 1) / 2).collect()
        } else {
            vec![0, 1, 2, 3, 5, 8, 13, 19]
        };
        for count in [rows - 1, rows + 2, 2 * rows + 1, 300] {
            let tops: Vec<i64> = if count < 100 {
                (0..count).collect()
            } else {
                vec![0, 1, 3, 100, 150, count - rows, count - rows + 2, count - 3]
            };
            let targets = |top: i64| -> Vec<i64> {
                if count < 100 {
                    (0..count).collect()
                } else {
                    let mut t: Vec<i64> =
                        ((top - 3 * rows).max(0)..(top + 4 * rows).min(count)).collect();
                    t.extend([0, count - 1]);
                    t
                }
            };
            let mixed = matches!(rows, 7 | 22);
            let jumps: Vec<i64> = if mixed {
                vec![1, 0, 3, 40, -50]
            } else {
                vec![1]
            };
            for smooth in [false, true] {
                if smooth && !mixed {
                    continue;
                }
                for &top in &tops {
                    cases.push((
                        rows,
                        count,
                        smooth,
                        sos.clone(),
                        jumps.clone(),
                        top,
                        targets(top),
                    ));
                }
            }
        }
    }
    let list = |v: &[i64]| Value::Array(v.iter().map(|&n| Value::from(n)).collect());
    for (rows, count, smooth, sos, jumps, top, targets) in cases {
        let args = Value::Array(vec![
            rows.into(),
            count.into(),
            smooth.into(),
            list(&sos),
            list(&jumps),
            list(&[top]),
            list(&targets),
        ]);
        let got = rpc
            .call("nvim_exec_lua", vec![VIEWS_LUA.into(), args])
            .expect("Neovim's");
        let got = got.as_array().expect("a list");
        assert_eq!(got[0].as_i64(), Some(rows), "the window's height");
        let mut theirs = got[1..].iter().map(|v| {
            let v = v.as_array().expect("three");
            [0, 1, 2].map(|i| v[i].as_i64().expect("a line"))
        });
        for &so in &sos {
            for &jump in &jumps {
                for &to in &targets {
                    let [was, after, cursor] = theirs.next().expect("as many");
                    let how = Scrolling { jump, smooth };
                    let ours =
                        moves::keep_in_view((was, to), (rows, count - 1), so, how, &|l| l < count);
                    // A 'scrolljump' larger than the window scrolls past
                    // the cursor and Neovim moves it; not modelled.
                    let neovims = (cursor == to).then_some(after);
                    assert_eq!(
                        ours, neovims,
                        "to {to} from top {was}, {rows} rows over {count} lines, \
                         'scrolloff' {so}, 'scrolljump' {jump}, 'smoothscroll' {smooth}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(theirs.next().is_none(), "no more");
    }
    assert!(checked > 100_000, "{checked}");
}
