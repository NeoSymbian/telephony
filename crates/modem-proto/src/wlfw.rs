// SPDX-License-Identifier: Apache-2.0
//! WLFW (QMI service 69, version 1): the WCN3990 Wi-Fi firmware's control service, run by the modem's WLAN domain
//! (`msm/modem/wlan_pd`) once it loaded `wlanmdsp.mbn` over TFTP (docs wifi.md W0/W1).
//!
//! Message ids, TLV numbers and layouts are facts from Linux ath10k `qmi_wlfw_v01.{c,h}` and the start-up order
//! from `qmi.c` (both ISC, Copyright (c) 2018 The Linux Foundation); this file is a new implementation, sans-I/O:
//! [`Client`] turns replies and indications into [`Step`]s the daemon carries out.
//!
//! Order (ath10k `ath10k_qmi_event_server_arrive`, then `_msa_ready`, then `_fw_ready_ind`):
//! indication register -> host capability (8-bit layout on WCN3990: DTB `qcom,snoc-host-cap-8bit-quirk`) ->
//! MSA info -> (the kernel assigns the MSA regions to the WLAN VMIDs) -> MSA ready -> capabilities;
//! on the MSA-ready indication: board data download -> calibration report; then the firmware-ready indication.
//! Configuring the copy engines (WLAN cfg) and mission mode come from the PDD in W2 ([`wlan_cfg`], [`wlan_mode`]).

use crate::qmi::{Builder, Msg, REQUEST};
use alloc::string::String;
use alloc::vec::Vec;

pub const SERVICE: u32 = 69;
pub const VERSION: u32 = 1;
pub const INSTANCE: u32 = 0;
/// ath10k's client id ("KNEL")
pub const CLIENT_ID: u32 = 0x4b4e_454c;

pub const IND_REGISTER: u16 = 0x20;
pub const FW_READY_IND: u16 = 0x21;
pub const WLAN_MODE: u16 = 0x22;
pub const WLAN_CFG: u16 = 0x23;
pub const CAP: u16 = 0x24;
pub const BDF_DOWNLOAD: u16 = 0x25;
pub const CAL_REPORT: u16 = 0x26;
pub const MSA_READY_IND: u16 = 0x2b;
pub const PIN_CONNECT_RESULT_IND: u16 = 0x2c;
pub const MSA_INFO: u16 = 0x2d;
pub const MSA_READY: u16 = 0x2e;
pub const HOST_CAP: u16 = 0x34;

/// board data bytes per BDF_DOWNLOAD request (QMI_WLFW_MAX_DATA_SIZE_V01)
pub const BDF_SEGMENT: usize = 6144;
/// memory regions an MSA info reply may list (QMI_WLFW_MAX_MEM_REG_V01)
const MAX_MEM_REG: usize = 2;

/// wlfw_driver_mode_enum_v01
pub const MODE_MISSION: u32 = 0;
pub const MODE_OFF: u32 = 4;

/// A piece of the MSA carve-out the firmware wants for itself (wlfw_memory_region_info_s_v01)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub addr: u64,
    pub size: u32,
    /// secure regions go to MSS_MSA + WLAN only; the others also to WLAN_CE (ath10k_qmi_map_msa_permission)
    pub secure: bool,
}

/// What the firmware says about itself (CAP reply)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    pub chip_id: u32,
    pub chip_family: u32,
    pub board_id: u32,
    pub soc_id: u32,
    pub fw_version: u32,
    pub fw_timestamp: String,
    pub fw_build_id: String,
}

fn request(txn: u16, id: u16) -> Builder {
    Builder::new(REQUEST, txn, id)
}

/// Indications wanted: fw ready + MSA ready, as ath10k (no xo calibration: the DTB has no `qcom,xo-cal-data`)
pub fn ind_register(txn: u16) -> Vec<u8> {
    request(txn, IND_REGISTER)
        .u8(0x10, 1) // fw_ready_enable
        .u8(0x13, 1) // msa_ready_enable
        .u32(0x15, CLIENT_ID)
        .finish()
}

