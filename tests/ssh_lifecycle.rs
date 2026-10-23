//! End-to-end tests for the SSH transport, against a real `sshd`.
//!
//! The host is `$NVMUX_TEST_SSH_HOST` (default `selftest`). If it does not
//! answer, tests skip; `$NVMUX_TEST_REQUIRE=ssh` makes that a failure. Setup:
//!
//! ```text
//! Host selftest
//!   HostName 127.0.0.1
//!   User <you>
//!   IdentityFile ~/.ssh/id_ed25519
//! ```
//!
//! Localhost still exercises the real ssh client, ControlMaster, unix-socket
//! forwarding and remote login shell; only latency is missing.

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

/// Skips are silent on purpose.
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

/// Kills the named sessions on drop. Connects only in `drop`, to spare each
/// test an extra ssh connect and probe.
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

    // A deferred call proves nvim is serving: `ssh -O forward` to a missing
    // remote socket also exits 0 and yields a connectable local socket.
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

    // Metadata lives on the session host, so a fresh transport finds it.
    let t2 = SshTransport::new(host()).expect("reconnect");
    let found = common::find_by_id(&t2, &id).expect("the session should have survived");
    assert_eq!(found.name, name);
}

/// Needs `StreamLocalBindUnlink=yes` on the master: `-O cancel` exits 0 but
/// leaves the socket file, and the next `-O forward` there fails with rc 255.
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

    // A fresh transport, so the forward is rebuilt rather than cached.
    let t2 = SshTransport::new(host()).expect("reconnect");
    let again = t2.local_socket_for(&session).expect("re-forward");
    assert_eq!(first, again, "the local path should be stable");
    assert!(again.exists(), "the forward was not rebuilt");

    let mut client =
        nvmux::rpc::Client::connect(&again, nvmux::rpc::PROBE_TIMEOUT).expect("connect");
    client.api_info().expect("usable after re-forward");
}

/// `count` sessions made through the shared master.
fn sessions(tag: &str, count: usize) -> (Vec<nvmux::session::Session>, Cleanup) {
    let names: Vec<String> = (0..count).map(|i| unique(&format!("{tag}{i}"))).collect();
    let guard = Cleanup::of(&names);
    let t = SshTransport::new(host()).expect("connect");
    let made = names
        .iter()
        .map(|name| {
            t.create_session(name, &common::launch(), common::anywhere())
                .expect("create")
        })
        .collect();
    (made, guard)
}

/// The local socket a transport using `dir` forwards `s` to.
fn forwarded_sock(dir: &Path, s: &nvmux::session::Session) -> PathBuf {
    nvmux::paths::forwarded_sock(dir, &nvmux::ids::host_token(&host()), &s.id).expect("path")
}

/// One `ssh -O forward` for all, each usable. Uses a private directory so
/// another test's forwards cannot pass for these.
#[test]
fn sessions_wanted_together_are_forwarded_together() {
    require_ssh!();
    let (sessions, _guard) = sessions("together", 3);
    let dir = PrivateDir::new("together");
    let t = SshTransport::with_dir(host(), dir.0.clone()).expect("connect");
    let socks: Vec<PathBuf> = sessions.iter().map(|s| forwarded_sock(&dir.0, s)).collect();

    t.reach_all(&sessions);
    for sock in &socks {
        assert!(
            sock.exists(),
            "not forwarded with the rest: {}",
            sock.display()
        );
    }
    for (s, sock) in sessions.iter().zip(&socks) {
        assert_eq!(&t.local_socket_for(s).expect("forward"), sock);
        let mut client =
            nvmux::rpc::Client::connect(sock, nvmux::rpc::PROBE_TIMEOUT).expect("connect");
        client
            .list_bufs()
            .expect("a deferred call through the forward");
    }
}

/// The master answers `-O forward` of a pair it already holds with success
/// but creates nothing, so success is trusted only where the socket exists.
#[test]
fn a_forward_whose_socket_has_gone_is_made_again() {
    require_ssh!();
    let (sessions, _guard) = sessions("regone", 2);
    let dir = PrivateDir::new("regone");
    let t = SshTransport::with_dir(host(), dir.0.clone()).expect("connect");
    let socks: Vec<PathBuf> = sessions.iter().map(|s| forwarded_sock(&dir.0, s)).collect();
    t.reach_all(&sessions);
    assert!(socks.iter().all(|s| s.exists()), "forwarded together");
    std::fs::remove_file(&socks[0]).expect("take a socket away");

    // A fresh transport over the master that holds both.
    let t2 = SshTransport::with_dir(host(), dir.0.clone()).expect("connect");
    t2.reach_all(&sessions);
    assert!(
        !socks[0].exists(),
        "the master was to say yes to a forward it holds and make nothing"
    );
    for (s, sock) in sessions.iter().zip(&socks) {
        assert_eq!(&t2.local_socket_for(s).expect("forward"), sock);
        let mut client =
            nvmux::rpc::Client::connect(sock, nvmux::rpc::PROBE_TIMEOUT).expect("connect");
        client.api_info().expect("usable through the forward");
    }
}

