//! A pseudoconsole for the `--remote-ui` client: the Windows pty.
//!
//! What [`crate::pty`] needs of one is what it needs of a Unix pty: bytes out
//! of the client as they come, bytes into it, a size, and a way to be rid of
//! it. The shape differs underneath. A pseudoconsole is two anonymous pipes
//! and a `conhost` between them, and its output pipe can be neither polled nor
//! waited on — so a thread reads it into [`Output`], which an event makes
//! waitable alongside the console's own input. And a client that exits does
//! not close that pipe the way one exiting closes a pty's slave: the
//! pseudoconsole outlives it until it is closed. So a second thread waits for
//! the client to exit and closes the pseudoconsole then, which flushes what the
//! client last drew and ends the output — the moment a Unix pty would say EOF.
//!
//! Created with no flags. `PSEUDOCONSOLE_INHERIT_CURSOR` would have it ask the
//! terminal where the cursor is and draw nothing until the answer came back —
//! through a relay that holds a first paint back until it has settled — and
//! `PSEUDOCONSOLE_WIN32_INPUT_MODE` would have it switch the terminal into
//! win32-input-mode, which changes how every key reaches nvmux, the prefix
//! included. The client draws on the alternate screen, where the cursor starts
//! at home anyway, and plain VT input is what the prefix machine reads.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, EXTENDED_STARTUPINFO_PRESENT, INFINITE, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

use super::{check, millis, owned, wide, Event};

/// A terminal size, as `portable-pty` spells one on Unix: rows and columns,
/// and pixels where the terminal says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl Default for PtySize {
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

fn coord(size: PtySize) -> COORD {
    COORD {
        X: size.cols.min(i16::MAX as u16) as i16,
        Y: size.rows.min(i16::MAX as u16) as i16,
    }
}

/// What to run on a pseudoconsole: a program, its arguments, and where.
/// Spelled as `portable-pty`'s builder is, so the one place that builds the
/// client's command reads the same on both platforms.
#[derive(Debug, Clone)]
pub struct Command {
    program: OsString,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
}

impl Command {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_os_string(),
            args: Vec::new(),
            cwd: None,
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) {
        self.args.push(arg.as_ref().to_os_string());
    }

    pub fn cwd(&mut self, dir: impl AsRef<Path>) {
        self.cwd = Some(dir.as_ref().to_path_buf());
    }
}

/// The program's full path, found on `$PATH` the way a shell would find it —
/// and never in the current directory, which is where `CreateProcessW` looks
/// first, and where a file named `nvim.exe` is not necessarily Neovim.
fn resolve(program: &OsStr) -> io::Result<PathBuf> {
    let program_path = Path::new(program);
    if program_path.components().count() > 1 {
        return Ok(program_path.to_path_buf());
    }
    let names: Vec<OsString> = if program_path.extension().is_some() {
        vec![program.to_os_string()]
    } else {
        let mut exe = program.to_os_string();
        exe.push(".exe");
        vec![exe, program.to_os_string()]
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        for name in &names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} not found on %PATH%", program.to_string_lossy()),
    ))
}

/// One argument as the C runtime splits a command line back into `argv`:
/// quoted when it has to be, with backslashes doubled only where a quote
/// follows them.
fn push_arg(line: &mut Vec<u16>, arg: &OsStr) {
    let units: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(arg).collect();
    let plain = !units.is_empty()
        && !units
            .iter()
            .any(|&u| u == u16::from(b' ') || u == u16::from(b'\t') || u == u16::from(b'"'));
    if plain {
        line.extend_from_slice(&units);
        return;
    }
    line.push(u16::from(b'"'));
    let mut backslashes = 0usize;
    for &u in &units {
        if u == u16::from(b'\\') {
            backslashes += 1;
            continue;
        }
        if u == u16::from(b'"') {
            line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2 + 1));
        } else {
            line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
        }
        backslashes = 0;
        line.push(u);
    }
    line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    line.push(u16::from(b'"'));
}

