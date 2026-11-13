//! Messages between the launcher and its Chromium host process (`omni-browser --omni-host`).
//! Each message is `[u32 length][u8 tag][u32 page id][payload]`, little-endian, where the
//! length counts everything after itself. Shared by both sides through `#[path]`.
#![allow(dead_code)] // each side uses its own half

// Launcher → host.
pub const OPEN: u8 = 1; // css: str
pub const CLOSE: u8 = 2;
pub const LOAD: u8 = 3; // url: str
pub const STOP: u8 = 4;
pub const RESIZE: u8 = 5; // w, h
pub const VISIBLE: u8 = 6; // shown: u8
pub const FOCUS: u8 = 7; // focused: u8
pub const MOUSE_MOVE: u8 = 8; // x, y, modifiers, leave: u8
pub const MOUSE_CLICK: u8 = 9; // x, y, modifiers, button, up: u8, count
pub const WHEEL: u8 = 10; // x, y, modifiers, dx, dy
pub const KEY: u8 = 11; // kind, windows code, native code, modifiers, character

// Host → launcher.
pub const FRAME: u8 = 101; // w, h, BGRA pixels
pub const LOADING: u8 = 102; // loading: u8
pub const ERROR: u8 = 103; // message: str (empty clears it)
pub const READY: u8 = 104;
pub const COPIED: u8 = 105; // text: str
/// The engine itself failed; page id 0. The host exits after sending it.
pub const FATAL: u8 = 106; // message: str

// Key event kinds, matching cef_key_event_type_t.
pub const KEY_RAWDOWN: i32 = 0;
pub const KEY_UP: i32 = 2;
pub const KEY_CHAR: i32 = 3;
// Mouse buttons, matching cef_mouse_button_type_t.
pub const BUTTON_LEFT: i32 = 0;
pub const BUTTON_MIDDLE: i32 = 1;
pub const BUTTON_RIGHT: i32 = 2;

/// Larger messages are a corrupt stream, not a frame (frames are capped at 64 MiB).
pub const MAX_MESSAGE: usize = 65 * 1024 * 1024;

pub struct Writer(Vec<u8>);

impl Writer {
    pub fn new(tag: u8, id: u32) -> Self {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&[0; 4]);
        buf.push(tag);
        buf.extend_from_slice(&id.to_le_bytes());
        Self(buf)
    }
    pub fn with_capacity(tag: u8, id: u32, extra: usize) -> Self {
        let mut w = Self::new(tag, id);
        w.0.reserve(extra);
        w
    }
    pub fn int(mut self, v: i32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn flag(mut self, v: bool) -> Self {
        self.0.push(v as u8);
        self
    }
    pub fn str(mut self, s: &str) -> Self {
        self.0.extend_from_slice(&(s.len() as u32).to_le_bytes());
        self.0.extend_from_slice(s.as_bytes());
        self
    }
    pub fn bytes(mut self, b: &[u8]) -> Self {
        self.0.extend_from_slice(b);
        self
    }
    pub fn finish(mut self) -> Vec<u8> {
        let len = (self.0.len() - 4) as u32;
        self.0[..4].copy_from_slice(&len.to_le_bytes());
        self.0
    }
}

/// One decoded message: its tag, page id, and a cursor over the payload.
pub struct Message<'a> {
    pub tag: u8,
    pub id: u32,
    rest: &'a [u8],
}

impl<'a> Message<'a> {
    pub fn int(&mut self) -> Option<i32> {
        let (head, rest) = self.rest.split_first_chunk::<4>()?;
        self.rest = rest;
        Some(i32::from_le_bytes(*head))
    }
    pub fn flag(&mut self) -> Option<bool> {
        let (&b, rest) = self.rest.split_first()?;
        self.rest = rest;
        Some(b != 0)
    }
    pub fn str(&mut self) -> Option<String> {
        let (head, rest) = self.rest.split_first_chunk::<4>()?;
        let len = u32::from_le_bytes(*head) as usize;
        let text = rest.get(..len)?;
        self.rest = &rest[len..];
        String::from_utf8(text.to_vec()).ok()
    }
    pub fn rest(&self) -> &'a [u8] {
        self.rest
    }
}

