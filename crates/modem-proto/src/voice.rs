// SPDX-License-Identifier: Apache-2.0
//! QMI VOICE (service 9, v2): calls (docs/telephony.md C5). Message ids and TLV layouts from libqmi's
//! `qmi-service-voice.json` (facts), checked against the C1 capture where it had them (incoming calls: the All Call
//! Status indications, Answer, End); Dial is from libqmi only.
//! C7 parses the complete indication atomically; malformed/overflowing lists never replace a live snapshot.

use crate::qmi::Msg;
use alloc::string::String;

pub const SERVICE: u32 = 9;
pub const INDICATION_REGISTER: u16 = 0x03;
pub const DIAL: u16 = 0x20;
pub const END: u16 = 0x21;
pub const ANSWER: u16 = 0x22;
pub const ALL_CALL_STATUS_IND: u16 = 0x2e;
pub const MANAGE_CALLS: u16 = 0x31;
/// a keypad tone to the far end while held (libqmi Start/Stop Continuous DTMF: TLV 1 {call id, digit} / {call id})
pub const START_CONT_DTMF: u16 = 0x29;
pub const STOP_CONT_DTMF: u16 = 0x2a;

/// Indication Register: call notifications (0x13) on.
pub const INDICATIONS: [(u8, &[u8]); 1] = [(0x13, &[1])];

/// The first call of an All Call Status indication.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CallInfo {
    pub id: u8,
    pub state: u8,
    /// direction 2 = mobile terminated
    pub incoming: bool,
    pub number: Option<String>,
}

/// TLV 0x01 = a count, then 7 bytes per call {id, state, type, direction, mode, multipart, als}; TLV 0x10 = the
/// remote numbers: a count, then {call id, presentation, u8 length, digits}.
pub fn all_call_status(m: &Msg) -> Option<CallInfo> {
    all_calls(m)?.into_iter().next()
}

pub fn all_calls(m: &Msg) -> Option<alloc::vec::Vec<CallInfo>> {
    let t = m.tlv(0x01)?;
    let count = usize::from(*t.first()?);
    if count > modem_ipc::MAX_CALLS {
        return None;
    }
    let entries = t.get(1..1 + 7 * count)?;
    let mut calls = alloc::vec::Vec::with_capacity(count);
    for c in entries.as_chunks::<7>().0 {
        if c[0] == 0 || calls.iter().any(|x: &CallInfo| x.id == c[0]) {
            return None;
        }
        calls.push(CallInfo {
            id: c[0],
            state: c[1],
            incoming: c[3] == 2,
            number: None,
        });
    }
    if let Some(r) = m.tlv(0x10) {
        let mut at = 1;
        for _ in 0..*r.first()? {
            let len = usize::from(*r.get(at + 2)?);
            let digits = r.get(at + 3..at + 3 + len)?;
            if let Some(c) = calls.iter_mut().find(|c| c.id == r[at]) {
                c.number = core::str::from_utf8(digits).ok().map(String::from);
            }
            at += 3 + len;
        }
    }
    Some(calls)
}

/// A number we may dial: digits with an optional leading `+`, `*` and `#`, at least 5 digits. The length floor keeps
/// tests away from emergency and other short codes (112, 911, 101..104): don't rely on this phone for them.
pub fn dialable(n: &str) -> bool {
    let body = n.strip_prefix('+').unwrap_or(n);
    body.bytes()
        .all(|c| c.is_ascii_digit() || c == b'*' || c == b'#')
        && body.bytes().filter(u8::is_ascii_digit).count() >= 5
        && n.len() <= 32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qmi::{Builder, INDICATION};
    use modem_ipc::call;

    fn ind(calls: &[u8], numbers: Option<&[u8]>) -> alloc::vec::Vec<u8> {
        let b = Builder::new(INDICATION, 1, ALL_CALL_STATUS_IND).tlv(1, calls);
        match numbers {
            Some(n) => b.tlv(0x10, n),
            None => b,
        }
        .finish()
    }

    // C1 mm.log, an incoming call: setup (0x0a) -> incoming -> conversation -> disconnecting -> end; MT, UMTS
    #[test]
    fn incoming_call_sequence_from_c1() {
        let mut nums = alloc::vec![1, 1, 0, 6];
        nums.extend_from_slice(b"099012");
        for (state, want) in [
            (0x0a, call::SETUP),
            (2, call::INCOMING),
            (3, call::CONVERSATION),
            (8, call::DISCONNECTING),
            (9, call::END),
        ] {
            let b = ind(&[1, 1, state, 0, 2, 3, 0, 0], Some(&nums));
            let c = all_call_status(&Msg::parse(&b).unwrap()).unwrap();
            assert_eq!((c.id, c.state, c.incoming), (1, want, true));
            assert_eq!(c.number.as_deref(), Some("099012"));
        }
    }

    #[test]
    fn no_calls_or_no_number() {
        let b = ind(&[0], None);
        assert_eq!(all_call_status(&Msg::parse(&b).unwrap()), None);
        let b = ind(&[1, 2, 5, 0, 1, 3, 0, 0], None);
        let c = all_call_status(&Msg::parse(&b).unwrap()).unwrap();
        assert_eq!(
            (c.state, c.incoming, c.number),
            (call::ALERTING, false, None)
        );
    }

    #[test]
    fn dial_guard() {
        assert!(dialable("+37400000012"));
        assert!(dialable("099000012"));
        for n in ["112", "911", "103", "+112", "", "12ab5678", "*#06#"] {
            assert!(!dialable(n), "{n}");
        }
    }
}
