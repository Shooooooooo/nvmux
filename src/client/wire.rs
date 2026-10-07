//! The client's line to its session's server: msgpack-RPC over the session's
//! unix socket, read and written without blocking, so that one `poll` can wait
//! on it and on the terminal together (see [`super`]).
//!
//! Not [`crate::rpc`], which is synchronous on purpose and says why: on a bare
//! channel nothing arrives that was not asked for. This channel has a UI
//! attached, so the server speaks first and constantly — a `redraw`
//! notification per frame, as large as the screen — and the client has to go
//! on reading keys, the clock and the window size while it does.
//!
//! # Framing
//!
//! msgpack carries no length prefix: the one way to know that a message has
//! all arrived is to walk it. [`frame_len`] does that without building
//! anything, by counting the values still owed — one to start with, minus one
//! for every value read, plus however many an array or a map announces — so a
//! frame is whole the moment that count reaches zero, however it is nested.
//! Only a whole frame is ever decoded, and it is decoded from a copy of its own
//! bytes, so a frame split across reads is simply waited for.
//!
//! # Writing
//!
//! What the client says — keys, mouse reports, pastes, resizes — is queued in
//! an [`Outbox`] and written as the socket takes it. A server busy in a
//! `:!make` does not read its socket, and a client that blocked writing to it
//! would stop reading the terminal and stop drawing with it.
//!
//! Everything the client sends is a notification but for the two questions it
//! needs answered (see `super::App`): Neovim's own TUI sends its input the same
//! way, and an error in one comes back as an `nvim_error_event` notification
//! rather than as a reply nobody is waiting for.

use std::io::{self, Read, Write};

/// msgpack-RPC message kinds.
pub const REQUEST: u64 = 0;
pub const RESPONSE: u64 = 1;
pub const NOTIFICATION: u64 = 2;

/// What [`frame_len`] makes of bytes that cannot begin a msgpack value. The
/// connection is not worth reading past it: nothing after a bad byte can be
/// trusted to start where a value starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed {
    /// Where in the bytes the bad marker was.
    pub at: usize,
}

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not a msgpack value at byte {}", self.at)
    }
}

impl std::error::Error for Malformed {}

/// How many bytes the msgpack value at the start of `buf` takes, if all of
/// them are there: `Ok(None)` for a value still arriving.
///
/// A count of values still owed rather than a recursion, so a value nested a
/// thousand deep costs no stack — and a count that outgrows what is left of
/// the buffer is a value still arriving, decided at once, since every value
/// takes at least a byte. That is also what keeps a header announcing four
/// billion elements from being walked one element at a time.
pub fn frame_len(buf: &[u8]) -> Result<Option<usize>, Malformed> {
    let mut at = 0usize;
    let mut owed: u64 = 1;
    while owed > 0 {
        if owed > (buf.len() - at) as u64 {
            return Ok(None);
        }
        let Some(&marker) = buf.get(at) else {
            return Ok(None);
        };
        // The header's own length, the payload after it, and how many values
        // follow it as its elements.
        let (head, body, children): (usize, Option<usize>, u64) = match marker {
            0x00..=0x7f | 0xe0..=0xff | 0xc0 | 0xc2 | 0xc3 => (1, Some(0), 0),
            0x80..=0x8f => (1, Some(0), 2 * u64::from(marker & 0x0f)),
            0x90..=0x9f => (1, Some(0), u64::from(marker & 0x0f)),
            0xa0..=0xbf => (1, Some(usize::from(marker & 0x1f)), 0),
            0xc4 | 0xd9 => (2, len_at(buf, at + 1, 1), 0),
            0xc5 | 0xda => (3, len_at(buf, at + 1, 2), 0),
            0xc6 | 0xdb => (5, len_at(buf, at + 1, 4), 0),
            // ext 8/16/32: the length, then a type byte, then the payload.
            0xc7 => (3, len_at(buf, at + 1, 1), 0),
            0xc8 => (4, len_at(buf, at + 1, 2), 0),
            0xc9 => (6, len_at(buf, at + 1, 4), 0),
            0xca => (5, Some(0), 0),
            0xcb => (9, Some(0), 0),
            0xcc | 0xd0 => (2, Some(0), 0),
            0xcd | 0xd1 => (3, Some(0), 0),
            0xce | 0xd2 => (5, Some(0), 0),
            0xcf | 0xd3 => (9, Some(0), 0),
            // fixext 1/2/4/8/16: a type byte and a payload of that size.
            0xd4 => (3, Some(0), 0),
            0xd5 => (4, Some(0), 0),
            0xd6 => (6, Some(0), 0),
            0xd7 => (10, Some(0), 0),
            0xd8 => (18, Some(0), 0),
            0xdc => match len_at(buf, at + 1, 2) {
                Some(n) => (3, Some(0), n as u64),
                None => return Ok(None),
            },
            0xdd => match len_at(buf, at + 1, 4) {
                Some(n) => (5, Some(0), n as u64),
                None => return Ok(None),
            },
            0xde => match len_at(buf, at + 1, 2) {
                Some(n) => (3, Some(0), 2 * n as u64),
                None => return Ok(None),
            },
            0xdf => match len_at(buf, at + 1, 4) {
                Some(n) => (5, Some(0), 2 * n as u64),
                None => return Ok(None),
            },
            // 0xc1 is the one byte msgpack never uses.
            0xc1 => return Err(Malformed { at }),
        };
        let Some(body) = body else {
            return Ok(None);
        };
        let end = at.saturating_add(head).saturating_add(body);
        if end > buf.len() {
            return Ok(None);
        }
        at = end;
        owed = owed - 1 + children;
    }
    Ok(Some(at))
}

