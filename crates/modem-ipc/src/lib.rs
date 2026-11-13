// SPDX-License-Identifier: Apache-2.0
//! The NeoModem server's contract (docs/telephony.md C5): MODEMD.EXE serves it, neoui's SYSSTART is its client.
//! - [`op::STATUS`]: the server writes [`Status::SIZE`] bytes ([`Status::to_bytes`]) into the des8 in arg 0;
//! - [`op::DIAL`]: arg 0 = the number (des8, ASCII digits, `+`, `*`, `#`);
//! - [`op::ANSWER`], [`op::HANGUP`]: arg 0 = call ID, or 0 for the selected call;
//! - in a call (C6): [`op::MUTE`] arg 0 = 1/0 (the mic), [`op::SPEAKER`] arg 0 = 1/0 (loudspeaker or earpiece),
//!   [`op::VOLUME`] arg 0 = 0..=10, [`op::DTMF`] arg 0 = the key (`0-9`, `*`, `#`) or -1 to stop its tone;
//! - no call (Dev tools): [`op::AUDIO_TEST`] arg 0 = 1 loudspeaker, 2 earpiece (the mic looped back to the
//!   amps), 0 stop; KErrInUse (-14) during a call.
//!
//! Each op completes with KErrNone or an error: KErrNotReady (-18) without a modem/call to act on, KErrArgument (-6)
//! for a number it refuses.
#![no_std]

pub const NAME: &str = "NeoModem";
/// major, minor, build
pub const VERSION: (i8, i8, i16) = (2, 0, 0);

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
    /// arg0 writable Status, arg1 last revision. One outstanding request per session.
    pub const NOTIFY: i32 = 10;
    pub const CANCEL_NOTIFY: i32 = 11;
    /// arg0 manage action; arg1 target call ID (0 = primary).
    pub const MANAGE: i32 = 12;
    /// arg0 1 = airplane, 0 = online.
    pub const AIRPLANE: i32 = 13;
    /// arg0 ASCII PIN descriptor. Never persisted or logged.
    pub const VERIFY_PIN: i32 = 14;
    /// arg0 volume 0..10, arg1 silent, arg2 vibration.
    pub const RING_SETTINGS: i32 = 15;
    pub const RETRY: i32 = 16;
}

/// [`Status::modem`]
pub mod modem {
    pub const BOOTING: u8 = 0;
    /// booted and serving, not yet online (radio off)
    pub const RUNNING: u8 = 1;
    pub const ONLINE: u8 = 2;
    pub const FAILED: u8 = 3;
    pub const RECOVERING: u8 = 4;
}

/// [`Status::sim`]
pub mod sim {
    pub const UNKNOWN: u8 = 0;
    pub const ABSENT: u8 = 1;
    pub const PRESENT: u8 = 2;
    pub const ERROR: u8 = 3;
    pub const PIN_REQUIRED: u8 = 4;
    pub const BLOCKED: u8 = 5;
}

/// Manage-calls actions. Answer/end additionally accept arg0 call ID (0 selects primary).
pub mod manage {
    pub const HOLD: i32 = 1;
    pub const RESUME: i32 = 2;
    pub const SWAP: i32 = 3;
    pub const MERGE: i32 = 4;
}

pub const MAX_CALLS: usize = 8;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Calls {
    pub len: u8,
    pub entries: [Call; MAX_CALLS],
}