/// Host capability, the 8-bit layout: daemon_support = 0
pub fn host_cap_8bit(txn: u16) -> Vec<u8> {
    request(txn, HOST_CAP).u8(0x10, 0).finish()
}

/// Where the MSA carve-out is (DTB wlan-msa@94d00000, 1 MB)
pub fn msa_info(txn: u16, addr: u64, size: u32) -> Vec<u8> {
    request(txn, MSA_INFO)
        .u64(0x01, addr)
        .u32(0x02, size)
        .finish()
}

pub fn msa_ready(txn: u16) -> Vec<u8> {
    request(txn, MSA_READY).finish()
}

pub fn cap(txn: u16) -> Vec<u8> {
    request(txn, CAP).finish()
}

/// Board data segment `seg` of a `total`-byte file (`data` <= 6144 bytes; `end` on the last)
pub fn bdf_segment(txn: u16, total: u32, seg: u32, data: &[u8], end: bool) -> Vec<u8> {
    let mut d = Vec::with_capacity(2 + data.len());
    d.extend_from_slice(&(data.len() as u16).to_le_bytes()); // DATA_LEN over 255 elements: a u16
    d.extend_from_slice(data);
    request(txn, BDF_DOWNLOAD)
        .u8(0x01, 1) // valid
        .u32(0x10, 0) // file_id
        .u32(0x11, total)
        .u32(0x12, seg)
        .tlv(0x13, &d)
        .u8(0x14, end as u8)
        .finish()
}

/// No stored calibration: an empty meta data list
pub fn cal_report(txn: u16) -> Vec<u8> {
    request(txn, CAL_REPORT).tlv(0x01, &[0]).finish()
}

pub fn wlan_mode(txn: u16, mode: u32) -> Vec<u8> {
    request(txn, WLAN_MODE)
        .u32(0x01, mode)
        .u8(0x10, 0) // hw_debug
        .finish()
}

/// One copy engine as the target sees it (wlfw_ce_tgt_pipe_cfg_s_v01)
#[derive(Clone, Copy, Debug)]
pub struct CePipe {
    pub num: u32,
    /// 1 = in (target to host), 2 = out, 3 = both
    pub dir: u32,
    pub entries: u32,
    pub max_bytes: u32,
    pub flags: u32,
}

