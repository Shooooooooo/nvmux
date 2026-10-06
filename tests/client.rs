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
use std::sync::mpsc;
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
    writer: Box<dyn Write + Send>,
    /// Everything the client has written, in order, and the screen it makes.
    output: Vec<u8>,
    parser: vt100::Parser,
    /// How far `output` has been scanned for queries.
    answered: usize,
    /// What this terminal answers a DA1 with.
    da1: &'static [u8],
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
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
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
            writer,
            output: Vec::new(),
            parser: vt100::Parser::new(ROWS, COLS, 0),
            answered: 0,
            da1,
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
        (0..ROWS)
            .map(|row| {
                let line: String = (0..COLS)
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
