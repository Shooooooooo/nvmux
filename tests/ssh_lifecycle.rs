//! End-to-end tests for the SSH transport, against a real `sshd`.
//!
//! The host comes from `$NVMUX_TEST_SSH_HOST`, defaulting to `selftest`; where
//! no such host answers these **skip** rather than fail. Set one up with:
//!
//! ```text
//! Host selftest
//!   HostName 127.0.0.1
//!   User <you>
//!   IdentityFile ~/.ssh/id_ed25519
//! ```
//!
//! Pointing that alias at localhost is not a cheat: it exercises the real ssh
//! client, a real ControlMaster, real unix-socket forwarding and a real remote
//! login shell — everything except latency. `$NVMUX_TEST_REQUIRE=ssh` turns
//! the skip into a failure.
// These drive a local transport, a pty or `sh` on this machine: Unix only.
#![cfg(unix)]

#[macro_use]
mod common;

use std::path::{Path, PathBuf};

use common::unique;
use nvmux::ssh::Ssh;
use nvmux::transport::remote::SshTransport;
use nvmux::transport::Transport;

fn host() -> String {
    std::env::var("NVMUX_TEST_SSH_HOST").unwrap_or_else(|_| "selftest".to_string())
}

/// Is the test host usable? Skips are silent on purpose.
fn reachable() -> bool {
    std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            &host(),
            "true",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

macro_rules! require_ssh {
    () => {
        require!(
            "ssh",
            reachable(),
            format!("{} is not reachable over ssh", host())
        );
    };
}

/// Removes every session this test file created, whatever happened.
///
/// The transport is built in `drop`, not held: one of these guards exists per
/// test, and connecting up front cost a second full ssh connect and probe per
/// test for a cleanup that usually has nothing to do.
struct Cleanup(Vec<String>);

impl Cleanup {
    fn of<S: AsRef<str>>(names: impl IntoIterator<Item = S>) -> Self {
        Self(names.into_iter().map(|n| n.as_ref().to_string()).collect())
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let Ok(t) = SshTransport::new(host()) else {
            return;
        };
        if let Ok(sessions) = t.list_sessions() {
            for s in sessions {
                if self.0.iter().any(|n| s.name.starts_with(n.as_str())) {
                    let _ = t.kill_session(&s);
                }
            }
        }
    }
}

#[test]
fn a_session_created_over_ssh_is_reachable_through_the_forward() {
    require_ssh!();
    let t = SshTransport::new(host()).expect("connect");
    let name = unique("reach");
    let _guard = Cleanup::of([&name]);

    let session = t
        .create_session(&name, &common::launch(), common::anywhere())
        .expect("create");
    assert!(
        session.pid > 1,
        "spawn should report a validated remote pid"
    );

    let sock = t.local_socket_for(&session).expect("forward");
    assert!(sock.exists(), "no local socket at {}", sock.display());
    assert!(
        sock.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains('-')),
        "the local end must be namespaced by host token: {}",
        sock.display()
    );

    let mut client =
        nvmux::rpc::Client::connect(&sock, nvmux::rpc::PROBE_TIMEOUT).expect("connect via forward");
    let info = client.api_info().expect("api_info through the forward");
    assert!(info.is_supported(), "remote nvim {info} is too old");

    // A deferred call too, which proves the session is actually serving rather
    // than merely that ssh accepted the connection — `ssh -O forward` to a
    // nonexistent remote socket also exits 0 and yields a working local socket.
    client
        .list_bufs()
        .expect("deferred call through the forward");
}

#[test]
fn a_remote_session_outlives_the_transport_that_made_it() {
    require_ssh!();
    let name = unique("outlive");
    let _guard = Cleanup::of([&name]);

    let id = {
        let t = SshTransport::new(host()).expect("connect");
        let s = t
            .create_session(&name, &common::launch(), common::anywhere())
            .expect("create");
        s.id
    }; // transport dropped, ssh commands finished

    // A completely fresh transport must find it, which is the whole point of
    // keeping the metadata on the session host.
    let t2 = SshTransport::new(host()).expect("reconnect");
    let found = common::find_by_id(&t2, &id).expect("the session should have survived");
    assert_eq!(found.name, name);
}