/// The whole command line, NUL-terminated.
fn command_line(program: &Path, args: &[OsString]) -> Vec<u16> {
    let mut line = Vec::new();
    push_arg(&mut line, program.as_os_str());
    for arg in args {
        line.push(u16::from(b' '));
        push_arg(&mut line, arg);
    }
    line.push(0);
    line
}

/// What the client has written and nothing has read yet, filled by the
/// thread that reads the pseudoconsole's output pipe.
#[derive(Debug)]
pub(crate) struct Output {
    state: Mutex<(VecDeque<u8>, bool)>,
    /// Set while there is something to take — bytes, or the end.
    event: Event,
}

impl Output {
    fn new() -> io::Result<Self> {
        Ok(Self {
            state: Mutex::new((VecDeque::new(), false)),
            event: Event::new()?,
        })
    }

    fn lock(&self) -> MutexGuard<'_, (VecDeque<u8>, bool)> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn push(&self, bytes: &[u8]) {
        let mut st = self.lock();
        st.0.extend(bytes);
        self.event.set();
    }

    fn end(&self) {
        let mut st = self.lock();
        st.1 = true;
        self.event.set();
    }

    /// Whether a read would return at once: bytes, or the end.
    pub(crate) fn ready(&self) -> bool {
        let st = self.lock();
        !st.0.is_empty() || st.1
    }

    /// Take what there is. `Ok(0)` is the end; nothing yet is `WouldBlock`.
    pub(crate) fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut st = self.lock();
        if st.0.is_empty() {
            if st.1 {
                return Ok(0);
            }
            self.event.reset();
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let n = st.0.len().min(buf.len());
        for (slot, byte) in buf.iter_mut().zip(st.0.drain(..n)) {
            *slot = byte;
        }
        if st.0.is_empty() && !st.1 {
            self.event.reset();
        }
        Ok(n)
    }

    /// Wait up to `timeout` for something to take.
    pub(crate) fn wait(&self, timeout: Duration) -> bool {
        if self.ready() {
            return true;
        }
        // SAFETY: a valid event handle this owns.
        let rc = unsafe { WaitForSingleObject(self.event.handle(), millis(timeout)) };
        rc == WAIT_OBJECT_0 && self.ready()
    }

    /// The event a wait on several handles can include.
    pub(crate) fn event(&self) -> HANDLE {
        self.event.handle()
    }
}

/// The pseudoconsole's handle, closed once — by whichever of the exit watcher
/// and `Drop` gets there first.
struct Console(Mutex<Option<HPCON>>);

impl Console {
    fn close(&self) {
        let taken = self.0.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(hpc) = taken {
            // SAFETY: a pseudoconsole this created, closed exactly once.
            unsafe { ClosePseudoConsole(hpc) };
        }
    }

    fn resize(&self, size: PtySize) -> io::Result<()> {
        let guard = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(hpc) = *guard else {
            return Ok(());
        };
        // SAFETY: a live pseudoconsole, held open by the lock.
        let hr = unsafe { ResizePseudoConsole(hpc, coord(size)) };
        if hr < 0 {
            return Err(io::Error::other(format!(
                "ResizePseudoConsole: HRESULT {hr:#x}"
            )));
        }
        Ok(())
    }
}

/// The client, on a pseudoconsole of its own.
pub struct Pty {
    console: Arc<Console>,
    process: Arc<OwnedHandle>,
    pid: u32,
    size: Mutex<PtySize>,
    output: Arc<Output>,
    input: Arc<File>,
}

impl std::fmt::Debug for Pty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pty")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

/// What the client's input is written through: the pseudoconsole's input pipe.
struct Input(Arc<File>);

impl Write for Input {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        (&*self.0).write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&*self.0).flush()
    }
}

