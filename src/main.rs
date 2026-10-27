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

/// How long exiting waits for the left session's notes to be cleared: enough
/// for an ssh round trip, short enough that a detach feels instant.
const SEEN_ON_THE_WAY_OUT: std::time::Duration = std::time::Duration::from_millis(300);

fn main() -> Result<()> {
    let cli = Cli::parse();

    let dir = paths::ensure_runtime_dir().context("preparing the nvmux runtime directory")?;
    logging::init(&dir)?;

    // nvmux's own client (see `nvmux::client`). Errors are logged, not
    // printed: its output is drawn.
    if let Some(sock) = &cli.client {
        if let Err(e) = nvmux::client::run(sock) {
            tracing::warn!(error = %format!("{e:#}"), "client: ended on an error");
            std::process::exit(1);
        }
        return Ok(());
    }

    // Before anything is spawned or written (see `paths::restrict_umask`).
    paths::restrict_umask();

    // Before any screen is drawn, so a `kill` or panic in the picker also
    // restores the terminal.
    nvmux::term::install_signal_safety_net();
    nvmux::panics::install();

    if let Err(e) = run(&cli) {
        eprintln!("nvmux: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

fn run(cli: &Cli) -> Result<()> {
    // First, so a nested nvmux fails before doing anything.
    nested::check()?;

    let location = cli.location();
    let remote = matches!(location, transport::Location::Ssh(_));
    // Before any client starts: clients resolve "auto" settings from it.
    config::set_remote(remote);

    // Checked up front rather than surfacing as a connection failure. Only
    // `nvmux <host>` with nvmux's own client needs no local nvim; the config
    // decides that below.
    if !remote {
        check_local_nvim()?;
    }

    // Config is local, so it comes before any transport.
    config::init(establish_settings()?);
    if remote && config::get().client.ui == config::Ui::Nvim {
        check_local_nvim()?;
    }

    // After the config, so users who need no colours skip the query and the
    // keystroke it can cost (see `palette::query`). nvmux's own client cannot
    // ask itself without the answers racing the keys (see `nvmux::client`).
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

/// Decide this run's settings, prompting once on a true first run: no config
/// file at the default path, `$NVMUX_CONFIG` unset, and stdin and stdout both
/// terminals. Failing to write the chosen config is reported but not fatal:
/// the prefix applies this run and the next run asks again.
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
/// The held attachment (or, with `[client] per_session`, the [`pool::Pool`])
/// lets `<prefix> Space`, `c`, `?` and switches back resume a client. With
/// `[client] lazy = false` the pool starts with a client per session (see
/// [`start_every_client`]). A front session's notes are cleared on arrival and
/// departure (see [`nvmux::notes`]).
fn session_loop(shared: &transport::Shared) -> Result<()> {
    let transport: &dyn transport::Transport = shared.as_ref();
    let mut attached: Option<pty::Attachment> = None;
    // Dropped on every exit, retiring what it holds.
    let mut pool = pool::Pool::configured();
    let mut opening = if config::get().client.lazy {
        ui::Listing::Ask
    } else {
        start_every_client(transport, &mut pool).map_or(ui::Listing::Ask, ui::Listing::Fresh)
    };
    // Settings the current clients were started with (see `settle`).
    let mut clients = config::get().client;
    let remote = matches!(transport.location(), transport::Location::Ssh(_));
    // For the picker, which lists, kills and renumbers as it floats.
    ui::picker::lend(shared);
    let mut message: Option<String> = None;
    // Where the next picker puts its cursor.
    let mut focus: Option<String> = None;
    // The last session relayed, to tell whether a resumed client announces.
    let mut shown: Option<String> = None;
    // The session under the picker, whose notes it clears before reading.
    let mut looked_at: Option<String> = None;
    // Posts other sessions' reports as cards (see `nvmux::tell`).
    let notifier = nvmux::tell::Notifier::start();
    // Clients restarting behind the front session, and whether the next
    // attach follows a dropped link or a client-kind change (see `Again`).
    let mut revival: Option<Revival> = None;
    let mut after_a_drop = false;
    let mut new_kind = false;

    loop {
        // `Esc` returns to a held or parked client; with none it does nothing.
        // `hand_off` is the name the picker leaves on screen, cleared by
        // whoever ends the attach first (see `nvmux::handoff`).
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
                if let Some(a) = attached.take() {
                    a.terminate();
                }
                // The client's exit output was discarded, so its modes are
                // still set.
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
        pool.keep_only(&listing);
        // The picker may have had the settings changed.
        new_kind |= settle(&mut clients, &mut attached, &mut pool);
        let mut highest = ui::highest_num(&listing);
        // Started under a switch's dissolve; never outlives the trip.
        let mut begun: Option<Begun> = None;
        // The client a floating picker switched to in place, on screen already
        // (see `pty::Others::hand_over`); relayed next.
        let mut chosen: Option<pty::Attachment> = None;
        let mut from_picker = true;
        // A `<prefix> n`/`p` switch's direction and origin, for the card.
        let mut stepped: Option<(nvmux::keys::Direction, nvmux::session::Session)> = None;
        // The row to reopen the floating picker on, after a prompt that went
        // nowhere (`r`, or `c` cancelled).
        let mut float_picker: Option<String> = None;

        loop {
            // `early`: the fork and probe ran under the outgoing dissolve.
            let t_open = std::time::Instant::now();
            // One begun for another session is dropped, retiring it.
            let started = begun.take().filter(|b| b.session.id == current.id);
            let early = started.is_some();
            let mut warm = false;
            if revival
                .as_ref()
                .is_some_and(|r| !r.collect(&mut pool, None))
            {
                revival = None;
            }
            // The client a floating picker switched to in place, its screen up
            // already; one for any other session goes back.
            let lit = chosen.take().and_then(|c| {
                if c.session_id == current.id && hand_off.is_none() {
                    pool.took(&c.session_id);
                    Some(c)
                } else {
                    pool.give_back(c);
                    None
                }
            });
            let opened = match (lit, attached.take()) {
                // As `resume_or_attach` would take it from the pool. A held
                // client is another session's, not kept: retired.
                (Some(mut lit), other) => {
                    if lit.started_ahead() {
                        lit.announce_on_arrival(announce::label(&current.name));
                    } else {
                        warm = true;
                    }
                    drop(other);
                    Ok(Some(lit))
                }
                (None, Some(a)) if a.session_id == current.id => Ok(Some(a)),
                // Retiring a client leaves its headless server running.
                // Hung up first and reaped after the new spawn, so the two
                // run in parallel. Nothing reads the old pty again, so its
                // restore sequence never reaches the terminal.
                (None, Some(mut other)) => {
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
                    // The reap the hangup deferred: explicit, so it stays
                    // after the spawn, a failed attach included.
                    drop(other);
                    spawned
                }
                (None, None) => resume_or_attach(
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

            // No session will clear the name, so clear it here.
            if !matches!(opened, Ok(Some(_))) {
                if let Some(h) = hand_off.take() {
                    h.take_down();
                }
            }
            // A failed attach returns to the picker with the reason shown.
            let mut attachment = match opened {
                Ok(Some(a)) => a,
                // The user gave up waiting: no message, and no client held.
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
            // A fresh client announces itself; a kept one only if it is not
            // the session just shown.
            if warm && shown.as_deref() != Some(current.id.as_str()) {
                attachment.announce_on_arrival(announce::label(&current.name));
            }
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

            // After a dropped link or a client-kind change, restart the other
            // clients behind this one (see `Again`). A running revival is
            // collected first so its clients are included.
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

            // On a thread, so the session is in front at once.
            let sock = attachment.sock().to_path_buf();
            nvmux::notes::later(sock.clone(), nvmux::notes::touch);
            notifier.watch(&current.id, &current.name, sock.clone());
            notifier.front(Some(&current.id));

            // What a floating picker opens on; its changes are taken back
            // below.
            ui::picker::hand(&listing, float_picker.as_deref());
            if std::mem::take(&mut float_settings) {
                attachment.float_the_settings();
            }
            if float_picker.take().is_some() {
                attachment.float_the_picker();
            }

            // Scoped so its borrows end before the match.
            let ended = {
                let mut neighbours = Neighbours {
                    transport,
                    pool: &mut pool,
                    revival: &revival,
                    current: &current,
                    begun: &mut begun,
                    chosen: &mut chosen,
                };
                pty::relay(attachment, highest, &mut neighbours)?
            };
            // The rows as a floating picker or prompt left them.
            if let Some(rows) = ui::picker::take_changed() {
                listing = rows;
                highest = ui::highest_num(&listing);
                pool.keep_only(&listing);
                if let Some(s) = listing.iter().find(|s| s.id == current.id) {
                    current = s.clone();
                }
            }
            notifier.front(None);
            // Its notes were seen: cleared by the picker, on a thread, or on
            // exit, briefly waited for.
            match &ended.0 {
                pty::Outcome::ToPicker => looked_at = Some(current.id.clone()),
                pty::Outcome::Detached | pty::Outcome::StdinClosed => {
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
                    // Opens on the rows in hand and re-lists behind them, so
                    // over ssh the first frame does not wait for the host.
                    opening = ui::Listing::in_hand(std::mem::take(&mut listing), shared);
                    break;
                }
                (pty::Outcome::Detached | pty::Outcome::StdinClosed, _) => return Ok(()),
                // The session ended, or the link dropped under the client
                // (see `reconnect`). Progress goes to stderr on the plain
                // terminal, which ssh may need for a passphrase.
                (pty::Outcome::ChildExited, _) => {
                    let host = transport.location().to_string();
                    let verdict = reconnect::recover(
                        transport,
                        |retry| eprintln!("{}", describe_retry(&host, retry)),
                        std::thread::sleep,
                    );
                    match verdict {
                        // A client ended by `:detach` leaves its session
                        // running; the re-listing brings it back.
                        reconnect::Verdict::Unneeded => {
                            listing.retain(|s| s.id != current.id);
                            opening = ui::Listing::in_hand(std::mem::take(&mut listing), shared);
                            break;
                        }
                        reconnect::Verdict::Restored => {
                            after_a_drop = true;
                            continue;
                        }
                        // Exit, not the picker: its first listing would use
                        // the failed link with an unbounded connect.
                        reconnect::Verdict::GaveUp(e) => {
                            tracing::warn!(%host, error = %e, "could not reconnect");
                            anyhow::bail!("{}", describe_reconnect_failure(&host, &e));
                        }
                    }
                }
                (pty::Outcome::CreateNew, held) => {
                    // Held, not killed, so a cancelled prompt resumes it.
                    attached = pool.set_aside(held);
                    if let ui::prompt::Outcome::Created(session) = ui::prompt::run(transport)? {
                        // `highest` decides how long a digit waits.
                        highest = highest.max(session.state.num);
                        listing.push(session.clone());
                        current = session;
                    }
                    continue;
                }
                // `c` on the floating picker when the prompt cannot float: it
                // runs full screen; if cancelled, the picker floats again.
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
                // `r` likewise; the picker floats again on the renamed row.
                (pty::Outcome::Rename(id), held) => {
                    attached = pool.set_aside(held);
                    new_kind |= settle(&mut clients, &mut attached, &mut pool);
                    let named = listing.iter().find(|s| s.id == id.as_str()).cloned();
                    if let Some(session) = named {
                        if let ui::prompt::Outcome::Renamed(name) =
                            ui::prompt::rename(transport, &session)?
                        {
                            // In place: a re-list would be an ssh round trip
                            // on a blank screen.
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
                    float_settings = left == ui::settings::Outcome::Float;
                    continue;
                }
                (pty::Outcome::Resettle, held) => {
                    attached = pool.set_aside(held);
                    new_kind |= settle(&mut clients, &mut attached, &mut pool);
                    continue;
                }
                (pty::Outcome::Switch(target), mut held) => {
                    // The landing card stays up for the next attach.
                    if let Some(h) = held.as_mut().and_then(pty::Attachment::take_left_standing) {
                        hand_off = Some(h);
                    }
                    // Held, so a switch to nowhere returns the user.
                    attached = pool.set_aside(held);
                    let changed = settle(&mut clients, &mut attached, &mut pool);
                    new_kind |= changed;
                    // Of the kind just retired: the attach starts afresh.
                    if let Some(old) = chosen.take_if(|_| changed) {
                        pool.took(&old.session_id);
                    }
                    stepped = match target {
                        pty::Target::Step(dir) => Some((dir, current.clone())),
                        pty::Target::Number(_) | pty::Target::Session(_) => None,
                    };
                    if let Some(b) = &begun {
                        current = b.session.clone();
                        continue;
                    }
                    // The listing in hand: re-listing here cost 88-110 ms of
                    // a ~215 ms switch. A session since gone fails its probe
                    // onto the picker, which re-lists.
                    let mut picked = pick(&listing, target, current.state.num).cloned();
                    if picked.is_none() {
                        // A session created since may be missing, so a miss
                        // re-lists before saying no. A failure goes to the
                        // picker: the screen here is blank.
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
                        Some(session) => current = session,
                        None => tracing::debug!(?target, "nothing to switch to"),
                    }
                    continue;
                }
            }
        }

        // Handed over to a switch that ended on the picker instead.
        if let Some(lit) = chosen.take() {
            pool.give_back(lit);
        }
        // Every exit from the relay loop was showing `current`: focus it.
        focus = Some(current.id.clone());
    }
    Ok(())
}

/// Apply settings changes clients cannot follow by being told (see
/// `pty::Attachment::open_desk`); returns whether `[client] ui` changed, which
/// retires every client so the new kind is started.
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

/// The attachment to relay next: from the pool if it has one, else a fresh one
/// (see [`attach`]).
///
/// The outer `Result` is giving up on the link; the inner one an attach
/// failure for the picker. `warm` is set for a resumed kept client (not one
/// started ahead, which is new to the terminal). `after_a_drop` is set when
/// the kept client had exited but the session remains, so it went with the
/// link (see [`Revival`]).
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
        // An unanswered probe is waited out on the attaching screen.
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
        // It may have gone with the link: recover that (bounded) before the
        // fresh attach, whose connect is unbounded. `hand_off` comes down
        // first, since the recovery prints on the plain terminal.
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

/// Restore the link, if it dropped, before a parked client that exited is
/// replaced: the recovery a front client's exit gets (see `session_loop`).
///
/// Nothing has reset the last client's keyboard mode here, so it is reset
/// before the line naming Ctrl-C: with xterm's `modifyOtherKeys` on, Ctrl-C
/// arrives as an escape sequence, not an interrupt.
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

/// Through an SSH forward, ECONNREFUSED means the ControlMaster died, not the
/// session: a dead remote nvim gives ECONNRESET (ssh accepts, then resets).
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
        // Reachable but busy; the bare error would read as a lost session.
        nvmux::NvmuxError::Rpc(nvmux::error::RpcError::Timeout(after)) => {
            format!("that session is busy and did not answer within {after:?}")
        }
        other => other.one_line(),
    }
}

/// The first attempt adds a line saying what is happening: a bare ssh error
/// would read as failure, not recovery.
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

/// nvmux's last words. The ssh errors here already name the host.
fn describe_reconnect_failure(host: &str, e: &nvmux::NvmuxError) -> String {
    format!(
        "could not reconnect: {}\nhint: the session keeps running; `nvmux {host}` will find it",
        e.one_line()
    )
}

/// Listing errors do not say what nvmux was doing (a script's refusal is shown
/// bare), so this does.
fn describe_listing_failure(e: &nvmux::NvmuxError) -> String {
    format!("could not list sessions: {}", e.one_line())
}

/// No `_` arm: a new kind of target must not silently do nothing.
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

/// [`pick`], stepping from `current`'s number in `sessions`, which a move in a
/// floating picker may have changed since `current` was taken.
fn pick_from<'a>(
    sessions: &'a [nvmux::session::Session],
    target: pty::Target,
    current: &nvmux::session::Session,
) -> Option<&'a nvmux::session::Session> {
    let from = sessions
        .iter()
        .find(|s| s.id == current.id)
        .map_or(current.state.num, |s| s.state.num);
    pick(sessions, target, from)
}

/// Where `current` sits in the `<prefix> n`/`p` order, for the landing card.
/// For a step from `left`, also the session beyond `left`, which the row drops
/// as it moves (three or more sessions only).
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

/// A started client and its in-flight probe: the non-blocking half of an
/// attach, so a switch can run it under the fade. Dropping it retires the
/// client.
struct Begun {
    /// The switch uses this rather than resolving its target again, which
    /// could give another session.
    session: nvmux::session::Session,
    sock: std::path::PathBuf,
    attachment: pty::Attachment,
    probe: pty::Probe,
}

/// Returns the crate's error type so the caller can tell a dropped connection
/// from a dead session.
fn begin_attachment(
    transport: &dyn transport::Transport,
    session: &nvmux::session::Session,
) -> nvmux::Result<Begun> {
    let sock = transport.local_socket_for(session)?;
    let notice = announce::label(&session.name);
    // Probe alongside the client (see `pty::spawn_client`).
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

/// The other sessions as a relay reaches them (see `pty::Others`): a switch's
/// next client begun under the fade, and kept clients lent to the floating
/// picker.
struct Neighbours<'a> {
    transport: &'a dyn transport::Transport,
    pool: &'a mut pool::Pool,
    revival: &'a Option<Revival>,
    current: &'a nvmux::session::Session,
    begun: &'a mut Option<Begun>,
    chosen: &'a mut Option<pty::Attachment>,
}

impl Neighbours<'_> {
    /// Revived clients count as kept.
    fn collect_revived(&mut self) {
        if let Some(r) = self.revival {
            r.collect(self.pool, Some(&self.current.id));
        }
    }
}

impl pty::Others for Neighbours<'_> {
    fn begin_next(&mut self, target: pty::Target) -> Option<nvmux::card::Card> {
        // A target naming nothing is left to the session loop, which can
        // afford a re-list. Resolved among the rows as they stand: a floating
        // picker or prompt may have changed them since, and `listing` takes
        // that in only after the relay.
        let rows = ui::picker::rows();
        let current = self.current;
        let session = pick_from(&rows, target, current)?;
        // A second client on the same server would shrink its screen to the
        // smaller UI, only to be dropped.
        if session.id == current.id {
            return None;
        }
        let step = match target {
            pty::Target::Step(dir) => Some((dir, current)),
            pty::Target::Number(_) | pty::Target::Session(_) => None,
        };
        let landing = nvmux::card::Card::arrival(
            nvmux::card::next_id(),
            &announce::label(&session.name),
            false,
        )
        .in_row(row_of(&rows, session, step));
        // Not for a session with a kept client, live or exited: an exited one
        // may need the link recovered first (see `resume_or_attach`).
        self.collect_revived();
        if !self.pool.has(&session.id) {
            match begin_attachment(self.transport, session) {
                Ok(b) => *self.begun = Some(b),
                // Not reported: the session loop retries.
                Err(e) => tracing::debug!(
                    id = %session.id,
                    error = %e,
                    "could not start the next client early"
                ),
            }
        }
        Some(landing)
    }

    fn lend(&mut self, session_id: &str) -> Option<pty::Attachment> {
        self.collect_revived();
        self.pool.lend(session_id)
    }

    fn give_back(&mut self, client: pty::Attachment) {
        self.pool.give_back(client);
    }

    fn hand_over(&mut self, client: pty::Attachment) {
        *self.chosen = Some(client);
    }
}

/// Start a client for every session and hold each in the pool until its first
/// visit (`[client] lazy = false`). Probes are not waited for: each is answered
/// on its client's thread. Returns the listing for the picker to open on,
/// saving a second listing.
///
/// Nothing is reported: the picker reports a failed listing, and a client that
/// cannot start leaves its session to a normal first attach.
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

/// Longest a [`Revival`] waits for the front session to paint: one waiting
/// for a key may not paint until it gets one.
const REVIVE_AFTER_AT_MOST: std::time::Duration = std::time::Duration::from_secs(3);

/// Clients restarted behind the front session, with `[client] lazy` off:
/// those a dropped link took, or every session's after a client-kind change
/// (see [`Again`]).
///
/// On a thread, so the forks do not delay the front session's relay, and only
/// after it paints, since each new client is sent its session's whole screen
/// over the same link.
struct Revival {
    held: mpsc::Receiver<pty::Parked>,
    stop: Arc<AtomicBool>,
    /// Clients of a kind changed meanwhile are released.
    ui: config::Ui,
}

impl Revival {
    /// `None` with nothing to start.
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
                // Before the paint wait: a forward sends nothing until a
                // client connects.
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

    /// Move clients held since last asked into the pool; returns whether more
    /// may come. Those for `front`, for a session with a live client (see
    /// [`pool::Pool::adopt`]), or of a changed kind are released.
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

/// Tells every client to retire before dropping any, so they exit in parallel.
impl Drop for Revival {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let mut held: Vec<pty::Parked> = self.held.try_iter().collect();
        for one in &mut held {
            one.tell_to_retire();
        }
    }
}

/// Which clients a [`Revival`] restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Again {
    /// Those a dropped link took. SSH only.
    Taken,
    /// Every session's the pool has no client for, after a client-kind change.
    Every,
}

impl Again {
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

    /// In `<prefix> n` order from `front`. For `Taken`, the pool releases its
    /// exited clients.
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

/// Likeliest next first.
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

/// A [`Revival`] thread's work, minus how a client starts. One that fails is
/// left to its visit.
fn revive<T>(
    sessions: &[nvmux::session::Session],
    painted: &mpsc::Receiver<()>,
    stop: &AtomicBool,
    handing: &mpsc::Sender<T>,
    mut start: impl FnMut(&nvmux::session::Session) -> nvmux::Result<Option<T>>,
) {
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

/// Retire a client on a thread: dropping one waits for its thread to hand it
/// over, which a switch about to fade cannot afford.
fn let_go(held: pty::Parked) {
    let _ = std::thread::Builder::new()
        .name("nvmux-let-go".into())
        .spawn(move || drop(held));
}

/// Wait on the attaching screen for the probe. `None` means the user gave up.
/// Failures drop the attachment explicitly, so the client is gone before the
/// picker reports it.
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
        // The client draws nothing until a key reaches the server, so the
        // notice says why the screen is blank.
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

    #[test]
    fn a_number_is_resolved_against_the_listing_in_hand() {
        let listing = listing_of(&[(1, "id000001"), (2, "id000002"), (3, "id000003")]);
        let picked = pick(&listing, pty::Target::Number(2), 1).expect("2 is on the list");
        assert_eq!(picked.id, "id000002");
    }

    #[test]
    fn a_step_walks_the_listing_in_hand() {
        let listing = listing_of(&[(1, "id000001"), (2, "id000002"), (3, "id000003")]);
        let next = pick(&listing, pty::Target::Step(Direction::Next), 2).expect("3 follows 2");
        assert_eq!(next.id, "id000003");
        let prev = pick(&listing, pty::Target::Step(Direction::Prev), 2).expect("1 precedes 2");
        assert_eq!(prev.id, "id000001");
    }

    /// Stepping from the number `current` was taken with would land on itself
    /// (`n`) or skip the session before it (`p`).
    #[test]
    fn a_switch_after_a_move_goes_by_the_rows_as_they_stand() {
        let taken = listing_of(&[(1, "id000001"), (2, "id000002"), (3, "id000003")]);
        // The third moved to the top, above `current`.
        let moved = listing_of(&[(1, "id000003"), (2, "id000001"), (3, "id000002")]);
        let current = &taken[0];
        let to = |target| {
            pick_from(&moved, target, current)
                .map(|s| s.id.clone())
                .expect("among the rows")
        };
        assert_eq!(to(pty::Target::Number(3)), "id000002");
        assert_eq!(to(pty::Target::Step(Direction::Next)), "id000002");
        assert_eq!(to(pty::Target::Step(Direction::Prev)), "id000003");
    }

    /// A miss is what makes the switch re-list before saying no.
    #[test]
    fn a_number_the_listing_does_not_have_resolves_to_nothing() {
        let listing = listing_of(&[(1, "id000001"), (2, "id000002")]);
        assert!(pick(&listing, pty::Target::Number(3), 1).is_none());
    }

    #[test]
    fn an_empty_listing_names_nothing() {
        assert!(pick(&[], pty::Target::Number(1), 0).is_none());
        assert!(pick(&[], pty::Target::Step(Direction::Next), 0).is_none());
    }

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

    /// As the picker hands it over, numbers resolved.
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

    /// Through a forward, "refused" means the master died, not the session.
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

    #[test]
    fn a_failed_listing_says_what_nvmux_was_doing() {
        let e = NvmuxError::Ssh(SshError::MasterDied("myhost".into()));
        let msg = describe_listing_failure(&e);
        assert!(msg.contains("list sessions"), "{msg}");
        assert!(msg.contains("myhost"), "{msg}");
    }

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

    /// `per_session` is not a kind change.
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

    /// No paint is coming to wait for.
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
