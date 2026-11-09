// SPDX-License-Identifier: Apache-2.0
//! QMI NAS (service 3): registration, the serving network's name and the signal (docs/telephony.md C5).
//! Message ids and TLV layouts from libqmi's `qmi-service-nas.json` (facts), checked against the C1 capture
//! (ModemManager on the same phone and SIM, 2026-11-08).

use crate::qmi::Msg;
use modem_ipc::{rat, reg};

pub const SERVICE: u32 = 3;
pub const REGISTER_INDICATIONS: u16 = 0x03;
pub const GET_SERVING_SYSTEM: u16 = 0x24; // also the serving system indication
pub const GET_PLMN_NAME: u16 = 0x44;
pub const SET_SYSTEM_SELECTION: u16 = 0x33;
pub const GET_SYSTEM_SELECTION: u16 = 0x34;
pub const GET_SIGNAL_INFO: u16 = 0x4f;
pub const SIGNAL_INFO_IND: u16 = 0x51;

/// Register Indications: serving system (0x13) and signal info (0x19) on.
pub const INDICATIONS: [(u8, &[u8]); 2] = [(0x13, &[1]), (0x19, &[1])];

/// Set System Selection Preference for voice only: mode preference (0x11) GSM | UMTS, service domain (0x18) CS only,
/// change duration (0x17) until power cycle, so nothing is persisted in the modem's NV (libqmi layouts).
pub const VOICE_ONLY: [(u8, &[u8]); 3] =
    [(0x11, &[0x0c, 0x00]), (0x18, &[0, 0, 0, 0]), (0x17, &[0])];