/// Detach and re-attach is the flow this tool exists for, and the one that
/// breaks without `StreamLocalBindUnlink=yes` on the master: `-O cancel` exits 0
/// but leaves the local socket file, and the next `-O forward` onto that path
/// fails with rc 255.
#[test]
fn a_forward_can_be_torn_down_and_rebuilt() {
    require_ssh!();
    let t = SshTransport::new(host()).expect("connect");
    let name = unique("reforward");
    let _guard = Cleanup::of([&name]);

    let session = t
        .create_session(&name, &common::launch(), common::anywhere())
        .expect("create");
    let first = t.local_socket_for(&session).expect("forward");
    assert!(first.exists());

    std::fs::remove_file(&first).ok();

    // A fresh transport has no memory of the forward, so this exercises the
    // rebuild rather than the cached path.
    let t2 = SshTransport::new(host()).expect("reconnect");
    let again = t2.local_socket_for(&session).expect("re-forward");
    assert_eq!(first, again, "the local path should be stable");
    assert!(again.exists(), "the forward was not rebuilt");

    let mut client =
        nvmux::rpc::Client::connect(&again, nvmux::rpc::PROBE_TIMEOUT).expect("connect");
    client.api_info().expect("usable after re-forward");
}

#[test]
fn renaming_a_remote_session_moves_no_socket() {
    require_ssh!();
    let t = SshTransport::new(host()).expect("connect");
    let name = unique("rename");
    let _guard = Cleanup::of([name.clone(), format!("{name}-after")]);

    let session = t
        .create_session(&name, &common::launch(), common::anywhere())
        .expect("create");
    let before = t.local_socket_for(&session).expect("forward");

    let after_name = format!("{name}-after");
    t.rename_session(&session, &after_name).expect("rename");

    let listed = common::find_by_id(&t, &session.id).expect("still listed");
    assert_eq!(listed.name, after_name);
    assert_eq!(
        t.local_socket_for(&listed).expect("forward"),
        before,
        "a rename must not move the socket, or every forward would have to be rebuilt"
    );
}

#[test]
fn killing_a_remote_session_removes_it_and_its_forward() {
    require_ssh!();
    let t = SshTransport::new(host()).expect("connect");
    let name = unique("kill");
    // Even the kill test needs a guard: if the kill under test fails, the
    // session would otherwise outlive the run on the remote host.
    let _guard = Cleanup::of([&name]);

    let session = t
        .create_session(&name, &common::launch(), common::anywhere())
        .expect("create");
    let sock = t.local_socket_for(&session).expect("forward");
    assert!(sock.exists());

    t.kill_session(&session).expect("kill");

    assert!(
        !t.list_sessions()
            .expect("list")
            .iter()
            .any(|s| s.id == session.id),
        "the session should be gone from the listing"
    );
    assert!(
        !sock.exists(),
        "the local end of the forward should have been cleaned up"
    );
}

#[test]
fn remote_names_with_shell_metacharacters_survive() {
    require_ssh!();
    let t = SshTransport::new(host()).expect("connect");
    // Two layers of shell stand between here and the remote file: ssh joins its
    // arguments and the remote login shell parses them again.
    let name = format!("{} $(id) 'q' \"d\"", unique("quote"));
    let _guard = Cleanup::of([unique("quote")]);

    let session = t
        .create_session(&name, &common::launch(), common::anywhere())
        .expect("create");
    let found = common::find_by_id(&t, &session.id).expect("listed");
    assert_eq!(
        found.name, name,
        "the name was mangled or evaluated in transit"
    );
}

#[test]
fn an_unreachable_host_fails_with_a_useful_message() {
    let err = SshTransport::new("nvmux-no-such-host.invalid".into())
        .expect_err("must fail")
        .to_string();
    assert!(
        err.contains("unreachable") || err.contains("ssh"),
        "unhelpful error: {err}"
    );
}

