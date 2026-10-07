//! Binary entry point.

use anyhow::{Context, Result};
use clap::Parser;

use nvmux::cli::Cli;
use nvmux::handoff::HandOff;
use nvmux::{
    announce, config, fade, logging, nested, nvim, palette, paths, pool, pty, reconnect, transport,
    ui,
};

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
    // during an attached session — hands back a usable terminal.
    nvmux::term::install_signal_safety_net();

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
    session_loop(transport.as_ref())
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
                        eprintln!("nvmux: could not write {}: {e}", path.display());
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
/// so a first visit takes one out of it as a switch back does.
fn session_loop(transport: &dyn transport::Transport) -> Result<()> {
    let mut attached: Option<pty::Attachment> = None;
    // Empty, and handing every client straight back, unless clients are kept
    // one per session or started ahead. Dropped on every way out of here,
    // which retires what it holds.
    let mut pool = pool::Pool::configured();
    if !config::get().client.lazy {
        start_every_client(transport, &mut pool);
    }
    let mut message: Option<String> = None;
    // The session the last trip through the picker led to, so the next one
    // opens with the cursor on it rather than on the first row.
    let mut focus: Option<String> = None;
    // The session the last relay showed, so that a kept client coming back in
    // place of another session's says where the user has landed, as a fresh
    // client does.
    let mut shown: Option<String> = None;

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
        let (mut current, mut listing, mut hand_off) = match ui::run(
            transport,
            message.take(),
            focus.as_deref(),
            attached.is_some() || focus.as_deref().is_some_and(|id| pool.holds(id)),
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
            } => (session, sessions, hand_off),
        };
        // A session the listing no longer has has nothing to come back to.
        pool.keep_only(&listing);
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
            // Out of the picker the user chose the session by its name, and
            // saw it — carried across, where the hand-off is on — so the
            // notice has nothing to add but a key the session is waiting for.
            if from_picker {
                attachment.arrive_unannounced();
            }
            from_picker = false;
            shown = Some(current.id.clone());

            // The relay, with the chance to start the next client while this
            // session is still dissolving: `begin_next` fires on a `<prefix>`
            // switch, before the fade, and what it leaves in `begun` is waited
            // for at the top of this loop. Scoped, so the closure's borrows
            // end before the match below — which assigns to the very things it
            // reads.
            let ended = {
                let mut begin_next = |target: pty::Target| {
                    // Resolved here, and not again below: the session started
                    // for is the session switched to, or the two could differ.
                    // A target that names nothing is left to the path below,
                    // which can re-list for it; that is a round trip, and this
                    // is not the place to spend one.
                    let Some(session) = pick(&listing, target, current.state.num) else {
                        return;
                    };
                    // A switch onto the session already attached reuses the
                    // client in hand. Starting a second one for it would put a
                    // second UI on that server — which shrinks its screen to
                    // the smaller of the two — for a client that would then be
                    // dropped unused.
                    if session.id == current.id {
                        return;
                    }
                    // Nor for one with a client kept for it, which has nothing
                    // to start: the switch resumes that one. Nor for one whose
                    // kept client has left, which may have left with the link:
                    // the switch recovers that first, and a start here would
                    // go over it before then (see `resume_or_attach`).
                    if pool.has(&session.id) {
                        return;
                    }
                    match begin_attachment(transport, session) {
                        Ok(b) => begun = Some(b),
                        // Not reported: the path below attempts the same
                        // attach a moment later, where a failure has a screen
                        // to land on and a message written for it.
                        Err(e) => tracing::debug!(
                            id = %session.id,
                            error = %e,
                            "could not start the next client early"
                        ),
                    }
                };
                pty::relay(attachment, highest, &mut begin_next)?
            };
            match ended {
                (pty::Outcome::ToPicker, held) => {
                    attached = pool.set_aside(held);
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
                        reconnect::Verdict::Unneeded => break,
                        reconnect::Verdict::Restored => continue,
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
                (pty::Outcome::ShowHelp, held) => {
                    attached = pool.set_aside(held);
                    ui::help::run()?;
                    continue;
                }
                (pty::Outcome::Switch(target), held) => {
                    // Held rather than killed, so a number that names nothing —
                    // or a cycle with nowhere to go — puts the user straight
                    // back where they were.
                    attached = pool.set_aside(held);
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
fn resume_or_attach(
    transport: &dyn transport::Transport,
    pool: &mut pool::Pool,
    session: &nvmux::session::Session,
    begun: Option<Begun>,
    warm: &mut bool,
    hand_off: &mut Option<HandOff>,
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
            attach(transport, session, begun, hand_off)
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
/// and for a fork and — over SSH — a forward per session, and not for a
/// single round trip more.
///
/// Nothing here is reported. A listing that fails is the picker's to report a
/// moment later, from the same call; a client that cannot be started leaves
/// its session to be attached on its first visit as if this had not run, where
/// a failure has a screen to land on and a message written for it.
fn start_every_client(transport: &dyn transport::Transport, pool: &mut pool::Pool) {
    let t_start = std::time::Instant::now();
    let listing = match transport.list_sessions() {
        Ok(listing) => listing,
        Err(e) => {
            tracing::debug!(error = %e, "could not list the sessions to start their clients");
            return;
        }
    };
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
}
