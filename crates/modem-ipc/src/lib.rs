// SPDX-License-Identifier: Apache-2.0
//! The NeoModem server's contract (docs/telephony.md C5): MODEMD.EXE serves it, neoui's SYSSTART is its client.
//! - [`op::STATUS`]: the server writes [`Status::SIZE`] bytes ([`Status::to_bytes`]) into the des8 in arg 0;
//! - [`op::DIAL`]: arg 0 = the number (des8, ASCII digits, `+`, `*`, `#`);
//! - [`op::ANSWER`], [`op::HANGUP`]: no arguments;
//! - in a call (C6): [`op::MUTE`] arg 0 = 1/0 (the mic), [`op::SPEAKER`] arg 0 = 1/0 (loudspeaker or earpiece),
//!   [`op::VOLUME`] arg 0 = 0..=10, [`op::DTMF`] arg 0 = the key (`0-9`, `*`, `#`) or -1 to stop its tone;
//! - no call (v1.2, Dev tools): [`op::AUDIO_TEST`] arg 0 = 1 loudspeaker, 2 earpiece (the mic looped back to the
//!   amps), 0 stop; KErrInUse (-14) during a call.
//!
//! Each op completes with KErrNone or an error: KErrNotReady (-18) without a modem/call to act on, KErrArgument (-6)
//! for a number it refuses.
#![no_std]

pub const NAME: &str = "NeoModem";
/// major, minor, build
pub const VERSION: (i8, i8, i16) = (1, 2, 0);

pub mod op {
    pub const STATUS: i32 = 1;
    pub const DIAL: i32 = 2;
    pub const ANSWER: i32 = 3;
    pub const HANGUP: i32 = 4;
    pub const MUTE: i32 = 5;
    pub const SPEAKER: i32 = 6;
    pub const VOLUME: i32 = 7;
    pub const DTMF: i32 = 8;
    pub const AUDIO_TEST: i32 = 9;
}

/// [`Status::modem`]
pub mod modem {
    pub const BOOTING: u8 = 0;
    /// booted and serving, not yet online (radio off)
    pub const RUNNING: u8 = 1;
    pub const ONLINE: u8 = 2;
    pub const FAILED: u8 = 3;
}

/// [`Status::sim`]
pub mod sim {
    pub const UNKNOWN: u8 = 0;
    pub const ABSENT: u8 = 1;
    pub const PRESENT: u8 = 2;
    pub const ERROR: u8 = 3;
}

/// [`Status::reg`]
pub mod reg {
    pub const NONE: u8 = 0;
    pub const SEARCHING: u8 = 1;
    pub const HOME: u8 = 2;
    pub const ROAMING: u8 = 3;
    pub const DENIED: u8 = 4;
}

/// [`Status::rat`]
pub mod rat {
    pub const NONE: u8 = 0;
    pub const GSM: u8 = 1;
    pub const UMTS: u8 = 2;
    pub const LTE: u8 = 3;
}

/// [`Call::state`]: QMI VOICE call states (libqmi `QmiVoiceCallState`).
pub mod call {
    pub const NONE: u8 = 0;
    pub const ORIGINATION: u8 = 1;
    pub const INCOMING: u8 = 2;
    pub const CONVERSATION: u8 = 3;
    pub const CC_IN_PROGRESS: u8 = 4;
    pub const ALERTING: u8 = 5;
    pub const HOLD: u8 = 6;
    pub const WAITING: u8 = 7;
    pub const DISCONNECTING: u8 = 8;
    pub const END: u8 = 9;
    pub const SETUP: u8 = 10;
}

const TEXT: usize = 32;

/// Short text in a fixed buffer (operator names, phone numbers); longer input is cut at 32 bytes.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Text {
    len: u8,
    buf: [u8; TEXT],
}

impl Text {
    pub fn new(s: &str) -> Self {
        let mut t = Text::default();
        let mut n = s.len().min(TEXT);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        t.buf[..n].copy_from_slice(&s.as_bytes()[..n]);
        t.len = n as u8;
        t
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }
}

/// The one call shown (C5 handles a single call).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Call {
    pub id: u8,
    pub state: u8,
    pub incoming: bool,
    pub number: Text,
}

/// The call's audio (C6): what the in-call buttons show.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Audio {
    /// the audio path runs (a call is connected and the ADSP carries it)
    pub on: bool,
    pub muted: bool,
    pub speaker: bool,
    /// 0..=10
    pub volume: u8,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Status {
    pub modem: u8,
    pub sim: u8,
    pub reg: u8,
    pub rat: u8,
    /// 0..=4
    pub bars: u8,
    pub operator: Text,
    pub call: Call,
    pub audio: Audio,
}

impl Status {
    /// v1.0's 74 bytes, then the audio's 4 (v1.1); a v1.0 reader takes the first 74
    pub const SIZE: usize = 8 + 2 * (1 + TEXT) + 4;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        b[..8].copy_from_slice(&[
            self.modem,
            self.sim,
            self.reg,
            self.rat,
            self.bars,
            self.call.id,
            self.call.state,
            self.call.incoming as u8,
        ]);
        for (i, t) in [self.operator, self.call.number].iter().enumerate() {
            let at = 8 + i * (1 + TEXT);
            b[at] = t.len;
            b[at + 1..at + 1 + TEXT].copy_from_slice(&t.buf);
        }
        let a = self.audio;
        b[Self::SIZE - 4..].copy_from_slice(&[
            a.on as u8,
            a.muted as u8,
            a.speaker as u8,
            a.volume,
        ]);
        b
    }

    /// None if `b` is too short or a text length is out of range (a v1.0 server's 74 bytes: no audio).
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let audio = b
            .get(Self::SIZE - 4..Self::SIZE)
            .map_or(Audio::default(), |a| Audio {
                on: a[0] != 0,
                muted: a[1] != 0,
                speaker: a[2] != 0,
                volume: a[3],
            });
        let b = b.get(..Self::SIZE - 4)?;
        let text = |i: usize| {
            let at = 8 + i * (1 + TEXT);
            let len = b[at];
            let mut buf = [0u8; TEXT];
            buf.copy_from_slice(&b[at + 1..at + 1 + TEXT]);
            (len as usize <= TEXT).then_some(Text { len, buf })
        };
        Some(Status {
            modem: b[0],
            sim: b[1],
            reg: b[2],
            rat: b[3],
            bars: b[4],
            operator: text(0)?,
            call: Call {
                id: b[5],
                state: b[6],
                incoming: b[7] != 0,
                number: text(1)?,
            },
            audio,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let s = Status {
            modem: modem::ONLINE,
            sim: sim::PRESENT,
            reg: reg::HOME,
            rat: rat::LTE,
            bars: 3,
            operator: Text::new("Ucom AM"),
            call: Call {
                id: 1,
                state: call::INCOMING,
                incoming: true,
                number: Text::new("+37400000012"),
            },
            audio: Audio {
                on: true,
                muted: true,
                speaker: false,
                volume: 7,
            },
        };
        let b = s.to_bytes();
        assert_eq!(Status::from_bytes(&b), Some(s));
        assert_eq!(Status::from_bytes(&b[..10]), None);
        // a v1.0 server: no audio bytes
        let old = Status::from_bytes(&b[..Status::SIZE - 4]).unwrap();
        assert_eq!((old.call, old.audio), (s.call, Audio::default()));
    }

    #[test]
    fn text_is_cut_on_a_char_boundary() {
        let long = "ééééééééééééééééééééé"; // 21 x 2 bytes
        assert_eq!(Text::new(long).as_str().len(), 32);
        assert_eq!(Text::new("Ucom").as_str(), "Ucom");
    }
}
