// SPDX-License-Identifier: Apache-2.0
//! The service registry locator, QMI service 64: which protection domains provide a service. The table is
//! Linux's for sdm670 (`drivers/soc/qcom/qcom_pd_mapper.c`; facts that match the firmware's .jsn files).

use crate::qmi::{Builder, Msg, RESPONSE};
use alloc::vec::Vec;

pub const SERVICE: u32 = 64;
pub const VERSION: u32 = 1;
pub const INSTANCE: u32 = 1;
const GET_DOMAIN_LIST: u16 = 0x20;
const PFR: u16 = 0x24;

/// (domain, QMI instance, the services it provides)
pub const DOMAINS: &[(&str, u32, &[&str])] = &[
    ("msm/adsp/audio_pd", 74, &["avs/audio"]),
    ("msm/adsp/root_pd", 74, &[]),
    ("msm/cdsp/root_pd", 76, &[]),
    ("msm/modem/root_pd", 180, &[]),
    ("msm/modem/wlan_pd", 180, &["kernel/elf_loader", "wlan/fw"]),
];

pub fn handle(m: &Msg) -> Vec<u8> {
    let r = Builder::new(RESPONSE, m.txn, m.id);
    match m.id {
        GET_DOMAIN_LIST => {
            let name = m.tlv(1).unwrap_or(&[]);
            let name = name.strip_suffix(&[0]).unwrap_or(name);
            let offset = m.u32(0x10).unwrap_or(0) as usize;
            let found: Vec<_> = DOMAINS
                .iter()
                .filter(|d| d.2.iter().any(|s| s.as_bytes() == name))
                .collect();
            // domain_list[]: a u8 count, then {u8 length + name, u32 instance, u8 data valid, u32 data}
            let mut list = alloc::vec![0u8];
            for d in found.iter().skip(offset) {
                list.push(d.0.len() as u8);
                list.extend_from_slice(d.0.as_bytes());
                list.extend_from_slice(&d.1.to_le_bytes());
                list.push(0);
                list.extend_from_slice(&0u32.to_le_bytes());
                list[0] += 1;
            }
            r.result(0)
                .u16(0x10, found.len() as u16)
                .u16(0x11, 1)
                .tlv(0x12, &list)
                .finish()
        }
        PFR => r.result(0).finish(),
        _ => r.result(2).finish(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qmi::REQUEST;

    #[test]
    fn audio_is_on_the_adsp() {
        let q = Builder::new(REQUEST, 5, GET_DOMAIN_LIST)
            .tlv(1, b"avs/audio")
            .finish();
        let r = handle(&Msg::parse(&q).unwrap());
        let m = Msg::parse(&r).unwrap();
        assert_eq!(m.result(), Some(Ok(())));
        let list = m.tlv(0x12).unwrap();
        assert_eq!(list[0], 1);
        assert_eq!(&list[2..2 + list[1] as usize], b"msm/adsp/audio_pd");
        assert_eq!(&list[2 + 17..2 + 21], &74u32.to_le_bytes());
    }

    #[test]
    fn unknown_service_is_an_empty_list() {
        let q = Builder::new(REQUEST, 6, GET_DOMAIN_LIST)
            .tlv(1, b"tms/servreg")
            .finish();
        let m = Msg::parse(&handle(&Msg::parse(&q).unwrap()))
            .unwrap()
            .tlv(0x10)
            .map(|t| t.to_vec());
        assert_eq!(m, Some(alloc::vec![0, 0]));
    }
}