impl Calls {
    pub fn as_slice(&self) -> &[Call] {
        &self.entries[..usize::from(self.len).min(MAX_CALLS)]
    }
    pub fn find(&self, id: u8) -> Option<&Call> {
        self.as_slice().iter().find(|c| c.id == id)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ring {
    pub volume: u8,
    pub silent: bool,
    pub vibrate: bool,
}
impl Default for Ring {
    fn default() -> Self {
        Self {
            volume: 7,
            silent: false,
            vibrate: true,
        }
    }
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

/// One modem call. Status.call is the selected UI call; Status.calls is authoritative.
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

/// The network's time (NITZ: QMI NAS Network Time), for the phone's automatic time.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Nitz {
    /// UTC as Unix seconds when the network sent it; 0 = none yet
    pub utc: u32,
    /// the local zone in quarter hours east of UTC, daylight saving included (3GPP TS 24.008)
    pub tz: i8,
    /// the daylight saving part of `tz`, in hours
    pub dst: u8,
    /// counts the network times received: a new one is a new value even if `utc` didn't move
    pub seq: u16,
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
    pub calls: Calls,
    /// Changes only when a new snapshot is published. NOTIFY compares this word.
    pub revision: u32,
    pub generation: u32,
    /// One user command outstanding; result is Symbian error or negative QMI error.
    pub pending: bool,
    pub result: i32,
    pub airplane: bool,
    pub radio_online: bool,
    /// 255 means unavailable.
    pub pin_attempts: u8,
    pub ring: Ring,
    pub missed: u32,
    pub nitz: Nitz,
}

impl Status {
    pub const SIZE: usize = 78 + 32 + MAX_CALLS * 36;

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
            let at = 8 + i * 33;
            b[at] = t.len;
            b[at + 1..at + 33].copy_from_slice(&t.buf);
        }
        b[74..78].copy_from_slice(&[
            self.audio.on as u8,
            self.audio.muted as u8,
            self.audio.speaker as u8,
            self.audio.volume,
        ]);
        b[78..82].copy_from_slice(&self.revision.to_le_bytes());
        b[82..86].copy_from_slice(&self.generation.to_le_bytes());
        b[86..90].copy_from_slice(&self.result.to_le_bytes());
        b[90..98].copy_from_slice(&[
            self.pending as u8,
            self.airplane as u8,
            self.radio_online as u8,
            self.pin_attempts,
            self.ring.volume,
            self.ring.silent as u8,
            self.ring.vibrate as u8,
            self.calls.len,
        ]);
        b[98..102].copy_from_slice(&self.missed.to_le_bytes());
        b[102..106].copy_from_slice(&self.nitz.utc.to_le_bytes());
        b[106] = self.nitz.tz as u8;
        b[107] = self.nitz.dst;
        b[108..110].copy_from_slice(&self.nitz.seq.to_le_bytes());
        for (i, c) in self.calls.as_slice().iter().enumerate() {
            let at = 110 + i * 36;
            b[at..at + 4].copy_from_slice(&[c.id, c.state, c.incoming as u8, c.number.len]);
            b[at + 4..at + 36].copy_from_slice(&c.number.buf);
        }
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let b = b.get(..Self::SIZE)?;
        let text = |at: usize| {
            let len = b[at];
            let mut buf = [0; 32];
            buf.copy_from_slice(&b[at + 1..at + 33]);
            (len <= 32 && core::str::from_utf8(&buf[..usize::from(len.min(32))]).is_ok())
                .then_some(Text { len, buf })
        };
        let mut calls = Calls {
            len: b[97],
            ..Default::default()
        };
        if usize::from(calls.len) > MAX_CALLS {
            return None;
        }
        for i in 0..usize::from(calls.len) {
            let at = 110 + i * 36;
            calls.entries[i] = Call {
                id: b[at],
                state: b[at + 1],
                incoming: b[at + 2] != 0,
                number: text(at + 3)?,
            };
        }
        Some(Self {
            modem: b[0],
            sim: b[1],
            reg: b[2],
            rat: b[3],
            bars: b[4],
            operator: text(8)?,
            call: Call {
                id: b[5],
                state: b[6],
                incoming: b[7] != 0,
                number: text(41)?,
            },
            audio: Audio {
                on: b[74] != 0,
                muted: b[75] != 0,
                speaker: b[76] != 0,
                volume: b[77],
            },
            revision: u32::from_le_bytes(b[78..82].try_into().ok()?),
            generation: u32::from_le_bytes(b[82..86].try_into().ok()?),
            result: i32::from_le_bytes(b[86..90].try_into().ok()?),
            pending: b[90] != 0,
            airplane: b[91] != 0,
            radio_online: b[92] != 0,
            pin_attempts: b[93],
            ring: Ring {
                volume: b[94],
                silent: b[95] != 0,
                vibrate: b[96] != 0,
            },
            calls,
            missed: u32::from_le_bytes(b[98..102].try_into().ok()?),
            nitz: Nitz {
                utc: u32::from_le_bytes(b[102..106].try_into().ok()?),
                tz: b[106] as i8,
                dst: b[107],
                seq: u16::from_le_bytes(b[108..110].try_into().ok()?),
            },
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
            ..Status::default()
        };
        let b = s.to_bytes();
        assert_eq!(Status::from_bytes(&b), Some(s));
        assert_eq!(Status::from_bytes(&b[..10]), None);
        assert_eq!(Status::from_bytes(&b[..78]), None);
    }

    #[test]
    fn text_is_cut_on_a_char_boundary() {
        let long = "ééééééééééééééééééééé"; // 21 x 2 bytes
        assert_eq!(Text::new(long).as_str().len(), 32);
        assert_eq!(Text::new("Ucom").as_str(), "Ucom");
    }
    #[test]
    fn v2_round_trip_preserves_call_ids_and_radio_result() {
        let mut s = Status {
            revision: 3,
            generation: 8,
            pending: true,
            result: -1012,
            airplane: true,
            pin_attempts: 2,
            missed: 7,
            nitz: Nitz {
                utc: 1_794_513_007,
                tz: -14,
                dst: 1,
                seq: 3,
            },
            ..Default::default()
        };
        s.calls.len = 2;
        s.calls.entries[0] = Call {
            id: 8,
            state: call::CONVERSATION,
            incoming: false,
            number: Text::new("+37400000001"),
        };
        s.calls.entries[1] = Call {
            id: 2,
            state: call::WAITING,
            incoming: true,
            number: Text::new("+37400000002"),
        };
        s.call = s.calls.entries[1];
        let mut bytes = s.to_bytes();
        assert_eq!(Status::from_bytes(&bytes), Some(s));
        bytes[97] = 9;
        assert!(Status::from_bytes(&bytes).is_none());
        bytes[97] = 2;
        bytes[113] = 33;
        assert!(Status::from_bytes(&bytes).is_none());
    }
}