/// A big-endian length of `width` bytes at `at`, if it has arrived.
fn len_at(buf: &[u8], at: usize, width: usize) -> Option<usize> {
    let bytes = buf.get(at..at + width)?;
    Some(bytes.iter().fold(0usize, |n, &b| (n << 8) | usize::from(b)))
}

/// What the socket has said, kept until it makes whole frames.
#[derive(Debug, Default)]
pub struct Inbox {
    buf: Vec<u8>,
    /// Where the bytes not yet handed out as a frame start.
    start: usize,
}

/// How much one read asks for. A full redraw of a large screen is a few
/// hundred kilobytes; reading it in pieces this size keeps the number of
/// passes over a frame still arriving small (see [`Inbox::take_frame`]).
const READ_CHUNK: usize = 64 * 1024;

impl Inbox {
    /// Read whatever the socket has, without waiting. `Ok(false)` once the
    /// far end has closed: the server has gone, or the link to it.
    pub fn fill(&mut self, from: &mut impl Read) -> io::Result<bool> {
        // Compact first, so the buffer does not grow by every frame ever read.
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        loop {
            let len = self.buf.len();
            self.buf.resize(len + READ_CHUNK, 0);
            match from.read(&mut self.buf[len..]) {
                Ok(0) => {
                    self.buf.truncate(len);
                    return Ok(false);
                }
                Ok(n) => {
                    self.buf.truncate(len + n);
                    // A short read is everything there was.
                    if n < READ_CHUNK {
                        return Ok(true);
                    }
                }
                Err(e) => {
                    self.buf.truncate(len);
                    return match e.kind() {
                        io::ErrorKind::WouldBlock => Ok(true),
                        io::ErrorKind::Interrupted => continue,
                        _ => Err(e),
                    };
                }
            }
        }
    }

    /// The next whole frame's bytes, if one has arrived.
    pub fn take_frame(&mut self) -> Result<Option<Vec<u8>>, Malformed> {
        let rest = &self.buf[self.start..];
        match frame_len(rest) {
            Ok(Some(n)) => {
                let frame = rest[..n].to_vec();
                self.start += n;
                Ok(Some(frame))
            }
            Ok(None) => Ok(None),
            Err(Malformed { at }) => Err(Malformed {
                at: self.start + at,
            }),
        }
    }
}