/// An anonymous pipe, read end first.
fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read: HANDLE = INVALID_HANDLE_VALUE;
    let mut write: HANDLE = INVALID_HANDLE_VALUE;
    // SAFETY: two out-pointers, no attributes: the handles are not inherited.
    check(unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) })?;
    Ok((owned(read)?, owned(write)?))
}

impl Pty {
    /// Start `cmd` on a new pseudoconsole of `size`, and hand back the writer
    /// its input goes through.
    pub fn spawn(cmd: Command, size: PtySize) -> io::Result<(Pty, Box<dyn Write + Send>)> {
        let program = resolve(&cmd.program)?;
        let (in_read, in_write) = pipe()?;
        let (out_read, out_write) = pipe()?;

        let mut hpc: HPCON = 0;
        // SAFETY: two valid pipe ends and an out-pointer. No flags: see the
        // module docs.
        let hr = unsafe {
            CreatePseudoConsole(
                coord(size),
                in_read.as_raw_handle(),
                out_write.as_raw_handle(),
                0,
                &mut hpc,
            )
        };
        if hr < 0 {
            return Err(io::Error::other(format!(
                "CreatePseudoConsole: HRESULT {hr:#x}"
            )));
        }
        // The pseudoconsole keeps its own copies of these two.
        drop(in_read);
        drop(out_write);
        let console = Arc::new(Console(Mutex::new(Some(hpc))));

        let process = match start(&program, &cmd, hpc) {
            Ok(started) => started,
            Err(e) => {
                console.close();
                return Err(e);
            }
        };
        let (process, pid) = (Arc::new(process.0), process.1);

        let output = Arc::new(Output::new()?);
        spawn_reader(File::from(out_read), Arc::clone(&output))?;
        spawn_watcher(Arc::clone(&process), Arc::clone(&console))?;

        let input = Arc::new(File::from(in_write));
        Ok((
            Pty {
                console,
                process,
                pid,
                size: Mutex::new(size),
                output,
                input: Arc::clone(&input),
            },
            Box::new(Input(input)),
        ))
    }

    pub fn resize(&self, size: PtySize) {
        if self.console.resize(size).is_ok() {
            *self.size.lock().unwrap_or_else(|p| p.into_inner()) = size;
        }
    }

    pub fn size(&self) -> Option<PtySize> {
        Some(*self.size.lock().unwrap_or_else(|p| p.into_inner()))
    }

    pub fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }

    /// End the client. There is no hangup to send a console process that it
    /// would act on and still go quickly, and nothing here needs one: a
    /// `--remote-ui` client killed outright leaves its server running, which
    /// is all a hangup was ever for.
    pub fn hang_up(&self) {
        if !self.try_wait() {
            // SAFETY: a process handle this owns.
            unsafe { TerminateProcess(self.process.as_raw_handle(), 1) };
        }
    }

    pub fn kill(&self) {
        self.hang_up();
    }

    /// Whether the client has exited.
    pub fn try_wait(&self) -> bool {
        // SAFETY: as above.
        unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) == WAIT_OBJECT_0 }
    }

    pub fn wait(&self) {
        // SAFETY: as above.
        unsafe { WaitForSingleObject(self.process.as_raw_handle(), INFINITE) };
    }

    /// The client's exit code, once it has one.
    pub fn exit_code(&self) -> Option<u32> {
        let mut code = 0u32;
        // SAFETY: as above, with an out-pointer.
        let ok = unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) };
        (ok != 0 && self.try_wait()).then_some(code)
    }

    pub(crate) fn output(&self) -> Arc<Output> {
        Arc::clone(&self.output)
    }

    /// Write `bytes` to the client's input in one write. A pipe here cannot be
    /// asked whether it has room, so this does not ask: a few bytes into a
    /// pipe with kilobytes of buffer, which the pseudoconsole reads for as
    /// long as it is up.
    pub fn write_now(&self, bytes: &[u8]) -> Option<isize> {
        (&*self.input).write(bytes).ok().map(|n| n as isize)
    }
}