/// A live forward must survive a listing, and a dead one must not.
///
/// The sweep that removes orphaned forwards runs against the same runtime
/// directory the remote listing scans — and when the "remote" host is this
/// machine, which `nvmux localhost` makes an ordinary case, those are literally
/// the same directory. A sweep that only asked "is anything serving this
/// socket?" would find the local end of a live forward, see that no `--listen`
/// process owns it, and delete the forward out from under an attached session.
#[test]
fn a_listing_keeps_live_forwards_and_removes_orphaned_ones() {
    require_ssh!();
    let t = SshTransport::new(host()).expect("connect");
    let name = unique("sweep");
    let _guard = Cleanup::of([&name]);

    let session = t
        .create_session(&name, &common::launch(), common::anywhere())
        .expect("create");
    let live = t.local_socket_for(&session).expect("forward");
    assert!(live.exists());

    let orphan = live.with_file_name(format!("{}-zzzzzzzz.sock", nvmux::ids::host_token(&host())));
    std::fs::write(&orphan, b"").expect("plant orphan");

    t.list_sessions().expect("list");

    assert!(
        live.exists(),
        "a listing deleted the live forward at {}",
        live.display()
    );
    assert!(!orphan.exists(), "the orphaned forward was not cleaned up");

    let mut client =
        nvmux::rpc::Client::connect(&live, nvmux::rpc::PROBE_TIMEOUT).expect("still connectable");
    client.api_info().expect("still usable after a listing");
}

