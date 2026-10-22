//! Binary entry point.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use anyhow::{Context, Result};
use clap::Parser;

use nvmux::cli::Cli;
use nvmux::handoff::HandOff;
use nvmux::{
    announce, config, fade, logging, nested, nvim, palette, paths, pool, pty, reconnect, transport,
    ui,
};

/// How long the way out of nvmux waits for the session being left to have
/// what it reported cleared: long enough for a round trip to a session over
/// ssh, short enough that a detach is still at once.
const SEEN_ON_THE_WAY_OUT: std::time::Duration = std::time::Duration::from_millis(300);

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Everything else depends on this existing and being ours.
    let dir = paths::ensure_runtime_dir().context("preparing the nvmux runtime directory")?;
    logging::init(&dir)?;

    // nvmux's own client, which an nvmux started on a pty to draw a session
    // (see `nvmux::client`). Nothing below is its business: it has a session to
    // draw and a relay to draw it for. Its failures are logged rather than
    // printed, since what it prints is drawn.
    if let Some(sock) = &cli.client {
        if let Err(e) = nvmux::client::run(sock) {
            tracing::warn!(error = %format!("{e:#}"), "client: ended on an error");
            std::process::exit(1);
        }
        return Ok(());
    }

    // Before anything is spawned — and before a first-run config file is written
    // in `run` — clamp the umask, and record the one it replaced so that what
    // nvmux spawns can be handed it back: see `paths::restrict_umask`.
    paths::restrict_umask();

    // Before any screen is drawn, so a `kill` during the picker — not only
    // during an attached session — hands back a usable terminal; and a panic
    // that ends nvmux does the same, while one it catches leaves the screen
    // as it is (see `nvmux::panics`).
    nvmux::term::install_signal_safety_net();
    nvmux::panics::install();

    if let Err(e) = run(&cli) {
        // A plain message, no backtrace: every error here is meant to be
        // actionable on its own.
        eprintln!("nvmux: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

fn run(cli: &Cli) -> Result<()> {
    // Before the version check, before the config, and before any first-run
    // prompt: a second nvmux inside a session cannot work, and the less it has
    // done by the time it says so the better.
    nested::check()?;

    let location = cli.location();
    let remote = matches!(location, transport::Location::Ssh(_));
    // Before any client is started: each is told it with its settings, and
    // predicts for a session over ssh where they say "auto".
    config::set_remote(remote);

    // The local nvim, checked up front rather than surfacing later as an
    // unexplained connection failure. It runs every local session, and draws
    // any session when Neovim's own client does — so the one run that does
    // without it is `nvmux <host>` drawn by nvmux's own client, which only the
    // config can tell, below. The remote nvim is checked by the SSH transport
    // when it connects.
    if !remote {
        check_local_nvim()?;
    }

    // Config is a local concern — the prefix machine and the picker both run
    // here — so it is established before any transport, `nvmux <host>`
    // included. On a genuine first run at an interactive terminal this asks for a
    // prefix and records it; otherwise it loads whatever exists (or the defaults).
    config::init(establish_settings()?);
    if remote && config::get().client.ui == config::Ui::Nvim {
        check_local_nvim()?;
    }

    // The fade dissolves every screen into the terminal's own background, so
    // it has to know what colour that is — and only the terminal can say. After
    // the config, so a user who turned the fade off never pays for the question
    // or the keystroke it can cost (see `palette::query`); before any screen
    // that would fade; and after the first-run screen, which does not.
    // The picker's afterglow and filter fade paint in colour too, so they ask
    // even with the fade off.
    // And nvmux's own client mixes colours for every animation it draws, so
    // it asks too — on the client's behalf, since a client on a pty cannot
    // ask without the answers racing the keys (see `nvmux::client`).
    if fade::configured()
        || nvmux::ui::effects::want_palette()
        || config::get().client.ui == config::Ui::Nvmux
    {
        palette::init(palette::query());
    }

    let transport = transport::open(location.clone())?;
    session_loop(&transport)
}

/// The local `nvim`: on `$PATH`, and new enough.
fn check_local_nvim() -> Result<()> {
    let version = nvim::check_local()?;
    tracing::debug!(%version, "local nvim");
    Ok(())
}

/// Decide this run's settings, prompting once on a true first run.
///
/// A first run is: no config file at the default path, `$NVMUX_CONFIG` unset, and
/// an interactive terminal (both stdin and stdout). Anything else — a file
/// already there, an explicit config, a piped/non-interactive run — just loads
/// normally. Failing to *write* the chosen config is reported but not fatal: the
/// prefix still applies this session, and the next run will ask again.
fn establish_settings() -> Result<config::Settings> {
    use std::io::IsTerminal;

    match config::first_run_target() {
        Some(path) if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => {
            match ui::setup::run()? {
                ui::setup::Outcome::Chosen(prefix) => {
                    if let Err(e) = config::write_default(&path, prefix) {
                        // The error names the path itself.
                        eprintln!("nvmux: could not write {e}");
                    }
                    Ok(config::with_prefix(prefix))
                }
                ui::setup::Outcome::Skipped => Ok(config::Settings::default()),
            }
        }
        _ => Ok(config::load()?),
    }
}

/// Alternate between the picker and an attached session until the user leaves.
///
/// The attachment is carried across iterations, so `<prefix> Space`, `<prefix> c` and
/// `<prefix> ?` come back to the *same* client rather than starting a new one.
///
/// With `[client] per_session` every client that leaves the front is carried
/// across in the [`pool::Pool`] instead — the one those three come back to, and
/// the one of every other session visited — so a switch back to a session
/// resumes its client rather than starting another.
///
/// With `[client] lazy = false` the pool starts out holding a client for every
/// session, started before the picker is drawn (see [`start_every_client`]),
/// so a first visit takes one out of it as a switch back does — and holds one
/// again for every session but the one in front once the settings have changed
/// the kind of client, which retires them all (see [`Again::Every`]).
///
/// Each session put in front is given the watcher its notes come from, and
/// what it kept is cleared, and again as it leaves: it has been looked at (see
/// [`nvmux::notes`]).
///
/// The picker opens on the rows it is handed, where there are any (see
/// [`ui::Listing`]): after `<prefix> Space` the ones this loop has in hand,
/// with a listing begun behind them on a thread `shared` is lent to.
fn session_loop(shared: &transport::Shared) -> Result<()> {
    let transport: &dyn transport::Transport = shared.as_ref();
    let mut attached: Option<pty::Attachment> = None;
    // Empty, and handing every client straight back, unless clients are kept
    // one per session or started ahead. Dropped on every way out of here,
    // which retires what it holds.
    let mut pool = pool::Pool::configured();
    // What the next picker opens on: the listing the clients were started
    // ahead from, the first time, and after that the rows in hand on the way
    // back from a session, or a listing of its own.
    let mut opening = if config::get().client.lazy {
        ui::Listing::Ask
    } else {
        start_every_client(transport, &mut pool).map_or(ui::Listing::Ask, ui::Listing::Fresh)
    };
    // The client settings the clients in hand were started under, for what
    // the settings screen changes of them that telling cannot reach (see
    // `settle`).
    let mut clients = config::get().client;
    let remote = matches!(transport.location(), transport::Location::Ssh(_));
    // For the picker, which lists, kills and renumbers as it floats.
    ui::picker::lend(shared);
    let mut message: Option<String> = None;
    // The session the last trip through the picker led to, so the next one
    // opens with the cursor on it rather than on the first row.
    let mut focus: Option<String> = None;
    // The session the last relay showed, so that a kept client coming back in
    // place of another session's says where the user has landed, as a fresh
    // client does.
    let mut shown: Option<String> = None;
    // The session the user was looking at until the picker came up over it:
    // what it was keeping to report is no news to them, so the picker clears
    // it before it first reads it (see `nvmux::notes`).
    let mut looked_at: Option<String> = None;
    // What posts cards for the session in front, with what the others have
    // to report as it happens (see `nvmux::tell`). Handed each session's
    // socket as the picker finds it, and each attached one's here.
    let notifier = nvmux::tell::Notifier::start();
    // The clients started again behind the session in front (see `Revival`);
    // whether the next attach is the first since a dropped link — the link
    // came back under the session in front, or a kept client was found to have
    // left with it; and whether it is the first since the settings changed the
    // kind of client (see `settle`). Those two say which clients (see `Again`).
    let mut revival: Option<Revival> = None;
    let mut after_a_drop = false;
    let mut new_kind = false;

    loop {
        // `<prefix> c` moves this to the session it just created. A client held
        // across the trip — or parked, if clients are kept — is what `Esc` goes
        // back to; without one — the first screen, a failed attach, a session
        // that exited — there is nothing behind the picker and `Esc` says so by
        // doing nothing.
        // `hand_off` is the session's name, left standing on the screen by the
        // picker for the session to take down (see `nvmux::handoff`). Whoever
        // ends the attach first takes it down: the relay, the attaching
        // screen, or one of the ways out below that never reach either.
        // `float_settings` is the settings, turned to float as they had the
        // terminal: to come up floating over the session as the next relay
        // brings it back (see `ui::settings::Outcome::Float`). Turned so on
        // the picker's `,`, it is the session the picker came up from.
        let seen = looked_at.take();
        let (mut current, mut listing, mut hand_off, mut float_settings) = match ui::run(
            transport,
            std::mem::take(&mut opening),
            message.take(),
            focus.as_deref(),
            attached.is_some() || focus.as_deref().is_some_and(|id| pool.holds(id)),
            seen.as_deref(),
            &notifier,
        )? {
            ui::Outcome::Quit => {
                // A client held across `<prefix> Space` is retired explicitly; its
                // `Drop` would do the same, this just says so.
                if let Some(a) = attached.take() {
                    a.terminate();
                }
                // The picker put its own modes back, but what that client
                // wrote on its way out was thrown away, so whatever else it
                // had on is still on. The same final reset the detach path
                // ends on, for the same reason: harmless once the client is
                // gone, and the shell wants a plain terminal.
                nvmux::term::reset_screen();
                break;
            }
            ui::Outcome::Attach {
                session,
                sessions,
                hand_off,
                float_settings,
            } => (session, sessions, hand_off, float_settings),
        };
        // A session the listing no longer has has nothing to come back to.
        pool.keep_only(&listing);
        // The picker may have had the settings changed.
        new_kind |= settle(&mut clients, &mut attached, &mut pool);
        // Derived rather than carried: one fewer thing for the picker to keep
        // in step, and the listing it handed over is what it would be derived
        // from anyway.
        let mut highest = ui::highest_num(&listing);
        // A client begun while the last session dissolved, waiting to be
        // waited for. Only a switch ever sets it, and a switch always comes
        // straight back round this loop, so it never outlives the trip.
        let mut begun: Option<Begun> = None;
        // Whether this pass attaches out of the picker — the first, and only
        // the first: every later one is a `<prefix>` switch, a reconnect or a
        // session made with `<prefix> c`.
        let mut from_picker = true;
        // A switch by `<prefix> n` or `<prefix> p`: which way, and the session
        // it left, for the card that says where it landed to move its row the
        // way it went. Set by the switch, and spent by the attach it leads to.
        let mut stepped: Option<(nvmux::keys::Direction, nvmux::session::Session)> = None;
        // A prompt from the picker floating over the session, full screen
        // where it could not float in the picker's place, answered with no
        // session to go to — `r`'s, or `c`'s cancelled: the row the cursor was
        // on, for the picker to come back up over this session with the cursor
        // there. Set by the prompt, and spent by the attach it leads to.
        let mut float_picker: Option<String> = None;

        loop {
            // Everything nvmux does between the keypress and a client ready to
            // relay: retiring the old one, the probe, and the spawn — and, for
            // a session slow to answer, the wait the user watches. `early` is
            // whether the fork and the probe happened under the outgoing
            // session's dissolve, in which case what is timed here is mostly
            // the wait for a round trip already in flight. Read with the
            // `timing: session painted` record the relay ends up emitting,
            // which is the session's own share of the same switch.
            let t_open = std::time::Instant::now();
            // Anything begun for a session other than the one being attached
            // to now is a client nobody asked for: dropping it retires it.
            let started = begun.take().filter(|b| b.session.id == current.id);
            let early = started.is_some();
            // Whether the client is one kept for this session, resumed rather
            // than started.
            let mut warm = false;
            // The clients started again since last asked, where the attach
            // below looks first.
            if revival
                .as_ref()
                .is_some_and(|r| !r.collect(&mut pool, None))
            {
                revival = None;
            }
            let opened = match attached.take() {
                Some(a) if a.session_id == current.id => Ok(Some(a)),
                // Retiring the old client leaves its server running: killing a
                // client does not kill a --headless --listen server.
                //
                // Hung up first and reaped after, with the new client's spawn —
                // or, where it was begun early, the wait for its probe — in
                // between: the two have nothing to say to each other — a
                // different session, a different server, a different pty — so
                // serialising them would charge the user the sum of the two.
                // Nothing reads the old pty master again either, so the dying
                // client's own restore sequence goes nowhere near the terminal.
                Some(mut other) => {
                    other.hang_up();
                    let spawned = resume_or_attach(
                        transport,
                        &mut pool,
                        &current,
                        started,
                        &mut warm,
                        &mut hand_off,
                        &mut after_a_drop,
                    )?;
                    // Explicitly, rather than by falling out of the arm: this is
                    // the wait the hangup deferred, and leaving it to a binding's
                    // drop would let the next edit here re-serialise it without
                    // noticing. It has to happen on the failure path too, which
                    // is why the spawn is held rather than returned from inside.
                    drop(other);
                    spawned
                }
                None => resume_or_attach(
                    transport,
                    &mut pool,
                    &current,
                    started,
                    &mut warm,
                    &mut hand_off,
                    &mut after_a_drop,
                )?,
            };

            tracing::debug!(
                ms = t_open.elapsed().as_secs_f64() * 1000.0,
                early,
                warm,
                "timing: retire + probe + spawn"
            );

            // No session is coming to take the name down: off the screen it
            // comes before the picker goes up over it, or it is what the shell
            // finds there afterwards.
            if !matches!(opened, Ok(Some(_))) {
                if let Some(h) = hand_off.take() {
                    h.take_down();
                }
            }
            // A failed attach must not end the program: the user can only act
            // on it from the picker, with the reason on screen.
            let mut attachment = match opened {
                Ok(Some(a)) => a,
                // Given up on: back to the picker, with the cursor on the
                // session that was not answering and nothing on the hint row
                // — the user knows what they did. No client is held: after a
                // `<prefix>` switch the old one was hung up before the spawn —
                // or, kept one per session, parked, where Enter on its row
                // resumes it — so `Esc` on the picker goes nowhere, as after
                // a failed attach.
                Ok(None) => {
                    tracing::info!(id = %current.id, "attach cancelled");
                    break;
                }
                Err(e) => {
                    tracing::warn!(id = %current.id, error = %e, "attach failed");
                    message = Some(describe_attach_failure(transport.location(), &e));
                    break;
                }
            };
            if let Some(h) = hand_off.take() {
                attachment.carry_name(h);
            }
            // A fresh client announces itself from the moment it is spawned. A
            // kept one has only to say so when it is not the session the user
            // was just looking at — back from the picker, or from `<prefix> c`
            // cancelled, it is.
            if warm && shown.as_deref() != Some(current.id.as_str()) {
                attachment.announce_on_arrival(announce::label(&current.name));
            }
            // Out of the picker, a session other than the one the user was just
            // in is announced as a switch by key is. Back to that one — `Esc`,
            // or Enter on its own row — the notice has nothing to add but a key
            // the session is waiting for.
            if from_picker && shown.as_deref() == Some(current.id.as_str()) {
                attachment.arrive_unannounced();
            }
            let step = stepped.take();
            attachment.place_in_row(row_of(
                &listing,
                &current,
                step.as_ref().map(|(dir, left)| (*dir, left)),
            ));
            from_picker = false;
            shown = Some(current.id.clone());

            // The first attach since a dropped link, or since the settings
            // changed the kind of client: the clients the link took, or every
            // other session's of the new kind, are started again behind this
            // one, once it has painted, unless `[client] lazy` says to wait
            // for their visits (see `Again`). Those of a revival still going
            // are collected first, so that the ones the link took again are
            // among them.
            let dropped = std::mem::take(&mut after_a_drop);
            let changed = std::mem::take(&mut new_kind);
            if let Some(again) = Again::after(dropped, changed, remote, config::get().client) {
                if let Some(earlier) = revival.take() {
                    earlier.collect(&mut pool, Some(&current.id));
                }
                revival = Revival::begin(
                    shared,
                    &mut pool,
                    &listing,
                    &current.id,
                    again,
                    &mut attachment,
                );
            }

            // The session is about to be looked at: its watcher put in if it
            // has none yet, so it records from now on, and what it kept
            // cleared. On a thread, so the session is in front at once (see
            // `nvmux::notes::touch`).
            let sock = attachment.sock().to_path_buf();
            nvmux::notes::later(sock.clone(), nvmux::notes::touch);
            // And told, from now until the relay ends, what the others have
            // to say — by its name as it is now, a rename since included.
            notifier.watch(&current.id, &current.name, sock.clone());
            notifier.front(Some(&current.id));

            // The rows the picker opens on if it floats over this session, and
            // the one it opens on back from the rename prompt; what it left of
            // them, if it changed them, is taken back below.
            ui::picker::hand(&listing, float_picker.as_deref());
            if std::mem::take(&mut float_settings) {
                attachment.float_the_settings();
            }
            if float_picker.take().is_some() {
                attachment.float_the_picker();
            }

            // The relay, with the chance to start the next client while this
            // session is still dissolving: `begin_next` fires on a `<prefix>`
            // switch, before the fade, and what it leaves in `begun` is waited
            // for at the top of this loop. What it hands back is the card the
            // switch lands with, which stands on the screen from the key to
            // the session it names (see `pty::relay`). Scoped, so the
            // closure's borrows end before the match below — which assigns to
            // the very things it reads.
            let ended = {
                let mut begin_next = |target: pty::Target| {
                    // Resolved here, and not again below: the session started
                    // for is the session switched to, or the two could differ.
                    // A target that names nothing is left to the path below,
                    // which can re-list for it; that is a round trip, and this
                    // is not the place to spend one.
                    let session = pick(&listing, target, current.state.num)?;
                    // A switch onto the session already attached reuses the
                    // client in hand. Starting a second one for it would put a
                    // second UI on that server — which shrinks its screen to
                    // the smaller of the two — for a client that would then be
                    // dropped unused; and it lands nowhere new.
                    if session.id == current.id {
                        return None;
                    }
                    let step = match target {
                        pty::Target::Step(dir) => Some((dir, &current)),
                        pty::Target::Number(_) | pty::Target::Session(_) => None,
                    };
                    let landing = nvmux::card::Card::arrival(
                        nvmux::card::next_id(),
                        &announce::label(&session.name),
                        false,
                    )
                    .in_row(row_of(&listing, session, step));
                    // Nor for one with a client kept for it, which has nothing
                    // to start: the switch resumes that one. Nor for one whose
                    // kept client has left, which may have left with the link:
                    // the switch recovers that first, and a start here would
                    // go over it before then (see `resume_or_attach`). Asked
                    // after the clients started again behind this session are
                    // in, so that one back already counts as kept.
                    if let Some(r) = &revival {
                        r.collect(&mut pool, Some(&current.id));
                    }
                    if !pool.has(&session.id) {
                        match begin_attachment(transport, session) {
                            Ok(b) => begun = Some(b),
                            // Not reported: the path below attempts the same
                            // attach a moment later, where a failure has a
                            // screen to land on and a message written for it.
                            Err(e) => tracing::debug!(
                                id = %session.id,
                                error = %e,
                                "could not start the next client early"
                            ),
                        }
                    }
                    Some(landing)
                };
                pty::relay(attachment, highest, &mut begin_next)?
            };
            // The rows as a picker floating over the session left them, or a
            // prompt floating there: a kill, a move, a session made or
            // renamed, a session another nvmux made. The session in front as
            // they have it too — its name, renamed, and its number, moved —
            // for what is said of it, and stepped from, from here on.
            if let Some(rows) = ui::picker::take_changed() {
                listing = rows;
                highest = ui::highest_num(&listing);
                pool.keep_only(&listing);
                if let Some(s) = listing.iter().find(|s| s.id == current.id) {
                    current = s.clone();
                }
            }
            // Out of the session: whatever comes next — the picker, the help,
            // the prompt, another session, or the shell — nobody is in front
            // to be told until the next attach says who is.
            notifier.front(None);
            // Whatever the session reported while it was in front was seen: it
            // is cleared on the way out — by the picker, if that is where the
            // user goes, before it reads it; otherwise on a thread, or, on the
            // way out of nvmux, waited for a moment so it gets said at all.
            match &ended.0 {
                pty::Outcome::ToPicker => looked_at = Some(current.id.clone()),
                pty::Outcome::Detached | pty::Outcome::StdinClosed => {
                    // Said while the clients kept out of sight are retired,
                    // which has nothing to wait for from it, and waited for
                    // once they are, for what is left of its budget.
                    let deadline = std::time::Instant::now() + SEEN_ON_THE_WAY_OUT;
                    let seeing = nvmux::notes::seen_begin(sock);
                    pool.retire_all();
                    seeing.wait_until(deadline);
                }
                pty::Outcome::CreateNew
                | pty::Outcome::CreateAfter(_)
                | pty::Outcome::Rename(_)
                | pty::Outcome::ShowHelp
                | pty::Outcome::ShowSettings
                | pty::Outcome::Resettle
                | pty::Outcome::Switch(_) => {
                    nvmux::notes::later(sock, nvmux::notes::seen);
                }
                _ => {}
            }
            match ended {
                (pty::Outcome::ToPicker, held) => {
                    attached = pool.set_aside(held);
                    // The picker opens on the rows the user last left it with,
                    // as this loop has kept them since — a session made with
                    // `<prefix> c` added, a switch's re-listing taken — and
                    // lists afresh behind them on a thread of its own. Over ssh
                    // that listing is a round trip to the host, which the
                    // picker's first frame no longer waits for (see
                    // `ui::Listing`).
                    opening = ui::Listing::in_hand(std::mem::take(&mut listing), shared);
                    break;
                }
                // The session keeps running either way: on `<prefix> d`
                // because the user asked, on a closed stdin because there is
                // no terminal left to ask from.
                (pty::Outcome::Detached | pty::Outcome::StdinClosed, _) => return Ok(()),
                // Two things look like this: the session ended, and the link
                // to its host dropped under the client. The transport can tell
                // which — see `reconnect` — and in the second case the session
                // is still there, so the loop goes round again onto the same
                // `current` with no client held, which is a fresh attach.
                //
                // On the plain terminal, deliberately: the relay has given it
                // back, the picker has not taken it, and a reconnection may
                // need it — ssh asks for a passphrase there. So the progress
                // is lines on stderr, the one thing that can be written
                // without owning the screen.
                (pty::Outcome::ChildExited, _) => {
                    let host = transport.location().to_string();
                    let verdict = reconnect::recover(
                        transport,
                        |retry| eprintln!("{}", describe_retry(&host, retry)),
                        std::thread::sleep,
                    );
                    match verdict {
                        // The session ended, as `:q` ends one: the picker opens
                        // on the rows in hand without it, and lists afresh
                        // behind them, as after `<prefix> Space` — the round
                        // trip a listing is, no longer between the session and
                        // the picker. A client ended by `:detach` leaves its
                        // session running, and the listing brings that back,
                        // with the cursor on it (see `App::select_session`).
                        reconnect::Verdict::Unneeded => {
                            listing.retain(|s| s.id != current.id);
                            opening = ui::Listing::in_hand(std::mem::take(&mut listing), shared);
                            break;
                        }
                        reconnect::Verdict::Restored => {
                            after_a_drop = true;
                            continue;
                        }
                        // Out, not back to the picker. The picker's first act
                        // is a listing over the very link that just failed
                        // seven times, on an unbounded connect this time, and
                        // its failure ends nvmux with ssh's words and none of
                        // these. Better to end here, saying what was tried and
                        // that the session is still there to come back to.
                        reconnect::Verdict::GaveUp(e) => {
                            tracing::warn!(%host, error = %e, "could not reconnect");
                            anyhow::bail!("{}", describe_reconnect_failure(&host, &e));
                        }
                    }
                }
                (pty::Outcome::CreateNew, held) => {
                    // Held rather than killed, so a cancelled prompt resumes it.
                    attached = pool.set_aside(held);
                    if let ui::prompt::Outcome::Created(session) = ui::prompt::run(transport)? {
                        // A new session can be numbered above anything the
                        // relay was told about, and the hint decides how long a
                        // digit waits. It is also a row the listing in hand does
                        // not have, and `<prefix> n` from here has to be able to
                        // find its way back to it.
                        highest = highest.max(session.state.num);
                        listing.push(session.clone());
                        current = session;
                    }
                    continue;
                }
                // `c` on the picker floating over the session, where the prompt
                // could not float in its place — on a terminal too small for
                // it: the session asked for full screen, and placed as the
                // picker full screen does, after the row the cursor was on:
                // then to it, as from there — or, the prompt cancelled, back to
                // this session, held as for `<prefix> c`, with the picker up
                // over it again on that row, as the picker full screen is back
                // after a cancel.
                (pty::Outcome::CreateAfter(after), held) => {
                    attached = pool.set_aside(held);
                    new_kind |= settle(&mut clients, &mut attached, &mut pool);
                    let after = after.map(|id| id.as_str().to_string());
                    match ui::prompt::create_after(transport, &listing, after.as_deref())? {
                        Some((session, rows)) => {
                            listing = rows;
                            pool.keep_only(&listing);
                            highest = ui::highest_num(&listing);
                            current = session;
                        }
                        None => float_picker = Some(after.unwrap_or_else(|| current.id.clone())),
                    }
                    continue;
                }
                // `r` on the picker floating over the session, where the prompt
                // could not float in its place: the name asked for full screen,
                // as the picker full screen asks; then back to the session, held
                // as for `<prefix> c`, with the picker up over it again, the
                // cursor on the row renamed — as the picker full screen is back
                // after its prompt.
                (pty::Outcome::Rename(id), held) => {
                    attached = pool.set_aside(held);
                    new_kind |= settle(&mut clients, &mut attached, &mut pool);
                    let named = listing.iter().find(|s| s.id == id.as_str()).cloned();
                    if let Some(session) = named {
                        if let ui::prompt::Outcome::Renamed(name) =
                            ui::prompt::rename(transport, &session)?
                        {
                            // Put in the rows in hand rather than listed for:
                            // a rename changes the name and nothing else, and
                            // a listing would be a round trip over ssh on a
                            // blank screen. The picker lists behind them.
                            for s in listing.iter_mut().chain(std::iter::once(&mut current)) {
                                if s.id == session.id {
                                    s.name.clone_from(&name);
                                }
                            }
                        }
                    }
                    float_picker = Some(id.as_str().to_string());
                    continue;
                }
                (pty::Outcome::ShowHelp, held) => {
                    attached = pool.set_aside(held);
                    ui::help::run()?;
                    continue;
                }
                (pty::Outcome::ShowSettings, held) => {
                    attached = pool.set_aside(held);
                    let left = ui::settings::run(remote)?;
                    new_kind |= settle(&mut clients, &mut attached, &mut pool);
                    // Turned to float: back up over the session, floating,
                    // as it comes back.
                    float_settings = left == ui::settings::Outcome::Float;
                    continue;
                }
                // The settings floated over the session and changed what kind
                // of client draws it, or whether clients are kept: settled
                // here, and the session attached again.
                (pty::Outcome::Resettle, held) => {
                    attached = pool.set_aside(held);
                    new_kind |= settle(&mut clients, &mut attached, &mut pool);
                    continue;
                }
                (pty::Outcome::Switch(target), mut held) => {
                    // The card the switch lands with, left standing on the
                    // screen for the next attach to take down as it draws the
                    // card again — or, attaching nowhere, as the one it falls
                    // back on does.
                    if let Some(h) = held.as_mut().and_then(pty::Attachment::take_left_standing) {
                        hand_off = Some(h);
                    }
                    // Held rather than killed, so a number that names nothing —
                    // or a cycle with nowhere to go — puts the user straight
                    // back where they were.
                    attached = pool.set_aside(held);
                    // What the settings changed of the clients, where the
                    // picker floating over the session went across to them
                    // and back before it switched: settled on the way out of
                    // the picker, as from the picker full screen. Nothing,
                    // where nothing changed.
                    new_kind |= settle(&mut clients, &mut attached, &mut pool);
                    stepped = match target {
                        pty::Target::Step(dir) => Some((dir, current.clone())),
                        pty::Target::Number(_) | pty::Target::Session(_) => None,
                    };
                    // Already resolved, and already started: the dissolve that
                    // has just finished ran over the top of its fork and its
                    // probe. Nothing left to look up.
                    if let Some(b) = &begun {
                        current = b.session.clone();
                        continue;
                    }
                    // Resolved against the listing in hand, which is the one
                    // thing a `<prefix>` switch used to pay for that an attach
                    // from the picker does not: there the listing had already
                    // happened, here it sat between the keypress and the new
                    // session's first frame. Measured on a host whose forks are
                    // expensive: 88-110 ms of a ~215 ms switch, and every
                    // millisecond of it on a screen the user is watching.
                    //
                    // Stale in one direction only, and the attach is what finds
                    // out: a session that has since gone is still named here,
                    // and the spawn's probe then fails with "that session is
                    // gone" onto the picker's hint row — which re-lists on the
                    // way, so the next switch is accurate. What the listing
                    // cannot do is invent a row, which is what the miss below
                    // is for.
                    let mut picked = pick(&listing, target, current.state.num).cloned();
                    if picked.is_none() {
                        // A listing in hand cannot prove a negative: a session
                        // created since it was taken — by another nvmux, or in
                        // another window — is missing rather than absent. So a
                        // miss is the one case that still pays for a listing,
                        // and it pays it before saying no.
                        //
                        // A failed listing goes back to the picker like a failed
                        // attach, and for a second reason besides: this is the
                        // one path from one relay straight into another, so the
                        // screen an error would land on is the cleared one the
                        // held branch of `pty::relay` just handed over — nothing
                        // else on it, and nothing the user could do from it. The
                        // hint row is both, next to the row it is about.
                        //
                        // Timed on its own because it is the one thing a
                        // `<prefix>` switch can pay that an attach from the
                        // picker does not: there, the listing happened before
                        // the user pressed anything. Only a miss pays it now.
                        let t_list = std::time::Instant::now();
                        let listed = transport.list_sessions();
                        tracing::debug!(
                            ms = t_list.elapsed().as_secs_f64() * 1000.0,
                            "timing: switch listing"
                        );
                        match listed {
                            Ok(fresh) => {
                                listing = fresh;
                                pool.keep_only(&listing);
                                highest = ui::highest_num(&listing);
                                picked = pick(&listing, target, current.state.num).cloned();
                            }
                            Err(e) => {
                                message = Some(describe_listing_failure(&e));
                                break;
                            }
                        }
                    }
                    match picked {
                        // The loop above retires — or parks — the old client and
                        // attaches the new one; an unchanged id reuses the
                        // client as it is.
                        Some(session) => current = session,
                        None => tracing::debug!(?target, "nothing to switch to"),
                    }
                    continue;
                }
            }
        }

        // Every way out of the relay loop leads back to the picker, and every
        // one of them was showing `current` — including the failures, where the
        // cursor lands on the row the message on the hint line is about.
        focus = Some(current.id.clone());
    }
    Ok(())
}

/// What the settings screen changed of the clients that they cannot follow by
/// being told (for what they can, see `pty::Attachment::open_desk`), and
/// whether that was the kind of client. The other kind, `[client] ui`, retires
/// every one there is — the one held across the screen and those kept — for
/// the next attach to start the new kind, and the other sessions' to be
/// started behind it or on their own next attaches (see [`Again::Every`]);
/// clients no longer kept, `[client] per_session`, retire those kept, and kept
/// ones are kept again from the next to leave the front.
fn settle(
    was: &mut config::ClientSettings,
    attached: &mut Option<pty::Attachment>,
    pool: &mut pool::Pool,
) -> bool {
    let now = config::get().client;
    let new_kind = now.ui != was.ui;
    if new_kind {
        if let Some(a) = attached.take() {
            a.terminate();
        }
        pool.retire_all();
    }
    pool.keep(now.per_session);
    *was = now;
    new_kind
}

/// The attachment to relay next, from the pool if it has one for the session
/// — kept with `[client] per_session`, and resumed: no fork, no probe, since
/// it is still attached to its server, and no first paint to wait for, since
/// its last screen goes straight back on; or started ahead with `[client]
/// lazy = false`, and relayed for the first time, from its first byte — and
/// otherwise a fresh one (see [`attach`]). With clients neither kept nor
/// started ahead the pool is empty, and this is `attach`.
///
/// The outer `Result` is nvmux giving up on the link altogether; the inner one
/// is an attach that failed, for the picker to say so. `warm` is set when the
/// client is a kept one — not one started ahead, which is as new to the
/// terminal as a fresh one and says which session it is as one does.
///
/// `hand_off` is the name the picker left standing, if it did; see
/// [`attach`]. The link's recovery prints to the screen it stands on, so it is
/// taken down before that.
///
/// `after_a_drop` is set when the client kept for the session had left, which
/// a session still there to be attached afresh says it did with the link:
/// the attach is then the first since a dropped link (see [`Revival`]).
#[allow(clippy::too_many_arguments)]
fn resume_or_attach(
    transport: &dyn transport::Transport,
    pool: &mut pool::Pool,
    session: &nvmux::session::Session,
    begun: Option<Begun>,
    warm: &mut bool,
    hand_off: &mut Option<HandOff>,
    after_a_drop: &mut bool,
) -> Result<nvmux::Result<Option<pty::Attachment>>> {
    Ok(match pool.take(&session.id) {
        // Its notice was written when it was started, under the name the
        // session had then. And a probe that has still not answered is waited
        // out as a fresh client's is, with the attaching screen up: the client
        // is no more cleared to be shown than one forked a moment ago.
        pool::Taken::Kept(ahead) if ahead.started_ahead() => {
            let mut ahead = *ahead;
            ahead.announce_on_arrival(announce::label(&session.name));
            match ahead.take_probe() {
                Some(probe) => finish_attachment(
                    Begun {
                        session: session.clone(),
                        sock: ahead.sock().to_path_buf(),
                        attachment: ahead,
                        probe,
                    },
                    hand_off,
                ),
                None => Ok(Some(ahead)),
            }
        }
        pool::Taken::Kept(kept) => {
            *warm = true;
            Ok(Some(*kept))
        }
        // One that left while parked may have left because the link it came
        // over went — with every other parked client, while the user was in
        // the picker. That is the front client's `ChildExited` found late,
        // and it gets the same bounded recovery before the fresh attach goes
        // over the link, whose own connect has no bound. (The early start a
        // switch makes under its fade is skipped for such a session, for the
        // same reason.)
        pool::Taken::Left => {
            if let Some(h) = hand_off.take() {
                h.take_down();
            }
            recover_the_link(transport)?;
            let attached = attach(transport, session, begun, hand_off);
            *after_a_drop |= matches!(attached, Ok(Some(_)));
            attached
        }
        pool::Taken::Absent => attach(transport, session, begun, hand_off),
    })
}

/// Bring the link back, if it went, before a fresh attach replaces a parked
/// client that left — the recovery a front client's exit gets, found late (see
/// `session_loop`). Locally, and wherever the link is fine, this is nothing.
///
/// Gives up the way that one does, with nvmux's last words: the fresh
/// attach's connect has no bound, and a link that failed seven times in a row
/// is not one to wait on without saying so.
///
/// Unlike a front client's exit, nothing has undone the keyboard mode the last
/// client left the terminal in, so it is undone here, before the first line
/// that names Ctrl-C: with xterm's `modifyOtherKeys` still on, Ctrl-C reaches
/// the tty as an escape sequence rather than as the byte that interrupts. A
/// kept client coming back sets its own again (see `nvmux::ledger`).
fn recover_the_link(transport: &dyn transport::Transport) -> Result<()> {
    use std::io::Write;
    let host = transport.location().to_string();
    match reconnect::recover(
        transport,
        |retry| {
            if retry.attempt == 1 {
                let mut out = std::io::stdout();
                let _ = out.write_all(b"\x1b[>4;0m");
                let _ = out.flush();
            }
            eprintln!("{}", describe_retry(&host, retry));
        },
        std::thread::sleep,
    ) {
        reconnect::Verdict::Unneeded | reconnect::Verdict::Restored => Ok(()),
        reconnect::Verdict::GaveUp(e) => {
            tracing::warn!(%host, error = %e, "could not reconnect");
            anyhow::bail!("{}", describe_reconnect_failure(&host, &e));
        }
    }
}

/// Explain why an attach failed, in terms of what actually went wrong.
///
/// Through an SSH forward, ECONNREFUSED means the *ControlMaster* died, not the
/// session: ssh accepts first and resets afterwards when it is the remote
/// process that has gone. Verified both ways — a dead remote nvim gives
/// ECONNRESET, a dead master gives ECONNREFUSED.
fn describe_attach_failure(location: &transport::Location, e: &nvmux::NvmuxError) -> String {
    let remote = matches!(location, transport::Location::Ssh(_));
    let host = location.to_string();
    match e {
        nvmux::NvmuxError::Rpc(nvmux::error::RpcError::ConnectionRefused(_)) if remote => {
            format!("the connection to {host} dropped — press enter to retry")
        }
        nvmux::NvmuxError::Rpc(nvmux::error::RpcError::Reset) if remote => {
            format!("that session is no longer running on {host}")
        }
        nvmux::NvmuxError::Rpc(rpc) if rpc.is_definitely_dead() => "that session is gone".into(),
        // Reachable, and not answering: a `:!make` still running, a prompt
        // nvmux will not answer for the user, CPU-bound Lua. The attach probe
        // no longer has a budget to run out of — the user waits on the
        // attaching screen and gives up from there — so this arm is for the
        // error type's sake, and for whatever grows a budget next. The bare
        // error ("timed out after 3s") would read as if nvmux had lost the
        // session.
        nvmux::NvmuxError::Rpc(nvmux::error::RpcError::Timeout(after)) => {
            format!("that session is busy and did not answer within {after:?}")
        }
        other => other.one_line(),
    }
}

/// One failed reconnection attempt, as a line on the terminal.
///
/// The first one also says what is going on and how to stop it: the client has
/// just vanished from the screen, and a line that opens with an ssh error would
/// read as nvmux failing rather than as nvmux recovering. Ctrl-C is worth
/// naming because the wait is the one moment nvmux sits idle on the plain
/// terminal — and it is safe: the session is on the far side and keeps running,
/// which is the whole reason there is anything to reconnect to.
fn describe_retry(host: &str, retry: &reconnect::Retry<'_>) -> String {
    let mut lines = String::new();
    if retry.attempt == 1 {
        lines.push_str(&format!(
            "nvmux: the connection to {host} dropped — reconnecting \
             (Ctrl-C quits nvmux; the session keeps running)\n"
        ));
    }
    lines.push_str(&format!(
        "nvmux: {} — trying again in {}s ({} of {})",
        retry.error.one_line(),
        retry.wait.as_secs(),
        retry.attempt,
        retry.of
    ));
    lines
}

/// Why the session could not be got back: nvmux's last words, so they say
/// what was being done, what ssh said, and what is left — a session still
/// running, and the command that finds it. The ssh errors that get this far
/// all name the host themselves, so the first line does not.
fn describe_reconnect_failure(host: &str, e: &nvmux::NvmuxError) -> String {
    format!(
        "could not reconnect: {}\nhint: the session keeps running; `nvmux {host}` will find it",
        e.one_line()
    )
}

/// Explain why a switch could not find out what to switch to.
///
/// Unlike an attach this is a listing, and the errors it raises already name the
/// host where they have one (`ssh: the connection to myhost died`). What none of
/// them says is what nvmux was attempting — a script's refusal is rendered bare,
/// on purpose — and on the hint row there is nothing else to say it.
fn describe_listing_failure(e: &nvmux::NvmuxError) -> String {
    format!("could not list sessions: {}", e.one_line())
}

/// The session a `<prefix>` switch names, in a listing.
///
/// Exhaustive, no `_` arm, for the reason `pty::act` is: a new way to name a
/// session must not be able to arrive here and do nothing.
fn pick(
    sessions: &[nvmux::session::Session],
    target: pty::Target,
    from: u32,
) -> Option<&nvmux::session::Session> {
    match target {
        pty::Target::Number(num) => sessions.iter().find(|s| s.state.num == num),
        pty::Target::Step(dir) => transport::neighbour(sessions, from, dir),
        pty::Target::Session(id) => sessions.iter().find(|s| s.id == id.as_str()),
    }
}

/// Where `current` is among the sessions `<prefix> n` and `<prefix> p` step
/// through, for the card that says where a switch landed: the session each
/// goes to from it — with two, the other is the one `n` goes to — and, for a
/// step from `left`, which way it went and the session beyond `left`, which
/// the row lets go of as it moves. Among three sessions or more: with fewer,
/// a step comes back round to where it started and the row has nowhere to
/// move.
fn row_of(
    listing: &[nvmux::session::Session],
    current: &nvmux::session::Session,
    stepped: Option<(nvmux::keys::Direction, &nvmux::session::Session)>,
) -> nvmux::card::Row {
    use nvmux::keys::Direction;
    let beside = |s: &nvmux::session::Session, dir| {
        transport::neighbour(listing, s.state.num, dir).filter(|n| n.id != current.id)
    };
    let next = beside(current, Direction::Next);
    let prev = beside(current, Direction::Prev).filter(|p| Some(&p.id) != next.map(|n| &n.id));
    let step = match (stepped, prev, next) {
        (Some((dir, left)), Some(prev), Some(next)) => {
            let (from, away) = match dir {
                Direction::Next => (prev, Direction::Prev),
                Direction::Prev => (next, Direction::Next),
            };
            transport::neighbour(listing, from.state.num, away)
                .filter(|_| from.id == left.id)
                .map(|gone| nvmux::card::Step {
                    forward: dir == Direction::Next,
                    gone: announce::label(&gone.name),
                })
        }
        _ => None,
    };
    let name = |s: &nvmux::session::Session| announce::label(&s.name);
    nvmux::card::Row {
        prev: prev.map(name),
        next: next.map(name),
        step,
    }
}

/// A client that has been started, and the probe in flight for it.
///
/// The half of an attach that does not block, kept so it can be begun at one
/// moment and waited for at another — which is the whole point: a switch
/// begins one before the session it is leaving dissolves (see
/// [`pty::relay`]), so the fork and the round trip run underneath the fade
/// rather than after it.
///
/// Dropping one retires the client and stops the probe, so a `Begun` that is
/// never waited for costs nothing beyond the fork it already paid for.
struct Begun {
    /// What it was begun for. Carried because the caller resolves the switch
    /// target to start it, and must not resolve it a second time and disagree.
    session: nvmux::session::Session,
    sock: std::path::PathBuf,
    attachment: pty::Attachment,
    probe: pty::Probe,
}

/// Start a client for `session` and its probe, and hand both back without
/// waiting for either.
///
/// Returns the crate's own error type rather than `anyhow`, so the caller can
/// tell a dropped connection from a dead session and say the right thing.
fn begin_attachment(
    transport: &dyn transport::Transport,
    session: &nvmux::session::Session,
) -> nvmux::Result<Begun> {
    let sock = transport.local_socket_for(session)?;
    // Every spawn is a change of session — the loop above reuses the client
    // otherwise — so the notice is unconditional here and one-shot there.
    let notice = announce::label(&session.name);
    // The client first and the probe alongside it, for the overlap
    // `pty::spawn_client` explains. A probe that cannot start takes the client
    // down with it — explicitly, so it is gone, and its pty with it, before
    // the error goes anywhere.
    let attachment = pty::spawn_client(&session.id, &sock, &notice)?;
    let probe = match pty::Probe::start(&session.id, &sock) {
        Ok(probe) => probe,
        Err(e) => {
            drop(attachment);
            return Err(e);
        }
    };
    Ok(Begun {
        session: session.clone(),
        sock,
        attachment,
        probe,
    })
}

/// Start a client for every session there is and hand each to the pool, to be
/// held out of sight until its session is first visited: `[client] lazy =
/// false` (see [`pty::Attachment::start_ahead`]).
///
/// Begun as a switch begins one under its fade (see [`begin_attachment`]), and
/// not waited for: each probe is answered on its client's own thread, which
/// retires a client its probe refuses. So the picker waits for the listing,
/// for a fork per session and — over SSH — one forward for them all (see
/// [`transport::Transport::reach_all`]), and not for a single round trip more.
///
/// The listing is handed back for the picker to open on. Over SSH it is the
/// one the greeting carried, which the first listing takes (see
/// `SshTransport::first_listing`): the picker's own would be a second script
/// run over the link, for the same answer.
///
/// Nothing here is reported. A listing that fails is the picker's to report a
/// moment later, from the same call; a client that cannot be started leaves
/// its session to be attached on its first visit as if this had not run, where
/// a failure has a screen to land on and a message written for it.
fn start_every_client(
    transport: &dyn transport::Transport,
    pool: &mut pool::Pool,
) -> Option<Vec<nvmux::session::Session>> {
    let t_start = std::time::Instant::now();
    let listing = match transport.list_sessions() {
        Ok(listing) => listing,
        Err(e) => {
            tracing::debug!(error = %e, "could not list the sessions to start their clients");
            return None;
        }
    };
    transport.reach_all(&listing);
    for session in &listing {
        match begin_attachment(transport, session) {
            Ok(begun) => pool.start_ahead(begun.attachment, Some(begun.probe)),
            Err(e) => tracing::debug!(
                id = %session.id,
                error = %e,
                "could not start a client ahead"
            ),
        }
    }
    tracing::debug!(
        ms = t_start.elapsed().as_secs_f64() * 1000.0,
        sessions = listing.len(),
        "timing: clients started ahead"
    );
    Some(listing)
}

/// The longest a [`Revival`] waits for the session in front to have painted
/// before it starts the clients behind it anyway: one waiting for a key may
/// paint nothing until it has one.
const REVIVE_AFTER_AT_MOST: std::time::Duration = std::time::Duration::from_secs(3);

/// Clients started again behind the session in front, once that has painted,
/// with `[client] lazy` off: the ones a dropped link took with it, or every
/// session's after the settings changed the kind of client (see [`Again`]).
///
/// Every client nvmux holds for a session not in front, parked or started
/// ahead, rides the link to its session's server, and leaves with it. As the
/// link comes back the session in front is attached again, and without this
/// the rest are each attached afresh on their next visit: a fork, a forward,
/// a probe and a first paint, over a link that has just shown how far away
/// the host is. With it they are started ahead again, as [`start_every_client`]
/// starts them as nvmux starts, and held by the pool until those visits.
///
/// A change of the kind of client retires every client there is, of the kind
/// no longer named (see [`settle`]), and the session in front is attached
/// again as the new kind. Without this the rest would wait for their next
/// visits too, as if `lazy` were on; with it they are started again, as the
/// new kind, as nvmux's start started them as the old.
///
/// On a thread, because each is a fork, and their forwards an ssh command
/// besides, and the session in front would otherwise wait for them all to have
/// been made before the relay that brings it back began. And not before that
/// session has painted (see [`pty::Attachment::tell_when_painted`]), because
/// each client started has its session's whole screen sent to it over the link
/// the screen in front is coming back over.
///
/// The thread hands over each client as it is held; [`Revival::collect`] takes
/// them into the pool, where the next attach and the next switch look first.
/// Dropped, a revival starts no more, and lets go of what it had not handed
/// over.
struct Revival {
    held: mpsc::Receiver<pty::Parked>,
    stop: Arc<AtomicBool>,
    /// The kind of client they are started as, which the settings screen can
    /// change before they are all in: a client of the other kind is let go.
    ui: config::Ui,
}

impl Revival {
    /// Start again, behind `attachment`, the clients `again` says, for the
    /// sessions the listing still has, the one in front aside: those that
    /// have left the pool — after a dropped link, the clients it took — or
    /// every one the pool has no client for. `None` with none to start.
    fn begin(
        shared: &transport::Shared,
        pool: &mut pool::Pool,
        listing: &[nvmux::session::Session],
        front: &str,
        again: Again,
        attachment: &mut pty::Attachment,
    ) -> Option<Self> {
        let sessions = again.sessions(pool, listing, front);
        if sessions.is_empty() {
            return None;
        }
        tracing::debug!(
            sessions = sessions.len(),
            ?again,
            "starting clients again behind the session in front"
        );
        let painted = attachment.tell_when_painted();
        let (handing, held) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let transport = Arc::clone(shared);
        let spawned = std::thread::Builder::new()
            .name("nvmux-revive".into())
            .spawn(move || {
                // All their forwards in one, while the session in front
                // paints: a forward sends nothing over the link until a
                // client connects through it.
                transport.reach_all(&sessions);
                revive(&sessions, &painted, &stopped, &handing, |session| {
                    let Begun {
                        attachment, probe, ..
                    } = begin_attachment(transport.as_ref(), session)?;
                    Ok(attachment.start_ahead(Some(probe)))
                });
            });
        if let Err(e) = spawned {
            tracing::debug!(error = %e, "could not start the clients again");
            return None;
        }
        Some(Self {
            held,
            stop,
            ui: config::get().client.ui,
        })
    }

    /// Take into the pool every client held since last asked, and say whether
    /// more may come. Let go of instead: one for `front`, the session in
    /// front, attached afresh since this began; one for a session visited
    /// since, whose own client the pool keeps (see [`pool::Pool::adopt`]); and
    /// every one, once the settings screen has changed the kind of client.
    fn collect(&self, pool: &mut pool::Pool, front: Option<&str>) -> bool {
        let other_kind = config::get().client.ui != self.ui;
        if other_kind {
            self.stop.store(true, Ordering::Relaxed);
        }
        loop {
            let held = match self.held.try_recv() {
                Ok(held) => held,
                Err(mpsc::TryRecvError::Empty) => return !other_kind,
                Err(mpsc::TryRecvError::Disconnected) => return false,
            };
            let unwanted = other_kind || front == Some(held.session_id());
            if let Some(held) = if unwanted {
                Some(held)
            } else {
                pool.adopt(held)
            } {
                let_go(held);
            }
        }
    }
}

/// Every client held and not handed over is told to retire, and then let go
/// of: side by side, as the pool lets go of its own (see `pool::Pool`'s
/// `Drop`).
impl Drop for Revival {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let mut held: Vec<pty::Parked> = self.held.try_iter().collect();
        for one in &mut held {
            one.tell_to_retire();
        }
    }
}

/// Which clients a [`Revival`] starts again, behind the first attach since a
/// dropped link or since the settings changed the kind of client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Again {
    /// The ones a dropped link took, which have left the pool: over SSH,
    /// where there is a link to drop.
    Taken,
    /// Every session's the pool has no client for, as the kind the settings
    /// have just changed to: after that change, which retires every client
    /// there is, every session's. Those a link that dropped as well took are
    /// among them.
    Every,
}

