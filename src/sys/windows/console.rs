//! The console nvmux itself runs in.
//!
//! On Unix nvmux reads its terminal as a stream of bytes, with the line
//! discipline switched off, and learns of a resize from `SIGWINCH`. A Windows
//! console hands a program *input records* instead — a key going down, a key
//! coming up, the window changing size — and has no signal for the resize but
//! one of those records. So this puts the console in the mode that makes the
//! records carry a terminal's bytes (`ENABLE_VIRTUAL_TERMINAL_INPUT`: every key
//! arrives as the characters of its VT sequence, one key-down record each),
//! and turns records back into that stream, noting a resize on the way.
//!
//! The one byte a record cannot carry as a character is NUL, which is what
//! `Ctrl-Space` — the default prefix — is. A console reports it as a key-down
//! with no character; this takes one with no key code either (a translated
//! byte), or the space or `2` key with Ctrl held, as the NUL it spells. A
//! prefix of `Ctrl-<letter>` is a byte that does have a character, and needs
//! none of that.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetNumberOfConsoleInputEvents, GetStdHandle, ReadConsoleInputW,
    SetConsoleCtrlHandler, SetConsoleMode, CONSOLE_MODE, DISABLE_NEWLINE_AUTO_RETURN,
    ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_INSERT_MODE, ENABLE_LINE_INPUT,
    ENABLE_MOUSE_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT, ENABLE_QUICK_EDIT_MODE,
    ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WINDOW_INPUT,
    INPUT_RECORD, KEY_EVENT, LEFT_CTRL_PRESSED, RIGHT_CTRL_PRESSED, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE, WINDOW_BUFFER_SIZE_EVENT,
};

use super::check;

const VK_SPACE: u16 = 0x20;
const VK_2: u16 = 0x32;

pub(crate) fn stdin_handle() -> HANDLE {
    // SAFETY: a query with no preconditions.
    unsafe { GetStdHandle(STD_INPUT_HANDLE) }
}

pub(crate) fn stdout_handle() -> HANDLE {
    // SAFETY: as above.
    unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }
}

fn mode(handle: HANDLE) -> io::Result<CONSOLE_MODE> {
    let mut mode = 0;
    // SAFETY: an out-pointer; a handle that is not a console fails cleanly.
    check(unsafe { GetConsoleMode(handle, &mut mode) })?;
    Ok(mode)
}

fn set_mode(handle: HANDLE, mode: CONSOLE_MODE) -> io::Result<()> {
    // SAFETY: as above.
    check(unsafe { SetConsoleMode(handle, mode) })
}

/// A console's two modes, as they were before nvmux changed them.
#[derive(Debug, Clone, Copy)]
pub struct Modes {
    input: CONSOLE_MODE,
    output: CONSOLE_MODE,
}

/// Put the console in raw mode for a relay, or for the colour query: no line
/// editing, no echo, Ctrl-C as a key rather than a signal — the Unix raw
/// mode's `ISIG` off — keys as VT bytes, a resize as a record, and escape
/// sequences interpreted on the way out. Says what it replaced.
pub fn enter_raw() -> io::Result<Modes> {
    let (input, output) = (stdin_handle(), stdout_handle());
    let saved = Modes {
        input: mode(input)?,
        output: mode(output)?,
    };
    let raw_input =
        (saved.input | ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_WINDOW_INPUT | ENABLE_EXTENDED_FLAGS)
            & !(ENABLE_LINE_INPUT
                | ENABLE_ECHO_INPUT
                | ENABLE_PROCESSED_INPUT
                | ENABLE_QUICK_EDIT_MODE
                | ENABLE_MOUSE_INPUT
                | ENABLE_INSERT_MODE);
    set_mode(input, raw_input)?;
    if let Err(e) = set_mode(output, saved.output | vt_output()) {
        let _ = set_mode(input, saved.input);
        return Err(e);
    }
    Ok(saved)
}

/// Escape sequences interpreted rather than printed, and no newline that
/// returns the carriage by itself — what every byte nvmux writes assumes.
fn vt_output() -> CONSOLE_MODE {
    ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN
}

/// Put back what [`enter_raw`] replaced.
pub fn restore(saved: Modes) -> io::Result<()> {
    let input = set_mode(stdin_handle(), saved.input);
    let output = set_mode(stdout_handle(), saved.output);
    input.and(output)
}

/// The modes the console had when nvmux started, for the handler below to put
/// back whatever happens. Atomics, since the handler runs on a thread of the
/// system's.
static SHELL_INPUT: AtomicU32 = AtomicU32::new(0);
static SHELL_OUTPUT: AtomicU32 = AtomicU32::new(0);
static SHELL_SAVED: AtomicBool = AtomicBool::new(false);

