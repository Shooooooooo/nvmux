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
use nvmux::transport::Transport;
use portable_pty::{CommandBuilder, PtySize};
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
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// The exit code, once the child has been seen to exit; it must not be
    /// signalled or waited for again after that.
    exited: Option<u32>,
    /// The client has been hung up, and what it asks from then on goes
    /// unanswered: it does not wait for an answer on its way out, and one
    /// written once it has gone fails on macOS. Neovim asks the terminal
    /// something whenever a UI attaches, a refresh's among them, so a
    /// question can come as the client goes.
    hung_up: bool,
    writer: Box<dyn Write + Send>,
    /// Everything the client has written, in order, and the screen it makes.
    output: Vec<u8>,
    parser: vt100::Parser,
    /// How far `output` has been scanned for queries.
    answered: usize,
    /// What this terminal answers a DA1 with.
    da1: &'static [u8],
    /// Its size: columns, rows.
    size: (u16, u16),
    incoming: mpsc::Receiver<Vec<u8>>,
    // Held so the pty outlives the child; dropped last.
    _master: Box<dyn portable_pty::MasterPty>,
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
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: size.1,
                cols: size.0,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_nvmux"));
        cmd.arg("--client");
        cmd.arg(sock);
        cmd.env("NVMUX_CONFIG", config);
        cmd.env("TERM", "xterm-256color");
        // The colours a parent nvmux would hand down: none, so the client
        // falls back on its own, whatever the environment running the tests.
        cmd.env_remove(nvmux::client::PALETTE_ENV);
        let child = pair.slave.spawn_command(cmd).expect("spawn the client");
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
            child,
            exited: None,
            hung_up: false,
            writer,
            output: Vec::new(),
            parser: vt100::Parser::new(size.1, size.0, 0),
            answered: 0,
            da1,
            size,
            incoming,
            _master: pair.master,
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
                    self.parser.process(&bytes);
                    self.output.extend_from_slice(&bytes);
                    self.answer();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return done(self),
            }
        }
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
        self.writer.write_all(bytes).expect("write to the pty");
        self.writer.flush().expect("flush the pty");
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
        let pid = self.child.process_id().expect("the client's pid");
        self.hung_up = true;
        nix::sys::signal::kill(Pid::from_raw(pid as i32), Signal::SIGHUP).expect("SIGHUP");
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.exited.is_none() && Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exited = Some(status.exit_code());
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
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
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

/// [`slow_link`], and a count of the bytes it has carried back from `to`.
fn counted_link(dir: &Path, to: &Path, delay: Duration) -> (PathBuf, Arc<AtomicUsize>) {
    use std::os::unix::net::{UnixListener, UnixStream};
    /// One direction: read as it comes, written `delay` after.
    fn carry(mut from: UnixStream, mut to: UnixStream, delay: Duration, count: Arc<AtomicUsize>) {
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
            }
            let _ = to.shutdown(std::net::Shutdown::Write);
        });
    }
    std::fs::create_dir_all(dir).expect("the link's directory");
    let path = dir.join("slow.sock");
    let listener = UnixListener::bind(&path).expect("bind the slow link");
    let to = to.to_path_buf();
    let back = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&back);
    std::thread::spawn(move || {
        for near in listener.incoming() {
            let (Ok(near), Ok(far)) = (near, UnixStream::connect(&to)) else {
                return;
            };
            let (Ok(near2), Ok(far2)) = (near.try_clone(), far.try_clone()) else {
                return;
            };
            carry(near, far, delay, Arc::new(AtomicUsize::new(0)));
            carry(far2, near2, delay, Arc::clone(&count));
        }
    });
    (path, back)
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