impl Again {
    /// What the first attach since a dropped link (`dropped`), or since the
    /// settings changed the kind of client (`new_kind`), starts again behind
    /// it, as the client settings now say: nothing with neither, nor with
    /// `[client] lazy` on, which starts each client on its session's visit.
    fn after(
        dropped: bool,
        new_kind: bool,
        remote: bool,
        client: config::ClientSettings,
    ) -> Option<Self> {
        if client.lazy {
            None
        } else if new_kind {
            Some(Self::Every)
        } else if dropped && remote {
            Some(Self::Taken)
        } else {
            None
        }
    }

    /// The sessions of the listing to start a client for, `front` aside, in
    /// the order `<prefix> n` visits them from it. For the ones a link took,
    /// the pool lets go of every client that has left it, and names them.
    fn sessions(
        self,
        pool: &mut pool::Pool,
        listing: &[nvmux::session::Session],
        front: &str,
    ) -> Vec<nvmux::session::Session> {
        match self {
            Self::Taken => {
                let left = pool.take_left();
                in_visiting_order(listing, front, |id| left.iter().any(|l| l == id))
            }
            Self::Every => in_visiting_order(listing, front, |id| !pool.holds(id)),
        }
    }
}

/// The sessions the listing has that `wanted` picks by id, `front` aside, in
/// the order `<prefix> n` visits them from `front`: the likeliest next first.
fn in_visiting_order(
    listing: &[nvmux::session::Session],
    front: &str,
    wanted: impl Fn(&str) -> bool,
) -> Vec<nvmux::session::Session> {
    let after = listing
        .iter()
        .position(|s| s.id == front)
        .map_or(0, |at| at + 1);
    listing[after..]
        .iter()
        .chain(&listing[..after])
        .filter(|s| s.id != front && wanted(&s.id))
        .cloned()
        .collect()
}

