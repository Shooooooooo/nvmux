//! What nvmux says in the editor in front, in the editor's own words:
//! `vim.notify`, so that it appears wherever the user has that go — the message
//! row, or a notification plugin's window — and is kept in `:messages`.
//!
//! Two things are said. Which session this is, on arriving in one that was not
//! chosen by name — a `<prefix>` switch — which nvmux used to say with a box of
//! its own drawn over the session ([`crate::announce`], kept, and no longer
//! armed). And, while a session is in front, what the others have to report
//! as it happens ([`Notifier`]): a notification a program in one of their
//! terminal buffers asked for (`∗`), a command there that failed (`!`) or
//! finished (`✓`) — the picker's signs, told while the picker is down. The
//! session in front is told its own only of a terminal that was not on the
//! screen when it spoke.
//!
//! # In the editor
//!
//! [`crate::announce`] says why this was once the shape not to take: a session
//! manager that writes on the row the editor talks to you on has stopped being
//! a proxy. It is the shape taken now, by choice, for what that buys: the
//! user's own notification plugin, its history and `:messages`, and nothing of
//! nvmux's own drawn over a session it owns no cells of.
//!
//! What it takes to work, found against Neovim 0.11 and 0.12 with a UI
//! attached:
//!
//! * **One line, in the room there is.** A message wider than `v:echospace`
//!   brings up the hit-enter prompt — the editor stopped until a key, by a
//!   message it did not ask for — so it is cut to fit when it is shown,
//!   whatever will show it.
//! * **Deferred.** A request that shows a message waits for whatever showing
//!   it does: the one too wide for its row was not answered until its prompt
//!   was. So the chunk ([`SAY`]) only schedules the call, and answers at once,
//!   and nvmux's thread never waits on the showing — a plugin's window, or a
//!   prompt the cut to fit did not foresee.
//! * **After the UI.** A message given before a UI attaches is not on the
//!   screen once it has, and nor is one a `:mode` repaint follows. So the chunk
//!   waits for `UIEnter` in a session with no UI yet — a client just started —
//!   and the relay says a kept client's arrival only once the repaint it asks
//!   for on that client's return is done.
//! * **Not to an editor waiting for a key.** The request would wait with it,
//!   and say what it says once the moment had gone, so such an editor is asked
//!   nothing ([`say`]); the news waits to be tried again.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use rmpv::Value;

use crate::error::RpcError;
use crate::notes::{self, Heard, Kind, News};
use crate::rpc;
use crate::ui::note;

/// What one call may take, from the connect to the answer.
const BUDGET: Duration = Duration::from_secs(1);

/// How long the notifier waits, once a session is in front, before its first
/// round of reads: long enough for the sessions handed to it at the same
/// moment to be read in that round.
const SETTLE: Duration = Duration::from_millis(30);

/// How much news waits for an editor that is waiting for a key; past it, the
/// oldest goes. The picker has all of it anyway.
const KEPT: usize = 8;

/// The chunk that says a message, sent with `nvim_exec_lua` and the message
/// and its level (`vim.log.levels`) as its arguments.
///
/// Scheduled rather than said, cut to `v:echospace` as it is said, and held
/// for the first UI where there is none (see the module docs). A plugin's
/// `vim.notify` that fails is that plugin's business: it is called through
/// `pcall`, and the editor goes on.
const SAY: &str = r#"local msg, level = ...
local function say()
  local room = vim.v.echospace
  if room < 2 then
    return
  end
  while vim.fn.strdisplaywidth(msg) > room and vim.fn.strchars(msg) > 1 do
    msg = vim.fn.strcharpart(msg, 0, vim.fn.strchars(msg) - 2) .. '…'
  end
  pcall(vim.notify, msg, level, { title = 'nvmux' })
end
if #vim.api.nvim_list_uis() == 0 then
  vim.api.nvim_create_autocmd('UIEnter', {
    once = true,
    callback = function()
      vim.defer_fn(say, 0)
    end,
  })
else
  vim.defer_fn(say, 0)
end
return true
"#;

/// How much a message matters, as `vim.log.levels` has it: what a plugin
/// colours it by, and the default `vim.notify` too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    /// The level's number in `vim.log.levels`.
    fn code(self) -> i64 {
        match self {
            Level::Info => 2,
            Level::Warn => 3,
            Level::Error => 4,
        }
    }
}

/// Say `message` in the editor on `sock`, at `level`. `Ok(false)`, with
/// nothing asked of it, where that editor is waiting for a key.
pub fn say(sock: &Path, message: &str, level: Level) -> Result<bool, RpcError> {
    let mut client = rpc::Client::connect(sock, BUDGET)?;
    if client.get_mode()?.blocking {
        return Ok(false);
    }
    client.call(
        "nvim_exec_lua",
        vec![
            Value::from(SAY),
            Value::Array(vec![Value::from(message), Value::from(level.code())]),
        ],
    )?;
    Ok(true)
}

