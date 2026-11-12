// SPDX-License-Identifier: Apache-2.0
//! UIM card/PIN facts from libqmi data/qmi-service-uim.json and qmi-enums-uim.h.
//! Only the primary GW application on slot 1 is used; no PIN value is retained.
use crate::qmi::Msg;
use modem_ipc::sim;

pub const REGISTER: u16 = 0x2e;
pub const GET_STATUS: u16 = 0x2f;
pub const STATUS_IND: u16 = 0x32;
pub const VERIFY_PIN: u16 = 0x26;

pub struct Card {
    pub state: u8,
    pub attempts: u8,
    pub pin_id: u8,
}

pub fn card(m: &Msg) -> Option<Card> {
    let b = m.tlv(0x10)?;
    let count = *b.get(8)?;
    if count == 0 {
        return Some(Card {
            state: sim::ABSENT,
            attempts: 255,
            pin_id: 1,
        });
    }
    let mut card = Card {
        state: match *b.get(9)? {
            0 => sim::ABSENT,
            1 => sim::PRESENT,
            2 => sim::ERROR,
            _ => sim::UNKNOWN,
        },
        attempts: 255,
        pin_id: 1,
    };
    if card.state != sim::PRESENT {
        return Some(card);
    }
    let primary = u16::from_le_bytes(b.get(..2)?.try_into().ok()?);
    // A provisioning index is {slot, application}; before PIN entry it may be 0xffff.
    // Then select the first SIM/USIM application on slot 1, rather than treating it as unlocked.
    // Older status vectors include only card presence. No app/PIN state can be inferred.
    if b.len() == 10 {
        return Some(card);
    }
    let mut at = 15;
    for app in 0..usize::from(*b.get(14)?) {
        let aid = usize::from(*b.get(at + 6)?);
        let tail = at + 7 + aid;
        let info = b.get(tail..tail + 7)?;
        let gsm = matches!(*b.get(at)?, 1 | 2);
        if (primary >> 8 == 0 && app == usize::from(primary & 255)) || (primary == 0xffff && gsm) {
            let upin = info[0] != 0;
            let (pin, attempts) = if upin {
                (*b.get(10)?, *b.get(11)?)
            } else {
                (info[1], info[2])
            };
            card.pin_id = if upin { 3 } else { 1 };
            card.attempts = attempts;
            card.state = match pin {
                1 => sim::PIN_REQUIRED,
                4 | 5 => sim::BLOCKED,
                2 | 3 => sim::PRESENT,
                _ => match *b.get(at + 1)? {
                    2 => sim::PIN_REQUIRED,
                    3 | 5 => sim::BLOCKED,
                    7 => sim::PRESENT,
                    _ => sim::UNKNOWN,
                },
            };
            return Some(card);
        }
        at = tail + 7;
    }
    Some(card)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qmi::{Builder, RESPONSE};
    fn status(app_pin: u8, upin: bool) -> Card {
        let mut b = alloc::vec![255, 255, 0, 0, 0, 0, 0, 0, 1, 1, 1, 2, 10, 0, 1];
        b.extend_from_slice(&[2, 2, 0, 0, 0, 0, 0, upin as u8, app_pin, 3, 10, 3, 3, 10]);
        let bytes = Builder::new(RESPONSE, 1, GET_STATUS)
            .result(0)
            .tlv(0x10, &b)
            .finish();
        card(&Msg::parse(&bytes).unwrap()).unwrap()
    }
    #[test]
    fn pin_state_before_provisioning_and_upin_replacement() {
        let c = status(1, false);
        assert_eq!((c.state, c.attempts, c.pin_id), (sim::PIN_REQUIRED, 3, 1));
        let c = status(4, false);
        assert_eq!(c.state, sim::BLOCKED);
        let c = status(2, false);
        assert_eq!(c.state, sim::PRESENT);
        let c = status(2, true);
        assert_eq!((c.state, c.attempts, c.pin_id), (sim::PIN_REQUIRED, 2, 3));
    }
    #[test]
    fn truncated_app_is_rejected() {
        let bytes = Builder::new(RESPONSE, 1, GET_STATUS)
            .result(0)
            .tlv(0x10, &[0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 3, 10, 0, 1, 2])
            .finish();
        assert!(card(&Msg::parse(&bytes).unwrap()).is_none());
    }
}
