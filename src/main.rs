//! Binary entry point.

use anyhow::{Context, Result};
use clap::Parser;

use nvmux::cli::Cli;
use nvmux::{announce, config, logging, nested, nvim, paths, pty, transport, ui};

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Everything else depends on this existing and being ours.
    let dir = paths::ensure_runtime_dir().context("preparing the nvmux runtime directory")?;
    logging::init(&dir)?;

    // Before anything is spawned — and before a first-run config file is written
    // in `run` — restrict the umask: see `paths::restrict_umask`.
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

    // Checked up front rather than surfacing later as an unexplained connection
    // failure. This is the nvim used as the --remote-ui client; the remote one
    // is checked by the SSH transport when it connects.
    let local_nvim = nvim::check_local()?;
    tracing::debug!(version = %local_nvim, "local nvim");

    // Config is a local concern — the prefix machine and the picker both run
    // here — so it is established before any transport, `nvmux <host>`
    // included. On a genuine first run at an interactive terminal this asks for a
    // prefix and records it; otherwise it loads whatever exists (or the defaults).
    config::init(establish_settings()?);

    let transport = transport::open(location.clone())?;
    session_loop(transport.as_ref())
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
fn session_loop(transport: &dyn transport::Transport) -> Result<()> {
    let mut attached: Option<pty::Attachment> = None;
    let mut message: Option<String> = None;
    // The session the last trip through the picker led to, so the next one
    // opens with the cursor on it rather than on the first row.
    let mut focus: Option<String> = None;
    // The listing that trip handed over, with anything created since, so the
    // next picker can be drawn from it while it lists afresh. None only for
    // the first picker, which has nothing to draw from.
    let mut in_hand: Option<Vec<nvmux::session::Session>> = None;

    loop {
        // `<prefix> c` moves this to the session it just created. A client held
        // across the trip is what `Esc` goes back to; without one — the first
        // screen, a failed attach, a session that exited — there is nothing
        // behind the picker and `Esc` says so by doing nothing.
        let (mut current, mut listing) = match ui::run(
            transport,
            message.take(),
            focus.as_deref(),
            attached.is_some(),
            in_hand.take(),
        )? {
            ui::Outcome::Quit => {
                // A client held across `<prefix> Space` is retired explicitly; its
                // `Drop` would do the same, this just says so.
                if let Some(a) = attached.take() {
                    a.terminate();
                }
                break;
            }
            ui::Outcome::Attach { session, sessions } => (session, sessions),
        };
        // Derived rather than carried: one fewer thing for the picker to keep
        // in step, and the listing it handed over is what it would be derived
        // from anyway.
        let mut highest = ui::highest_num(&listing);

        loop {
            // Everything nvmux does between the keypress and a client ready to
            // relay: retiring the old one, the probe, and the spawn. Read with
            // the `timing: session painted` record the relay ends up emitting,
            // which is the session's own share of the same switch.
            let t_open = std::time::Instant::now();
            let opened = match attached.take() {
                Some(a) if a.session_id == current.id => Ok(a),
                // Retiring the old client leaves its server running: killing a
                // --remote-ui client does not kill a --headless --listen server.
                //
                // Hung up first and reaped after, with the new client's spawn in
                // between: the two have nothing to say to each other — a
                // different session, a different server, a different pty — so
                // serialising them would charge the user the sum of the two.
                // Nothing reads the old pty master again either, so the dying
                // client's own restore sequence goes nowhere near the terminal.
                Some(mut other) => {
                    other.hang_up();
                    let spawned = new_attachment(transport, &current);
                    // Explicitly, rather than by falling out of the arm: this is
                    // the wait the hangup deferred, and leaving it to a binding's
                    // drop would let the next edit here re-serialise it without
                    // noticing. It has to happen on the failure path too, which
                    // is why the spawn is held rather than returned from inside.
                    drop(other);
                    spawned
                }
                None => new_attachment(transport, &current),
            };

            tracing::debug!(
                ms = t_open.elapsed().as_secs_f64() * 1000.0,
                "timing: retire + probe + spawn"
            );

            // A failed attach must not end the program: the user can only act
            // on it from the picker, with the reason on screen.
            let attachment = match opened {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(id = %current.id, error = %e, "attach failed");
                    message = Some(describe_attach_failure(transport.location(), &e));
                    break;
                }
            };

            match pty::relay(attachment, highest)? {
                (pty::Outcome::ToPicker, held) => {
                    attached = held;
                    break;
                }
                // The session keeps running either way: on `<prefix> d`
                // because the user asked, on a closed stdin because there is
                // no terminal left to ask from.
                (pty::Outcome::Detached | pty::Outcome::StdinClosed, _) => return Ok(()),
                (pty::Outcome::ChildExited, _) => break,
                (pty::Outcome::CreateNew, held) => {
                    // Held rather than killed, so a cancelled prompt resumes it.
                    attached = held;
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
                    attached = held;
                    ui::help::run()?;
                    continue;
                }
                (pty::Outcome::Switch(target), held) => {
                    // Held rather than killed, so a number that names nothing —
                    // or a cycle with nowhere to go — puts the user straight
                    // back where they were.
                    attached = held;
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
                        // The loop above retires the old client and attaches the
                        // new one; an unchanged id reuses the client as it is.
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
        in_hand = Some(listing);
    }
    Ok(())
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
        // nvmux will not answer for the user, CPU-bound Lua. The bare error
        // ("timed out after 3s") reads as if nvmux had lost the session.
        nvmux::NvmuxError::Rpc(nvmux::error::RpcError::Timeout(after)) => {
            format!("that session is busy and did not answer within {after:?}")
        }
        other => one_line(other),
    }
}

/// Explain why a switch could not find out what to switch to.
///
/// Unlike an attach this is a listing, and the errors it raises already name the
/// host where they have one (`ssh: the connection to myhost died`). What none of
/// them says is what nvmux was attempting — a script's refusal is rendered bare,
/// on purpose — and on the hint row there is nothing else to say it.
fn describe_listing_failure(e: &nvmux::NvmuxError) -> String {
    format!("could not list sessions: {}", one_line(e))
}

/// Collapse an error to something that fits on one line.
///
/// The hint row it lands on is exactly one row; a multi-line error would be
/// truncated at the first newline and lose the part that explains itself.
fn one_line(e: &nvmux::NvmuxError) -> String {
    e.to_string().lines().collect::<Vec<_>>().join(" — ")
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

/// Returns the crate's own error type rather than `anyhow`, so the caller can
/// tell a dropped connection from a dead session and say the right thing.
fn new_attachment(
    transport: &dyn transport::Transport,
    session: &nvmux::session::Session,
) -> nvmux::Result<pty::Attachment> {
    let sock = transport.local_socket_for(session)?;
    // Every spawn is a change of session — the loop above reuses the client
    // otherwise — so the notice is unconditional here and one-shot there.
    let notice = announce::label(&session.name);
    pty::spawn(&session.id, &sock, &notice)
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

    /// The hint row is one row here too.
    #[test]
    fn a_multi_line_listing_failure_is_flattened_to_one_line() {
        let e = NvmuxError::Session(SessionError::NotReady {
            name: "x".into(),
            timeout: std::time::Duration::from_secs(1),
            log: "x.log".into(),
            log_tail: "line one\nline two".into(),
        });
        let msg = describe_listing_failure(&e);
        assert!(!msg.contains('\n'), "{msg:?}");
        assert!(
            msg.contains("line one") && msg.contains("line two"),
            "{msg:?}"
        );
    }
}