/// A directory for this test's end of the connection, and every master that
/// ever binds a socket in it retired when the test is done.
///
/// Every other test here reaches the host through the one master nvmux keeps
/// in the real runtime directory, and they run in parallel: a master pulled
/// out from under one of their live forwards would fail *them*. The tests
/// about a master dying, or its socket going, bring up their own here and
/// leave that one alone.
///
/// Under `/tmp`, as nvmux's own runtime directory is, and not `$TMPDIR`: on
/// macOS that is long enough that a master's socket — with the seventeen
/// bytes ssh adds to it while binding — would not fit in a socket path.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn new(tag: &str) -> Self {
        let dir = PathBuf::from(format!("/tmp/nvmux-t-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }

    /// The sockets masters bound here, each under a name of its own beside
    /// the `ControlPath` they are linked to.
    fn master_sockets(&self, control_path: &Path) -> Vec<PathBuf> {
        let prefix = format!("{}.", control_path.file_name().unwrap().to_string_lossy());
        let mut found: Vec<PathBuf> = std::fs::read_dir(&self.0)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .map(|e| e.path())
            .collect();
        found.sort();
        found
    }

    /// How many master processes are bound somewhere in here — the ones still
    /// linked to the `ControlPath` and the ones not.
    fn masters_running(&self) -> usize {
        let needle = format!("ControlPath={}/", self.0.display());
        let out = std::process::Command::new("ps")
            .args(["-A", "-o", "command="])
            .output()
            .expect("ps");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.starts_with("ssh ") && l.contains(" -M ") && l.contains(&needle))
            .count()
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        // Telling every master in here to exit stops a test's masters
        // outliving it by a minute of `ControlPersist`. By their names only:
        // the local end of a forward is also a socket in here, and what
        // answers on that is a session's Neovim.
        for entry in std::fs::read_dir(&self.0).into_iter().flatten().flatten() {
            if entry.file_name().to_string_lossy().starts_with("cm") {
                exit_master_at(&entry.path());
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `ssh -O exit`, through whichever of a master's names is given.
fn exit_master_at(socket: &Path) -> std::process::Output {
    std::process::Command::new("ssh")
        .arg("-o")
        .arg(format!("ControlPath={}", socket.display()))
        .args(["-O", "exit", &host()])
        .output()
        .expect("run ssh -O exit")
}

/// A master on a `ControlPath` of this test's own, killable without
/// disturbing anything — see [`PrivateDir`].
struct PrivateMaster {
    ssh: Ssh,
    dir: PrivateDir,
}

impl PrivateMaster {
    fn up(tag: &str) -> Self {
        let dir = PrivateDir::new(tag);
        std::fs::create_dir_all(&dir.0).expect("scratch directory");
        // Short, because a ControlPath is a socket path like any other.
        let ssh = Ssh::new(host(), dir.0.join("cm"));
        ssh.ensure_master().expect("bring up a master");
        assert!(ssh.is_master_alive(), "the master did not come up");
        Self { ssh, dir }
    }

    /// What a sleeping laptop does, near enough: the master goes and every
    /// forward on it goes too.
    fn kill(&self) {
        let out = exit_master_at(self.ssh.control_path());
        assert!(
            out.status.success() || !self.ssh.is_master_alive(),
            "could not stop the master: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// The fact every reconnection decision turns on, and the one that made the
/// bug it guards against silent: `ssh` handed a `ControlPath` with no master
/// on it does **not** fail. It opens a connection of its own, says nothing,
/// and works — so a shell that is alive proves the host is reachable and
/// nothing whatever about the master that every `-O forward` needs.
///
/// Which is why `run_script` asks ssh itself before replacing a dead shell,
/// rather than inferring a master from the shell it is about to start. Get
/// that wrong and the picker lists sessions perfectly over a direct connection
/// while no session can be attached at all — "the connection to <host> dropped
/// — press enter to retry", repeating for as long as the user presses enter.
#[test]
fn a_shell_started_without_a_master_quietly_makes_its_own_connection() {
    require_ssh!();
    let m = PrivateMaster::up("no-master");
    m.kill();
    assert!(!m.ssh.is_master_alive(), "the master should be gone");

    let mut shell = m.ssh.start_shell().expect("ssh runs anyway");
    assert_eq!(
        shell
            .run("printf alive", &[])
            .expect("the shell answers")
            .stdout,
        "alive",
        "a shell with no master must still work — that is the whole trap"
    );
    assert!(
        !m.ssh.is_master_alive(),
        "starting a shell must not be mistaken for starting a master"
    );

    // And this is what the working shell was hiding. Nothing can be attached
    // until something brings the master back.
    m.ssh
        .forward(
            &m.dir.0.join("l.sock"),
            Path::new("/tmp/nvmux-no-such.sock"),
        )
        .expect_err("there is no master to forward on");
}

/// The other half of the same coin, and the premise behind asking the shell at
/// all: a shell that *was* started over a master dies with it. That is what
/// lets an attach check the link with a zero-timeout poll on a pipe instead of
/// forking `ssh -O check` every time.
#[test]
fn a_shell_started_over_a_master_dies_with_it() {
    require_ssh!();
    let m = PrivateMaster::up("with-master");

    let mut shell = m.ssh.start_shell().expect("shell");
    assert_eq!(shell.run("printf up", &[]).expect("runs").stdout, "up");

    m.kill();

    // The client is a separate process, so its death is not instantaneous.
    assert!(
        common::wait_until(std::time::Duration::from_secs(5), || !shell.is_alive()),
        "a shell over a master must not outlive it, or a live shell would \
         vouch for a master that has gone"
    );

    // Brought back the way the transport brings it back, over the name the
    // dead master was linked to — which it leaves behind.
    m.ssh.ensure_master().expect("reconnect");
    assert!(m.ssh.is_master_alive(), "the master did not come back");
    let mut again = m.ssh.start_shell().expect("shell");
    assert_eq!(again.run("printf back", &[]).expect("runs").stdout, "back");
}

/// What left the picker unable to attach anything: a master and its shell
/// both still running — so everything that asks the shell hears the link is
/// fine — and no socket at the `ControlPath` for `ssh -O forward` to reach
/// the master by. Every attach failed with `Control socket connect(...): No
/// such file or directory`, and pressing enter asked again in the same way.
///
/// However the socket went, the attach has to bring a master back.
#[test]
fn an_attach_with_no_master_behind_the_control_path_brings_one_back() {
    require_ssh!();
    let name = unique("unreachable");
    let _guard = Cleanup::of([&name]);
    let session = SshTransport::new(host())
        .expect("connect")
        .create_session(&name, &common::launch(), common::anywhere())
        .expect("create");

    let dir = PrivateDir::new("gone");
    let t = SshTransport::with_dir(host(), dir.0.clone()).expect("connect");
    let control_path =
        nvmux::paths::control_path(&dir.0, &nvmux::ids::host_token(&host())).expect("path");
    std::fs::remove_file(&control_path).expect("take the master's socket away");

    let sock = t
        .local_socket_for(&session)
        .expect("the attach should bring a master back rather than fail");
    let mut client =
        nvmux::rpc::Client::connect(&sock, nvmux::rpc::PROBE_TIMEOUT).expect("connect");
    client
        .api_info()
        .expect("usable through the master it brought back");

    // And the transport's scripts run over that master too, not the old one.
    assert!(
        t.list_sessions()
            .expect("list")
            .iter()
            .any(|s| s.id == session.id),
        "the session should still be listed"
    );
}

/// ssh removes a master's `ControlPath` by name when the master exits, whoever
/// holds that name by then. A master bound at the shared name that had lost it
/// would, on its way out, take it from whichever master was linked there
/// since — the master every later `-O forward` needs, with its shell still up.
/// Bound at a name of its own, a master that leaves removes only that.
#[test]
fn a_master_leaving_takes_only_its_own_name_with_it() {
    require_ssh!();
    let m = PrivateMaster::up("leaving");
    let first = m.dir.master_sockets(m.ssh.control_path());
    assert_eq!(
        first.len(),
        1,
        "one master, bound at a name of its own: {first:?}"
    );

    // However a master comes to lose the shared name: another nvmux clearing
    // it, a sweep of /tmp, a stray `rm`.
    std::fs::remove_file(m.ssh.control_path()).expect("take the name");
    m.ssh.ensure_master().expect("a second master");
    assert!(m.ssh.is_master_alive(), "the second master is not linked");

    // The first one leaves, as it would once its last client had — a separate
    // process, so not instantly.
    exit_master_at(&first[0]);
    assert!(
        common::wait_until(std::time::Duration::from_secs(5), || !first[0].exists()),
        "the first master did not exit"
    );
    assert!(
        m.ssh.is_master_alive(),
        "the first master took the second one's ControlPath with it"
    );
}

/// Two nvmux bringing a master up at the same moment — both of them after one
/// dropped link, say — must end with one master and not two. Bound at the
/// shared name, the second found it taken, and ssh answers that by switching
/// multiplexing off and staying up anyway: an idle connection nothing could
/// reach, for as long as the network held.
#[test]
fn two_masters_brought_up_at_once_leave_one_running() {
    require_ssh!();
    let dir = PrivateDir::new("twice");
    std::fs::create_dir_all(&dir.0).expect("scratch directory");
    let control_path = dir.0.join("cm");
    let (a, b) = (
        Ssh::new(host(), control_path.clone()),
        Ssh::new(host(), control_path.clone()),
    );

    std::thread::scope(|s| {
        let a = s.spawn(|| a.ensure_master());
        let b = s.spawn(|| b.ensure_master());
        a.join().expect("thread").expect("first master");
        b.join().expect("thread").expect("second master");
    });

    assert!(a.is_master_alive(), "neither master is linked");
    // The loser is told to exit, and takes a moment to.
    assert!(
        common::wait_until(std::time::Duration::from_secs(5), || {
            dir.masters_running() == 1
        }),
        "{} masters left running for one ControlPath",
        dir.masters_running()
    );
}

/// The chosen command runs on the host that owns the session, not on this one,
/// and every word of it survives ssh's argument join and the remote login
/// shell's second parse.
#[test]
fn a_chosen_command_runs_on_the_remote_host_with_its_arguments_intact() {
    require_ssh!();
    let t = SshTransport::new(host()).expect("connect");
    let name = unique("remote-command");
    let _guard = Cleanup::of([&name]);

    let launch = common::launch_with("--clean");
    let session = t
        .create_session(&name, &launch, common::anywhere())
        .expect("create");

    // Asked of the remote host, since that is where the process is.
    let remote = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            &host(),
            "ps",
            "-ww",
            "-o",
            "args=",
            "-p",
        ])
        .arg(session.pid.to_string())
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default();
    assert!(
        remote.contains("--clean"),
        "the chosen argument never reached the remote nvim: {remote:?}"
    );

    // Reachable through the forward like any other session, and recorded.
    let sock = t.local_socket_for(&session).expect("forward");
    let mut client =
        nvmux::rpc::Client::connect(&sock, nvmux::rpc::PROBE_TIMEOUT).expect("connect via forward");
    client.api_info().expect("api_info through the forward");

    let listed = t.list_sessions().expect("list");
    let stored = listed
        .iter()
        .find(|s| s.id == session.id)
        .expect("the session should be listed");
    assert_eq!(
        stored.command,
        launch.line(),
        "the remote metadata must record what launched it"
    );

    t.kill_session(&session).expect("kill");
}