/// Say which session this is, in the editor on `sock`: the attach notice, as
/// the editor gives it. On a thread of its own, so that nothing waits for it,
/// and best effort: an editor waiting for a key is not told, and has its own
/// prompt on the screen to say why it is not answering.
pub fn arrive(sock: PathBuf, name: String) {
    let spawned = std::thread::Builder::new()
        .name("nvmux-tell".into())
        .spawn(move || {
            if let Err(e) = say(&sock, &name, Level::Info) {
                tracing::debug!(error = %e, "tell: could not say which session this is");
            }
        });
    if let Err(e) = spawned {
        tracing::debug!(error = %e, "tell: no thread to say which session this is on");
    }
}

/// What the editor in front is told of `news`: its sign, the session it is
/// from — `None` for the session in front itself — and what it says, as the
/// picker's caption would put it.
pub fn message(news: &News, from: Option<&str>) -> String {
    let sign = note::mark(news.kind);
    let what = note::what(news.kind, &news.title, &news.text);
    match from {
        Some(name) => format!("{sign} {name} · {what}"),
        None => format!("{sign} {what}"),
    }
}

/// How much news of `kind` matters: a notification wants the user, and a
/// failure is an error.
fn level(kind: Kind) -> Level {
    match kind {
        Kind::Notified => Level::Warn,
        Kind::Failed => Level::Error,
        Kind::Finished | Kind::Busy(_) => Level::Info,
    }
}

/// Of what a session said, what to tell, oldest first: what it numbered past
/// `heard` — the highest it had said by the last read — and nothing while
/// `heard` is `None`, a first read, which is there to find out how far the
/// session had got. Of the session in front, `here`, only what came from a
/// terminal that was off the screen: the rest it was showing.
fn to_tell(heard: Option<u64>, news: &[News], here: bool) -> Vec<&News> {
    let Some(heard) = heard else {
        return Vec::new();
    };
    news.iter()
        .filter(|n| n.seq > heard)
        .filter(|n| !here || n.shown == Some(false))
        .collect()
}

/// What the notifier is handed.
enum Command {
    /// Read this session, called this, through this socket, from now on.
    Watch {
        id: String,
        name: String,
        sock: PathBuf,
    },
    /// This session is in front now; `None`, one of nvmux's own screens is.
    Front(Option<String>),
}

/// One session the notifier reads.
struct Watched {
    id: String,
    name: String,
    sock: PathBuf,
    /// The highest number among what it has said, by the last read; `None`
    /// until it has been read since nvmux's own screens were last up.
    heard: Option<u64>,
}

/// Tells the editor in front, as it happens, what the other sessions have to
/// report — and of its own terminals, what the ones off the screen do.
///
/// It reads every session it is handed every [`notes::POLL`] while a session
/// is in front, and none while one of nvmux's own screens is: the picker has
/// a reader of its own, and shows it all. So coming back from one of those,
/// what was said meanwhile counts as told, and the first round only finds out
/// how far each session has got.
///
/// The reads are [`notes::hear`]'s, which ask nothing of an editor waiting for
/// a key — this runs for as long as nvmux does, and a session may wait for its
/// key all night.
///
/// Dropping it stops the thread without waiting for it, as with
/// [`notes::Poller`]: a read in flight finishes to nobody.
pub struct Notifier {
    commands: Sender<Command>,
}

impl Notifier {
    /// Start reading, with no session handed over yet and none in front.
    pub fn start() -> Self {
        let (commands, inbox) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("nvmux-notifier".into())
            .spawn(move || run(&inbox));
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "tell: no thread to read the sessions on");
        }
        Self { commands }
    }

    /// Read session `id`, called `name`, through `sock` from now on — or, if it
    /// is read already, by this name and through this socket.
    pub fn watch(&self, id: &str, name: &str, sock: PathBuf) {
        let _ = self.commands.send(Command::Watch {
            id: id.to_string(),
            name: name.to_string(),
            sock,
        });
    }

    /// Session `id` is in front now — handed over already, with [`watch`] —
    /// or, with `None`, one of nvmux's own screens is.
    ///
    /// [`watch`]: Notifier::watch
    pub fn front(&self, id: Option<&str>) {
        let _ = self.commands.send(Command::Front(id.map(str::to_string)));
    }
}