/// What the handler writes on its way out; [`crate::term`]'s reset.
static RESET: std::sync::OnceLock<&'static [u8]> = std::sync::OnceLock::new();

/// The Windows side of the Unix signal safety net: remember the console's
/// modes as the shell left them, turn on the escape-sequence processing every
/// screen of nvmux's relies on, and put everything back if the console is
/// closed, or Ctrl-C or Ctrl-Break reach nvmux while it is not in raw mode.
///
/// A console control handler runs on a thread of its own rather than in a
/// signal context, so it may do what it likes; it returns `FALSE`, which lets
/// the default handler end the process as it would have.
pub fn install_safety_net(reset: &'static [u8]) {
    let _ = RESET.set(reset);
    if !SHELL_SAVED.load(Ordering::SeqCst) {
        if let (Ok(input), Ok(output)) = (mode(stdin_handle()), mode(stdout_handle())) {
            SHELL_INPUT.store(input, Ordering::SeqCst);
            SHELL_OUTPUT.store(output, Ordering::SeqCst);
            SHELL_SAVED.store(true, Ordering::SeqCst);
            let _ = set_mode(stdout_handle(), output | vt_output());
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: a handler that touches only atomics and the console.
        unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) };
    });
}

unsafe extern "system" fn on_ctrl(_event: u32) -> i32 {
    if SHELL_SAVED.load(Ordering::SeqCst) {
        let _ = set_mode(stdin_handle(), SHELL_INPUT.load(Ordering::SeqCst));
        // Escape sequences still interpreted for the reset below; the shell's
        // own output mode comes back after it.
        let _ = set_mode(
            stdout_handle(),
            SHELL_OUTPUT.load(Ordering::SeqCst) | vt_output(),
        );
    }
    if let Some(reset) = RESET.get() {
        use std::io::Write;
        let _ = std::io::stdout().write_all(reset);
        let _ = std::io::stdout().flush();
    }
    if SHELL_SAVED.load(Ordering::SeqCst) {
        let _ = set_mode(stdout_handle(), SHELL_OUTPUT.load(Ordering::SeqCst));
    }
    0
}

/// The console's input as the bytes a terminal would have sent, with a resize
/// noted wherever one went by.
#[derive(Debug)]
pub struct ConsoleInput {
    handle: HANDLE,
    bytes: VecDeque<u8>,
    /// The first half of a character outside the Basic Multilingual Plane,
    /// waiting for its second.
    high: Option<u16>,
    resized: bool,
}

impl ConsoleInput {
    pub fn new() -> Self {
        Self {
            handle: stdin_handle(),
            bytes: VecDeque::new(),
            high: None,
            resized: false,
        }
    }

    /// The handle a wait can include: signalled while there are records.
    pub fn handle(&self) -> HANDLE {
        self.handle
    }

    /// How many records are waiting, without taking any.
    pub fn waiting(&self) -> io::Result<u32> {
        let mut n = 0;
        // SAFETY: an out-pointer.
        check(unsafe { GetNumberOfConsoleInputEvents(self.handle, &mut n) })?;
        Ok(n)
    }

    /// Take every record there is now, without waiting for more.
    pub fn drain(&mut self) -> io::Result<()> {
        let mut records = [INPUT_RECORD::default(); 128];
        while self.waiting()? > 0 {
            let mut read = 0u32;
            // SAFETY: a buffer of `records.len()` records and an out-pointer.
            check(unsafe {
                ReadConsoleInputW(
                    self.handle,
                    records.as_mut_ptr(),
                    records.len() as u32,
                    &mut read,
                )
            })?;
            for record in &records[..read as usize] {
                self.take(record);
            }
            if read == 0 {
                break;
            }
        }
        Ok(())
    }

    fn take(&mut self, record: &INPUT_RECORD) {
        match u32::from(record.EventType) {
            KEY_EVENT => {
                // SAFETY: the record says which member of the union it is.
                let key = unsafe { record.Event.KeyEvent };
                if key.bKeyDown == 0 {
                    return;
                }
                // SAFETY: as above.
                let unit = unsafe { key.uChar.UnicodeChar };
                let ctrl = key.dwControlKeyState & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0;
                let nul = unit == 0
                    && (key.wVirtualKeyCode == 0
                        || (ctrl && matches!(key.wVirtualKeyCode, VK_SPACE | VK_2)));
                if unit == 0 && !nul {
                    // A modifier on its own, or a key with nothing to say.
                    return;
                }
                for _ in 0..key.wRepeatCount.max(1) {
                    self.push(unit);
                }
            }
            WINDOW_BUFFER_SIZE_EVENT => self.resized = true,
            // Focus and menu records, and mouse records the VT input mode
            // does not send: nothing a terminal would have said.
            _ => {}
        }
    }