/// Split complete messages off the front of `buf`. Returns how many bytes were consumed,
/// or `Err` when the stream is corrupt.
pub fn parse<'a>(buf: &'a [u8], out: &mut Vec<Message<'a>>) -> Result<usize, ()> {
    let mut at = 0;
    while let Some(head) = buf.get(at..at + 4) {
        let len = u32::from_le_bytes(head.try_into().unwrap()) as usize;
        if !(5..=MAX_MESSAGE).contains(&len) {
            return Err(());
        }
        let Some(body) = buf.get(at + 4..at + 4 + len) else {
            break;
        };
        out.push(Message {
            tag: body[0],
            id: u32::from_le_bytes(body[1..5].try_into().unwrap()),
            rest: &body[5..],
        });
        at += 4 + len;
    }
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_and_partial_tails_wait() {
        let mut stream = Writer::new(LOAD, 7).str("https://x/").finish();
        stream.extend(Writer::new(RESIZE, 7).int(704).int(-1).finish());
        let full = stream.len();
        stream.extend(&Writer::new(CLOSE, 7).finish()[..3]);
        let mut out = Vec::new();
        assert_eq!(parse(&stream, &mut out), Ok(full));
        assert_eq!((out[0].tag, out[0].id), (LOAD, 7));
        assert_eq!(out[0].str().as_deref(), Some("https://x/"));
        assert_eq!((out[1].int(), out[1].int(), out[1].int()), (Some(704), Some(-1), None));
    }

    #[test]
    fn corrupt_lengths_are_rejected() {
        let mut out = Vec::new();
        assert!(parse(&[1, 0, 0, 0, 9], &mut out).is_err());
        assert!(parse(&u32::MAX.to_le_bytes(), &mut out).is_err());
    }
    #[test]
    fn every_split_of_a_message_waits_for_the_complete_payload() {
        let frame = Writer::new(LOAD, 42).str("https://example.test/é?q=☃").finish();
        for split in 0..frame.len() {
            let mut messages = Vec::new();
            assert_eq!(parse(&frame[..split], &mut messages), Ok(0), "split {split}");
            assert!(messages.is_empty());
        }
        let mut messages = Vec::new();
        assert_eq!(parse(&frame, &mut messages), Ok(frame.len()));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].str().as_deref(), Some("https://example.test/é?q=☃"));
    }

    #[test]
    fn byte_at_a_time_stream_preserves_order_and_exactly_once_delivery() {
        let mut wire = Writer::new(LOAD, 8).str("one").finish();
        wire.extend(Writer::new(LOAD, 9).str("two").finish());
        wire.extend(Writer::new(CLOSE, 8).finish());
        let mut pending = Vec::new();
        let mut received = Vec::new();
        for byte in wire {
            pending.push(byte);
            let consumed = {
                let mut messages = Vec::new();
                let consumed = parse(&pending, &mut messages).unwrap();
                received.extend(messages.iter_mut().map(|m| (m.tag, m.id, m.str())));
                consumed
            };
            pending.drain(..consumed);
        }
        assert!(pending.is_empty());
        assert_eq!(received, vec![(LOAD, 8, Some("one".into())), (LOAD, 9, Some("two".into())), (CLOSE, 8, None)]);
    }

    #[test]
    fn payload_readers_reject_truncation_and_invalid_utf8() {
        for payload in [vec![0, 0, 0], vec![5, 0, 0, 0, b'a'], vec![1, 0, 0, 0, 0xff]] {
            let frame = Writer::new(LOAD, 1).bytes(&payload).finish();
            let mut messages = Vec::new();
            parse(&frame, &mut messages).unwrap();
            assert_eq!(messages[0].str(), None);
        }
        let frame = Writer::new(VISIBLE, 1).flag(false).flag(true).int(i32::MIN).finish();
        let mut messages = Vec::new();
        parse(&frame, &mut messages).unwrap();
        let message = &mut messages[0];
        assert_eq!(message.flag(), Some(false));
        assert_eq!(message.flag(), Some(true));
        assert_eq!(message.int(), Some(i32::MIN));
        assert_eq!(message.int(), None);
        assert_eq!(message.flag(), None);
    }

    #[test]
    fn frame_length_limits_distinguish_incomplete_from_invalid() {
        for length in [0, 1, 4, MAX_MESSAGE as u32 + 1, u32::MAX] {
            assert!(parse(&length.to_le_bytes(), &mut Vec::new()).is_err());
        }
        for length in [5, MAX_MESSAGE as u32] {
            assert_eq!(parse(&length.to_le_bytes(), &mut Vec::new()), Ok(0));
        }
    }

}