/// One argument of something the client sends, as msgpack will spell it.
#[derive(Debug, Clone, PartialEq)]
pub enum Arg<'a> {
    Nil,
    Bool(bool),
    Int(i64),
    /// A msgpack `str`. Bytes rather than `&str`, because a paste is whatever
    /// the terminal sent and Neovim takes a `String` that is not UTF-8 as it
    /// comes.
    Str(&'a [u8]),
    Array(Vec<Arg<'a>>),
    Map(Vec<(&'a str, Arg<'a>)>),
}

impl<'a> Arg<'a> {
    /// A string argument.
    pub fn str(s: &'a str) -> Self {
        Arg::Str(s.as_bytes())
    }
}

/// What the client has to say, waiting for the socket to take it.
#[derive(Debug, Default)]
pub struct Outbox {
    buf: Vec<u8>,
    next_id: u32,
}

impl Outbox {
    /// Queue a notification: a call nothing waits for the answer to.
    pub fn notify(&mut self, method: &str, args: &[Arg<'_>]) {
        put_array_len(&mut self.buf, 3);
        put_uint(&mut self.buf, NOTIFICATION);
        put_str(&mut self.buf, method.as_bytes());
        put_array(&mut self.buf, args);
    }

    /// Queue a request, and say which reply will answer it.
    pub fn request(&mut self, method: &str, args: &[Arg<'_>]) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        put_array_len(&mut self.buf, 4);
        put_uint(&mut self.buf, REQUEST);
        put_uint(&mut self.buf, u64::from(id));
        put_str(&mut self.buf, method.as_bytes());
        put_array(&mut self.buf, args);
        id
    }

    /// Queue the answer to a request the server sent: the client serves
    /// nothing, and says so rather than leave the caller waiting for ever.
    pub fn refuse(&mut self, msgid: u64, why: &str) {
        put_array_len(&mut self.buf, 4);
        put_uint(&mut self.buf, RESPONSE);
        put_uint(&mut self.buf, msgid);
        put_str(&mut self.buf, why.as_bytes());
        self.buf.push(0xc0);
    }

    /// Whether anything is still waiting to be written.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Write as much as the socket takes without waiting, and keep the rest.
    pub fn send(&mut self, to: &mut impl Write) -> io::Result<()> {
        let mut sent = 0;
        while sent < self.buf.len() {
            match to.write(&self.buf[sent..]) {
                Ok(0) => break,
                Ok(n) => sent += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    self.buf.drain(..sent);
                    return Err(e);
                }
            }
        }
        self.buf.drain(..sent);
        Ok(())
    }

    /// What is queued, for the tests.
    #[cfg(test)]
    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }
}

fn put_array(buf: &mut Vec<u8>, items: &[Arg<'_>]) {
    put_array_len(buf, items.len());
    for item in items {
        put_arg(buf, item);
    }
}

fn put_arg(buf: &mut Vec<u8>, arg: &Arg<'_>) {
    match arg {
        Arg::Nil => buf.push(0xc0),
        Arg::Bool(false) => buf.push(0xc2),
        Arg::Bool(true) => buf.push(0xc3),
        Arg::Int(n) if *n >= 0 => put_uint(buf, *n as u64),
        Arg::Int(n) => put_negative(buf, *n),
        Arg::Str(s) => put_str(buf, s),
        Arg::Array(items) => put_array(buf, items),
        Arg::Map(pairs) => {
            put_map_len(buf, pairs.len());
            for (key, value) in pairs {
                put_str(buf, key.as_bytes());
                put_arg(buf, value);
            }
        }
    }
}

fn put_uint(buf: &mut Vec<u8>, n: u64) {
    match n {
        0..=0x7f => buf.push(n as u8),
        0x80..=0xff => buf.extend_from_slice(&[0xcc, n as u8]),
        0x100..=0xffff => {
            buf.push(0xcd);
            buf.extend_from_slice(&(n as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            buf.push(0xce);
            buf.extend_from_slice(&(n as u32).to_be_bytes());
        }
        _ => {
            buf.push(0xcf);
            buf.extend_from_slice(&n.to_be_bytes());
        }
    }
}

fn put_negative(buf: &mut Vec<u8>, n: i64) {
    debug_assert!(n < 0);
    if n >= -32 {
        buf.push(n as i8 as u8);
    } else if n >= i64::from(i8::MIN) {
        buf.extend_from_slice(&[0xd0, n as i8 as u8]);
    } else if n >= i64::from(i16::MIN) {
        buf.push(0xd1);
        buf.extend_from_slice(&(n as i16).to_be_bytes());
    } else if n >= i64::from(i32::MIN) {
        buf.push(0xd2);
        buf.extend_from_slice(&(n as i32).to_be_bytes());
    } else {
        buf.push(0xd3);
        buf.extend_from_slice(&n.to_be_bytes());
    }
}

fn put_str(buf: &mut Vec<u8>, s: &[u8]) {
    let n = s.len();
    match n {
        0..=31 => buf.push(0xa0 | n as u8),
        32..=0xff => buf.extend_from_slice(&[0xd9, n as u8]),
        0x100..=0xffff => {
            buf.push(0xda);
            buf.extend_from_slice(&(n as u16).to_be_bytes());
        }
        _ => {
            buf.push(0xdb);
            buf.extend_from_slice(&(n as u32).to_be_bytes());
        }
    }
    buf.extend_from_slice(s);
}

fn put_array_len(buf: &mut Vec<u8>, n: usize) {
    match n {
        0..=15 => buf.push(0x90 | n as u8),
        16..=0xffff => {
            buf.push(0xdc);
            buf.extend_from_slice(&(n as u16).to_be_bytes());
        }
        _ => {
            buf.push(0xdd);
            buf.extend_from_slice(&(n as u32).to_be_bytes());
        }
    }
}

fn put_map_len(buf: &mut Vec<u8>, n: usize) {
    match n {
        0..=15 => buf.push(0x80 | n as u8),
        16..=0xffff => {
            buf.push(0xde);
            buf.extend_from_slice(&(n as u16).to_be_bytes());
        }
        _ => {
            buf.push(0xdf);
            buf.extend_from_slice(&(n as u32).to_be_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmpv::Value;

    fn encoded(v: &Value) -> Vec<u8> {
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, v).expect("encode");
        buf
    }

    /// Every shape msgpack has, at both ends of its length classes, measured
    /// against the encoder that is the reference for what a frame is.
    #[test]
    fn a_whole_value_is_measured_exactly() {
        let long = "x".repeat(70_000);
        let values = [
            Value::Nil,
            Value::from(true),
            Value::from(5),
            Value::from(-5),
            Value::from(200),
            Value::from(-200),
            Value::from(70_000),
            Value::from(-70_000),
            Value::from(u64::MAX),
            Value::from(i64::MIN),
            Value::F32(1.5),
            Value::F64(2.5),
            Value::from("short"),
            Value::from("x".repeat(40)),
            Value::from("x".repeat(300)),
            Value::from(long.as_str()),
            Value::Binary(vec![1, 2, 3]),
            Value::Ext(1, vec![0xcd, 0x03, 0xe8]),
            Value::Ext(2, vec![7]),
            Value::Ext(3, vec![1; 16]),
            Value::Ext(4, vec![1; 17]),
            Value::Array((0..20).map(Value::from).collect()),
            Value::Map(vec![(Value::from("k"), Value::Array(vec![Value::Nil]))]),
            Value::Array(vec![Value::Array(vec![Value::Array(vec![])])]),
        ];
        for v in values {
            let bytes = encoded(&v);
            assert_eq!(frame_len(&bytes), Ok(Some(bytes.len())), "{v:?}");
            // And with the next frame's first byte behind it, which is not
            // taken.
            let mut more = bytes.clone();
            more.push(0x90);
            assert_eq!(frame_len(&more), Ok(Some(bytes.len())), "{v:?}");
        }
    }

    /// Cut anywhere short of its end, a frame is still arriving — never a
    /// shorter frame, and never an error.
    #[test]
    fn a_frame_cut_short_anywhere_is_still_arriving() {
        let v = Value::Array(vec![
            Value::from(2),
            Value::from("redraw"),
            Value::Array(vec![Value::Array(vec![
                Value::from("grid_line"),
                Value::Array(vec![
                    Value::from(1),
                    Value::from(0),
                    Value::from(0),
                    Value::Array(vec![Value::Array(vec![Value::from("é"), Value::from(3)])]),
                    Value::from(false),
                ]),
            ])]),
        ]);
        let bytes = encoded(&v);
        for cut in 0..bytes.len() {
            assert_eq!(frame_len(&bytes[..cut]), Ok(None), "cut at {cut}");
        }
    }

    /// A header announcing more elements than there are bytes left is
    /// answered at once, however many it announces.
    #[test]
    fn an_enormous_announced_length_is_not_walked() {
        let bytes = [0xdd, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert_eq!(frame_len(&bytes), Ok(None));
        let bytes = [0xdf, 0xff, 0xff, 0xff, 0xff];
        assert_eq!(frame_len(&bytes), Ok(None));
    }

    #[test]
    fn the_one_unused_marker_is_malformed() {
        assert_eq!(frame_len(&[0x92, 0x01, 0xc1]), Err(Malformed { at: 2 }));
    }

    /// Frames come out whole and in order however the bytes were split by
    /// the reads that brought them.
    #[test]
    fn the_inbox_hands_out_whole_frames_in_order() {
        let a = encoded(&Value::Array(vec![Value::from(2), Value::from("a")]));
        let b = encoded(&Value::Array(vec![Value::from(2), Value::from("b")]));
        let mut all = a.clone();
        all.extend_from_slice(&b);
        let mut inbox = Inbox::default();
        let (first, second) = all.split_at(a.len() + 1);
        inbox.fill(&mut &first[..]).expect("read");
        assert_eq!(inbox.take_frame(), Ok(Some(a.clone())));
        assert_eq!(inbox.take_frame(), Ok(None), "b is not all here yet");
        inbox.fill(&mut &second[..]).expect("read");
        assert_eq!(inbox.take_frame(), Ok(Some(b.clone())));
        assert_eq!(inbox.take_frame(), Ok(None));
    }

    /// The far end closing is what the caller ends on.
    #[test]
    fn a_closed_socket_says_so() {
        let mut inbox = Inbox::default();
        assert!(!inbox.fill(&mut &b""[..]).expect("read"));
    }

    /// What the outbox writes is what rmpv reads back, in every shape the
    /// client sends.
    #[test]
    fn the_outbox_speaks_msgpack_rpc() {
        let mut out = Outbox::default();
        out.notify(
            "nvim_ui_attach",
            &[
                Arg::Int(80),
                Arg::Int(-3),
                Arg::Map(vec![("rgb", Arg::Bool(true)), ("x", Arg::Nil)]),
            ],
        );
        let id = out.request("nvim_list_uis", &[]);
        out.refuse(9, "no");
        out.notify("nvim_paste", &[Arg::Str(b"\xff\xfe"), Arg::Int(-70_000)]);

        let mut rd = out.bytes();
        let note = rmpv::decode::read_value(&mut rd).expect("note");
        assert_eq!(
            note,
            Value::Array(vec![
                Value::from(2),
                Value::from("nvim_ui_attach"),
                Value::Array(vec![
                    Value::from(80),
                    Value::from(-3),
                    Value::Map(vec![
                        (Value::from("rgb"), Value::from(true)),
                        (Value::from("x"), Value::Nil),
                    ]),
                ]),
            ])
        );
        let req = rmpv::decode::read_value(&mut rd).expect("request");
        assert_eq!(
            req,
            Value::Array(vec![
                Value::from(0),
                Value::from(id),
                Value::from("nvim_list_uis"),
                Value::Array(vec![]),
            ])
        );
        let refusal = rmpv::decode::read_value(&mut rd).expect("refusal");
        assert_eq!(
            refusal,
            Value::Array(vec![
                Value::from(1),
                Value::from(9),
                Value::from("no"),
                Value::Nil
            ])
        );
        let paste = rmpv::decode::read_value(&mut rd).expect("paste");
        let args = paste.as_array().expect("array")[2].clone();
        let args = args.as_array().expect("args");
        // Bytes that are not UTF-8 go as a `str` all the same.
        assert_eq!(args[0].as_slice(), Some(&b"\xff\xfe"[..]));
        assert_eq!(args[1], Value::from(-70_000));
        assert!(rd.is_empty());
    }

    /// Requests are told apart by their ids.
    #[test]
    fn every_request_has_its_own_id() {
        let mut out = Outbox::default();
        let a = out.request("a", &[]);
        let b = out.request("b", &[]);
        assert_ne!(a, b);
    }

    /// A socket that takes part of a write keeps the rest for next time.
    #[test]
    fn what_the_socket_does_not_take_is_kept() {
        struct Narrow(Vec<u8>);
        impl Write for Narrow {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.0.len() >= 4 {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                let n = buf.len().min(4 - self.0.len());
                self.0.extend_from_slice(&buf[..n]);
                Ok(n)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut out = Outbox::default();
        out.notify("nvim_input", &[Arg::str("abc")]);
        let whole = out.bytes().to_vec();
        let mut narrow = Narrow(Vec::new());
        out.send(&mut narrow).expect("send");
        assert_eq!(narrow.0, whole[..4]);
        assert_eq!(out.bytes(), &whole[4..]);
    }
}