/// What a [`Revival`]'s thread does, but for how a client is started: wait for
/// the session in front to have painted — or to have been let go of, or for
/// [`REVIVE_AFTER_AT_MOST`] — and then start one for each of `sessions` in
/// turn, and hand each over as it is held, until `stop` is set or nobody is
/// left to hand them to. One that cannot be started is left to its visit.
fn revive<T>(
    sessions: &[nvmux::session::Session],
    painted: &mpsc::Receiver<()>,
    stop: &AtomicBool,
    handing: &mpsc::Sender<T>,
    mut start: impl FnMut(&nvmux::session::Session) -> nvmux::Result<Option<T>>,
) {
    // Told, let go of, or waited out: any way, the screen in front is as far
    // along as it will get on its own.
    let _ = painted.recv_timeout(REVIVE_AFTER_AT_MOST);
    for session in sessions {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match start(session) {
            Ok(Some(held)) => {
                if handing.send(held).is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(e) => tracing::debug!(
                id = %session.id,
                error = %e,
                "could not start a client again"
            ),
        }
    }
}

/// Retire a client held out of sight without waiting for it here: letting go
/// of one waits for its thread to hand the client over, which a switch about
/// to fade has no time for.
fn let_go(held: pty::Parked) {
    // A thread that cannot be had takes the client down with it: dropped, and
    // so retired, here after all.
    let _ = std::thread::Builder::new()
        .name("nvmux-let-go".into())
        .spawn(move || drop(held));
}

/// Wait, on the attaching screen, for the session to take the client
/// [`begin_attachment`] started. `None` is the user giving up on that wait.
///
/// A probe that says no and a wait the user gives up on end the same way, by
/// dropping the attachment — explicitly, so the client is gone, and its pty
/// with it, before the picker says anything about it.
///
/// `hand_off` is the name the picker left standing: the attaching screen takes
/// it down, and drops it, if its spinner has to go up.
fn finish_attachment(
    begun: Begun,
    hand_off: &mut Option<HandOff>,
) -> nvmux::Result<Option<pty::Attachment>> {
    let Begun {
        session,
        sock,
        mut attachment,
        probe,
    } = begun;
    match ui::attaching::run(probe, &session.name, &sock, hand_off) {
        Ok(ui::attaching::Verdict::Ready) => Ok(Some(attachment)),
        // Attached like any other, and the client is already on its way — but
        // it will draw nothing until a key reaches the server, so the notice
        // is the one thing that can say why the screen is blank. It is nvmux's
        // own box rather than the editor's, which is what lets it go up on a
        // session that is not drawing at all.
        Ok(ui::attaching::Verdict::Blocked) => {
            attachment.note_waiting_for_a_key();
            Ok(Some(attachment))
        }
        Ok(ui::attaching::Verdict::Cancelled) => {
            drop(attachment);
            Ok(None)
        }
        Err(e) => {
            drop(attachment);
            Err(e)
        }
    }
}

/// The attachment to relay next: one already begun, waited out, or a whole
/// fresh one. The two halves are the same work either way; what differs is
/// whether the fork and the probe have had a dissolve to run underneath.
fn attach(
    transport: &dyn transport::Transport,
    session: &nvmux::session::Session,
    begun: Option<Begun>,
    hand_off: &mut Option<HandOff>,
) -> nvmux::Result<Option<pty::Attachment>> {
    match begun {
        Some(begun) => finish_attachment(begun, hand_off),
        None => finish_attachment(begin_attachment(transport, session)?, hand_off),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nvmux::error::{RpcError, SessionError, SshError};
    use nvmux::keys::Direction;
    use nvmux::NvmuxError;
    use transport::Location;

    /// The listing the picker handed over is what a `<prefix>` number resolves
    /// against, so the switch pays for no listing of its own.
    #[test]
    fn a_number_is_resolved_against_the_listing_in_hand() {
        let listing = listing_of(&[(1, "id000001"), (2, "id000002"), (3, "id000003")]);
        let picked = pick(&listing, pty::Target::Number(2), 1).expect("2 is on the list");
        assert_eq!(picked.id, "id000002");
    }

    /// `n` and `p` walk the same listing, from the number the user is sitting on.
    #[test]
    fn a_step_walks_the_listing_in_hand() {
        let listing = listing_of(&[(1, "id000001"), (2, "id000002"), (3, "id000003")]);
        let next = pick(&listing, pty::Target::Step(Direction::Next), 2).expect("3 follows 2");
        assert_eq!(next.id, "id000003");
        let prev = pick(&listing, pty::Target::Step(Direction::Prev), 2).expect("1 precedes 2");
        assert_eq!(prev.id, "id000001");
    }

    /// A number the listing does not have resolves to nothing rather than to
    /// something else — which is what sends the switch off to re-list before it
    /// says no, since a listing in hand cannot tell "no such session" from
    /// "created since I was taken".
    #[test]
    fn a_number_the_listing_does_not_have_resolves_to_nothing() {
        let listing = listing_of(&[(1, "id000001"), (2, "id000002")]);
        assert!(pick(&listing, pty::Target::Number(3), 1).is_none());
    }

    /// An empty listing names nothing, by either kind of key.
    #[test]
    fn an_empty_listing_names_nothing() {
        assert!(pick(&[], pty::Target::Number(1), 0).is_none());
        assert!(pick(&[], pty::Target::Step(Direction::Next), 0).is_none());
    }

    /// The card that says where a switch landed shows the session `p` goes
    /// to and the one `n` goes to, round the ends; with two, only the other,
    /// as the one `n` goes to; alone, neither.
    #[test]
    fn an_arrival_names_the_sessions_either_side() {
        let listing = listing_of(&[(1, "id000001"), (2, "id000002"), (3, "id000003")]);
        let row = row_of(&listing, &listing[0], None);
        assert_eq!(
            (row.prev.as_deref(), row.next.as_deref(), row.step),
            (Some("s3"), Some("s2"), None)
        );
        let two = &listing[..2];
        let row = row_of(two, &two[1], None);
        assert_eq!((row.prev, row.next.as_deref()), (None, Some("s1")));
        let row = row_of(&listing[..1], &listing[0], None);
        assert_eq!((row.prev, row.next), (None, None));
    }

    /// A step says which way it went, and the session beyond the one it left,
    /// which the row lets go of: forward round the end, and back.
    #[test]
    fn a_step_says_which_way_it_went() {
        let listing = listing_of(&[
            (1, "id000001"),
            (2, "id000002"),
            (3, "id000003"),
            (4, "id000004"),
        ]);
        let gone = |row: nvmux::card::Row| row.step.map(|s| (s.forward, s.gone));
        let forward = row_of(&listing, &listing[0], Some((Direction::Next, &listing[3])));
        assert_eq!(gone(forward), Some((true, "s3".to_string())));
        let back = row_of(&listing, &listing[1], Some((Direction::Prev, &listing[2])));
        assert_eq!(gone(back), Some((false, "s4".to_string())));
        let jumped = row_of(&listing, &listing[0], Some((Direction::Next, &listing[2])));
        assert_eq!(gone(jumped), None, "not a step from where it says it left");
        let two = &listing[..2];
        assert_eq!(
            gone(row_of(two, &two[1], Some((Direction::Next, &two[0])))),
            None
        );
    }

    /// A listing as the picker hands it over: resolved numbers and all.
    fn listing_of(rows: &[(u32, &str)]) -> Vec<nvmux::session::Session> {
        rows.iter()
            .map(|(num, id)| {
                let mut s =
                    nvmux::session::Session::new((*id).into(), format!("s{num}"), 100, *num);
                s.state.num = *num;
                s
            })
            .collect()
    }

    fn refused() -> NvmuxError {
        NvmuxError::Rpc(RpcError::ConnectionRefused("x.sock".into()))
    }

    /// Through a forward, "refused" is the master, not the session.
    #[test]
    fn a_refused_forward_blames_the_connection_not_the_session() {
        let msg = describe_attach_failure(&Location::Ssh("myhost".into()), &refused());
        assert!(msg.contains("myhost"), "{msg}");
        assert!(msg.contains("dropped"), "{msg}");
        assert!(msg.contains("retry"), "{msg}");
    }

    #[test]
    fn a_reset_forward_means_the_remote_session_is_gone() {
        let msg = describe_attach_failure(
            &Location::Ssh("myhost".into()),
            &NvmuxError::Rpc(RpcError::Reset),
        );
        assert!(msg.contains("no longer running on myhost"), "{msg}");
    }

    /// Locally the same errno means the session itself.
    #[test]
    fn a_refused_local_socket_means_the_session_is_gone() {
        let msg = describe_attach_failure(&Location::Local, &refused());
        assert_eq!(msg, "that session is gone");
    }

    /// The hint row is one row: any other error is flattened onto it.
    #[test]
    fn other_errors_are_flattened_to_one_line() {
        let e = NvmuxError::Session(SessionError::NotReady {
            name: "x".into(),
            timeout: std::time::Duration::from_secs(1),
            log: "x.log".into(),
            log_tail: "line one\nline two".into(),
        });
        let msg = describe_attach_failure(&Location::Local, &e);
        assert!(!msg.contains('\n'), "{msg:?}");
        assert!(
            msg.contains("line one") && msg.contains("line two"),
            "{msg:?}"
        );
    }

    /// The first line of a reconnection is the one that explains it; the ones
    /// after say only what happened and when the next try is.
    #[test]
    fn a_reconnection_is_explained_once_and_then_counted() {
        let e = NvmuxError::Ssh(SshError::Unreachable("myhost".into()));
        let first = describe_retry(
            "myhost",
            &reconnect::Retry {
                attempt: 1,
                of: 6,
                error: &e,
                wait: std::time::Duration::from_secs(1),
            },
        );
        assert!(first.contains("dropped"), "{first}");
        assert!(first.contains("Ctrl-C"), "{first}");
        assert!(first.contains("keeps running"), "{first}");
        assert!(first.contains("in 1s (1 of 6)"), "{first}");
        assert_eq!(first.lines().count(), 2);

        let later = describe_retry(
            "myhost",
            &reconnect::Retry {
                attempt: 3,
                of: 6,
                error: &e,
                wait: std::time::Duration::from_secs(4),
            },
        );
        assert!(!later.contains("dropped"), "{later}");
        assert!(later.contains("myhost is unreachable"), "{later}");
        assert!(later.contains("in 4s (3 of 6)"), "{later}");
        assert_eq!(later.lines().count(), 1);
        assert!(later.starts_with("nvmux: "), "every line names its author");
    }

    /// Giving up is nvmux's last message, so it has to carry the reason, the
    /// reassurance and the way back.
    #[test]
    fn giving_up_says_why_and_how_to_come_back() {
        let e = NvmuxError::Ssh(SshError::Unreachable("myhost".into()));
        let msg = describe_reconnect_failure("myhost", &e);
        assert!(msg.starts_with("could not reconnect: "), "{msg}");
        assert!(msg.contains("myhost is unreachable"), "{msg}");
        assert!(msg.contains("keeps running"), "{msg}");
        assert!(msg.contains("`nvmux myhost`"), "{msg}");
        assert_eq!(msg.lines().count(), 2, "the reason, then the hint");
    }

    /// A listing failure reaches the picker with no other context around it, so
    /// the message has to say what was being attempted as well as what failed.
    #[test]
    fn a_failed_listing_says_what_nvmux_was_doing() {
        let e = NvmuxError::Ssh(SshError::MasterDied("myhost".into()));
        let msg = describe_listing_failure(&e);
        assert!(msg.contains("list sessions"), "{msg}");
        assert!(msg.contains("myhost"), "{msg}");
    }

    /// A script's refusal is rendered bare on purpose, so on its own it reads as
    /// a statement about nothing in particular. The prefix is what anchors it.
    #[test]
    fn a_bare_script_failure_is_not_left_to_speak_for_itself() {
        let e = NvmuxError::Session(SessionError::ScriptFailed(
            "runtime directory /tmp/nvmux-1000 is not owned by us".into(),
        ));
        let msg = describe_listing_failure(&e);
        assert!(msg.contains("could not list sessions"), "{msg}");
        assert!(msg.contains("is not owned by us"), "{msg}");
    }

    fn ids(sessions: Vec<nvmux::session::Session>) -> Vec<String> {
        sessions.into_iter().map(|s| s.id).collect()
    }

    /// The clients a dropped link took are started again in the order
    /// `<prefix> n` visits their sessions from the one in front, round the
    /// end; none for the one in front, nor for a session the listing no
    /// longer has.
    #[test]
    fn the_clients_a_link_took_are_started_again_in_the_order_n_visits_them() {
        let listing = listing_of(&[
            (1, "id000001"),
            (2, "id000002"),
            (3, "id000003"),
            (4, "id000004"),
        ]);
        let left = ["id000001", "id000003", "id000004", "id000009"].map(String::from);
        let took = |id: &str| left.iter().any(|l| l == id);
        assert_eq!(
            ids(in_visiting_order(&listing, "id000002", took)),
            ["id000003", "id000004", "id000001"]
        );
        assert_eq!(
            ids(in_visiting_order(&listing, "id000003", took)),
            ["id000004", "id000001"],
            "the one in front was started again"
        );
    }

    /// After a change of the kind of client the pool holds none, and every
    /// session's is started again, in the order `<prefix> n` visits them from
    /// the one in front, round the end, but the one in front's; where a link
    /// that dropped would start none, having taken none.
    #[test]
    fn every_client_but_the_one_in_front_is_started_again_as_the_new_kind() {
        let listing = listing_of(&[
            (1, "id000001"),
            (2, "id000002"),
            (3, "id000003"),
            (4, "id000004"),
        ]);
        let mut pool = pool::Pool::new(true);
        assert_eq!(
            ids(Again::Every.sessions(&mut pool, &listing, "id000003")),
            ["id000004", "id000001", "id000002"]
        );
        assert!(Again::Taken
            .sessions(&mut pool, &listing, "id000003")
            .is_empty());
    }

    /// With `lazy` off, a change of the kind of client starts every session's
    /// again, wherever the sessions are and whether or not the link dropped
    /// as well; a dropped link alone starts the ones it took, over SSH; and
    /// nothing else starts anything. With `lazy` on, nothing does.
    #[test]
    fn clients_are_started_again_unless_lazy() {
        let client = |lazy| config::ClientSettings {
            lazy,
            ..config::ClientSettings::default()
        };
        for remote in [false, true] {
            for dropped in [false, true] {
                assert_eq!(
                    Again::after(dropped, true, remote, client(false)),
                    Some(Again::Every),
                    "remote {remote}, dropped {dropped}"
                );
                for new_kind in [false, true] {
                    assert_eq!(
                        Again::after(dropped, new_kind, remote, client(true)),
                        None,
                        "lazy, remote {remote}, dropped {dropped}, new kind {new_kind}"
                    );
                }
            }
        }
        assert_eq!(
            Again::after(true, false, true, client(false)),
            Some(Again::Taken)
        );
        assert_eq!(
            Again::after(true, false, false, client(false)),
            None,
            "no link to drop"
        );
        assert_eq!(Again::after(false, false, true, client(false)), None);
        assert_eq!(Again::after(false, false, false, client(false)), None);
    }

    /// Settling says when the kind of client changed, and only once: what it
    /// settles to is what it compares with next. Whether clients are kept is
    /// no change of kind.
    #[test]
    fn settling_says_when_the_kind_of_client_changed() {
        let running = config::get().client;
        let other = config::Ui::ALL
            .into_iter()
            .find(|ui| *ui != running.ui)
            .expect("two kinds of client");
        let mut attached = None;
        let mut pool = pool::Pool::new(running.per_session);
        let mut was = config::ClientSettings {
            ui: other,
            ..running
        };
        assert!(settle(&mut was, &mut attached, &mut pool), "a new kind");
        assert_eq!(was, running);
        assert!(!settle(&mut was, &mut attached, &mut pool), "settled twice");
        was.per_session = !was.per_session;
        assert!(
            !settle(&mut was, &mut attached, &mut pool),
            "clients kept or not"
        );
    }

    /// Nothing is started again before the session in front has painted, and
    /// then each in turn, handed over as it is held.
    #[test]
    fn nothing_is_started_again_before_the_session_in_front_has_painted() {
        let sessions = listing_of(&[(1, "id000001"), (2, "id000002"), (3, "id000003")]);
        let (told, painted) = mpsc::channel();
        let (handing, handed) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            revive(
                &sessions,
                &painted,
                &AtomicBool::new(false),
                &handing,
                |s| Ok(Some(s.id.clone())),
            );
        });
        assert!(
            handed
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "a client was started before the session in front had painted"
        );
        told.send(()).expect("still waiting to be told");
        assert_eq!(
            handed.iter().collect::<Vec<_>>(),
            ["id000001", "id000002", "id000003"]
        );
        worker.join().expect("the revival panicked");
    }

    /// A session in front whose relay ended before it painted holds nothing
    /// back: there is no paint coming to wait for.
    #[test]
    fn a_session_in_front_let_go_of_unpainted_holds_nothing_back() {
        let sessions = listing_of(&[(1, "id000001")]);
        let (told, painted) = mpsc::channel::<()>();
        drop(told);
        let (handing, handed) = mpsc::channel();
        let start = std::time::Instant::now();
        revive(
            &sessions,
            &painted,
            &AtomicBool::new(false),
            &handing,
            |s| Ok(Some(s.id.clone())),
        );
        assert!(
            start.elapsed() < REVIVE_AFTER_AT_MOST / 2,
            "waited for a paint that could not come"
        );
        assert_eq!(handed.try_recv().as_deref(), Ok("id000001"));
    }

    /// Told to stop, a revival starts nothing; with nobody left to hand its
    /// clients to, it stops at the first it could not hand over; and one it
    /// cannot start is left to its visit, and the next started.
    #[test]
    fn a_revival_stops_when_told_or_unheard_and_passes_over_a_failure() {
        let sessions = listing_of(&[(1, "id000001"), (2, "id000002"), (3, "id000003")]);
        let (told, painted) = mpsc::channel::<()>();
        drop(told);

        let (handing, handed) = mpsc::channel::<String>();
        let mut starts = 0;
        revive(&sessions, &painted, &AtomicBool::new(true), &handing, |s| {
            starts += 1;
            Ok(Some(s.id.clone()))
        });
        assert_eq!(starts, 0, "started a client after it was told to stop");
        assert!(handed.try_recv().is_err());

        let (handing, handed) = mpsc::channel::<String>();
        drop(handed);
        let mut starts = 0;
        revive(
            &sessions,
            &painted,
            &AtomicBool::new(false),
            &handing,
            |s| {
                starts += 1;
                Ok(Some(s.id.clone()))
            },
        );
        assert_eq!(starts, 1, "went on starting clients nobody would take");

        let (handing, handed) = mpsc::channel::<String>();
        revive(
            &sessions,
            &painted,
            &AtomicBool::new(false),
            &handing,
            |s| {
                if s.id == "id000002" {
                    Err(refused())
                } else {
                    Ok(Some(s.id.clone()))
                }
            },
        );
        drop(handing);
        assert_eq!(handed.iter().collect::<Vec<_>>(), ["id000001", "id000003"]);
    }
}
