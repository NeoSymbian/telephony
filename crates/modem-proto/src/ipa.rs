// SPDX-License-Identifier: Apache-2.0
//! The IPA QMI handshake (docs/telephony.md C5b): once the kernel has IPA running, the modem must be told where IPA's
//! tables live (INIT_DRIVER), and it answers when its side is up (DRIVER_INIT_COMPLETE, INDICATION_REGISTER); then
//! we send INIT_COMPLETE. Service 0x31: we serve instance 1, the modem serves instance 2.
//! Message ids, TLV layouts and the IPA v3.5.1 values from Linux drivers/net/ipa (ipa_qmi.c, ipa_qmi_msg.c,
//! ipa_data-v3.5.1.c; facts). Every address is an offset in IPA's SRAM (its shared-memory offset is 0 here, which the
//! kernel checks).

use crate::qmi::{Builder, Msg, RESPONSE};
use alloc::vec::Vec;

pub const SERVICE: u32 = 0x31;
pub const VERSION: u32 = 1;
pub const INSTANCE: u32 = 1;
pub const INDICATION_REGISTER: u16 = 0x20;
pub const INIT_DRIVER: u16 = 0x21;
pub const INIT_COMPLETE: u16 = 0x22;
pub const DRIVER_INIT_COMPLETE: u16 = 0x35;

fn pair(a: u32, b: u32) -> [u8; 8] {
    let mut v = [0u8; 8];
    v[..4].copy_from_slice(&a.to_le_bytes());
    v[4..].copy_from_slice(&b.to_le_bytes());
    v
}

/// INIT_DRIVER's TLVs for IPA v3.5.1 (Linux init_modem_driver_req): platform MSM_ANDROID, the header table, the
/// route tables (start, last index 7), filter starts, the modem's SRAM, the control endpoint (10, AP_MODEM_RX),
/// skip_uc_load 0, the header processing contexts, the hashed tables.
pub fn init_driver_tlvs() -> Vec<(u8, Vec<u8>)> {
    let u32v = |v: u32| v.to_le_bytes().to_vec();
    alloc::vec![
        (0x10, u32v(3)),
        (0x11, pair(0x688, 0x7c7).to_vec()),
        (0x12, pair(0x508, 7).to_vec()),
        (0x13, pair(0x608, 7).to_vec()),
        (0x14, u32v(0x308)),
        (0x15, u32v(0x408)),
        (0x16, pair(0xbd8, 0x1024).to_vec()),
        (0x17, u32v(10)),
        (0x18, alloc::vec![0]),
        (0x19, pair(0x7d0, 0x9cf).to_vec()),
        (0x1b, pair(0x488, 7).to_vec()),
        (0x1c, pair(0x588, 7).to_vec()),
        (0x1d, u32v(0x288)),
        (0x1e, u32v(0x388)),
    ]
}

/// Where the handshake is.
#[derive(Default, Debug)]
pub struct Handshake {
    pub modem_ready: bool,
    pub uc_ready: bool,
    pub indications: bool,
    pub complete_sent: bool,
}

impl Handshake {
    /// A request from the modem to our server: the response to send back, or None for one Linux has no handler for
    /// (its QMI core then sends nothing). An error answer to the modem's INSTALL_FILTER_RULE (0x23) or CONFIG (0x27)
    /// makes it assert ("ipa_qmi_client.c:754 ... resp.result == QMI_RESULT_SUCCESS", C6 2026-11-09); silence, as
    /// under C1 Linux, it lives with.
    pub fn request(&mut self, m: &Msg) -> Option<Vec<u8>> {
        let r = Builder::new(RESPONSE, m.txn, m.id);
        match m.id {
            DRIVER_INIT_COMPLETE => {
                self.uc_ready = true;
                Some(r.result(0).finish())
            }
            INDICATION_REGISTER => {
                self.indications = true;
                Some(r.result(0).finish())
            }
            _ => None,
        }
    }

    /// True once: when everything is in, INIT_COMPLETE goes out.
    pub fn complete_due(&mut self) -> bool {
        let due = self.modem_ready && self.uc_ready && self.indications && !self.complete_sent;
        if due {
            self.complete_sent = true;
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qmi::REQUEST;

    #[test]
    fn init_driver_is_linuxs_123_bytes() {
        let mut b = Builder::new(REQUEST, 1, INIT_DRIVER);
        for (t, v) in init_driver_tlvs() {
            b = b.tlv(t, &v);
        }
        let msg = b.finish();
        assert_eq!(msg.len() - 7, 123);
        let m = Msg::parse(&msg).unwrap();
        assert_eq!(m.tlv(0x11), Some(&[0x88, 6, 0, 0, 0xc7, 7, 0, 0][..]));
        assert_eq!(m.u32(0x17), Some(10));
        assert_eq!(m.tlv(0x18), Some(&[0u8][..]));
    }

    #[test]
    fn complete_after_all_three_in_any_order() {
        let mut h = Handshake::default();
        let req = |id| Builder::new(REQUEST, 7, id).u8(0x01, 0).finish();
        let r = req(DRIVER_INIT_COMPLETE);
        let resp = h.request(&Msg::parse(&r).unwrap()).unwrap();
        assert_eq!(Msg::parse(&resp).unwrap().result(), Some(Ok(())));
        assert!(!h.complete_due());
        let r = req(INDICATION_REGISTER);
        h.request(&Msg::parse(&r).unwrap());
        assert!(!h.complete_due());
        h.modem_ready = true;
        assert!(h.complete_due());
        assert!(!h.complete_due());
        // the modem's filter-rule and config requests: no answer (as Linux)
        assert!(h.request(&Msg::parse(&req(0x23)).unwrap()).is_none());
        assert!(h.request(&Msg::parse(&req(0x27)).unwrap()).is_none());
    }
}