/// Closing the pseudoconsole ends the client if it is somehow still there,
/// and its output with it; the reader thread takes that as the end.
impl Drop for Pty {
    fn drop(&mut self) {
        self.console.close();
    }
}

/// `CreateProcessW` on the pseudoconsole: the process handle and its pid.
fn start(program: &Path, cmd: &Command, hpc: HPCON) -> io::Result<(OwnedHandle, u32)> {
    let mut bytes = 0usize;
    // SAFETY: the documented size query, which fails and says how much.
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut bytes) };
    // u64s, for the list's pointer alignment.
    let mut list = vec![0u64; bytes.div_ceil(8)];
    let list_ptr = list.as_mut_ptr().cast();
    // SAFETY: a buffer of the size asked for.
    check(unsafe { InitializeProcThreadAttributeList(list_ptr, 1, 0, &mut bytes) })?;
    // SAFETY: the attribute's value is the pseudoconsole handle itself.
    let attached = check(unsafe {
        UpdateProcThreadAttribute(
            list_ptr,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            hpc as *const std::ffi::c_void,
            std::mem::size_of::<HPCON>(),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    });
    if let Err(e) = attached {
        // SAFETY: an initialised list.
        unsafe { DeleteProcThreadAttributeList(list_ptr) };
        return Err(e);
    }

    // SAFETY: plain data, filled in below.
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    // Stdio from the pseudoconsole and nowhere else: without this a client
    // can end up writing to whatever nvmux's own stdout was redirected to.
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    si.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    si.lpAttributeList = list_ptr;

    let application = wide(program.as_os_str());
    let mut line = command_line(program, &cmd.args);
    let cwd = cmd.cwd.as_ref().map(|dir| wide(dir.as_os_str()));
    // SAFETY: plain data, filled in by the call.
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: NUL-terminated wide strings that outlive the call, a mutable
    // command line as the call requires, and startup info whose attribute
    // list is initialised. No handles inherited: the pseudoconsole is how the
    // client gets its stdio.
    let created = check(unsafe {
        CreateProcessW(
            application.as_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT,
            std::ptr::null(),
            cwd.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            &si.StartupInfo,
            &mut pi,
        )
    });
    // SAFETY: an initialised list, no longer needed either way.
    unsafe { DeleteProcThreadAttributeList(list_ptr) };
    created?;
    // SAFETY: a thread handle the call gave us and nothing here uses.
    unsafe { CloseHandle(pi.hThread) };
    Ok((owned(pi.hProcess)?, pi.dwProcessId))
}

/// Read the pseudoconsole's output until it ends, into `output`.
fn spawn_reader(mut pipe: File, output: Arc<Output>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("nvmux-conpty-reader".into())
        .spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => output.push(&buf[..n]),
                }
            }
            output.end();
        })
        .map(drop)
}