    /// One UTF-16 unit of input, as the UTF-8 a terminal sends.
    fn push(&mut self, unit: u16) {
        let units: Vec<u16> = match (self.high.take(), unit) {
            (None, 0xD800..=0xDBFF) => {
                self.high = Some(unit);
                return;
            }
            (Some(high), 0xDC00..=0xDFFF) => vec![high, unit],
            (Some(high), _) => vec![high, unit],
            (None, _) => vec![unit],
        };
        for c in char::decode_utf16(units) {
            let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
            let mut buf = [0u8; 4];
            self.bytes.extend(c.encode_utf8(&mut buf).as_bytes());
        }
    }

    pub fn has_bytes(&self) -> bool {
        !self.bytes.is_empty()
    }

    /// Take what has been typed so far.
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let n = self.bytes.len().min(buf.len());
        for (slot, byte) in buf.iter_mut().zip(self.bytes.drain(..n)) {
            *slot = byte;
        }
        n
    }

    /// Whether a resize has gone by since this was last asked.
    pub fn take_resized(&mut self) -> bool {
        std::mem::take(&mut self.resized)
    }
}

impl Default for ConsoleInput {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Console::{KEY_EVENT_RECORD, KEY_EVENT_RECORD_0};

    fn key(down: bool, unit: u16, vk: u16, state: u32) -> INPUT_RECORD {
        let mut record = INPUT_RECORD {
            EventType: KEY_EVENT as u16,
            ..Default::default()
        };
        record.Event.KeyEvent = KEY_EVENT_RECORD {
            bKeyDown: i32::from(down),
            wRepeatCount: 1,
            wVirtualKeyCode: vk,
            wVirtualScanCode: 0,
            uChar: KEY_EVENT_RECORD_0 { UnicodeChar: unit },
            dwControlKeyState: state,
        };
        record
    }

    fn typed(records: &[INPUT_RECORD]) -> Vec<u8> {
        let mut input = ConsoleInput::new();
        for r in records {
            input.take(r);
        }
        let mut buf = [0u8; 64];
        let n = input.read(&mut buf);
        buf[..n].to_vec()
    }

    /// A VT sequence arrives a character a record, and goes out as its bytes;
    /// the key coming back up says nothing.
    #[test]
    fn key_downs_become_the_bytes_they_carry() {
        let up_arrow: Vec<_> = "\x1b[A"
            .encode_utf16()
            .map(|u| key(true, u, 0, 0))
            .collect();
        assert_eq!(typed(&up_arrow), b"\x1b[A");
        assert_eq!(
            typed(&[
                key(true, u16::from(b'x'), 0, 0),
                key(false, u16::from(b'x'), 0, 0)
            ]),
            b"x"
        );
    }

    /// Ctrl-Space is NUL, the default prefix, and the one byte a record
    /// cannot carry as a character.
    #[test]
    fn ctrl_space_is_a_nul_however_the_console_spells_it() {
        assert_eq!(typed(&[key(true, 0, 0, 0)]), [0], "a translated NUL");
        assert_eq!(typed(&[key(true, 0, VK_SPACE, LEFT_CTRL_PRESSED)]), [0]);
        assert_eq!(typed(&[key(true, 0, VK_2, RIGHT_CTRL_PRESSED)]), [0]);
        // A modifier going down on its own is not a keystroke.
        assert_eq!(typed(&[key(true, 0, 0x11, LEFT_CTRL_PRESSED)]), b"");
    }

    /// Outside the Basic Multilingual Plane a character is two records.
    #[test]
    fn a_surrogate_pair_is_one_character() {
        let crab: Vec<_> = "🦀".encode_utf16().map(|u| key(true, u, 0, 0)).collect();
        assert_eq!(crab.len(), 2);
        assert_eq!(typed(&crab), "🦀".as_bytes());
    }

    #[test]
    fn a_resize_is_noted_and_types_nothing() {
        let mut input = ConsoleInput::new();
        let record = INPUT_RECORD {
            EventType: WINDOW_BUFFER_SIZE_EVENT as u16,
            ..Default::default()
        };
        input.take(&record);
        assert!(input.take_resized());
        assert!(!input.take_resized(), "asked once");
        assert!(!input.has_bytes());
    }
}