/// A session whose one window shows the text `setline` puts there, drawn by
/// a client over a slow link, scrolls jumping rather than sliding so that
/// what a prediction shows can be read off the screen at once.
///
/// With it, how many bytes have come back over the link from Neovim.
fn drawn_slowly(
    scratch: &Scratch,
    tag: &str,
    setline: &str,
) -> (
    nvmux::rpc::Client<std::os::unix::net::UnixStream>,
    Terminal,
    Arc<AtomicUsize>,
) {
    let (sock, mut rpc, config) = session(scratch, tag);
    rpc.command(&format!("setlocal noswapfile | {setline}"))
        .expect("setline");
    std::fs::write(&config, "[effects.scroll]\nenabled = false\n").expect("config");
    let (slow, back) = counted_link(&scratch.0.join("link"), &sock, SLOW);

    let mut term = Terminal::spawn(&slow, &config);
    assert!(
        term.pump_until(Duration::from_secs(20), |s| s.contains("line 1")),
        "never drawn:\n{}",
        term.text()
    );
    // The agent's lines, and the answer to the attach's fence.
    term.pump_until(Duration::from_secs(3), |_| false);
    (rpc, term, back)
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

/// Type `presses` pages, `gap` apart, into a window of `line N`s drawn over
/// a slow link: say how long after its key each page was first on the
/// screen, after making sure the view never went back and ended where
/// Neovim's did.
fn pages(
    rpc: &mut nvmux::rpc::Client<std::os::unix::net::UnixStream>,
    term: &mut Terminal,
    gap: Duration,
    presses: usize,
) -> Vec<Option<Duration>> {
    // A page is the window less two lines: 20.
    let page = |k: usize| 1 + 20 * k as u32;
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
    for k in 0..presses {
        typed.push(Instant::now());
        term.type_bytes(b"\x06");
        watch(term, t0 + gap * (k as u32 + 1), &mut drawn);
    }
    let tops = watch(term, Instant::now() + 4 * SLOW, &mut drawn);
    assert!(
        tops.windows(2).all(|w| w[0] < w[1]),
        "it went back: {tops:?}"
    );
    assert_eq!(tops.last(), Some(&page(presses)), "{tops:?}");
    assert_eq!(
        rpc.eval("line('w0')").ok().and_then(|v| v.as_u64()),
        Some(u64::from(page(presses))),
        "Neovim went elsewhere"
    );
    typed
        .iter()
        .zip(&drawn)
        .map(|(typed, drawn)| drawn.map(|d| d - *typed))
        .collect()
}

/// Pages typed faster than a slow link answers: each is on the screen before
/// Neovim could have heard of it, the lines it shows sent before it was
/// typed.
#[test]
fn pages_typed_faster_than_the_link_answers_keep_up() {
    require_nvim!();
    let scratch = Scratch::new("client-burst");
    let (mut rpc, mut term, _) = drawn_slowly(
        &scratch,
        "client-burst",
        "call setline(1, map(range(1, 2000), '\"line \" . v:val'))",
    );
    let late = pages(&mut rpc, &mut term, Duration::from_millis(100), 10);
    for (k, late) in late.iter().enumerate() {
        assert!(
            late.is_some_and(|l| l < SLOW),
            "page {} waited: {late:?}",
            k + 1
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
    let (mut rpc, mut term, _) = drawn_slowly(
        &scratch,
        "client-run",
        "call setline(1, map(range(1, 2000), '\"line \" . v:val'))",
    );
    // A round trip of this link types fifty pages, more than the most
    // windows of lines it keeps either way (twenty-four, some twenty-seven
    // pages); thirty-two are as far ahead as a view may go.
    let late = pages(&mut rpc, &mut term, Duration::from_millis(10), 32);
    for (k, late) in late.iter().enumerate() {
        assert!(
            late.is_some_and(|l| l < 2 * SLOW),
            "page {} waited for Neovim: {late:?}",
            k + 1
        );
    }
}

/// A run of pages longer than the lines the client keeps lets go of those
/// furthest behind rather than start again from two windows: every page of
/// it is on the screen before Neovim could have heard of it. And edits after
/// it, among the lines kept, are drawn as Neovim draws them.
#[test]
fn a_long_run_of_pages_lets_go_of_the_lines_behind() {
    require_nvim!();
    let scratch = Scratch::new("client-long-run");
    let (mut rpc, mut term, _) = drawn_slowly(
        &scratch,
        "client-long-run",
        "call setline(1, map(range(1, 6000), '\"line \" . v:val'))",
    );
    // Past the 4000 lines the client keeps at most, before the end.
    let late = pages(&mut rpc, &mut term, Duration::from_millis(30), 200);
    for (k, late) in late.iter().enumerate() {
        assert!(
            late.is_some_and(|l| l < SLOW),
            "page {} waited: {late:?}",
            k + 1
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
    let (mut rpc, mut term, back) = drawn_slowly(
        &scratch,
        "client-edits",
        "call setline(1, map(range(1, 600), \
         '\"line \" . v:val . \" \" . repeat(\"abcdefghij\", 6)'))",
    );
    let before = back.load(Ordering::Relaxed);
    typed(&mut term, b"x");
    let cost = back.load(Ordering::Relaxed) - before;
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

/// A config for a wide terminal: every window's text up to 60 columns, and
/// centred as `centre` says. A session in one window, its status line its
/// own colour, and a terminal 160 columns wide.
fn wide(
    scratch: &Scratch,
    tag: &str,
    centre: &str,
) -> (
    Terminal,
    nvmux::rpc::Client<std::os::unix::net::UnixStream>,
    PathBuf,
) {
    wide_with(scratch, tag, centre, "")
}

/// [`wide`], with `more` in the config's `[client]` as well.
fn wide_with(
    scratch: &Scratch,
    tag: &str,
    centre: &str,
    more: &str,
) -> (
    Terminal,
    nvmux::rpc::Client<std::os::unix::net::UnixStream>,
    PathBuf,
) {
    let (sock, mut rpc, config) = session(scratch, tag);
    std::fs::write(
        &config,
        format!("[client]\ncentre = \"{centre}\"\nwindow_width = 60\n{more}"),
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

/// The editor centred: Neovim is only as wide as its windows side by side
/// want, and in the middle of the terminal, its status line run on to both
/// edges; a split widens it and has the windows the same width, closing it
/// narrows it again.
#[test]
fn a_wide_terminal_has_the_editor_centred_as_wide_as_its_windows_want() {
    require_nvim!();
    let scratch = Scratch::new("client-centre");
    let (mut term, mut rpc, _) = wide(&scratch, "client-centre", "editor");
    let at = |n: usize| format!("{}the left window", " ".repeat(n));
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s.starts_with(&at(50))),
        "one window is not 60 wide in the middle:\n{}",
        term.text()
    );
    assert_eq!(widths(&mut rpc), columns(60, &[60]));
    // The status line, under the window: across the terminal. Waited for,
    // and said of in full where it does not come: macOS has drawn it in the
    // editor's own background, as Linux has never been seen to.
    let status = Some(vt100::Color::Rgb(0x12, 0x34, 0x56));
    let bg = |t: &Terminal, col: u16| t.parser.screen().cell(10, col).map(vt100::Cell::bgcolor);
    let across = |t: &Terminal| [0, 80, 159].iter().all(|&col| bg(t, col) == status);
    if !term.pump(Duration::from_secs(5), across) {
        let cells: Vec<_> = [0, 80, 159]
            .iter()
            .map(|&col| {
                let c = term.parser.screen().cell(10, col);
                c.map(|c| (c.bgcolor(), c.fgcolor(), c.inverse()))
            })
            .collect();
        let editor = rpc
            .eval("[&termguicolors, &cmdheight, execute('messages')]")
            .expect("ask the editor");
        panic!(
            "the status line is not its colour across the terminal: {cells:?}; \
             'termguicolors', 'cmdheight' and the messages: {editor}\n{}",
            term.text()
        );
    }
    let cell = |t: &Terminal, col: u16| t.parser.screen().cell(5, col).map(vt100::Cell::bgcolor);
    assert_eq!(
        cell(&term, 0),
        cell(&term, 80),
        "a margin is the editor's own background"
    );

    rpc.command("vsplit").expect("vsplit");
    assert!(
        term.pump_until(Duration::from_secs(5), |s| s.starts_with(&at(19))),
        "a split did not widen it:\n{}",
        term.text()
    );
    assert!(
        common::wait_until(Duration::from_secs(5), || widths(&mut rpc)
            == columns(121, &[60, 60])),
        "the windows are not the same width: {}",
        widths(&mut rpc)
    );

    rpc.command("close").expect("close");
    assert!(
        term.pump_until(Duration::from_secs(5), |s| s.starts_with(&at(50))),
        "closing it did not narrow it again:\n{}",
        term.text()
    );
    assert_eq!(term.hang_up(), Some(0), "the client's exit");
}

/// Each window's text centred: Neovim has the whole terminal, and a window
/// wider than the text gets has it that wide in its middle, its status line's
/// words over it; the client gives the windows their width back as it goes.
#[test]
fn a_wide_window_has_its_text_centred_in_it() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-text");
    let (mut term, mut rpc, _) = wide(&scratch, "client-centre-text", "windows");
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
    let (mut term, mut rpc, sock) = wide(&scratch, "client-centre-left", "windows");
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

/// A UI that keeps every window on grid 1 sees the windows whole: the client
/// attaching again without `ext_multigrid` lets go of the windows it had
/// narrowed, and once that UI has gone and the client has windows of its own
/// again, narrows them again.
#[test]
fn a_ui_without_windows_of_its_own_sees_them_whole() {
    require_nvim!();
    let scratch = Scratch::new("client-centre-ui");
    let (mut term, mut rpc, sock) = wide(&scratch, "client-centre-ui", "windows");
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
    let (mut term, mut rpc, _) = wide(&scratch, "client-centre-term", "windows");
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

/// With the editor centred, a window showing a terminal has the editor take
/// the whole terminal while it is on show — the terminal's window all of it
/// the window beside it does not want — and centred again once it is not.
#[test]
fn a_terminal_on_show_has_the_editor_take_the_whole_terminal() {
    require_nvim!();
    let scratch = Scratch::new("client-editor-term");
    let (mut term, mut rpc, _) = wide(&scratch, "client-editor-term", "editor");
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s
            .starts_with(&format!("{}the left window", " ".repeat(50)))),
        "never drawn centred:\n{}",
        term.text()
    );
    rpc.command("vsplit | terminal")
        .expect("a terminal beside it");
    assert!(
        common::wait_until(Duration::from_secs(5), || {
            term.pump_until(Duration::from_millis(20), |_| false);
            widths(&mut rpc) == columns(160, &[99, 60])
        }),
        "not the whole terminal: {}\n{}",
        widths(&mut rpc),
        term.text()
    );
    rpc.command("close").expect("the terminal's window closed");
    assert!(
        common::wait_until(Duration::from_secs(5), || {
            term.pump_until(Duration::from_millis(20), |_| false);
            widths(&mut rpc) == columns(60, &[60])
        }),
        "not centred again: {}\n{}",
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
    let (mut term, mut rpc, _) = wide(&scratch, "client-smart", "windows");
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

/// Its width judged as a window's text always is, a Markdown window's is as
/// wide as its tables and code blocks, which wrapping would break, and no
/// wider for its paragraphs, which it does not — where this Neovim has its
/// Markdown parser to say which lines are which.
#[test]
fn a_markdown_window_has_room_for_its_tables() {
    require_nvim!();
    let scratch = Scratch::new("client-smart-md");
    let (mut term, mut rpc, _) = wide(&scratch, "client-smart-md", "windows");
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

/// Judged as nothing but `window_width`, with `smart_width` off: a
/// `'textwidth'` is the buffer's own business.
#[test]
fn widths_judged_by_nothing_are_window_width() {
    require_nvim!();
    let scratch = Scratch::new("client-smart-off");
    let (mut term, mut rpc, _) = wide_with(
        &scratch,
        "client-smart-off",
        "windows",
        "smart_width = false\n",
    );
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

/// With the editor centred, it is as wide as its windows' texts side by side
/// want, each judged by what it shows, and each window is given what it
/// wants rather than all the same.
#[test]
fn a_centred_editor_gives_each_window_what_it_wants() {
    require_nvim!();
    let scratch = Scratch::new("client-smart-editor");
    let (mut term, mut rpc, _) = wide(&scratch, "client-smart-editor", "editor");
    assert!(
        term.pump_until(Duration::from_secs(15), |s| s
            .starts_with(&format!("{}the left window", " ".repeat(50)))),
        "never drawn centred:\n{}",
        term.text()
    );
    rpc.command("setlocal textwidth=40").expect("a textwidth");
    assert!(
        until_widths(&mut term, &mut rpc, &columns(40, &[40])),
        "one window: {}",
        widths(&mut rpc)
    );
    rpc.command("vsplit | enew | setlocal textwidth=30")
        .expect("another window, its own buffer");
    assert!(
        until_widths(&mut term, &mut rpc, &columns(71, &[30, 40])),
        "two: {}",
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