/// Wait for the client to exit, then close its pseudoconsole: see the module
/// docs for why the exit alone ends nothing.
fn spawn_watcher(process: Arc<OwnedHandle>, console: Arc<Console>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("nvmux-conpty-watcher".into())
        .spawn(move || {
            // SAFETY: a process handle the Arc keeps open.
            unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) };
            console.close();
        })
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(program: &str, args: &[&str]) -> String {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let wide = command_line(Path::new(program), &args);
        String::from_utf16_lossy(&wide[..wide.len() - 1])
    }

    /// The C runtime's rules, which is what `nvim.exe` splits its command
    /// line with: a pipe path and plain flags go as they are, and anything
    /// with a space or a quote in it is quoted so it comes back one word.
    #[test]
    fn a_command_line_splits_back_into_the_same_arguments() {
        assert_eq!(
            line(
                r"C:\nvim\bin\nvim.exe",
                &["--server", r"\\.\pipe\nvmux-a-b-c", "--remote-ui"]
            ),
            r"C:\nvim\bin\nvim.exe --server \\.\pipe\nvmux-a-b-c --remote-ui"
        );
        assert_eq!(
            line(r"C:\Program Files\nvim.exe", &[]),
            r#""C:\Program Files\nvim.exe""#
        );
        assert_eq!(line("x", &["a b", ""]), r#"x "a b" """#);
        assert_eq!(line("x", &[r#"say "hi""#]), r#"x "say \"hi\"""#);
        assert_eq!(line("x", &[r"trailing\ "]), r#"x "trailing\ ""#);
        assert_eq!(line("x", &[r"ends\"]), r"x ends\");
        assert_eq!(line("x", &[r"sp ends\"]), r#"x "sp ends\\""#);
    }

    /// Read everything the client says until its output ends, or `within`
    /// runs out: what it said, and whether it ended.
    fn read_to_end(pty: &Pty, within: Duration) -> (String, bool) {
        let output = pty.output();
        let deadline = std::time::Instant::now() + within;
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        while std::time::Instant::now() < deadline {
            output.wait(Duration::from_millis(50));
            match output.read(&mut buf) {
                Ok(0) => return (String::from_utf8_lossy(&seen).into_owned(), true),
                Ok(n) => seen.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("reading the pseudoconsole: {e}"),
            }
        }
        (String::from_utf8_lossy(&seen).into_owned(), false)
    }

    /// What the client draws comes through, and its exit ends the output —
    /// the end the relay reads, as it reads a Unix pty's.
    #[test]
    fn a_clients_output_comes_through_and_its_exit_ends_it() {
        let mut cmd = Command::new("cmd");
        cmd.arg("/c");
        cmd.arg("echo");
        cmd.arg("nvmux-conpty-ok");
        let (pty, _input) = Pty::spawn(cmd, PtySize::default()).expect("spawn");
        let (said, ended) = read_to_end(&pty, Duration::from_secs(10));
        assert!(said.contains("nvmux-conpty-ok"), "the client said {said:?}");
        assert!(ended, "the output never ended once the client had exited");
        assert!(pty.try_wait());
    }

    /// Input written to the pseudoconsole reaches the client as typing.
    #[test]
    fn what_is_written_is_typed_to_the_client() {
        let mut cmd = Command::new("cmd");
        cmd.arg("/q");
        let (pty, mut input) = Pty::spawn(cmd, PtySize::default()).expect("spawn");
        input
            .write_all(b"echo nvmux-typed-%OS%\r\nexit\r\n")
            .expect("type");
        let (said, ended) = read_to_end(&pty, Duration::from_secs(10));
        assert!(
            said.contains("nvmux-typed-Windows_NT"),
            "the client said {said:?}"
        );
        assert!(ended, "the client did not exit when told to");
    }

    /// A client that would run for ever is ended by a hang-up, and its output
    /// with it; a resize before then is no trouble.
    #[test]
    fn a_hang_up_ends_a_client_that_would_not_end() {
        let (pty, _input) = Pty::spawn(Command::new("cmd"), PtySize::default()).expect("spawn");
        pty.resize(PtySize {
            rows: 30,
            cols: 100,
            ..PtySize::default()
        });
        assert_eq!(pty.size().map(|s| (s.rows, s.cols)), Some((30, 100)));
        assert!(!pty.try_wait(), "an interactive cmd waits for its input");
        pty.hang_up();
        let (_, ended) = read_to_end(&pty, Duration::from_secs(10));
        assert!(ended, "the output never ended after the hang-up");
        assert!(pty.try_wait());
    }

    /// A program that is not there is an error at once, not a client that
    /// never says anything.
    #[test]
    fn a_program_not_on_the_path_is_not_found() {
        let Err(err) = Pty::spawn(Command::new("nvmux-no-such-program"), PtySize::default()) else {
            panic!("a client was started with nothing to start");
        };
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
    }
}