fn run(inbox: &Receiver<Command>) {
    let mut watched: Vec<Watched> = Vec::new();
    let mut front: Option<String> = None;
    // News for the session in front that it could not be told yet.
    let mut kept: Vec<(String, Level)> = Vec::new();
    let mut due: Option<Instant> = None;
    loop {
        let next = match due {
            Some(at) => inbox.recv_timeout(at.saturating_duration_since(Instant::now())),
            None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match next {
            Ok(Command::Watch { id, name, sock }) => {
                match watched.iter_mut().find(|w| w.id == id) {
                    Some(w) => {
                        w.name = name;
                        w.sock = sock;
                    }
                    None => watched.push(Watched {
                        id,
                        name,
                        sock,
                        heard: None,
                    }),
                }
            }
            Ok(Command::Front(id)) => {
                if id == front {
                    continue;
                }
                // Back from one of nvmux's own screens: what was said while it
                // was up was on it, or is old by now.
                if front.is_none() {
                    for w in &mut watched {
                        w.heard = None;
                    }
                }
                front = id;
                kept.clear();
                due = front.is_some().then(|| Instant::now() + SETTLE);
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(id) = front.as_deref() {
                    round(&mut watched, id, &mut kept);
                }
                due = front.is_some().then(|| Instant::now() + notes::POLL);
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Read every session, all at a time, and tell the session in front, `front`,
/// what is news.
fn round(watched: &mut [Watched], front: &str, kept: &mut Vec<(String, Level)>) {
    let heard: Vec<(usize, Heard)> = std::thread::scope(|s| {
        let reads: Vec<_> = watched
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let sock = &w.sock;
                s.spawn(move || (i, notes::hear(sock)))
            })
            .collect();
        reads.into_iter().filter_map(|r| r.join().ok()).collect()
    });
    for (i, heard) in heard {
        let Heard::Said(news) = heard else {
            continue;
        };
        let w = &mut watched[i];
        let here = w.id == front;
        for n in to_tell(w.heard, &news, here) {
            kept.push((
                message(n, (!here).then_some(w.name.as_str())),
                level(n.kind),
            ));
        }
        let top = news.iter().map(|n| n.seq).max().unwrap_or(0);
        w.heard = Some(w.heard.map_or(top, |h| h.max(top)));
    }
    if kept.len() > KEPT {
        kept.drain(..kept.len() - KEPT);
    }
    let Some(sock) = watched.iter().find(|w| w.id == front).map(|w| &w.sock) else {
        kept.clear();
        return;
    };
    while let Some((msg, level)) = kept.first() {
        match say(sock, msg, *level) {
            Ok(true) => {
                kept.remove(0);
            }
            // Waiting for a key: the next round tries again.
            Ok(false) => break,
            Err(e) => {
                tracing::debug!(error = %e, "tell: could not tell the session in front");
                kept.clear();
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn news(kind: Kind, title: &str, text: &str, seq: u64, shown: Option<bool>) -> News {
        News {
            kind,
            title: title.to_string(),
            text: text.to_string(),
            seq,
            shown,
        }
    }

    /// The picker's sign, the session it came from, and the picker's words
    /// for it — without the session's name for the session in front.
    #[test]
    fn a_message_is_the_sign_the_session_and_what_happened() {
        let finished = news(Kind::Finished, "cargo test", "exited with code 0", 1, None);
        assert_eq!(
            message(&finished, Some("api-server")),
            "✓ api-server · cargo test: exited with code 0"
        );
        let failed = news(Kind::Failed, "cargo build", "exited with code 101", 2, None);
        assert_eq!(
            message(&failed, Some("api-server")),
            "! api-server · cargo build: exited with code 101"
        );
        let notified = news(
            Kind::Notified,
            "Claude Code",
            "Claude needs your permission",
            3,
            None,
        );
        assert_eq!(
            message(&notified, Some("blog")),
            "∗ blog · Claude Code · Claude needs your permission"
        );
        assert_eq!(
            message(&notified, None),
            "∗ Claude Code · Claude needs your permission"
        );
    }

    #[test]
    fn a_notification_is_a_warning_and_a_failure_an_error() {
        assert_eq!(level(Kind::Notified), Level::Warn);
        assert_eq!(level(Kind::Failed), Level::Error);
        assert_eq!(level(Kind::Finished), Level::Info);
        assert_eq!(
            [Level::Info, Level::Warn, Level::Error].map(Level::code),
            [2, 3, 4],
            "vim.log.levels"
        );
    }

    /// The first read only finds out how far a session has got; after it,
    /// what is numbered past that is news, oldest first, once.
    #[test]
    fn news_is_what_is_numbered_past_the_last_read() {
        let said = [
            news(Kind::Finished, "make", "exited with code 0", 4, Some(true)),
            news(Kind::Failed, "make", "exited with code 2", 7, Some(true)),
        ];
        assert!(to_tell(None, &said, false).is_empty(), "a first read");
        let told: Vec<u64> = to_tell(Some(4), &said, false)
            .iter()
            .map(|n| n.seq)
            .collect();
        assert_eq!(told, [7]);
        assert!(to_tell(Some(7), &said, false).is_empty(), "told already");
    }

    /// In front, a session is told only what came from a terminal off the
    /// screen: not one it was showing, and not a progress message, which it
    /// shows itself.
    #[test]
    fn the_session_in_front_is_told_only_of_a_terminal_off_the_screen() {
        let said = [
            news(Kind::Finished, "sh", "exited with code 0", 1, Some(true)),
            news(Kind::Failed, "sh", "exited with code 3", 2, Some(false)),
            news(Kind::Failed, "claude", "failed", 3, None),
        ];
        let told: Vec<u64> = to_tell(Some(0), &said, true)
            .iter()
            .map(|n| n.seq)
            .collect();
        assert_eq!(told, [2]);
        assert_eq!(
            to_tell(Some(0), &said, false).len(),
            3,
            "from elsewhere, all"
        );
    }
}