/// One refused pair fails the command but the others are still made, and
/// nothing says which failed. So after a failed batch no socket is trusted:
/// an existing one may be the stale file that blocked the refused pair.
#[test]
fn a_pair_refused_fails_the_batch_and_stops_none_of_the_rest() {
    require_ssh!();
    let m = PrivateMaster::up("refused");
    let remote = PathBuf::from("/tmp/nvmux-no-such.sock");
    let pairs = [
        (m.dir.0.join("a.sock"), remote.clone()),
        (m.dir.0.join("no-such-dir/b.sock"), remote.clone()),
        (m.dir.0.join("c.sock"), remote.clone()),
    ];
    assert!(
        !m.ssh.forward_all(&pairs),
        "a pair refused fails the command"
    );
    assert!(
        pairs[0].0.exists() && pairs[2].0.exists(),
        "and the pairs either side of it are made all the same"
    );
    assert!(
        m.ssh.forward_all(&[pairs[0].clone(), pairs[2].clone()]),
        "asking again for pairs the master holds is a yes"
    );
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
    // In case the kill under test fails.
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
    // Two shell layers: ssh joins its arguments and the remote login shell
    // parses them again.
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

/// With `nvmux localhost` the orphan sweep and the remote listing scan the
/// same directory. A sweep that only checked for a `--listen` owner would
/// delete the local end of a live forward.
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

/// A private runtime directory; every master bound in it exits on drop.
///
/// Other tests share the real runtime directory's master in parallel, so tests
/// that kill a master or remove its socket use their own here.
///
/// Under `/tmp`, not `$TMPDIR`: macOS's is too long for a socket path once ssh
/// adds 17 bytes while binding.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn new(tag: &str) -> Self {
        let dir = PathBuf::from(format!("/tmp/nvmux-t-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }

    /// Each master's own socket, named after `control_path` plus a suffix.
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

    /// Master processes using this directory, linked to the `ControlPath` or
    /// not.
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
        // Otherwise masters outlive the test by `ControlPersist`. Match by
        // name: forwarded sockets here are answered by Neovim, not ssh.
        for entry in std::fs::read_dir(&self.0).into_iter().flatten().flatten() {
            if entry.file_name().to_string_lossy().starts_with("cm") {
                exit_master_at(&entry.path());
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `ssh -O exit` via any of a master's socket names.
fn exit_master_at(socket: &Path) -> std::process::Output {
    std::process::Command::new("ssh")
        .arg("-o")
        .arg(format!("ControlPath={}", socket.display()))
        .args(["-O", "exit", &host()])
        .output()
        .expect("run ssh -O exit")
}

/// A master in a [`PrivateDir`], safe to kill.
struct PrivateMaster {
    ssh: Ssh,
    dir: PrivateDir,
}

impl PrivateMaster {
    fn up(tag: &str) -> Self {
        let dir = PrivateDir::new(tag);
        std::fs::create_dir_all(&dir.0).expect("scratch directory");
        // Short: a ControlPath is subject to the socket path limit.
        let ssh = Ssh::new(host(), dir.0.join("cm"));
        ssh.ensure_master().expect("bring up a master");
        assert!(ssh.is_master_alive(), "the master did not come up");
        Self { ssh, dir }
    }

    /// Like a laptop sleeping: the master and its forwards go.
    fn kill(&self) {
        let out = exit_master_at(self.ssh.control_path());
        assert!(
            out.status.success() || !self.ssh.is_master_alive(),
            "could not stop the master: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// `ssh` with a `ControlPath` that has no master does **not** fail: it silently
/// opens its own connection. A live shell proves nothing about the master that
/// `-O forward` needs, so `run_script` asks ssh directly. Getting this wrong
/// listed sessions fine but made every attach fail with "connection dropped".
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

    // What the working shell hides: no master to forward on.
    m.ssh
        .forward(
            &m.dir.0.join("l.sock"),
            Path::new("/tmp/nvmux-no-such.sock"),
        )
        .expect_err("there is no master to forward on");
}

/// A shell started over a master dies with it, so an attach can check the link
/// with a zero-timeout poll on a pipe instead of forking `ssh -O check`.
#[test]
fn a_shell_started_over_a_master_dies_with_it() {
    require_ssh!();
    let m = PrivateMaster::up("with-master");

    let mut shell = m.ssh.start_shell().expect("shell");
    assert_eq!(shell.run("printf up", &[]).expect("runs").stdout, "up");

    m.kill();

    // A separate process, so not instant.
    assert!(
        common::wait_until(std::time::Duration::from_secs(5), || !shell.is_alive()),
        "a shell over a master must not outlive it, or a live shell would \
         vouch for a master that has gone"
    );

    // Reconnect as the transport does, over the name the dead master left.
    m.ssh.ensure_master().expect("reconnect");
    assert!(m.ssh.is_master_alive(), "the master did not come back");
    let mut again = m.ssh.start_shell().expect("shell");
    assert_eq!(again.run("printf back", &[]).expect("runs").stdout, "back");
}

/// Regression: master and shell alive but the `ControlPath` socket gone, so
/// every attach failed with `Control socket connect(...): No such file or
/// directory`. The attach must bring a master back.
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

    // Scripts run over the new master too.
    assert!(
        t.list_sessions()
            .expect("list")
            .iter()
            .any(|s| s.id == session.id),
        "the session should still be listed"
    );
}

/// ssh unlinks a master's socket path on exit, whoever holds it by then. Bound
/// at the shared name, an old master would remove a newer one's
/// `ControlPath`; bound at its own name, it removes only that.
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

    // E.g. another nvmux, a /tmp sweep, a stray `rm`.
    std::fs::remove_file(m.ssh.control_path()).expect("take the name");
    m.ssh.ensure_master().expect("a second master");
    assert!(m.ssh.is_master_alive(), "the second master is not linked");

    // The first exits, as after its last client; not instantly.
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

/// Two concurrent `ensure_master` calls (e.g. two nvmux after a dropped link)
/// must leave one master. Otherwise ssh, finding the name taken, disables
/// multiplexing and stays up as an unreachable idle connection.
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
    // The loser is told to exit; it takes a moment.
    assert!(
        common::wait_until(std::time::Duration::from_secs(5), || {
            dir.masters_running() == 1
        }),
        "{} masters left running for one ControlPath",
        dir.masters_running()
    );
}

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