/// The copy engines, the service -> pipe map {service, dir, pipe} and the shadow registers {id, offset}
pub fn wlan_cfg(
    txn: u16,
    pipes: &[CePipe],
    services: &[[u32; 3]],
    shadow: &[(u16, u16)],
) -> Vec<u8> {
    let mut t = alloc::vec![pipes.len() as u8];
    for p in pipes {
        for v in [p.num, p.dir, p.entries, p.max_bytes, p.flags] {
            t.extend_from_slice(&v.to_le_bytes());
        }
    }
    let mut s = alloc::vec![services.len() as u8];
    for v in services.iter().flatten() {
        s.extend_from_slice(&v.to_le_bytes());
    }
    let mut r = alloc::vec![shadow.len() as u8];
    for (id, off) in shadow {
        r.extend_from_slice(&id.to_le_bytes());
        r.extend_from_slice(&off.to_le_bytes());
    }
    request(txn, WLAN_CFG)
        .tlv(0x11, &t)
        .tlv(0x12, &s)
        .tlv(0x13, &r)
        .finish()
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// The MSA info reply's regions, each checked to lie inside the carve-out (ath10k rejects the reply otherwise)
pub fn msa_regions(m: &Msg, msa_addr: u64, msa_size: u32) -> Option<Vec<Region>> {
    let t = m.tlv(0x03)?;
    let n = *t.first()? as usize;
    if n > MAX_MEM_REG || t.len() < 1 + 13 * n {
        return None;
    }
    let end = msa_addr + msa_size as u64;
    let mut v = Vec::new();
    for i in 0..n {
        let e = &t[1 + 13 * i..];
        let addr = u64::from_le_bytes(e[..8].try_into().ok()?);
        let size = le32(e, 8)?;
        if addr < msa_addr || size > msa_size || addr + size as u64 > end {
            return None;
        }
        v.push(Region {
            addr,
            size,
            secure: e[12] != 0,
        });
    }
    Some(v)
}

/// A string inside a struct: a u8 length, then the bytes
fn inner_str(b: &[u8]) -> Option<String> {
    let n = *b.first()? as usize;
    Some(String::from_utf8_lossy(b.get(1..1 + n)?).into_owned())
}

pub fn caps(m: &Msg) -> Caps {
    let mut c = Caps::default();
    if let Some(t) = m.tlv(0x10) {
        c.chip_id = le32(t, 0).unwrap_or(0);
        c.chip_family = le32(t, 4).unwrap_or(0);
    }
    c.board_id = m.tlv(0x11).and_then(|t| le32(t, 0)).unwrap_or(0);
    c.soc_id = m.tlv(0x12).and_then(|t| le32(t, 0)).unwrap_or(0);
    if let Some(t) = m.tlv(0x13) {
        c.fw_version = le32(t, 0).unwrap_or(0);
        c.fw_timestamp = t.get(4..).and_then(inner_str).unwrap_or_default();
    }
    if let Some(t) = m.tlv(0x14) {
        // a string at the top level: just the bytes (the TLV gives the length)
        c.fw_build_id =
            String::from_utf8_lossy(t.split(|&b| b == 0).next().unwrap_or(t)).into_owned();
    }
    c
}

/// What the daemon has to do next
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// send this QMI request to the WLFW server
    Send(Vec<u8>),
    /// hand these MSA regions to the WLAN VMIDs (TrustZone), then call [`Client::msa_assigned`]
    AssignMsa(Vec<Region>),
    /// read the board data file, then call [`Client::board_data`]
    LoadBoardData,
    /// the firmware said who it is
    Caps(Caps),
    /// the firmware is ready for WLAN cfg + mission mode (W2)
    Ready,
    Failed(&'static str, u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    IndRegister,
    HostCap,
    MsaInfo,
    AssignMsa,
    MsaReady,
    Cap,
    /// capabilities known; waiting for the MSA-ready indication
    WaitMsaInd,
    LoadBoardData,
    /// board data segment `next` is in flight
    Bdf,
    CalReport,
    /// waiting for the firmware-ready indication
    WaitFwReady,
    Ready,
    Failed,
}

pub struct Client {
    state: State,
    txn: u16,
    msa: (u64, u32),
    msa_ind: bool,
    fw_ready_ind: bool,
    bdf: Vec<u8>,
    next: usize,
}

impl Client {
    /// `msa` = the carve-out's address and size (DTB wlan-msa)
    pub fn new(msa_addr: u64, msa_size: u32) -> Self {
        Client {
            state: State::Idle,
            txn: 0,
            msa: (msa_addr, msa_size),
            msa_ind: false,
            fw_ready_ind: false,
            bdf: Vec::new(),
            next: 0,
        }
    }

    pub fn ready(&self) -> bool {
        self.state == State::Ready
    }

    fn txn(&mut self) -> u16 {
        self.txn = self.txn.wrapping_add(1).max(1);
        self.txn
    }

    fn fail(&mut self, what: &'static str, e: u16) -> Vec<Step> {
        self.state = State::Failed;
        alloc::vec![Step::Failed(what, e)]
    }

    /// The server (69) appeared: start over
    pub fn start(&mut self) -> Vec<Step> {
        *self = Client::new(self.msa.0, self.msa.1);
        self.state = State::IndRegister;
        let t = self.txn();
        alloc::vec![Step::Send(ind_register(t))]
    }

    /// The kernel assigned (or failed to assign) the MSA regions
    pub fn msa_assigned(&mut self, ok: bool) -> Vec<Step> {
        if self.state != State::AssignMsa {
            return Vec::new();
        }
        if !ok {
            return self.fail("assign MSA", 0);
        }
        self.state = State::MsaReady;
        let t = self.txn();
        alloc::vec![Step::Send(msa_ready(t))]
    }

    /// The board data file (empty = none found: the firmware then runs without one, which ath10k doesn't do)
    pub fn board_data(&mut self, data: Vec<u8>) -> Vec<Step> {
        if self.state != State::LoadBoardData {
            return Vec::new();
        }
        if data.is_empty() {
            return self.fail("no board data", 0);
        }
        self.bdf = data;
        self.next = 0;
        self.state = State::Bdf;
        self.send_segment()
    }

    fn send_segment(&mut self) -> Vec<Step> {
        let at = self.next * BDF_SEGMENT;
        let end = (at + BDF_SEGMENT).min(self.bdf.len());
        let t = self.txn();
        let seg = bdf_segment(
            t,
            self.bdf.len() as u32,
            self.next as u32,
            &self.bdf[at..end],
            end == self.bdf.len(),
        );
        alloc::vec![Step::Send(seg)]
    }

    fn maybe_ready(&mut self) -> Vec<Step> {
        if self.state == State::WaitFwReady && self.fw_ready_ind {
            self.state = State::Ready;
            return alloc::vec![Step::Ready];
        }
        Vec::new()
    }

    /// A message from the WLFW server: a reply to our request, or an indication
    pub fn message(&mut self, payload: &[u8]) -> Vec<Step> {
        let Some(m) = Msg::parse(payload) else {
            return Vec::new();
        };
        if m.ty == crate::qmi::INDICATION {
            match m.id {
                MSA_READY_IND => {
                    self.msa_ind = true;
                    if self.state == State::WaitMsaInd {
                        self.state = State::LoadBoardData;
                        return alloc::vec![Step::LoadBoardData];
                    }
                }
                FW_READY_IND => {
                    self.fw_ready_ind = true;
                    return self.maybe_ready();
                }
                _ => {}
            }
            return Vec::new();
        }
        if m.ty != crate::qmi::RESPONSE || m.txn != self.txn {
            return Vec::new(); // late or foreign
        }
        if let Some(Err(e)) = m.result() {
            let what = match self.state {
                State::IndRegister => "indication register",
                State::HostCap => "host capability",
                State::MsaInfo => "MSA info",
                State::MsaReady => "MSA ready",
                State::Cap => "capabilities",
                State::Bdf => "board data",
                State::CalReport => "calibration report",
                _ => "reply",
            };
            return self.fail(what, e);
        }
        match (self.state, m.id) {
            (State::IndRegister, IND_REGISTER) => {
                self.state = State::HostCap;
                let t = self.txn();
                alloc::vec![Step::Send(host_cap_8bit(t))]
            }
            (State::HostCap, HOST_CAP) => {
                self.state = State::MsaInfo;
                let t = self.txn();
                alloc::vec![Step::Send(msa_info(t, self.msa.0, self.msa.1))]
            }
            (State::MsaInfo, MSA_INFO) => match msa_regions(&m, self.msa.0, self.msa.1) {
                Some(r) => {
                    self.state = State::AssignMsa;
                    alloc::vec![Step::AssignMsa(r)]
                }
                None => self.fail("MSA regions out of range", 0),
            },
            (State::MsaReady, MSA_READY) => {
                self.state = State::Cap;
                let t = self.txn();
                alloc::vec![Step::Send(cap(t))]
            }
            (State::Cap, CAP) => {
                let mut v = alloc::vec![Step::Caps(caps(&m))];
                if self.msa_ind {
                    self.state = State::LoadBoardData;
                    v.push(Step::LoadBoardData);
                } else {
                    self.state = State::WaitMsaInd;
                }
                v
            }
            (State::Bdf, BDF_DOWNLOAD) => {
                self.next += 1;
                if self.next * BDF_SEGMENT < self.bdf.len() {
                    return self.send_segment();
                }
                self.bdf = Vec::new();
                self.state = State::CalReport;
                let t = self.txn();
                alloc::vec![Step::Send(cal_report(t))]
            }
            (State::CalReport, CAL_REPORT) => {
                self.state = State::WaitFwReady;
                self.maybe_ready()
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qmi::{INDICATION, RESPONSE};

    const MSA: (u64, u32) = (0x94d0_0000, 0x10_0000);

    fn reply(req: &[u8]) -> Builder {
        let m = Msg::parse(req).unwrap();
        Builder::new(RESPONSE, m.txn, m.id).result(0)
    }

    fn sent(steps: &[Step]) -> &[u8] {
        match &steps[0] {
            Step::Send(b) => b,
            s => panic!("expected a send, got {s:?}"),
        }
    }

    #[test]
    fn encodings_follow_ath10k_layouts() {
        // ind_register: 0x10 u8 1, 0x13 u8 1, 0x15 u32 client id
        let m = ind_register(7);
        let p = Msg::parse(&m).unwrap();
        assert_eq!((p.ty, p.txn, p.id), (REQUEST, 7, IND_REGISTER));
        assert_eq!(p.u8(0x10), Some(1));
        assert_eq!(p.u8(0x13), Some(1));
        assert_eq!(p.u32(0x15), Some(CLIENT_ID));
        // host cap, 8-bit: only daemon_support as one byte
        let p = host_cap_8bit(1);
        assert_eq!(Msg::parse(&p).unwrap().tlv(0x10), Some(&[0u8][..]));
        // MSA info: a u64 address, a u32 size
        let m = msa_info(2, MSA.0, MSA.1);
        let p = Msg::parse(&m).unwrap();
        assert_eq!(p.tlv(0x01), Some(&MSA.0.to_le_bytes()[..]));
        assert_eq!(p.u32(0x02), Some(MSA.1));
        // board data: u16 length before the bytes
        let m = bdf_segment(3, 10, 0, &[1, 2, 3], true);
        let p = Msg::parse(&m).unwrap();
        assert_eq!(p.tlv(0x13), Some(&[3u8, 0, 1, 2, 3][..]));
        assert_eq!(p.u8(0x14), Some(1));
        assert_eq!(p.u32(0x11), Some(10));
    }

    #[test]
    fn regions_must_lie_inside_the_carve_out() {
        let mut t = alloc::vec![1u8];
        t.extend_from_slice(&(MSA.0 + 0x1000).to_le_bytes());
        t.extend_from_slice(&0x8000u32.to_le_bytes());
        t.push(1);
        let m = Builder::new(RESPONSE, 1, MSA_INFO)
            .result(0)
            .tlv(0x03, &t)
            .finish();
        let r = msa_regions(&Msg::parse(&m).unwrap(), MSA.0, MSA.1).unwrap();
        assert_eq!(
            r,
            alloc::vec![Region {
                addr: MSA.0 + 0x1000,
                size: 0x8000,
                secure: true
            }]
        );
        let mut bad = t.clone();
        bad[1..9].copy_from_slice(&(MSA.0 + 0xff000).to_le_bytes()); // runs past the end
        let m = Builder::new(RESPONSE, 1, MSA_INFO)
            .result(0)
            .tlv(0x03, &bad)
            .finish();
        assert_eq!(msa_regions(&Msg::parse(&m).unwrap(), MSA.0, MSA.1), None);
    }

    #[test]
    fn whole_start_up_in_ath10k_order() {
        let mut c = Client::new(MSA.0, MSA.1);
        let s = c.start();
        assert_eq!(Msg::parse(sent(&s)).unwrap().id, IND_REGISTER);
        let s = c.message(&reply(sent(&s)).finish());
        assert_eq!(Msg::parse(sent(&s)).unwrap().id, HOST_CAP);
        let s = c.message(&reply(sent(&s)).finish());
        let req = sent(&s).to_vec();
        assert_eq!(Msg::parse(&req).unwrap().id, MSA_INFO);
        let mut t = alloc::vec![1u8];
        t.extend_from_slice(&MSA.0.to_le_bytes());
        t.extend_from_slice(&MSA.1.to_le_bytes());
        t.push(0);
        let s = c.message(&reply(&req).tlv(0x03, &t).finish());
        assert_eq!(
            s,
            alloc::vec![Step::AssignMsa(alloc::vec![Region {
                addr: MSA.0,
                size: MSA.1,
                secure: false
            }])]
        );
        // the MSA-ready indication may come before the capabilities reply
        let ind = Builder::new(INDICATION, 0, MSA_READY_IND).finish();
        assert!(c.message(&ind).is_empty());
        let s = c.msa_assigned(true);
        assert_eq!(Msg::parse(sent(&s)).unwrap().id, MSA_READY);
        let s = c.message(&reply(sent(&s)).finish());
        let req = sent(&s).to_vec();
        assert_eq!(Msg::parse(&req).unwrap().id, CAP);
        let mut fw = 0x1234u32.to_le_bytes().to_vec();
        fw.push(3);
        fw.extend_from_slice(b"t20");
        let caps_reply = reply(&req)
            .tlv(0x10, &[0x80, 0, 0, 0, 0x40, 0, 0, 0])
            .u32(0x11, 0xff)
            .tlv(0x13, &fw)
            .tlv(0x14, b"WLAN.HL.2.0\0")
            .finish();
        let s = c.message(&caps_reply);
        let Step::Caps(k) = &s[0] else { panic!() };
        assert_eq!(
            (k.chip_id, k.chip_family, k.board_id, k.fw_version),
            (0x80, 0x40, 0xff, 0x1234)
        );
        assert_eq!(
            (k.fw_timestamp.as_str(), k.fw_build_id.as_str()),
            ("t20", "WLAN.HL.2.0")
        );
        assert_eq!(s[1], Step::LoadBoardData);
        // 6144 + 100 bytes: two segments, the second marked as the end
        let s = c.board_data(alloc::vec![7u8; BDF_SEGMENT + 100]);
        let p = Msg::parse(sent(&s)).unwrap();
        assert_eq!(
            (p.id, p.u32(0x12), p.u8(0x14)),
            (BDF_DOWNLOAD, Some(0), Some(0))
        );
        let s = c.message(&reply(sent(&s)).finish());
        let p = Msg::parse(sent(&s)).unwrap();
        assert_eq!(
            (p.u32(0x12), p.u8(0x14), p.tlv(0x13).unwrap().len()),
            (Some(1), Some(1), 102)
        );
        let s = c.message(&reply(sent(&s)).finish());
        assert_eq!(Msg::parse(sent(&s)).unwrap().id, CAL_REPORT);
        assert!(c.message(&reply(sent(&s)).finish()).is_empty());
        assert!(!c.ready());
        let ind = Builder::new(INDICATION, 0, FW_READY_IND).finish();
        assert_eq!(c.message(&ind), alloc::vec![Step::Ready]);
        assert!(c.ready());
    }

    #[test]
    fn an_error_reply_stops_it() {
        let mut c = Client::new(MSA.0, MSA.1);
        let s = c.start();
        let m = Msg::parse(sent(&s)).unwrap();
        let r = Builder::new(RESPONSE, m.txn, m.id).result(0x31).finish();
        let s = c.message(&r);
        assert!(matches!(s[0], Step::Failed("indication register", _)));
    }
}