/// Get System Selection Preference: the mode preference bitmask (TLV 0x11; C1 read 0x1f = all five).
pub fn mode_preference(m: &Msg) -> Option<u16> {
    let t = m.tlv(0x11)?;
    Some(u16::from_le_bytes([*t.first()?, *t.get(1)?]))
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Serving {
    pub reg: u8,
    pub rat: u8,
    /// circuit- and packet-switched attach (1 attached, 2 detached): CS carries calls (CSFB from LTE)
    pub cs: u8,
    pub ps: u8,
    /// MCC, MNC and the network's description, when the modem gave them (TLV 0x12)
    pub plmn: Option<(u16, u16)>,
    pub name: Option<alloc::string::String>,
}

/// Get Serving System response / indication. TLV 0x01 = registration state, CS and PS attach, network type,
/// a radio interface count and that many radio interfaces; 0x10 = roaming indicator (0 on); 0x12 = current PLMN
/// {u16 mcc, u16 mnc, u8 length, description}.
pub fn serving(m: &Msg) -> Option<Serving> {
    let t = m.tlv(0x01)?;
    let n = *t.get(4)? as usize;
    let radio = if n > 0 { *t.get(5)? } else { 0 };
    let roaming = m.u8(0x10) == Some(0);
    let state = match t[0] {
        1 if roaming => reg::ROAMING,
        1 => reg::HOME,
        2 => reg::SEARCHING,
        3 => reg::DENIED,
        _ => reg::NONE,
    };
    let rat = if state == reg::HOME || state == reg::ROAMING {
        match radio {
            4 => rat::GSM,
            5 | 9 => rat::UMTS, // UMTS, TD-SCDMA
            8 => rat::LTE,
            _ => rat::NONE,
        }
    } else {
        rat::NONE
    };
    let mut s = Serving {
        reg: state,
        rat,
        cs: *t.get(1)?,
        ps: *t.get(2)?,
        ..Default::default()
    };
    if let Some(p) = m.tlv(0x12).filter(|p| p.len() >= 5) {
        s.plmn = Some((
            u16::from_le_bytes([p[0], p[1]]),
            u16::from_le_bytes([p[2], p[3]]),
        ));
        s.name = p.get(5..5 + p[4] as usize).and_then(ascii);
    }
    Some(s)
}

/// Get PLMN Name response TLV 0x10: SPN {enc, len, bytes}, short and long names {enc, ci, spare, len, bytes}.
/// The long name, else the short one, else the SPN; only ASCII ones (enc 0 = "unspecified", ASCII in C1).
pub fn plmn_name(m: &Msg) -> Option<alloc::string::String> {
    let t = m.tlv(0x10)?;
    let spn_len = *t.get(1)? as usize;
    let spn = (t[0] == 0).then(|| t.get(2..2 + spn_len)).flatten();
    let mut at = 2 + spn_len;
    let mut names = [None, None];
    for n in &mut names {
        let enc = *t.get(at)?;
        let len = *t.get(at + 3)? as usize;
        let s = t.get(at + 4..at + 4 + len)?;
        *n = (enc == 0).then_some(s);
        at += 4 + len;
    }
    [names[1], names[0], spn]
        .into_iter()
        .flatten()
        .find_map(ascii)
}

/// Bars 0..=4 for the technology in use (`modem_ipc::rat`) from Get Signal Info / the Signal Info indication: LTE
/// RSRP (TLV 0x14 {i8 rssi, i8 rsrq, i16 rsrp, i16 snr}), WCDMA (0x13 {i8 rssi, i16 ecio}) or GSM (0x12 {i8 rssi})
/// RSSI. On 3G the modem still sends an LTE TLV with invalid values (C5), so only the current one counts. None when
/// that TLV is missing or invalid.
pub fn bars(m: &Msg, rat: u8) -> Option<u8> {
    // my call: the usual phone thresholds
    let steps = |v: i32, top: i32| (0..4).filter(|i| v >= top - 10 * i).count() as u8;
    match rat {
        rat::LTE => {
            let l = m.tlv(0x14).filter(|l| l.len() >= 4)?;
            let rsrp = i16::from_le_bytes([l[2], l[3]]) as i32;
            (-140..0).contains(&rsrp).then(|| steps(rsrp, -85))
        }
        rat::UMTS | rat::GSM => {
            let t = if rat == rat::UMTS { 0x13 } else { 0x12 };
            let rssi = *m.tlv(t)?.first()? as i8 as i32;
            (-120..0).contains(&rssi).then(|| steps(rssi, -77))
        }
        _ => None,
    }
}

fn ascii(b: &[u8]) -> Option<alloc::string::String> {
    let s = core::str::from_utf8(b).ok()?.trim();
    (!s.is_empty() && s.bytes().all(|c| (0x20..0x7f).contains(&c))).then(|| s.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qmi::{Builder, INDICATION, RESPONSE};

    fn msg(b: &[u8]) -> Msg<'_> {
        Msg::parse(b).unwrap()
    }

    // C1 mm.log: searching, then registered on LTE and on UMTS (CSFB for a call), roaming off
    #[test]
    fn serving_system_from_c1() {
        let b = Builder::new(RESPONSE, 1, GET_SERVING_SYSTEM)
            .result(0)
            .tlv(1, &[2, 2, 2, 0, 1, 0])
            .finish();
        let s = serving(&msg(&b)).unwrap();
        assert_eq!((s.reg, s.rat), (reg::SEARCHING, rat::NONE));
        let mut plmn = alloc::vec::Vec::new();
        plmn.extend_from_slice(&283u16.to_le_bytes());
        plmn.extend_from_slice(&10u16.to_le_bytes());
        plmn.push(7);
        plmn.extend_from_slice(b"Ucom AM");
        let b = Builder::new(INDICATION, 2, GET_SERVING_SYSTEM)
            .tlv(1, &[1, 1, 1, 2, 1, 8])
            .u8(0x10, 1)
            .tlv(0x12, &plmn)
            .finish();
        let s = serving(&msg(&b)).unwrap();
        assert_eq!(
            (s.reg, s.rat, s.plmn),
            (reg::HOME, rat::LTE, Some((283, 10)))
        );
        assert_eq!(s.name.as_deref(), Some("Ucom AM"));
        let b = Builder::new(INDICATION, 3, GET_SERVING_SYSTEM)
            .tlv(1, &[1, 1, 1, 2, 1, 5])
            .u8(0x10, 0)
            .finish();
        let s = serving(&msg(&b)).unwrap();
        assert_eq!((s.reg, s.rat, s.name), (reg::ROAMING, rat::UMTS, None));
    }

    // C1 mm.log: Get PLMN Name 283/10 -> SPN "Ucom", short "Ucom", long "Ucom AM", all encoding 0
    #[test]
    fn plmn_name_from_c1() {
        let mut t = alloc::vec![0, 4];
        t.extend_from_slice(b"Ucom");
        t.extend_from_slice(&[0, 0, 0, 4]);
        t.extend_from_slice(b"Ucom");
        t.extend_from_slice(&[0, 0, 0, 7]);
        t.extend_from_slice(b"Ucom AM");
        let b = Builder::new(RESPONSE, 4, GET_PLMN_NAME)
            .result(0)
            .tlv(0x10, &t)
            .finish();
        assert_eq!(plmn_name(&msg(&b)).as_deref(), Some("Ucom AM"));
    }

    // C1 mm.log: LTE {rssi -54, rsrq -18, rsrp -93, snr 36}; WCDMA {rssi -78, ecio 7}
    #[test]
    fn bars_from_c1_signal() {
        let b = Builder::new(INDICATION, 5, SIGNAL_INFO_IND)
            .tlv(0x14, &[0xca, 0xee, 0xa3, 0xff, 0x24, 0x00])
            .finish();
        assert_eq!(bars(&msg(&b), rat::LTE), Some(3));
        let b = Builder::new(INDICATION, 6, SIGNAL_INFO_IND)
            .tlv(0x13, &[0xb2, 0x07, 0x00])
            .finish();
        assert_eq!(bars(&msg(&b), rat::UMTS), Some(3));
        let b = Builder::new(INDICATION, 7, SIGNAL_INFO_IND)
            .tlv(0x12, &[(-110i8) as u8])
            .finish();
        assert_eq!(bars(&msg(&b), rat::GSM), Some(0));
        let b = Builder::new(INDICATION, 8, SIGNAL_INFO_IND).finish();
        assert_eq!(bars(&msg(&b), rat::LTE), None);
    }

    // C5 on the phone: on 3G an LTE TLV with invalid values comes along; the WCDMA one counts
    #[test]
    fn bars_follow_the_technology() {
        let b = Builder::new(INDICATION, 9, SIGNAL_INFO_IND)
            .tlv(0x14, &[0x80, 0x80, 0x00, 0x80, 0x00, 0x80])
            .tlv(0x13, &[0xb2, 0x07, 0x00])
            .finish();
        assert_eq!(bars(&msg(&b), rat::UMTS), Some(3));
        assert_eq!(bars(&msg(&b), rat::LTE), None);
        assert_eq!(bars(&msg(&b), rat::NONE), None);
    }
}
