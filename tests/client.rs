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

/// Pages typed faster than a slow link answers: each is on the screen before
/// Neovim could have heard of it, the lines it shows sent before it was
/// typed, and the view never goes back, and ends where Neovim's does.
#[test]
fn pages_typed_faster_than_the_link_answers_keep_up() {
    require_nvim!();
    let scratch = Scratch::new("client-burst");
    let (mut rpc, mut term, _) = drawn_slowly(
        &scratch,
        "client-burst",
        "call setline(1, map(range(1, 2000), '\"line \" . v:val'))",
    );
    // A page is the window less two lines: 20.
    let page = |k: usize| 1 + 20 * k as u32;
    let gap = Duration::from_millis(100);
    let presses = 10;
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
        watch(&mut term, t0 + gap * (k as u32 + 1), &mut drawn);
    }
    let tops = watch(&mut term, Instant::now() + 4 * SLOW, &mut drawn);

    for (k, (typed, drawn)) in typed.iter().zip(&drawn).enumerate() {
        let late = drawn.map(|d| d - *typed);
        assert!(
            late.is_some_and(|l| l < SLOW),
            "page {} waited: {late:?}",
            k + 1
        );
    }
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
