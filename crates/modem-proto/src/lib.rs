// SPDX-License-Identifier: Apache-2.0
//! The apps side of the Pixel 3a XL's modem (docs/telephony.md C4, C5): a QRTR router for node 1 with the services
//! the modem needs from it (rmtfs, the service locator, TFTP) and QMI clients for DMS, UIM, NAS and VOICE that keep
//! a [`modem_ipc::Status`] and place, answer and end calls. Pure logic over a [`Platform`] (the kernel's HAL on the
//! phone, a fake in tests), so it is unit-tested on the host.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod apr;
pub mod calllog;
pub mod ipa;
pub mod locator;
pub mod nas;
pub mod qmi;
pub mod qrtr;
pub mod recovery;
pub mod rmtfs;
pub mod schedule;
pub mod ssctl;
pub mod tftp;
pub mod uim;
pub mod voice;
/// the Wi-Fi firmware's control service client (NeoSymbian/neolink `wlfw`)
pub use wlfw;

pub use modem_ipc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use modem_ipc::{Call, Status, Text, call};
use qrtr::{Header, NODE_BCAST, PORT_CTRL};

/// What the router needs from the system.
pub trait Platform {
    /// One whole QRTR packet to `node`: the modem (node 0) or the ADSP, each on its own GLINK IPCRTR.
    fn send(&mut self, node: u32, packet: &[u8]) -> Result<(), i32>;
    /// The rmtfs carve-out the modem reads its EFS from.
    fn rmtfs_read(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), i32>;
    fn rmtfs_write(&mut self, offset: u32, data: &[u8]) -> Result<(), i32>;
    fn log(&mut self, args: fmt::Arguments);
    /// The size of a read-only file on the modem's firmware partition (modem_a, "IMAGE/WLANMDSP.MBN"), for TFTP.
    fn fw_size(&mut self, _path: &str) -> Result<usize, i32> {
        Err(-5) // KErrNotSupported
    }
    /// Its bytes at `offset` into `buf`: how many (fewer at its end).
    fn fw_read(&mut self, _path: &str, _offset: usize, _buf: &mut [u8]) -> Result<usize, i32> {
        Err(-5)
    }
    /// Hand the Wi-Fi firmware's MSA regions to its VMIDs (TrustZone; kernel HAL WlanMsa). Wi-Fi W1.
    fn wlan_msa(&mut self, _regions: &[wlfw::Region]) -> Result<(), i32> {
        Err(-5)
    }
    /// The WCN3990 board data file for `board_id` (vendor_a), or empty if there is none.
    fn board_data(&mut self, _board_id: u32) -> Vec<u8> {
        Vec::new()
    }
    /// The firmware is in mission mode: start the host's copy engines, HTC and WMI (kernel HAL WlanStart, W2).
    fn wlan_start(&mut self) -> Result<(), i32> {
        Err(-5)
    }
    /// The wall clock, UTC Unix milliseconds, for the call log; 0 = unknown.
    fn utc_ms(&mut self) -> i64 {
        0
    }
}

pub const NODE: u32 = 1;
const MODEM_NODE: u32 = 0;
const RMTFS_PORT: u32 = 0x10;
const LOCATOR_PORT: u32 = 0x11;
const TFTP_PORT: u32 = 0x12;
const IPA_PORT: u32 = 0x13;
/// our SSCTL client port (the "subsystem up" events, ssctl.rs)
const SSCTL_PORT: u32 = 0x25;
/// our WLFW client port (the Wi-Fi firmware's control service, wlfw.rs)
const WLFW_PORT: u32 = 0x26;
/// DTB wlan-msa@94d00000: the MSA carve-out the Wi-Fi firmware may use
const WLAN_MSA: (u64, u32) = (0x94d0_0000, 0x10_0000);
const FIRST_TRANSFER_PORT: u32 = 0x100;
/// (service, version, instance, our port); the IPA server is announced only once the kernel runs IPA
const SERVICES: [(u32, u32, u32, u32); 4] = [
    (rmtfs::SERVICE, rmtfs::VERSION, rmtfs::INSTANCE, RMTFS_PORT),
    (
        locator::SERVICE,
        locator::VERSION,
        locator::INSTANCE,
        LOCATOR_PORT,
    ),
    (tftp::SERVICE, tftp::VERSION, tftp::INSTANCE, TFTP_PORT),
    (ipa::SERVICE, ipa::VERSION, ipa::INSTANCE, IPA_PORT),
];
const DMS: u32 = 2;
const UIM: u32 = 11;
const NAS: u32 = nas::SERVICE;
const VOICE: u32 = voice::SERVICE;
const IPA: u32 = ipa::SERVICE;
/// Our QMI clients: (service, our port)
const CLIENTS: [(u32, u32); 5] = [
    (DMS, 0x20),
    (UIM, 0x21),
    (NAS, 0x22),
    (VOICE, 0x23),
    (IPA, 0x24),
];
const DMS_GET_REVISION: u16 = 0x23;
const DMS_GET_IDS: u16 = 0x25;
const DMS_SET_OPERATING_MODE: u16 = 0x2e;
const UIM_GET_CARD_STATUS: u16 = 0x2f;
/// NAS polls, as ModemManager does (indications cover the rest)
const POLL_MS: u64 = 30_000;
const POST_CALL_MS: u64 = 30_000;
pub const KERR_NOT_READY: i32 = -18;
pub const KERR_ARGUMENT: i32 = -6;
pub const KERR_IN_USE: i32 = -14;

/// A server the modem announced: service, version|instance word, node, port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Server {
    pub service: u32,
    pub instance: u32,
    pub node: u32,
    pub port: u32,
}

/// One QMI client: its server, our port, the transaction counter and the requests still unanswered (txn, id).
struct Client {
    service: u32,
    dst: (u32, u32),
    port: u32,
    txn: u16,
    pending: Vec<(u16, u16)>,
}

pub struct Modem {
    /// Log every QRTR packet both ways (bring-up).
    pub trace: bool,
    rmtfs: Option<rmtfs::Rmtfs>,
    /// Startup policy: false prevents automatic online requests (not a runtime power-off command).
    pub radio_enabled: bool,
    /// Terminal radio failure; late packets cannot recreate clients without a reboot.
    radio_failed: bool,
    pub servers: Vec<Server>,
    pub files: Vec<(String, Vec<u8>)>,
    pub revision: Option<String>,
    pub imei: Option<String>,
    /// The first card's state from UIM: 0 absent, 1 present, 2 error.
    pub card: Option<u8>,
    /// The kernel runs IPA (MODEMD's HAL IpaStart succeeded): then the IPA handshake runs and LTE stays allowed.
    /// Without it, voice-only: 2G/3G circuit-switched (NAS system selection, until power cycle), because on LTE data
    /// the modem asserts in its IPA path ("IPA Assert: new_free_space > 0", C5).
    pub ipa_up: bool,
    ipa: ipa::Handshake,
    /// INIT_DRIVER couldn't go out yet: tick retries it (last tried at ipa_init_at)
    ipa_init_due: bool,
    ipa_init_at: u64,
    /// What NeoModem clients see (modem_ipc::op::STATUS)
    pub status: Status,
    transfers: Vec<tftp::Transfer>,
    /// firmware reads in progress (the modem's WLAN domain image)
    readers: Vec<tftp::Reader>,
    /// the Wi-Fi firmware's control service once it appears: its address and our client (W1)
    wlfw: Option<((u32, u32), wlfw::Client)>,
    wlfw_board: u32,
    next_port: u32,
    clients: Vec<Client>,
    /// the nodes whose HELLO we answered (the modem, the ADSP)
    hello_seen: Vec<u32>,
    /// the SSCTL servers (node, port): the modem's, the ADSP's
    ssctl: [Option<(u32, u32)>; 2],
    ssctl_txn: u16,
    online_sent: bool,
    plmn_asked: Option<(u16, u16)>,
    /// Get Network Time was sent (once registered): the indication may have come before we subscribed
    time_asked: bool,
    hangup_pending: bool,
    /// the call watchdog: the (id, state) tick last saw, since when (ms), and when END was last tried
    call_seen: (u8, u8),
    call_since: u64,
    hangup_at: u64,
    last_poll: u64,
    /// (final END observed, next read-only NAS query), bounded to 30 seconds.
    post_call: Option<(u64, u64)>,
    /// NAS circuit-switched attach, for the log
    cs: u8,
    selection_sent: bool,
    /// essential packets the transport refused, sent again by `tick` in order
    outbox: Vec<Unsent>,
    outbox_at: u64,
    command: Option<(u32, u16, u16, u64)>,
    now: u64,
    radio_request: Option<(u16, bool)>,
    radio_at: u64,
    /// refused Set Operating Mode requests for the current intent (bounded by RADIO_REFUSALS)
    radio_refusals: u8,
    pin_id: u8,
    answered: Vec<u8>,
    /// The call log (C8); MODEMD saves it when `log.dirty`.
    pub log: calllog::CallLog,
    /// Caller ID for our calls: modem_ipc::clir (C8).
    pub clir: i32,
    /// the call waiting request with the network (modem_ipc::waiting)
    waiting_request: i32,
    /// a call being rejected as busy (UDUB); END follows if the modem refuses it
    rejecting: Option<u8>,
    response_seen: bool,
    heartbeat_at: u64,
}

/// (type, our port, destination, payload)
type Unsent = (u32, u32, (u32, u32), Vec<u8>);
/// refused Set Operating Mode requests before MODEMD stops asking (until the intent changes)
const RADIO_REFUSALS: u8 = 3;
/// essential packets kept for a retry at most; an older one gives way (logged)
const OUTBOX: usize = 16;
/// how often `tick` retries them (a refused send can take ~200 ms in the kernel)
const OUTBOX_RETRY_MS: u64 = 1000;

/// "350000000000012" -> "35xxxxxxxxxxx12": never log a whole IMEI.
pub fn mask(id: &str) -> String {
    id.char_indices()
        .map(|(i, c)| if i < 2 || i + 2 >= id.len() { c } else { 'x' })
        .collect()
}

impl Modem {
    pub fn new(images: [Vec<u8>; 4], rmtfs_phys: u64) -> Self {
        Self::with_rmtfs(Some(rmtfs::Rmtfs::new(images, rmtfs_phys)))
    }

    /// ADSP support before radio partitions and carve-out are available. No RMTFS is advertised or served.
    pub fn support_only() -> Self {
        Self::with_rmtfs(None)
    }

    /// The radio storage is attached (a recovery can boot the modem again without reading the partitions).
    pub fn radio_attached(&self) -> bool {
        self.rmtfs.is_some()
    }

    /// Attach real radio storage without resetting ADSP discovery, transactions or queued replies.
    pub fn attach_radio(&mut self, images: [Vec<u8>; 4], phys: u64) -> Result<(), i32> {
        if self.rmtfs.is_some() {
            return Err(KERR_IN_USE);
        }
        if phys == 0
            || phys % 512 != 0
            || phys.checked_add(0x20_0000).is_none()
            || images.iter().any(Vec::is_empty)
        {
            return Err(KERR_ARGUMENT);
        }
        self.rmtfs = Some(rmtfs::Rmtfs::new(images, phys));
        Ok(())
    }

    fn with_rmtfs(rmtfs: Option<rmtfs::Rmtfs>) -> Self {
        Modem {
            trace: false,
            rmtfs,
            radio_enabled: true,
            radio_failed: false,
            servers: Vec::new(),
            files: Vec::new(),
            revision: None,
            imei: None,
            card: None,
            // 255: unknown until UIM reports it (0 would read as "no tries left")
            status: Status {
                pin_attempts: 255,
                ..Status::default()
            },
            transfers: Vec::new(),
            readers: Vec::new(),
            wlfw: None,
            wlfw_board: 0xff,
            next_port: FIRST_TRANSFER_PORT,
            clients: Vec::new(),
            hello_seen: Vec::new(),
            ssctl: [None; 2],
            ssctl_txn: 0,
            online_sent: false,
            plmn_asked: None,
            time_asked: false,
            hangup_pending: false,
            call_seen: (0, 0),
            call_since: 0,
            hangup_at: 0,
            last_poll: 0,
            post_call: None,
            cs: 0,
            selection_sent: false,
            outbox: Vec::new(),
            outbox_at: 0,
            command: None,
            now: 0,
            radio_request: None,
            radio_at: 0,
            radio_refusals: 0,
            pin_id: 1,
            answered: Vec::new(),
            log: calllog::CallLog::default(),
            clir: modem_ipc::clir::NETWORK,
            waiting_request: 0,
            rejecting: None,
            response_seen: false,
            heartbeat_at: 0,
            ipa_up: false,
            ipa: ipa::Handshake::default(),
            ipa_init_due: false,
            ipa_init_at: 0,
        }
    }

    /// One packet out; false if the transport refused it (logged).
    fn send(
        &mut self,
        p: &mut dyn Platform,
        ty: u32,
        src_port: u32,
        dst: (u32, u32),
        payload: &[u8],
    ) -> bool {
        let h = Header {
            ty,
            src_node: NODE,
            src_port,
            confirm_rx: 0,
            size: 0,
            dst_node: dst.0,
            dst_port: dst.1,
        };
        if self.trace {
            trace(p, ">", &h, payload);
        }
        if let Err(e) = p.send(dst.0, &qrtr::encode(&h, payload)) {
            p.log(format_args!(
                "qrtr: send type {ty} to {}:{} failed ({e})",
                dst.0, dst.1 as i32
            ));
            return false;
        }
        true
    }

    /// A packet that must arrive (flow control, control, SSCTL, the subscriptions): refused, or behind others that
    /// were, it waits in the outbox and `tick` sends it in order. True if it went now. One lost RESUME_TX stalls
    /// the sender's flow for good (2026-11-09 review F07).
    fn send_must(
        &mut self,
        p: &mut dyn Platform,
        ty: u32,
        src_port: u32,
        dst: (u32, u32),
        payload: &[u8],
    ) -> bool {
        if self.outbox.is_empty() && self.send(p, ty, src_port, dst, payload) {
            return true;
        }
        if self.outbox.len() >= OUTBOX {
            let (ty, ..) = self.outbox.remove(0);
            p.log(format_args!("qrtr: outbox full: type {ty} dropped"));
        }
        self.outbox.push((ty, src_port, dst, payload.to_vec()));
        false
    }

    /// The outbox, oldest first, until the transport refuses one.
    fn flush(&mut self, p: &mut dyn Platform) {
        while let Some((ty, port, dst, data)) = self.outbox.first().cloned() {
            if !self.send(p, ty, port, dst, &data) {
                return;
            }
            self.outbox.remove(0);
        }
    }

    fn ctrl(&mut self, p: &mut dyn Platform, ty: u32, dst: (u32, u32), words: [u32; 4]) {
        self.send_must(p, ty, PORT_CTRL, dst, &qrtr::ctrl(ty, words));
    }

    /// Our servers: IPA's only while the kernel runs IPA.
    fn services(&self) -> impl Iterator<Item = (u32, u32, u32, u32)> {
        let ipa_up = self.ipa_up;
        let radio = self.rmtfs.is_some();
        SERVICES
            .into_iter()
            .filter(move |s| (radio || s.0 != rmtfs::SERVICE) && (ipa_up && radio || s.0 != IPA))
    }

    fn announce(&mut self, p: &mut dyn Platform, dst: (u32, u32)) {
        let services: Vec<_> = self.services().collect();
        for (service, version, instance, port) in services {
            self.ctrl(
                p,
                qrtr::TYPE_NEW_SERVER,
                dst,
                [service, qrtr::instance_word(version, instance), NODE, port],
            );
        }
    }

    /// Say HELLO to the modem's router and announce our services.
    pub fn start(&mut self, p: &mut dyn Platform) {
        if self.rmtfs.is_none() || !self.radio_enabled || self.radio_failed {
            return;
        }
        self.status.modem = modem_ipc::modem::RUNNING;
        self.ctrl(p, qrtr::TYPE_HELLO, (MODEM_NODE, PORT_CTRL), [0; 4]);
        self.announce(p, (MODEM_NODE, PORT_CTRL));
    }

    /// One QMI request from our client of `service`; false if the modem hasn't announced that service or the
    /// transport refused the packet.
    fn request(
        &mut self,
        p: &mut dyn Platform,
        service: u32,
        id: u16,
        tlvs: &[(u8, &[u8])],
    ) -> bool {
        self.qmi_request(p, service, id, tlvs, false)
    }

    /// A request that must arrive (`send_must`): true once sent or queued, false only without the service.
    fn request_must(
        &mut self,
        p: &mut dyn Platform,
        service: u32,
        id: u16,
        tlvs: &[(u8, &[u8])],
    ) -> bool {
        self.qmi_request(p, service, id, tlvs, true)
    }

    fn qmi_request(
        &mut self,
        p: &mut dyn Platform,
        service: u32,
        id: u16,
        tlvs: &[(u8, &[u8])],
        must: bool,
    ) -> bool {
        let Some(c) = self.clients.iter_mut().find(|c| c.service == service) else {
            return false;
        };
        c.txn = c.txn.wrapping_add(1).max(1);
        let mut b = qmi::Builder::new(qmi::REQUEST, c.txn, id);
        for (t, v) in tlvs {
            b = b.tlv(*t, v);
        }
        if c.pending.len() >= 16 {
            c.pending.remove(0); // never answered: forget the oldest
        }
        c.pending.push((c.txn, id));
        let (port, dst) = (c.port, c.dst);
        if must {
            self.send_must(p, qrtr::TYPE_DATA, port, dst, &b.finish());
            return true;
        }
        self.send(p, qrtr::TYPE_DATA, port, dst, &b.finish())
    }

    /// The first requests once the modem announces a service we are a client of.
    fn client_up(&mut self, p: &mut dyn Platform, s: Server) {
        if s.node == MODEM_NODE
            && (self.rmtfs.is_none() || !self.radio_enabled || self.radio_failed)
        {
            return;
        }
        let Some(&(_, port)) = CLIENTS.iter().find(|c| c.0 == s.service) else {
            return;
        };
        let dst = (s.node, s.port);
        match self.clients.iter_mut().find(|c| c.service == s.service) {
            Some(c) => c.dst = dst,
            None => self.clients.push(Client {
                service: s.service,
                dst,
                port,
                txn: 0,
                pending: Vec::new(),
            }),
        }
        match s.service {
            DMS => {
                self.request_must(p, DMS, DMS_GET_REVISION, &[]);
                self.request_must(p, DMS, DMS_GET_IDS, &[]);
                self.maybe_online(p);
            }
            UIM => {
                self.request_must(p, UIM, uim::REGISTER, &[(1, &1u32.to_le_bytes())]);
                self.request_must(p, UIM, UIM_GET_CARD_STATUS, &[]);
            }
            NAS => {
                self.request_must(p, NAS, nas::REGISTER_INDICATIONS, &nas::INDICATIONS);
                self.request(p, NAS, nas::GET_SERVING_SYSTEM, &[]);
                self.request(p, NAS, nas::GET_SIGNAL_INFO, &[]);
            }
            VOICE => {
                self.request_must(p, VOICE, voice::INDICATION_REGISTER, &voice::INDICATIONS);
            }
            _ if self.ipa_up => {
                // the modem's IPA server (0x31 instance 2): where IPA's tables are. It must arrive: without it the
                // modem's LTE downlink asserts within a minute ("IPA Assert: new_free_space > 0", C6 2026-11-09)
                self.ipa_init_due = !self.init_driver(p);
            }
            _ => {}
        }
    }

    /// A processor's SSCTL server appeared. Once both are known, tell each that the other is up, in Linux's order
    /// (sysmon: the running modem hears of the ADSP, then the ADSP gets the replay of the modem). An ADSP restart
    /// re-announces its server and so sends both again.
    fn ssctl_up(&mut self, p: &mut dyn Platform, s: Server) {
        let k = match (s.instance & 0xff, s.instance >> 8) {
            (ssctl::VERSION, ssctl::MODEM) => 0,
            (ssctl::VERSION, ssctl::ADSP) => 1,
            _ => return,
        };
        self.ssctl[k] = Some((s.node, s.port));
        let [Some(modem), Some(adsp)] = self.ssctl else {
            return;
        };
        let mut now = true;
        for (dst, name) in [(modem, "adsp"), (adsp, "modem")] {
            self.ssctl_txn = self.ssctl_txn.wrapping_add(1).max(1);
            let mut b = qmi::Builder::new(qmi::REQUEST, self.ssctl_txn, ssctl::SUBSYS_EVENT_REQ);
            for (t, v) in ssctl::event_tlvs(name) {
                b = b.tlv(t, &v);
            }
            now &= self.send_must(p, qrtr::TYPE_DATA, SSCTL_PORT, dst, &b.finish());
        }
        p.log(format_args!(
            "ssctl: {} the modem \"adsp up\" and the adsp \"modem up\"",
            if now { "told" } else { "queued for" }
        ));
    }

    /// INIT_DRIVER to the modem's IPA server: true once sent.
    fn init_driver(&mut self, p: &mut dyn Platform) -> bool {
        let tlvs = ipa::init_driver_tlvs();
        let refs: Vec<(u8, &[u8])> = tlvs.iter().map(|(t, v)| (*t, v.as_slice())).collect();
        let sent = self.request(p, IPA, ipa::INIT_DRIVER, &refs);
        p.log(format_args!(
            "ipa: INIT_DRIVER {}",
            if sent { "sent" } else { "not sent yet" }
        ));
        sent
    }

    /// IPA's INIT_COMPLETE indication, from our server to the modem's IPA server, once the handshake is in.
    fn ipa_complete(&mut self, p: &mut dyn Platform) {
        if !self.ipa.complete_due() {
            return;
        }
        let Some(dst) = self
            .clients
            .iter()
            .find(|c| c.service == IPA)
            .map(|c| c.dst)
        else {
            return;
        };
        let ind = qmi::Builder::new(qmi::INDICATION, 1, ipa::INIT_COMPLETE)
            .result(0)
            .finish();
        self.send_must(p, qrtr::TYPE_DATA, IPA_PORT, dst, &ind);
        p.log(format_args!("ipa: handshake complete"));
    }

    /// The radio on (DMS Set Operating Mode online) once there is a SIM: ModemManager does the same on enable.
    fn maybe_online(&mut self, p: &mut dyn Platform) {
        // a disabled, terminally failed or storage-less radio is never switched (ADSP-only service, PR #2)
        if !self.radio_enabled || self.radio_failed || self.rmtfs.is_none() {
            return;
        }
        // one request at a time, 2 s apart, and at most RADIO_REFUSALS refused ones per intent: the modem answers
        // "invalid transition" when it is already in the mode (C5), which must not become a resend every 2 s
        if self.radio_request.is_some()
            || (self.online_sent && self.now.saturating_sub(self.radio_at) < 2000)
            || self.radio_refusals >= RADIO_REFUSALS
        {
            return;
        }
        let online = !self.status.airplane;
        if (!online || self.status.sim == modem_ipc::sim::PRESENT)
            && (self.status.radio_online != online || !self.online_sent)
            && self.request_must(
                p,
                DMS,
                DMS_SET_OPERATING_MODE,
                &[(1, &[if online { 0 } else { 1 }])],
            )
        {
            self.online_sent = true;
            self.radio_at = self.now;
            let txn = self
                .clients
                .iter()
                .find(|c| c.service == DMS)
                .map_or(0, |c| c.txn);
            self.radio_request = Some((txn, online));
            self.status.pending = true;
            self.status.result = 0;
        }
    }

    /// The user's airplane intent: always recorded (the caller persists it); the radio follows as soon as it can
    /// (now, or from `tick` once there is a SIM and a DMS client). Only a user command in flight refuses it.
    pub fn airplane(&mut self, p: &mut dyn Platform, off: bool) -> Result<(), i32> {
        if self.command.is_some() {
            return Err(KERR_IN_USE);
        }
        if self.status.airplane != off {
            self.radio_refusals = 0; // a new intent may be asked for again
        }
        self.status.airplane = off;
        if off {
            self.post_call = None;
        }
        // Queue the latest intent behind an in-flight mode request; its confirmation is not the new intent.
        if self.radio_request.is_none() {
            self.radio_at = 0;
            self.online_sent = false;
        }
        self.maybe_online(p);
        Ok(())
    }

    fn user_request(
        &mut self,
        p: &mut dyn Platform,
        service: u32,
        id: u16,
        tlvs: &[(u8, &[u8])],
    ) -> Result<(), i32> {
        if self.command.is_some() || self.radio_request.is_some() {
            return Err(KERR_IN_USE);
        }
        if !self.request(p, service, id, tlvs) {
            return Err(KERR_NOT_READY);
        }
        let txn = self
            .clients
            .iter()
            .find(|c| c.service == service)
            .map_or(0, |c| c.txn);
        self.command = Some((service, id, txn, self.now));
        self.status.pending = true;
        self.status.result = 0;
        Ok(())
    }

    pub fn verify_pin(&mut self, p: &mut dyn Platform, pin: &[u8]) -> Result<(), i32> {
        if self.status.sim != modem_ipc::sim::PIN_REQUIRED {
            return Err(KERR_NOT_READY);
        }
        if !(4..=8).contains(&pin.len()) || !pin.iter().all(u8::is_ascii_digit) {
            return Err(KERR_ARGUMENT);
        }
        let mut info = alloc::vec![self.pin_id, pin.len() as u8];
        info.extend_from_slice(pin);
        // Card slot 1 session, as qmicli-uim verify-pin; usable before GW provisioning.
        let result = self.user_request(p, UIM, uim::VERIFY_PIN, &[(1, &[6, 0]), (2, &info)]);
        info.fill(0);
        result
    }

    fn target(&self, id: u8) -> Option<Call> {
        if id == 0 {
            Some(self.status.call)
        } else {
            self.status
                .calls
                .find(id)
                .copied()
                .or_else(|| (self.status.call.id == id).then_some(self.status.call))
        }
    }

    pub fn answer_call(&mut self, p: &mut dyn Platform, id: u8) -> Result<(), i32> {
        let c = self.target(id).ok_or(KERR_NOT_READY)?;
        if !c.incoming || !matches!(c.state, call::INCOMING | call::WAITING) {
            return Err(KERR_NOT_READY);
        }
        if c.state == call::WAITING {
            self.user_request(p, VOICE, voice::MANAGE_CALLS, &[(1, &[3])])
        } else {
            self.user_request(p, VOICE, voice::ANSWER, &[(1, &[c.id])])
        }
    }

    pub fn end_call(&mut self, p: &mut dyn Platform, id: u8) -> Result<(), i32> {
        let c = self.target(id).ok_or(KERR_NOT_READY)?;
        if matches!(c.state, call::NONE | call::END) {
            return Err(KERR_NOT_READY);
        }
        if c.incoming && matches!(c.state, call::INCOMING | call::WAITING) {
            // Reject = User Determined User Busy (3GPP 22.030 CHLD=0, oFono's set_udub): the caller hears busy /
            // declined. A plain END released it like a lost call and the caller's network said "not reachable"
            // (2026-11-14). QMI VOICE Manage Calls type 1 = release held or UDUB the waiting/incoming call; END is the
            // fallback when the modem refuses it.
            self.log.rejected(c.id);
            self.user_request(p, VOICE, voice::MANAGE_CALLS, &[(1, &[1])])?;
            self.rejecting = Some(c.id);
            return Ok(());
        }
        // DIAL can publish its indication before its response. Cancellation bypasses the
        // user-command slot only for that selected outgoing call, never an existing held call.
        let dialing = self
            .command
            .is_some_and(|(s, msg, _, _)| s == VOICE && msg == voice::DIAL)
            && !c.incoming
            && c.id == self.status.call.id
            && matches!(
                c.state,
                call::ORIGINATION
                    | call::CC_IN_PROGRESS
                    | call::ALERTING
                    | call::CONVERSATION
                    | call::DISCONNECTING
            );
        if c.id == 0 || dialing {
            return self.hangup(p);
        }
        self.user_request(p, VOICE, voice::END, &[(1, &[c.id])])
    }

    pub fn manage(&mut self, p: &mut dyn Platform, action: i32, id: u8) -> Result<(), i32> {
        use modem_ipc::manage;
        let c = self.target(id).ok_or(KERR_NOT_READY)?;
        let held = self
            .status
            .calls
            .as_slice()
            .iter()
            .any(|c| c.state == call::HOLD);
        let active = self
            .status
            .calls
            .as_slice()
            .iter()
            .any(|c| c.state == call::CONVERSATION);
        let waiting = self
            .status
            .calls
            .as_slice()
            .iter()
            .any(|c| c.state == call::WAITING);
        // QmiVoiceSupplementaryServiceType: hold/accept=3, select=4, conference=5.
        match action {
            manage::HOLD if c.state == call::CONVERSATION => {
                self.user_request(p, VOICE, voice::MANAGE_CALLS, &[(1, &[3])])
            }
            manage::RESUME if c.state == call::HOLD => {
                self.user_request(p, VOICE, voice::MANAGE_CALLS, &[(1, &[4]), (0x10, &[c.id])])
            }
            manage::SWAP if active && (held || waiting) => {
                self.user_request(p, VOICE, voice::MANAGE_CALLS, &[(1, &[3])])
            }
            manage::MERGE if active && held => {
                self.user_request(p, VOICE, voice::MANAGE_CALLS, &[(1, &[5])])
            }
            manage::HOLD | manage::RESUME | manage::SWAP | manage::MERGE => Err(KERR_NOT_READY),
            _ => Err(KERR_ARGUMENT),
        }
    }

    /// Call waiting on the network (modem_ipc::op::CALL_WAITING): query it, or turn it on or off. The answer arrives
    /// in `status.waiting`.
    pub fn call_waiting(&mut self, p: &mut dyn Platform, action: i32) -> Result<(), i32> {
        use modem_ipc::waiting;
        let r = match action {
            waiting::QUERY => self.user_request(
                p,
                VOICE,
                voice::GET_CALL_WAITING,
                &[(0x10, &[voice::CLASS_VOICE])],
            ),
            waiting::ACTIVATE | waiting::DEACTIVATE => self.user_request(
                p,
                VOICE,
                voice::SET_SUPS_SERVICE,
                &[(1, &[action as u8, voice::REASON_CALL_WAITING])],
            ),
            _ => return Err(KERR_ARGUMENT),
        };
        if r.is_ok() {
            self.waiting_request = action;
            self.status.waiting = waiting::BUSY;
        }
        r
    }

    /// Rebuild volatile state after a confirmed kernel stop. RAM EFS and user intent survive.
    /// `keep_adsp`: the ADSP kept running, so its servers, queued replies, HELLO and TFTP traffic stay. The radio
    /// storage (RAM EFS), the startup policy, the trace switch and the port counter always do; the Wi-Fi firmware's
    /// client goes with the modem (it lives on the modem node) and starts again when its service reappears.
    pub fn reset(&mut self, keep_adsp: bool) {
        let rmtfs = self
            .rmtfs
            .take()
            .map(|mut r| rmtfs::Rmtfs::new(core::mem::take(&mut r.images), r.phys));
        let ipa_up = self.ipa_up;
        let old = self.status;
        let not_modem = |node: u32| keep_adsp && node != MODEM_NODE;
        let retained: Vec<Server> = self
            .servers
            .iter()
            .copied()
            .filter(|s| not_modem(s.node))
            .collect();
        let mut outbox = core::mem::take(&mut self.outbox);
        outbox.retain(|(_, _, dst, _)| not_modem(dst.0));
        let mut hello = core::mem::take(&mut self.hello_seen);
        hello.retain(|&n| not_modem(n));
        let mut transfers = core::mem::take(&mut self.transfers);
        transfers.retain(|t| not_modem(t.peer.0));
        let mut readers = core::mem::take(&mut self.readers);
        readers.retain(|r| not_modem(r.peer.0));
        let (trace, radio_enabled, next_port) = (self.trace, self.radio_enabled, self.next_port);
        // the modem's calls are gone with it: log them as ended now
        self.log.observe(&[], self.now, 0);
        let (log, clir) = (core::mem::take(&mut self.log), self.clir);
        *self = Self::with_rmtfs(rmtfs);
        self.log = log;
        self.clir = clir;
        self.trace = trace;
        self.radio_enabled = radio_enabled;
        self.next_port = next_port;
        self.outbox = outbox;
        self.hello_seen = hello;
        self.transfers = transfers;
        self.readers = readers;
        self.ipa_up = ipa_up;
        self.status.airplane = old.airplane;
        self.status.ring = old.ring;
        self.status.missed = old.missed;
        self.status.log_revision = old.log_revision.wrapping_add(1);
        self.status.generation = old.generation.wrapping_add(1);
        self.status.revision = old.revision;
        self.servers = retained;
        for s in &self.servers {
            if s.service == ssctl::SERVICE
                && s.instance == qrtr::instance_word(ssctl::VERSION, ssctl::ADSP)
            {
                self.ssctl[1] = Some((s.node, s.port));
            }
        }
    }

    fn card_status(&mut self, p: &mut dyn Platform, m: &qmi::Msg) {
        if let Some(c) = uim::card(m) {
            self.status.sim = c.state;
            self.status.pin_attempts = c.attempts;
            self.pin_id = c.pin_id;
            self.card = Some(match c.state {
                modem_ipc::sim::ABSENT => 0,
                modem_ipc::sim::ERROR => 2,
                _ => 1,
            });
            self.maybe_online(p);
        }
    }

    /// The periodic work: NAS serving system and signal every 30 s. `now_ms` is any monotonic clock.
    pub fn tick(&mut self, p: &mut dyn Platform, now_ms: u64) {
        self.now = now_ms;
        if self.response_seen || self.heartbeat_at == 0 {
            self.heartbeat_at = now_ms;
            self.response_seen = false;
        }
        if self.radio_request.is_some() && now_ms.saturating_sub(self.radio_at) >= 15_000 {
            self.radio_request = None;
            self.status.pending = false;
            self.status.result = -33;
        }
        if self
            .command
            .is_some_and(|(_, _, _, t)| now_ms.saturating_sub(t) >= 15_000)
        {
            self.command = None;
            self.status.pending = false;
            self.status.result = -33;
            if self.status.waiting == modem_ipc::waiting::BUSY {
                self.status.waiting = modem_ipc::waiting::ERROR;
            }
        }
        self.maybe_online(p);
        if !self.outbox.is_empty() && now_ms.saturating_sub(self.outbox_at) >= OUTBOX_RETRY_MS {
            self.outbox_at = now_ms;
            self.flush(p);
        }
        self.call_watch(p, now_ms);
        if self.ipa_init_due && now_ms.saturating_sub(self.ipa_init_at) >= 2000 {
            self.ipa_init_at = now_ms; // every 2 s until it goes out (a refused send takes up to 2 s)
            self.ipa_init_due = !self.init_driver(p);
        }
        let mut post_queried = false;
        if let Some((start, next)) = self.post_call {
            if self.status.airplane || now_ms.saturating_sub(start) >= POST_CALL_MS {
                p.log(format_args!(
                    "nas: post-call observation ended after {}ms rat {}",
                    now_ms.saturating_sub(start),
                    self.status.rat
                ));
                self.post_call = None;
            } else if now_ms >= next {
                self.post_call = Some((start, now_ms.saturating_add(1000)));
                self.request(p, NAS, nas::GET_SERVING_SYSTEM, &[]);
                post_queried = true;
            }
        }
        if now_ms.saturating_sub(self.last_poll) < POLL_MS {
            return;
        }
        self.last_poll = now_ms;
        if !post_queried && self.post_call.is_none() {
            self.request(p, NAS, nas::GET_SERVING_SYSTEM, &[]);
        }
        self.request(p, NAS, nas::GET_SIGNAL_INFO, &[]);
    }

    /// Three missed 30-second NAS poll cycles: the transport may be alive while the modem is wedged.
    pub fn watchdog(&self) -> bool {
        !self.status.airplane
            && self.clients.iter().any(|c| c.service == NAS)
            && self.heartbeat_at != 0
            && self.now.saturating_sub(self.heartbeat_at) > 90_000
    }

    /// The call can't stick (C6 2026-11-09: a hangup that never reached the modem left it "dialling" for good, and
    /// every redial got -14): an END not yet sent goes again each second; a call stuck in a passing state ends here
    /// (disconnecting 5 s, dialling 30 s without a word from the modem).
    fn call_watch(&mut self, p: &mut dyn Platform, now_ms: u64) {
        let c = self.status.call;
        if (c.id, c.state) != self.call_seen {
            self.call_seen = (c.id, c.state);
            self.call_since = now_ms;
        }
        if self.hangup_pending && c.id != 0 && now_ms.saturating_sub(self.hangup_at) >= 1000 {
            self.hangup_at = now_ms;
            let _ = self.hangup(p);
        }
        let limit = match c.state {
            call::DISCONNECTING => 5_000,
            call::ORIGINATION | call::CC_IN_PROGRESS | call::SETUP => 30_000,
            _ => return,
        };
        if now_ms.saturating_sub(self.call_since) > limit {
            p.log(format_args!(
                "voice: call {} stuck in state {}: ended here",
                c.id, c.state
            ));
            self.status.call.state = call::END;
            self.hangup_pending = false;
        }
    }

    /// Invalidate the protocol after processor loss; hardware must be stopped before reset/reload.
    pub fn lost(&mut self) {
        // the modem's protocol state goes; the still-running ADSP's (servers, queued replies, HELLO) stays (PR #2).
        // A recovery's reset() after the kernel stop clears radio_failed again.
        self.reset(true);
        self.radio_failed = true;
        self.status.modem = modem_ipc::modem::FAILED;
    }

    /// Place a call (modem_ipc::op::DIAL). KErrArgument for a number [`voice::dialable`] refuses, KErrInUse while a
    /// call is up, KErrNotReady before VOICE is there.
    pub fn dial(&mut self, p: &mut dyn Platform, number: &str) -> Result<(), i32> {
        // Caller ID (C8): a typed *31#/#31# wins, else the setting's prefix (Symbian^3 sends CLIR the same way).
        let shown = number
            .strip_prefix("*31#")
            .or_else(|| number.strip_prefix("#31#"))
            .unwrap_or(number);
        let prefix = match self.clir {
            _ if shown.len() != number.len() => "",
            modem_ipc::clir::HIDE => "#31#",
            modem_ipc::clir::SHOW => "*31#",
            _ => "",
        };
        let wire = alloc::format!("{prefix}{number}");
        let number = shown;
        if !voice::dialable(number) {
            return Err(KERR_ARGUMENT);
        }
        if self.status.airplane {
            return Err(KERR_NOT_READY);
        }
        if self
            .status
            .calls
            .as_slice()
            .iter()
            .any(|c| !matches!(c.state, call::NONE | call::END | call::HOLD))
            || (!matches!(self.status.call.state, call::NONE | call::END | call::HOLD))
        {
            return Err(KERR_IN_USE);
        }
        self.user_request(p, VOICE, voice::DIAL, &[(1, wire.as_bytes())])?;
        self.hangup_pending = false;
        self.status.call = Call {
            id: 0,
            state: call::ORIGINATION,
            incoming: false,
            number: Text::new(number),
            presentation: 0,
        };
        p.log(format_args!("voice: dial {}{}", prefix, mask(number)));
        Ok(())
    }

    /// Answer the incoming call (modem_ipc::op::ANSWER); the modem refuses it before "incoming" (C1: during setup).
    pub fn answer(&mut self, p: &mut dyn Platform) -> Result<(), i32> {
        let c = self.status.call;
        if !(c.incoming && matches!(c.state, call::INCOMING | call::WAITING)) {
            return Err(KERR_NOT_READY);
        }
        self.request(p, VOICE, voice::ANSWER, &[(0x01, &[c.id])])
            .then_some(())
            .ok_or(KERR_NOT_READY)
    }

    /// End, reject or cancel the call (modem_ipc::op::HANGUP). It always ends: the call shows "disconnecting" at
    /// once; an END that can't go out yet (no call id, or the modem takes nothing) is retried by `tick`, whose
    /// watchdog ends the call if the modem never confirms.
    pub fn hangup(&mut self, p: &mut dyn Platform) -> Result<(), i32> {
        let c = self.status.call;
        if matches!(c.state, call::NONE | call::END) {
            return Err(KERR_NOT_READY);
        }
        self.status.call.state = call::DISCONNECTING;
        self.hangup_pending = c.id == 0 || !self.request(p, VOICE, voice::END, &[(0x01, &[c.id])]);
        Ok(())
    }

    /// A keypad tone during a connected call (modem_ipc::op::DTMF): `Some(key)` starts it (`0-9`, `*`, `#`), `None`
    /// stops it.
    pub fn dtmf(&mut self, p: &mut dyn Platform, key: Option<u8>) -> Result<(), i32> {
        let c = self.status.call;
        if c.state != call::CONVERSATION || c.id == 0 {
            return Err(KERR_NOT_READY);
        }
        let sent = match key {
            Some(k) if k.is_ascii_digit() || k == b'*' || k == b'#' => {
                self.request(p, VOICE, voice::START_CONT_DTMF, &[(0x01, &[c.id, k])])
            }
            Some(_) => return Err(KERR_ARGUMENT),
            None => self.request_must(p, VOICE, voice::STOP_CONT_DTMF, &[(0x01, &[c.id])]),
        };
        sent.then_some(()).ok_or(KERR_NOT_READY)
    }

    /// One QRTR packet from the modem.
    pub fn handle(&mut self, p: &mut dyn Platform, packet: &[u8]) {
        let Some((h, payload)) = qrtr::decode(packet) else {
            p.log(format_args!(
                "qrtr: not a v1 packet ({} bytes)",
                packet.len()
            ));
            return;
        };
        if self.trace {
            trace(p, "<", &h, payload);
        }
        if h.dst_node != NODE && h.dst_node != NODE_BCAST {
            return;
        }
        if h.src_node == MODEM_NODE
            && (self.radio_failed || !self.radio_enabled || self.rmtfs.is_none())
        {
            return; // no SSR yet: late packets must not resurrect a terminally failed radio
        }
        let from = (h.src_node, h.src_port);
        // flow control: a sender marks a packet confirm_rx and waits for RESUME_TX before it sends that port more.
        // Every type, control included: unconfirmed NEW_SERVERs stalled the modem's announcements, and DMS or UIM
        // never arrived (C5, 2026-11-09). Linux confirms whatever a socket receives.
        if h.confirm_rx != 0 && h.ty != qrtr::TYPE_RESUME_TX {
            self.send_must(
                p,
                qrtr::TYPE_RESUME_TX,
                h.dst_port,
                from,
                &qrtr::ctrl(qrtr::TYPE_RESUME_TX, [NODE, h.dst_port, 0, 0]),
            );
        }
        match h.ty {
            qrtr::TYPE_DATA => self.data(p, &h, payload),
            qrtr::TYPE_HELLO if !self.hello_seen.contains(&h.src_node) => {
                self.hello_seen.push(h.src_node);
                self.ctrl(p, qrtr::TYPE_HELLO, (h.src_node, PORT_CTRL), [0; 4]);
                self.announce(p, (h.src_node, PORT_CTRL));
            }
            qrtr::TYPE_NEW_SERVER | qrtr::TYPE_DEL_SERVER => {
                let Some((_, [service, instance, node, port])) = qrtr::ctrl_words(payload) else {
                    return;
                };
                let s = Server {
                    service,
                    instance,
                    node,
                    port,
                };
                self.servers.retain(|x| *x != s);
                if h.ty == qrtr::TYPE_DEL_SERVER || (service == 0 && port == 0) {
                    return;
                }
                self.servers.push(s);
                if service == ssctl::SERVICE {
                    self.ssctl_up(p, s);
                }
                p.log(format_args!(
                    "qrtr: server {service} v{} i{} at {node}:{port}",
                    instance & 0xff,
                    instance >> 8
                ));
                self.client_up(p, s);
                if service == wlfw::SERVICE && node == MODEM_NODE {
                    let mut c = wlfw::Client::new(WLAN_MSA.0, WLAN_MSA.1);
                    let steps = c.start();
                    self.wlfw = Some(((node, port), c));
                    p.log(format_args!(
                        "wlfw: the Wi-Fi firmware's service is up; starting"
                    ));
                    self.wlfw_steps(p, steps);
                }
            }
            qrtr::TYPE_NEW_LOOKUP => {
                // the remote looks up our services: matching NEW_SERVERs, then an empty one ends the list
                let Some((_, [service, instance, _, _])) = qrtr::ctrl_words(payload) else {
                    return;
                };
                // what the remote expects to find here (Wi-Fi W0: services the modem waits for)
                p.log(format_args!(
                    "qrtr: lookup {service} i{} from {}:{}",
                    instance >> 8,
                    h.src_node,
                    h.src_port
                ));
                let services: Vec<_> = self.services().collect();
                for (s, v, i, port) in services {
                    if (service == 0 || service == s)
                        && (instance == 0 || instance == qrtr::instance_word(v, i))
                    {
                        self.ctrl(
                            p,
                            qrtr::TYPE_NEW_SERVER,
                            from,
                            [s, qrtr::instance_word(v, i), NODE, port],
                        );
                    }
                }
                self.ctrl(p, qrtr::TYPE_NEW_SERVER, from, [0; 4]);
            }
            _ => {} // BYE, DEL_CLIENT, RESUME_TX: nothing to undo yet
        }
    }

    fn data(&mut self, p: &mut dyn Platform, h: &Header, payload: &[u8]) {
        let from = (h.src_node, h.src_port);
        match h.dst_port {
            IPA_PORT if self.ipa_up => {
                let Some(m) = qmi::Msg::parse(payload) else {
                    return;
                };
                if m.ty != qmi::REQUEST {
                    return;
                }
                let resp = self.ipa.request(&m);
                p.log(format_args!(
                    "ipa: request {:#x} from the modem{}",
                    m.id,
                    if resp.is_some() {
                        ""
                    } else {
                        " (no handler: unanswered, as Linux)"
                    }
                ));
                if let Some(resp) = resp {
                    self.send_must(p, qrtr::TYPE_DATA, IPA_PORT, from, &resp);
                }
                self.ipa_complete(p);
            }
            RMTFS_PORT | LOCATOR_PORT => {
                let Some(m) = qmi::Msg::parse(payload) else {
                    return;
                };
                let resp = if h.dst_port == RMTFS_PORT {
                    let Some(rmtfs) = &mut self.rmtfs else {
                        return;
                    };
                    rmtfs.handle(p, &m)
                } else {
                    locator::handle(&m)
                };
                if h.dst_port == LOCATOR_PORT {
                    let name = m.tlv(1).unwrap_or(&[]);
                    p.log(format_args!(
                        "locator: {:?}",
                        core::str::from_utf8(name).unwrap_or("?")
                    ));
                }
                self.send_must(p, qrtr::TYPE_DATA, h.dst_port, from, &resp);
            }
            SSCTL_PORT => {
                let Some(m) = qmi::Msg::parse(payload).filter(|m| m.ty == qmi::RESPONSE) else {
                    return; // the answers only (the processors send other messages on this port too)
                };
                let (who, what) = if h.src_node == MODEM_NODE {
                    ("modem", "adsp")
                } else {
                    ("adsp", "modem")
                };
                p.log(format_args!(
                    "ssctl: the {who} took \"{what} up\" -> {:?}",
                    m.result().unwrap_or(Err(u16::MAX))
                ));
            }
            TFTP_PORT => match tftp::request(payload, from, self.next_port) {
                tftp::Action::Reply(r) => {
                    p.log(format_args!("tftp: refused {:?}", tftp_name(payload)));
                    self.send_must(p, qrtr::TYPE_DATA, TFTP_PORT, from, &r);
                }
                tftp::Action::Start(t, first) => {
                    p.log(format_args!("tftp: write {:?}", t.name));
                    self.next_port += 1;
                    let port = t.port;
                    self.transfers.push(t);
                    self.send_must(p, qrtr::TYPE_DATA, port, from, &first);
                }
                tftp::Action::Read(q) => match p.fw_size(&q.path) {
                    Ok(size) => {
                        p.log(format_args!(
                            "tftp: read {:?} ({size} bytes), blksize {} wsize {} rsize {} seek {:?}",
                            tftp_name(payload),
                            q.blksize,
                            q.wsize,
                            q.rsize,
                            q.seek
                        ));
                        let port = self.next_port;
                        self.next_port += 1;
                        let (r, first) = tftp::start_read(q, from, port, size, &mut |f, at, b| {
                            p.fw_read(f, at, b)
                        });
                        if self.readers.len() >= 8 {
                            self.readers.remove(0); // abandoned reads: keep the newest
                        }
                        self.readers.push(r);
                        self.send_must(p, qrtr::TYPE_DATA, port, from, &first);
                    }
                    Err(e) => {
                        p.log(format_args!("tftp: refused {:?} ({e})", tftp_name(payload)));
                        let r = tftp::error(1, "File not found");
                        self.send_must(p, qrtr::TYPE_DATA, TFTP_PORT, from, &r);
                    }
                },
                tftp::Action::Ignore => {}
            },
            WLFW_PORT => {
                let steps = match &mut self.wlfw {
                    Some((dst, c)) if *dst == from => c.message(payload),
                    _ => return,
                };
                self.wlfw_steps(p, steps);
            }
            port if self
                .readers
                .iter()
                .any(|r| r.port == port && r.peer == from) =>
            {
                let i = self
                    .readers
                    .iter()
                    .position(|r| r.port == port)
                    .unwrap_or(0);
                let (out, done) = tftp::reader(&self.readers[i], payload, &mut |f, at, b| {
                    p.fw_read(f, at, b)
                });
                for d in &out {
                    self.send_must(p, qrtr::TYPE_DATA, port, from, d);
                }
                if done {
                    let r = self.readers.remove(i);
                    p.log(format_args!("tftp: read of {} done", r.path));
                }
            }
            port if self.clients.iter().any(|c| c.port == port && c.dst == from) => {
                self.client(p, port, payload)
            }
            port => {
                let Some(i) = self.transfers.iter().position(|t| t.port == port) else {
                    p.log(format_args!(
                        "qrtr: {} bytes for unserved port {port} from {}:{}, dropped",
                        payload.len(),
                        from.0,
                        from.1
                    ));
                    return;
                };
                let (reply, done) = tftp::transfer(&mut self.transfers[i], payload);
                if let Some(r) = reply {
                    self.send_must(p, qrtr::TYPE_DATA, port, from, &r);
                }
                if done {
                    let t = self.transfers.remove(i);
                    p.log(format_args!(
                        "tftp: {:?} {} bytes kept in RAM",
                        t.name,
                        t.data.len()
                    ));
                    self.files.retain(|f| f.0 != t.name);
                    self.files.push((t.name, t.data));
                }
            }
        }
    }

    /// Carry out what the WLFW client asks for (wlfw.rs): sends, the MSA assignment, the board data
    fn wlfw_steps(&mut self, p: &mut dyn Platform, mut steps: Vec<wlfw::Step>) {
        while !steps.is_empty() {
            let mut next = Vec::new();
            for s in steps {
                let Some((dst, _)) = &self.wlfw else {
                    return;
                };
                let dst = *dst;
                match s {
                    wlfw::Step::Send(m) => {
                        let id = qmi::Msg::parse(&m).map_or(0, |q| q.id);
                        p.log(format_args!("wlfw: request {id:#x}"));
                        self.send_must(p, qrtr::TYPE_DATA, WLFW_PORT, dst, &m);
                    }
                    wlfw::Step::AssignMsa(regions) => {
                        for r in &regions {
                            p.log(format_args!(
                                "wlfw: MSA region {:#x} + {:#x}{}",
                                r.addr,
                                r.size,
                                if r.secure { " (secure)" } else { "" }
                            ));
                        }
                        let r = p.wlan_msa(&regions);
                        p.log(format_args!("wlfw: MSA assigned -> {r:?}"));
                        if let Some((_, c)) = &mut self.wlfw {
                            next.extend(c.msa_assigned(r.is_ok()));
                        }
                    }
                    wlfw::Step::LoadBoardData => {
                        let data = p.board_data(self.wlfw_board);
                        p.log(format_args!("wlfw: board data {} bytes", data.len()));
                        if let Some((_, c)) = &mut self.wlfw {
                            next.extend(c.board_data(data));
                        }
                    }
                    wlfw::Step::Caps(k) => {
                        self.wlfw_board = k.board_id;
                        p.log(format_args!(
                            "wlfw: chip {:#x} family {:#x} board {:#x} soc {:#x} fw {:#x} {} {}",
                            k.chip_id,
                            k.chip_family,
                            k.board_id,
                            k.soc_id,
                            k.fw_version,
                            k.fw_timestamp,
                            k.fw_build_id
                        ));
                    }
                    wlfw::Step::Ready => p.log(format_args!("wlfw: firmware ready (W1 done)")),
                    wlfw::Step::Enabled => {
                        let r = p.wlan_start();
                        p.log(format_args!(
                            "wlfw: mission mode; host copy engines start -> {r:?}"
                        ));
                    }
                    wlfw::Step::Failed(what, e) => p.log(format_args!("wlfw: {what} failed ({e})")),
                }
            }
            steps = next;
        }
    }

    fn client(&mut self, p: &mut dyn Platform, port: u32, payload: &[u8]) {
        let Some(m) = qmi::Msg::parse(payload) else {
            return;
        };
        let Some(c) = self.clients.iter_mut().find(|c| c.port == port) else {
            return;
        };
        let service = c.service;
        match m.ty {
            qmi::RESPONSE => {
                let Some(k) = c.pending.iter().position(|&r| r == (m.txn, m.id)) else {
                    p.log(format_args!(
                        "qmi: service {service} response {:#x} txn {} not ours",
                        m.id, m.txn
                    ));
                    return;
                };
                c.pending.remove(k);
                self.response_seen = true;
                self.response(p, service, &m);
            }
            qmi::INDICATION => self.indication(p, service, &m),
            _ => {}
        }
    }

    fn response(&mut self, p: &mut dyn Platform, service: u32, m: &qmi::Msg) {
        if self
            .command
            .is_some_and(|(s, id, txn, _)| s == service && id == m.id && txn == m.txn)
        {
            self.command = None;
            self.status.pending = false;
            self.status.result = match m.result() {
                Some(Ok(())) => 0,
                Some(Err(e)) => -1000 - i32::from(e),
                None => -20,
            };
        }
        if (service, m.id) == (UIM, uim::VERIFY_PIN) {
            if let Some(n) = m.u8(0x10) {
                self.status.pin_attempts = n;
                if n == 0 {
                    self.status.sim = modem_ipc::sim::BLOCKED;
                }
            }
            self.request(p, UIM, UIM_GET_CARD_STATUS, &[]);
        }
        if (service, m.id) == (DMS, DMS_SET_OPERATING_MODE)
            && self.radio_request.is_some_and(|(txn, _)| txn == m.txn)
        {
            let (_, online) = self.radio_request.take().unwrap();
            self.status.pending = self.command.is_some();
            if m.result() == Some(Ok(())) {
                self.radio_refusals = 0;
                self.status.radio_online = online;
                p.log(format_args!(
                    "dms: confirmed online {} desired airplane {}",
                    online, self.status.airplane
                ));
                self.status.modem = if online {
                    modem_ipc::modem::ONLINE
                } else {
                    modem_ipc::modem::RUNNING
                };
                if !online {
                    self.status.reg = modem_ipc::reg::NONE;
                    self.status.rat = modem_ipc::rat::NONE;
                    self.status.bars = 0;
                }
            } else {
                self.radio_refusals = self.radio_refusals.saturating_add(1);
                self.status.result = -2;
            }
        }
        let text = |t: Option<&[u8]>| t.map(|b| String::from_utf8_lossy(b).into_owned());
        let Some(Ok(())) = m.result() else {
            p.log(format_args!(
                "qmi: service {service} request {:#x} failed {:?}",
                m.id,
                m.result()
            ));
            if (service, m.id) == (VOICE, voice::DIAL) {
                self.status.call.state = call::END;
            }
            if service == VOICE && matches!(m.id, voice::GET_CALL_WAITING | voice::SET_SUPS_SERVICE)
            {
                self.status.waiting = modem_ipc::waiting::ERROR;
            }
            if (service, m.id) == (VOICE, voice::MANAGE_CALLS)
                && let Some(id) = self.rejecting.take()
            {
                p.log(format_args!("voice: busy reject refused; ending call {id}"));
                self.request(p, VOICE, voice::END, &[(1, &[id])]);
            }
            return;
        };
        match (service, m.id) {
            (DMS, DMS_GET_REVISION) => {
                self.revision = text(m.tlv(0x01));
                p.log(format_args!(
                    "dms: revision {}",
                    self.revision.as_deref().unwrap_or("?")
                ));
            }
            (DMS, DMS_GET_IDS) => {
                self.imei = text(m.tlv(0x11));
                p.log(format_args!(
                    "dms: IMEI {}",
                    self.imei.as_deref().map(mask).unwrap_or_default()
                ));
            }
            (DMS, DMS_SET_OPERATING_MODE) => self.maybe_online(p),
            (UIM, UIM_GET_CARD_STATUS) => self.card_status(p, m),
            (NAS, nas::GET_SERVING_SYSTEM) => self.serving(p, m),
            (NAS, nas::GET_SIGNAL_INFO) => self.signal(m),
            (NAS, nas::SET_SYSTEM_SELECTION) => p.log(format_args!("nas: voice-only mode set")),
            (NAS, nas::GET_SYSTEM_SELECTION) => p.log(format_args!(
                "nas: mode preference {:#x}",
                nas::mode_preference(m).unwrap_or(0)
            )),
            (NAS, nas::GET_NETWORK_TIME) => self.network_time(p, m),
            (NAS, nas::GET_PLMN_NAME) => {
                if let Some(n) = nas::plmn_name(m) {
                    p.log(format_args!("nas: operator {n}"));
                    self.status.operator = Text::new(&n);
                }
            }
            (IPA, ipa::INIT_DRIVER) => {
                p.log(format_args!("ipa: the modem took INIT_DRIVER"));
                self.ipa.modem_ready = true;
                self.ipa_complete(p);
            }
            (VOICE, voice::MANAGE_CALLS) => self.rejecting = None,
            (VOICE, voice::GET_CALL_WAITING) => {
                let on = m.u8(0x10).is_some_and(|c| c & voice::CLASS_VOICE != 0);
                self.status.waiting = if on {
                    modem_ipc::waiting::ON
                } else {
                    modem_ipc::waiting::OFF
                };
            }
            (VOICE, voice::SET_SUPS_SERVICE) => {
                self.status.waiting = if self.waiting_request == modem_ipc::waiting::ACTIVATE {
                    modem_ipc::waiting::ON
                } else {
                    modem_ipc::waiting::OFF
                };
            }
            (VOICE, voice::DIAL) => {
                self.status.call.id = m.u8(0x10).unwrap_or(0);
                if self.hangup_pending {
                    self.hangup_pending = false;
                    let _ = self.hangup(p);
                }
            }
            _ => {} // the indication registrations, answer, end: their indications tell the rest
        }
    }

    fn indication(&mut self, p: &mut dyn Platform, service: u32, m: &qmi::Msg) {
        match (service, m.id) {
            (NAS, nas::GET_SERVING_SYSTEM) => self.serving(p, m),
            (NAS, nas::SIGNAL_INFO_IND) => self.signal(m),
            (NAS, nas::NETWORK_TIME_IND) => self.network_time(p, m),
            (UIM, uim::STATUS_IND) => self.card_status(p, m),
            (VOICE, voice::ALL_CALL_STATUS_IND) => {
                let Some(calls) = voice::all_calls(m) else {
                    return;
                };
                let old = self.status.calls;
                let mut next = modem_ipc::Calls {
                    len: calls.len() as u8,
                    ..Default::default()
                };
                for (i, c) in calls.into_iter().enumerate() {
                    let previous = old.find(c.id).copied().or_else(|| {
                        (self.status.call.id == c.id || self.status.call.id == 0)
                            .then_some(self.status.call)
                    });
                    next.entries[i] = Call {
                        id: c.id,
                        state: c.state,
                        incoming: c.incoming,
                        number: c.number.map_or_else(
                            || previous.map_or(Text::default(), |x| x.number),
                            |n| Text::new(&n),
                        ),
                        presentation: c.presentation,
                    };
                    if previous.is_none_or(|old| (old.id, old.state) != (c.id, c.state)) {
                        p.log(format_args!(
                            "voice: call {} state {} incoming {}",
                            c.id, c.state, c.incoming
                        ));
                    }
                    if c.state == call::CONVERSATION && !self.answered.contains(&c.id) {
                        self.answered.push(c.id);
                    }
                }
                for c in old.as_slice() {
                    let ended = next.find(c.id).is_none_or(|n| n.state == call::END);
                    if ended
                        && !matches!(c.state, call::NONE | call::END)
                        && next.find(c.id).is_none()
                    {
                        p.log(format_args!(
                            "voice: call {} absent from new list (ended)",
                            c.id
                        ));
                    }
                    if ended
                        && c.incoming
                        && !matches!(c.state, call::NONE | call::END)
                        && !self.answered.contains(&c.id)
                        && !self.log.is_rejected(c.id)
                    {
                        self.status.missed = self.status.missed.wrapping_add(1);
                    }
                }
                self.answered
                    .retain(|id| next.find(*id).is_some_and(|c| c.state != call::END));
                let active = |c: &Call| !matches!(c.state, call::NONE | call::END);
                let old_active = old.as_slice().iter().any(active)
                    || (old.len == 0 && active(&self.status.call));
                if next.as_slice().iter().any(active) {
                    self.post_call = None;
                } else if old_active && !self.status.airplane {
                    self.post_call = Some((self.now, self.now));
                    p.log(format_args!(
                        "nas: final call END at {}ms; observing LTE return",
                        self.now
                    ));
                }
                self.status.calls = next;
                let utc = p.utc_ms();
                if self.log.observe(next.as_slice(), self.now, utc) {
                    self.status.log_revision = self.status.log_revision.wrapping_add(1);
                }
                // Incoming/waiting gets UI priority; audio independently scans the complete list.
                self.status.call = next
                    .as_slice()
                    .iter()
                    .min_by_key(|c| match c.state {
                        call::INCOMING | call::WAITING => 0,
                        call::CONVERSATION => 1,
                        call::ORIGINATION | call::ALERTING | call::CC_IN_PROGRESS => 2,
                        call::HOLD => 3,
                        call::DISCONNECTING => 4,
                        _ => 5,
                    })
                    .copied()
                    .unwrap_or(Call {
                        state: if self.status.call.state != call::NONE {
                            call::END
                        } else {
                            call::NONE
                        },
                        ..self.status.call
                    });
                if self.hangup_pending && self.status.call.id != 0 {
                    self.hangup_pending = false;
                    let _ = self.hangup(p);
                }
            }
            _ => {}
        }
    }

    fn serving(&mut self, p: &mut dyn Platform, m: &qmi::Msg) {
        let Some(s) = nas::serving(m) else {
            return;
        };
        if (s.reg, s.rat, s.cs) != (self.status.reg, self.status.rat, self.cs) {
            p.log(format_args!(
                "nas: registration {} rat {} cs {} ps {}",
                s.reg, s.rat, s.cs, s.ps
            ));
        }
        self.cs = s.cs;
        self.status.reg = s.reg;
        self.status.rat = s.rat;
        let registered = matches!(s.reg, modem_ipc::reg::HOME | modem_ipc::reg::ROAMING);
        if registered
            && s.rat == modem_ipc::rat::LTE
            && let Some((start, _)) = self.post_call.take()
        {
            p.log(format_args!(
                "nas: post-call LTE confirmed after {}ms",
                self.now.saturating_sub(start)
            ));
        }
        if registered {
            // online, whether or not our Set Operating Mode was needed (C5: "invalid transition" when it already was)
            self.status.modem = modem_ipc::modem::ONLINE;
            self.status.radio_online = true;
            // voice-only once registered: sent as soon as NAS appeared, the modem refused it (QMI error 25) and its
            // SIM service never came up (C5, 2026-11-09)
            if !self.ipa_up && !self.selection_sent {
                self.selection_sent = true;
                self.request(p, NAS, nas::SET_SYSTEM_SELECTION, &nas::VOICE_ONLY);
                self.request(p, NAS, nas::GET_SYSTEM_SELECTION, &[]);
            }
            // the network's time (NITZ) once: its indication may have come before we subscribed
            if !self.time_asked {
                self.time_asked = true;
                self.request(p, NAS, nas::GET_NETWORK_TIME, &[]);
            }
        }
        if !registered {
            self.status.bars = 0;
            self.status.operator = Text::default();
            self.plmn_asked = None;
            return;
        }
        if let Some(n) = s.name {
            self.status.operator = Text::new(&n);
        } else if let Some((mcc, mnc)) = s.plmn.filter(|&q| self.plmn_asked != Some(q)) {
            self.plmn_asked = Some((mcc, mnc));
            let mut q = [0u8; 4];
            q[..2].copy_from_slice(&mcc.to_le_bytes());
            q[2..].copy_from_slice(&mnc.to_le_bytes());
            self.request(p, NAS, nas::GET_PLMN_NAME, &[(0x01, &q)]);
        }
    }

    /// The network's time (NITZ) for the phone's automatic time: a new value each time the network sends one.
    fn network_time(&mut self, p: &mut dyn Platform, m: &qmi::Msg) {
        let Some(t) = nas::network_time(m) else {
            p.log(format_args!("nas: no network time in {:#x}", m.id));
            return;
        };
        p.log(format_args!(
            "nas: network time {} UTC, zone {} quarter hours, dst {}",
            t.utc, t.tz, t.dst
        ));
        self.status.nitz = modem_ipc::Nitz {
            utc: t.utc,
            tz: t.tz,
            dst: t.dst,
            seq: self.status.nitz.seq.wrapping_add(1),
        };
    }

    fn signal(&mut self, m: &qmi::Msg) {
        if let Some(b) = nas::bars(m, self.status.rat) {
            let registered = matches!(
                self.status.reg,
                modem_ipc::reg::HOME | modem_ipc::reg::ROAMING
            );
            self.status.bars = if registered { b } else { 0 };
        }
    }
}

/// The first 40 bytes as hex (a log line must stay under the monitor's 256 bytes).
struct Hex<'a>(&'a [u8]);

impl fmt::Display for Hex<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0.iter().take(40) {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// One line per packet: direction, type, src -> dst, size, and the QMI header or control words.
fn trace(p: &mut dyn Platform, dir: &str, h: &Header, payload: &[u8]) {
    let (src, dst) = (h.src_port as i32, h.dst_port as i32);
    if h.ty != qrtr::TYPE_DATA {
        let w = qrtr::ctrl_words(payload).map(|c| c.1).unwrap_or_default();
        p.log(format_args!(
            "{dir} ctrl {} {}:{src} -> {}:{dst} {w:?}",
            h.ty, h.src_node, h.dst_node
        ));
        return;
    }
    let confirm = if h.confirm_rx != 0 { " confirm" } else { "" };
    match qmi::Msg::parse(payload) {
        Some(m) => p.log(format_args!(
            "{dir} data {}:{src} -> {}:{dst} {} B qmi type {} id {:#x} txn {}{confirm} {}",
            h.src_node,
            h.dst_node,
            payload.len(),
            m.ty,
            m.id,
            m.txn,
            Hex(payload)
        )),
        None => p.log(format_args!(
            "{dir} data {}:{src} -> {}:{dst} {} B{confirm} {}",
            h.src_node,
            h.dst_node,
            payload.len(),
            Hex(payload)
        )),
    }
}

fn tftp_name(b: &[u8]) -> &str {
    b.get(2..)
        .and_then(|s| s.split(|&c| c == 0).next())
        .and_then(|s| core::str::from_utf8(s).ok())
        .unwrap_or("?")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::format;

    #[derive(Default)]
    struct Fake {
        sent: Vec<Vec<u8>>,
        nodes: Vec<u32>,
        /// refuse this many sends (no receive intent yet)
        refuse: usize,
        mem: Vec<u8>,
        logs: Vec<String>,
    }

    impl Platform for Fake {
        fn send(&mut self, node: u32, packet: &[u8]) -> Result<(), i32> {
            if self.refuse > 0 {
                self.refuse -= 1;
                return Err(KERR_NOT_READY);
            }
            self.sent.push(packet.to_vec());
            self.nodes.push(node);
            Ok(())
        }
        fn rmtfs_read(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), i32> {
            buf.copy_from_slice(&self.mem[offset as usize..offset as usize + buf.len()]);
            Ok(())
        }
        fn rmtfs_write(&mut self, offset: u32, data: &[u8]) -> Result<(), i32> {
            self.mem[offset as usize..offset as usize + data.len()].copy_from_slice(data);
            Ok(())
        }
        fn log(&mut self, args: fmt::Arguments) {
            self.logs.push(format!("{args}"));
        }
    }

    fn fake() -> Fake {
        Fake {
            mem: alloc::vec![0; 0x20_0000],
            ..Default::default()
        }
    }

    fn modem() -> Modem {
        let img = |n: usize, fill: u8| alloc::vec![fill; n];
        Modem::new(
            [
                img(0x20_0000, 1),
                img(0x20_0000, 2),
                img(0x20_0000, 3),
                img(0x2_0000, 4),
            ],
            0xfc20_1000,
        )
    }

    fn from_modem(ty: u32, src_port: u32, dst_port: u32, payload: &[u8]) -> Vec<u8> {
        let h = Header {
            ty,
            src_node: 0,
            src_port,
            dst_node: NODE,
            dst_port,
            ..Default::default()
        };
        qrtr::encode(&h, payload)
    }

    fn reply(f: &Fake) -> qmi::Msg<'_> {
        let (_, pay) = qrtr::decode(f.sent.last().unwrap()).unwrap();
        qmi::Msg::parse(pay).unwrap()
    }

    #[test]
    fn support_router_serves_adsp_without_advertising_radio_storage() {
        let (mut m, mut f) = (Modem::support_only(), fake());
        m.ipa_up = true; // even a prematurely set IPA flag must not expose unprepared radio resources
        assert!(m.services().all(|s| s.0 != rmtfs::SERVICE && s.0 != IPA));
        m.start(&mut f);
        assert!(f.sent.is_empty());
        let hello = Header {
            ty: qrtr::TYPE_HELLO,
            src_node: 5,
            src_port: PORT_CTRL,
            dst_node: NODE,
            dst_port: PORT_CTRL,
            ..Default::default()
        };
        m.handle(
            &mut f,
            &qrtr::encode(&hello, &qrtr::ctrl(qrtr::TYPE_HELLO, [0; 4])),
        );
        assert_eq!(f.sent.len(), 3); // HELLO, locator, TFTP
        assert!(f.nodes.iter().all(|&node| node == 5));
        let q = qmi::Builder::new(qmi::REQUEST, 3, 0x21)
            .tlv(1, b"avs/audio")
            .finish();
        let h = Header {
            ty: qrtr::TYPE_DATA,
            dst_port: LOCATOR_PORT,
            ..hello
        };
        m.handle(&mut f, &qrtr::encode(&h, &q));
        assert_eq!(reply(&f).result(), Some(Ok(())));
        let before = f.sent.len();
        let alloc = qmi::Builder::new(qmi::REQUEST, 4, 4).finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 17, RMTFS_PORT, &alloc));
        assert_eq!(f.sent.len(), before); // no reply containing an invented address
    }

    #[test]
    fn attaching_storage_preserves_adsp_discovery_and_retry_queue() {
        let (mut m, mut f) = (Modem::support_only(), fake());
        f.refuse = 1;
        let h = Header {
            ty: qrtr::TYPE_HELLO,
            src_node: 5,
            src_port: PORT_CTRL,
            dst_node: NODE,
            dst_port: PORT_CTRL,
            ..Default::default()
        };
        let hello = qrtr::encode(&h, &qrtr::ctrl(qrtr::TYPE_HELLO, [0; 4]));
        m.handle(&mut f, &hello);
        let queued = m.outbox.clone();
        assert_eq!(queued.len(), 3);
        assert_eq!(m.attach_radio(Default::default(), 0), Err(KERR_ARGUMENT));
        assert!(m.rmtfs.is_none());
        let storage = || core::array::from_fn(|_| alloc::vec![1; 512]);
        assert_eq!(m.attach_radio(storage(), 0xfc20_1000), Ok(()));
        assert_eq!(m.outbox, queued);
        assert_eq!(m.hello_seen, [5]);
        assert_eq!(m.attach_radio(storage(), 0xfc20_1000), Err(KERR_IN_USE));
        m.handle(&mut f, &hello); // existing ADSP HELLO stays suppressed after attachment
        assert_eq!(m.outbox, queued);
        m.tick(&mut f, 1000);
        assert!(m.outbox.is_empty());
        assert!(f.nodes.iter().all(|&node| node == 5));
        assert!(m.services().any(|s| s.0 == rmtfs::SERVICE));
    }

    #[test]
    fn failed_radio_cannot_block_queued_adsp_replies() {
        let (mut m, mut f) = (modem(), fake());
        f.refuse = 2;
        m.ctrl(&mut f, qrtr::TYPE_HELLO, (MODEM_NODE, PORT_CTRL), [0; 4]);
        m.ctrl(&mut f, qrtr::TYPE_HELLO, (5, PORT_CTRL), [0; 4]);
        m.lost();
        assert_eq!(m.outbox.len(), 1);
        assert_eq!(m.outbox[0].2.0, 5);
        m.tick(&mut f, 1000); // second refusal remains queued
        assert_eq!(m.outbox.len(), 1);
        m.tick(&mut f, 2000);
        assert!(m.outbox.is_empty());
        assert_eq!(f.nodes, [5]);
    }

    #[test]
    fn late_radio_packets_after_failure_cannot_recreate_head_of_line_blocking() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(DMS, 68));
        m.lost();
        f.sent.clear();
        f.nodes.clear();
        f.refuse = 1;
        let mut late = ns(DMS, 68);
        late[16] = 1; // even its flow-control response must not requeue dead-radio traffic
        m.handle(&mut f, &late);
        assert!(m.clients.is_empty());
        assert!(m.outbox.is_empty());
        let h = Header {
            ty: qrtr::TYPE_HELLO,
            src_node: 5,
            src_port: PORT_CTRL,
            dst_node: NODE,
            dst_port: PORT_CTRL,
            ..Default::default()
        };
        m.handle(
            &mut f,
            &qrtr::encode(&h, &qrtr::ctrl(qrtr::TYPE_HELLO, [0; 4])),
        );
        assert_eq!(m.outbox.len(), 4); // HELLO and the still-attached support services
        m.tick(&mut f, 1000);
        assert!(m.outbox.is_empty());
        assert_eq!(f.nodes, [5, 5, 5, 5]);
        assert_eq!(m.status.modem, modem_ipc::modem::FAILED);
    }

    #[test]
    fn refused_online_transition_retries_the_same_transaction_once() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(DMS, 68));
        f.sent.clear();
        f.nodes.clear();
        m.status.sim = modem_ipc::sim::PRESENT;
        f.refuse = 1;
        m.maybe_online(&mut f);
        assert!(m.online_sent); // accepted for ordered delivery, not yet reported online
        assert!(f.sent.is_empty());
        let queued = m.outbox[0].3.clone();
        m.maybe_online(&mut f); // no duplicate transaction while queued
        assert_eq!(m.outbox.len(), 1);
        m.tick(&mut f, 500);
        assert!(f.sent.is_empty());
        m.tick(&mut f, 1000);
        m.tick(&mut f, 2000);
        assert_eq!(f.sent.len(), 1);
        let (_, actual) = qrtr::decode(&f.sent[0]).unwrap();
        assert_eq!(actual, queued);
        assert_eq!(qmi::Msg::parse(actual).unwrap().id, DMS_SET_OPERATING_MODE);
        assert_ne!(m.status.modem, modem_ipc::modem::ONLINE);
    }

    #[test]
    fn refused_storage_and_locator_replies_keep_their_transactions() {
        for (port, id) in [(RMTFS_PORT, 4), (LOCATOR_PORT, 0x21)] {
            let (mut m, mut f) = (modem(), fake());
            let request = qmi::Builder::new(qmi::REQUEST, 7, id)
                .tlv(1, b"avs/audio")
                .finish();
            f.refuse = 1;
            m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 17, port, &request));
            assert!(f.sent.is_empty());
            assert_eq!(m.outbox.len(), 1);
            let queued = m.outbox[0].3.clone();
            m.tick(&mut f, 1000);
            m.tick(&mut f, 2000);
            assert_eq!(f.sent.len(), 1);
            let (header, actual) = qrtr::decode(&f.sent[0]).unwrap();
            assert_eq!((header.src_port, header.dst_port), (port, 17));
            assert_eq!(actual, queued);
            let reply = qmi::Msg::parse(actual).unwrap();
            assert_eq!((reply.ty, reply.txn, reply.id), (qmi::RESPONSE, 7, id));
            assert_eq!(reply.result(), Some(Ok(())));
        }
    }

    #[test]
    fn disabled_radio_does_not_send_boot_announcements_or_online_requests() {
        let (mut m, mut f) = (modem(), fake());
        m.radio_enabled = false;
        m.status.sim = modem_ipc::sim::PRESENT;
        m.start(&mut f);
        m.maybe_online(&mut f);
        m.tick(&mut f, 30000);
        assert!(f.sent.is_empty());
        assert!(!m.online_sent);
    }

    #[test]
    fn start_says_hello_and_announces_three_services() {
        let (mut m, mut f) = (modem(), fake());
        m.start(&mut f);
        assert_eq!(f.sent.len(), 4);
        let (h, p) = qrtr::decode(&f.sent[1]).unwrap();
        assert_eq!(
            (h.ty, h.dst_node, h.dst_port),
            (qrtr::TYPE_NEW_SERVER, 0, PORT_CTRL)
        );
        assert_eq!(qrtr::ctrl_words(p), Some((4, [14, 1, 1, RMTFS_PORT])));
    }

    // C6: the ADSP's router (node 5) says HELLO on its own IPCRTR; its audio PD then asks our locator for avs/audio
    #[test]
    fn the_adsp_gets_hello_services_and_the_locator() {
        let (mut m, mut f) = (modem(), fake());
        let adsp = |ty, src_port, dst_port, payload: &[u8]| {
            let h = Header {
                ty,
                src_node: 5,
                src_port,
                dst_node: NODE,
                dst_port,
                ..Default::default()
            };
            qrtr::encode(&h, payload)
        };
        m.handle(
            &mut f,
            &adsp(
                qrtr::TYPE_HELLO,
                PORT_CTRL,
                PORT_CTRL,
                &qrtr::ctrl(qrtr::TYPE_HELLO, [0; 4]),
            ),
        );
        assert!(f.sent.len() >= 4 && f.nodes.iter().all(|&n| n == 5));
        let (h, _) = qrtr::decode(&f.sent[0]).unwrap();
        assert_eq!((h.ty, h.dst_node), (qrtr::TYPE_HELLO, 5));
        let before = f.sent.len();
        m.handle(
            &mut f,
            &adsp(
                qrtr::TYPE_HELLO,
                PORT_CTRL,
                PORT_CTRL,
                &qrtr::ctrl(qrtr::TYPE_HELLO, [0; 4]),
            ),
        );
        assert_eq!(f.sent.len(), before); // one HELLO per node
        let q = qmi::Builder::new(qmi::REQUEST, 3, 0x21)
            .tlv(1, b"avs/audio")
            .finish();
        m.handle(&mut f, &adsp(qrtr::TYPE_DATA, 0x4001, LOCATOR_PORT, &q));
        assert_eq!(*f.nodes.last().unwrap(), 5);
        assert_eq!(reply(&f).result(), Some(Ok(())));
    }

    // C6: once both SSCTL servers are known, the modem hears "adsp up" and the ADSP "modem up" (Linux sysmon)
    #[test]
    fn subsystems_hear_each_other_is_up() {
        let (mut m, mut f) = (modem(), fake());
        let ssctl = ssctl_ns;
        m.handle(&mut f, &ssctl(0, ssctl::MODEM, 4));
        assert!(f.sent.is_empty()); // the ADSP isn't up yet
        m.handle(&mut f, &ssctl(5, ssctl::ADSP, 5));
        assert_eq!(f.sent.len(), 2);
        for (pkt, (node, port, name)) in f
            .sent
            .iter()
            .zip([(0, 4, &b"\x04adsp"[..]), (5, 5, &b"\x05modem"[..])])
        {
            let (h, p) = qrtr::decode(pkt).unwrap();
            assert_eq!(
                (h.dst_node, h.dst_port, h.src_port),
                (node, port, SSCTL_PORT)
            );
            let q = qmi::Msg::parse(p).unwrap();
            assert_eq!((q.ty, q.id), (qmi::REQUEST, ssctl::SUBSYS_EVENT_REQ));
            assert_eq!(q.tlv(0x01), Some(name));
            assert_eq!((q.u32(0x02), q.u32(0x10)), (Some(1), Some(0)));
        }
        assert_eq!(f.nodes, [0, 5]);
        // the answer is logged
        let txn = qmi::Msg::parse(qrtr::decode(&f.sent[1]).unwrap().1)
            .unwrap()
            .txn;
        let h = Header {
            ty: qrtr::TYPE_DATA,
            src_node: 5,
            src_port: 5,
            dst_node: NODE,
            dst_port: SSCTL_PORT,
            ..Default::default()
        };
        let r = ok(txn, ssctl::SUBSYS_EVENT_REQ).finish();
        m.handle(&mut f, &qrtr::encode(&h, &r));
        assert!(
            f.logs
                .iter()
                .any(|l| l.contains("adsp took \"modem up\" -> Ok"))
        );
    }

    // the C1 sequence (rmtfs.log 2026-11-08): open fs1, alloc 2 MB, read sector 1 x 1 to +0x200
    #[test]
    fn rmtfs_open_alloc_and_read() {
        let (mut m, mut f) = (modem(), fake());
        let open = qmi::Builder::new(qmi::REQUEST, 1, 1)
            .tlv(1, b"/boot/modem_fs1")
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 9, RMTFS_PORT, &open));
        assert_eq!(reply(&f).u32(0x10), Some(0));
        let alloc = qmi::Builder::new(qmi::REQUEST, 2, 4)
            .u32(1, 0)
            .u32(2, 0x20_0000)
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 17, RMTFS_PORT, &alloc));
        assert_eq!(reply(&f).tlv(0x10), Some(&0xfc20_1000u64.to_le_bytes()[..]));
        let mut iov = alloc::vec![1u8];
        for w in [1u32, 0xfc20_1200, 1] {
            iov.extend_from_slice(&w.to_le_bytes());
        }
        let read = qmi::Builder::new(qmi::REQUEST, 3, 3)
            .u32(1, 0)
            .u8(2, 0)
            .tlv(3, &iov)
            .u8(4, 0)
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 18, RMTFS_PORT, &read));
        assert_eq!(reply(&f).result(), Some(Ok(())));
        assert!(f.mem[0x200..0x400].iter().all(|&b| b == 1));
        assert_eq!(m.rmtfs.as_ref().unwrap().reads, 1);
    }

    #[test]
    fn rmtfs_unknown_partition_is_refused() {
        let (mut m, mut f) = (modem(), fake());
        let open = qmi::Builder::new(qmi::REQUEST, 1, 1)
            .tlv(1, b"/boot/modem_fsg_oem_1")
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 14, RMTFS_PORT, &open));
        assert!(matches!(reply(&f).result(), Some(Err(_))));
    }

    #[test]
    fn rmtfs_write_lands_in_ram_only() {
        let (mut m, mut f) = (modem(), fake());
        f.mem[0..512].fill(9);
        let mut iov = alloc::vec![1u8];
        for w in [3u32, 0xfc20_1000, 1] {
            iov.extend_from_slice(&w.to_le_bytes());
        }
        let w = qmi::Builder::new(qmi::REQUEST, 4, 3)
            .u32(1, 1)
            .u8(2, 1)
            .tlv(3, &iov)
            .u8(4, 0)
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 20, RMTFS_PORT, &w));
        assert!(
            m.rmtfs.as_ref().unwrap().images[1][3 * 512..4 * 512]
                .iter()
                .all(|&b| b == 9)
        );
        assert_eq!(m.rmtfs.as_ref().unwrap().writes, 1);
    }

    #[test]
    fn confirmed_control_packets_get_resume_tx_too() {
        let (mut m, mut f) = (modem(), fake());
        let mut pkt = ns(15, 22);
        pkt[16] = 1;
        m.handle(&mut f, &pkt);
        let (h, p) = qrtr::decode(&f.sent[0]).unwrap();
        assert_eq!((h.ty, h.dst_port), (qrtr::TYPE_RESUME_TX, PORT_CTRL));
        assert_eq!(qrtr::ctrl_words(p), Some((7, [NODE, PORT_CTRL, 0, 0])));
        assert_eq!(m.servers.len(), 1);
    }

    #[test]
    fn confirm_rx_gets_resume_tx() {
        let (mut m, mut f) = (modem(), fake());
        let q = qmi::Builder::new(qmi::REQUEST, 1, 0x24).finish();
        let mut pkt = from_modem(qrtr::TYPE_DATA, 30, LOCATOR_PORT, &q);
        pkt[16] = 1;
        m.handle(&mut f, &pkt);
        assert_eq!(f.sent.len(), 2); // RESUME_TX, then the locator's reply
        let (h, p) = qrtr::decode(&f.sent[0]).unwrap();
        assert_eq!((h.ty, h.dst_port), (qrtr::TYPE_RESUME_TX, 30));
        assert_eq!(qrtr::ctrl_words(p), Some((7, [NODE, LOCATOR_PORT, 0, 0])));
    }

    // 2026-11-09 review F07: a refused RESUME_TX goes out on a later tick, once (else the sender's flow stalls)
    #[test]
    fn a_refused_resume_tx_is_sent_again() {
        let (mut m, mut f) = (modem(), fake());
        let mut pkt = ns(15, 22);
        pkt[16] = 1;
        f.refuse = 1;
        m.handle(&mut f, &pkt);
        assert!(f.sent.is_empty());
        m.tick(&mut f, 500); // retried at most every second
        assert!(f.sent.is_empty());
        m.tick(&mut f, 1000);
        m.tick(&mut f, 2000);
        let resumes = f
            .sent
            .iter()
            .filter(|p| qrtr::decode(p).unwrap().0.ty == qrtr::TYPE_RESUME_TX)
            .count();
        assert_eq!(resumes, 1);
    }

    // F07: refused SSCTL events go out on tick, in order, once each, and the log doesn't claim them before
    #[test]
    fn refused_ssctl_events_go_out_on_tick_in_order() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ssctl_ns(0, ssctl::MODEM, 4));
        f.refuse = 1;
        m.handle(&mut f, &ssctl_ns(5, ssctl::ADSP, 5));
        assert!(f.sent.is_empty()); // the second waits behind the refused first
        assert!(f.logs.iter().any(|l| l.contains("ssctl: queued for")));
        assert!(!f.logs.iter().any(|l| l.contains("ssctl: told")));
        m.tick(&mut f, 1000);
        m.tick(&mut f, 2000);
        assert_eq!(f.nodes, [0, 5]);
    }

    // F07: the VOICE subscription is one-shot at the service's arrival: refused, it must still go (no calls without it)
    #[test]
    fn a_refused_voice_subscription_is_sent_again() {
        let (mut m, mut f) = (modem(), fake());
        f.refuse = 1;
        m.handle(&mut f, &ns(VOICE, 50));
        assert!(f.sent.is_empty());
        m.tick(&mut f, 1000);
        assert_eq!(f.sent.len(), 1);
        assert_eq!(req(&f.sent[0]).2, voice::INDICATION_REGISTER);
    }

    fn ssctl_ns(node: u32, instance: u32, port: u32) -> Vec<u8> {
        let h = Header {
            ty: qrtr::TYPE_NEW_SERVER,
            src_node: node,
            src_port: PORT_CTRL,
            dst_node: NODE,
            dst_port: PORT_CTRL,
            ..Default::default()
        };
        let w = [
            ssctl::SERVICE,
            qrtr::instance_word(ssctl::VERSION, instance),
            node,
            port,
        ];
        qrtr::encode(&h, &qrtr::ctrl(qrtr::TYPE_NEW_SERVER, w))
    }

    fn ns(service: u32, port: u32) -> Vec<u8> {
        from_modem(
            qrtr::TYPE_NEW_SERVER,
            PORT_CTRL,
            PORT_CTRL,
            &qrtr::ctrl(4, [service, 1, 0, port]),
        )
    }

    /// (our port, its port, message id, txn) of a packet we sent
    fn req(pkt: &[u8]) -> (u32, u32, u16, u16) {
        let (h, p) = qrtr::decode(pkt).unwrap();
        let m = qmi::Msg::parse(p).unwrap();
        (h.src_port, h.dst_port, m.id, m.txn)
    }

    fn ok(txn: u16, id: u16) -> qmi::Builder {
        qmi::Builder::new(qmi::RESPONSE, txn, id).result(0)
    }

    #[test]
    fn dms_and_uim_answers_then_online() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(DMS, 68));
        assert_eq!(f.sent.len(), 2);
        assert_eq!(req(&f.sent[0]), (0x20, 68, DMS_GET_REVISION, 1));
        assert_eq!(req(&f.sent[1]), (0x20, 68, DMS_GET_IDS, 2));
        let rev = ok(1, DMS_GET_REVISION).tlv(1, b"MPSS.AT.4.0.2").finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 68, 0x20, &rev));
        assert_eq!(m.revision.as_deref(), Some("MPSS.AT.4.0.2"));
        let ids = ok(2, DMS_GET_IDS).tlv(0x11, b"351234567890123").finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 68, 0x20, &ids));
        assert!(f.logs.iter().any(|l| l == "dms: IMEI 35xxxxxxxxxxx23"));
        assert!(!f.logs.iter().any(|l| l.contains("351234567890123")));
        m.handle(&mut f, &ns(UIM, 55));
        assert_eq!(
            req(f.sent.last().unwrap()),
            (0x21, 55, UIM_GET_CARD_STATUS, 2)
        );
        // a response on a transaction we never sent is not ours
        let stray = ok(9, UIM_GET_CARD_STATUS)
            .tlv(0x10, &[0, 0, 0, 0, 0, 0, 0, 0, 1, 1])
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 55, 0x21, &stray));
        assert_eq!(m.card, None);
        let card = ok(2, UIM_GET_CARD_STATUS)
            .tlv(0x10, &[0, 0, 0, 0, 0, 0, 0, 0, 1, 1])
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 55, 0x21, &card));
        assert_eq!((m.card, m.status.sim), (Some(1), modem_ipc::sim::PRESENT));
        let online = f.sent.last().unwrap().clone();
        assert_eq!(req(&online), (0x20, 68, DMS_SET_OPERATING_MODE, 3));
        let (_, pay) = qrtr::decode(&online).unwrap();
        assert_eq!(qmi::Msg::parse(pay).unwrap().tlv(1), Some(&[0u8][..]));
        m.handle(
            &mut f,
            &from_modem(
                qrtr::TYPE_DATA,
                68,
                0x20,
                &ok(3, DMS_SET_OPERATING_MODE).finish(),
            ),
        );
        assert_eq!(m.status.modem, modem_ipc::modem::ONLINE);
    }

    #[test]
    fn nas_registration_name_and_bars() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(NAS, 51));
        let ids: Vec<_> = f.sent.iter().map(|s| req(s).2).collect();
        assert_eq!(
            ids,
            [
                nas::REGISTER_INDICATIONS,
                nas::GET_SERVING_SYSTEM,
                nas::GET_SIGNAL_INFO
            ]
        );
        let mut plmn = 283u16.to_le_bytes().to_vec();
        plmn.extend_from_slice(&10u16.to_le_bytes());
        plmn.push(0); // no description: ask for the name
        let ind = qmi::Builder::new(qmi::INDICATION, 1, nas::GET_SERVING_SYSTEM)
            .tlv(1, &[1, 1, 1, 2, 1, 8])
            .tlv(0x12, &plmn)
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 51, 0x22, &ind));
        assert_eq!(
            (m.status.reg, m.status.rat),
            (modem_ipc::reg::HOME, modem_ipc::rat::LTE)
        );
        // registered: voice-only (once), the network time (once), then the name
        let n = f.sent.len();
        let sel = &f.sent[n - 4];
        assert_eq!(req(sel).2, nas::SET_SYSTEM_SELECTION);
        let (_, pay) = qrtr::decode(sel).unwrap();
        let sel = qmi::Msg::parse(pay).unwrap();
        assert_eq!(
            (sel.tlv(0x11), sel.tlv(0x18), sel.tlv(0x17)),
            (Some(&[0x0c, 0][..]), Some(&[0u8; 4][..]), Some(&[0u8][..]))
        );
        assert_eq!(req(&f.sent[n - 3]).2, nas::GET_SYSTEM_SELECTION);
        assert_eq!(req(&f.sent[n - 2]).2, nas::GET_NETWORK_TIME);
        let (_, _, id, txn) = req(f.sent.last().unwrap());
        assert_eq!(id, nas::GET_PLMN_NAME);
        let mut t = alloc::vec![0, 0, 0, 0, 0, 4];
        t.extend_from_slice(b"Ucom");
        t.extend_from_slice(&[0, 0, 0, 7]);
        t.extend_from_slice(b"Ucom AM");
        let name = ok(txn, nas::GET_PLMN_NAME).tlv(0x10, &t).finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 51, 0x22, &name));
        assert_eq!(m.status.operator.as_str(), "Ucom AM");
        let sig = qmi::Builder::new(qmi::INDICATION, 2, nas::SIGNAL_INFO_IND)
            .tlv(0x14, &[0xca, 0xee, 0xa3, 0xff, 0x24, 0x00])
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 51, 0x22, &sig));
        assert_eq!(m.status.bars, 3);
        let n = f.sent.len();
        m.tick(&mut f, 10_000);
        assert_eq!(f.sent.len(), n);
        m.tick(&mut f, 40_000);
        assert_eq!(f.sent.len(), n + 2);
    }

    #[test]
    fn ipa_handshake_through_the_router() {
        let (mut m, mut f) = (modem(), fake());
        m.ipa_up = true;
        m.start(&mut f);
        assert_eq!(f.sent.len(), 5); // HELLO + rmtfs, locator, TFTP and IPA servers
        let n = f.sent.len();
        m.handle(
            &mut f,
            &from_modem(
                qrtr::TYPE_NEW_SERVER,
                PORT_CTRL,
                PORT_CTRL,
                &qrtr::ctrl(4, [ipa::SERVICE, qrtr::instance_word(1, 2), 0, 30]),
            ),
        );
        assert_eq!(f.sent.len(), n + 1);
        let (_, dst, id, txn) = req(f.sent.last().unwrap());
        assert_eq!((dst, id), (30, ipa::INIT_DRIVER));
        m.handle(
            &mut f,
            &from_modem(
                qrtr::TYPE_DATA,
                30,
                0x24,
                &ok(txn, ipa::INIT_DRIVER).finish(),
            ),
        );
        for id in [ipa::DRIVER_INIT_COMPLETE, ipa::INDICATION_REGISTER] {
            let q = qmi::Builder::new(qmi::REQUEST, 3, id).u8(1, 0).finish();
            m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 41, IPA_PORT, &q));
        }
        let (h, pay) = qrtr::decode(f.sent.last().unwrap()).unwrap();
        let ind = qmi::Msg::parse(pay).unwrap();
        assert_eq!((h.src_port, h.dst_port), (IPA_PORT, 30)); // to the modem's server, not the requester
        assert_eq!((ind.ty, ind.id), (qmi::INDICATION, ipa::INIT_COMPLETE));
        assert!(f.logs.iter().any(|l| l == "ipa: handshake complete"));
    }

    fn sent_tlv(f: &Fake, t: u8) -> Vec<u8> {
        let (_, pay) = qrtr::decode(f.sent.last().unwrap()).unwrap();
        qmi::Msg::parse(pay).unwrap().tlv(t).unwrap().to_vec()
    }

    // C8: caller ID goes as a *31#/#31# prefix; a typed one wins; the shown number has none
    #[test]
    fn caller_id_prefix() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(VOICE, 50));
        m.clir = modem_ipc::clir::HIDE;
        m.dial(&mut f, "+37400000012").unwrap();
        assert_eq!(sent_tlv(&f, 1), b"#31#+37400000012");
        assert_eq!(m.status.call.number.as_str(), "+37400000012");
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(VOICE, 50));
        m.clir = modem_ipc::clir::HIDE;
        m.dial(&mut f, "*31#+37400000012").unwrap();
        assert_eq!(sent_tlv(&f, 1), b"*31#+37400000012");
        assert_eq!(m.status.call.number.as_str(), "+37400000012");
    }

    // C8: call waiting asks the network; the answer (or a refusal) lands in status.waiting
    #[test]
    fn call_waiting_query_and_set() {
        use modem_ipc::waiting;
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(VOICE, 50));
        m.call_waiting(&mut f, waiting::QUERY).unwrap();
        assert_eq!(m.status.waiting, waiting::BUSY);
        let (_, _, id, txn) = req(f.sent.last().unwrap());
        assert_eq!(
            (id, sent_tlv(&f, 0x10)),
            (voice::GET_CALL_WAITING, alloc::vec![1])
        );
        let reply = ok(txn, id).u8(0x10, 0x01).finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 50, 0x23, &reply));
        assert_eq!(m.status.waiting, waiting::ON);
        m.call_waiting(&mut f, waiting::DEACTIVATE).unwrap();
        let (_, _, id, txn) = req(f.sent.last().unwrap());
        assert_eq!(
            (id, sent_tlv(&f, 1)),
            (voice::SET_SUPS_SERVICE, alloc::vec![2, 0x0f])
        );
        m.handle(
            &mut f,
            &from_modem(qrtr::TYPE_DATA, 50, 0x23, &ok(txn, id).finish()),
        );
        assert_eq!(m.status.waiting, waiting::OFF);
        m.call_waiting(&mut f, waiting::ACTIVATE).unwrap();
        let (_, _, id, txn) = req(f.sent.last().unwrap());
        let refused = qmi::Builder::new(qmi::RESPONSE, txn, id).result(3).finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 50, 0x23, &refused));
        assert_eq!(m.status.waiting, waiting::ERROR);
        assert_eq!(m.call_waiting(&mut f, 9), Err(KERR_ARGUMENT));
    }

    #[test]
    fn calls_out_and_in() {
        let (mut m, mut f) = (modem(), fake());
        assert_eq!(m.dial(&mut f, "+37400000012"), Err(KERR_NOT_READY));
        m.handle(&mut f, &ns(VOICE, 50));
        assert_eq!(req(f.sent.last().unwrap()).2, voice::INDICATION_REGISTER);
        assert_eq!(m.dial(&mut f, "112"), Err(KERR_ARGUMENT));
        assert_eq!(m.dial(&mut f, "+37400000012"), Ok(()));
        assert!(f.logs.iter().all(|l| !l.contains("37400000012")));
        let (_, _, id, txn) = req(f.sent.last().unwrap());
        assert_eq!(id, voice::DIAL);
        assert_eq!(m.dial(&mut f, "+37400000099"), Err(KERR_IN_USE));
        // hang up before the modem numbered the call: sent once it has the id
        assert_eq!(m.hangup(&mut f), Ok(()));
        let n = f.sent.len();
        m.handle(
            &mut f,
            &from_modem(
                qrtr::TYPE_DATA,
                50,
                0x23,
                &ok(txn, voice::DIAL).u8(0x10, 1).finish(),
            ),
        );
        assert_eq!(f.sent.len(), n + 1);
        assert_eq!(req(f.sent.last().unwrap()).2, voice::END);
        let status = |calls: &[u8]| {
            qmi::Builder::new(qmi::INDICATION, 1, voice::ALL_CALL_STATUS_IND)
                .tlv(1, calls)
                .finish()
        };
        m.handle(
            &mut f,
            &from_modem(
                qrtr::TYPE_DATA,
                50,
                0x23,
                &status(&[1, 1, 9, 0, 1, 3, 0, 0]),
            ),
        );
        assert_eq!(
            (m.status.call.state, m.status.call.number.as_str()),
            (call::END, "+37400000012")
        );
        // incoming: answer refused during setup, accepted when incoming
        m.handle(
            &mut f,
            &from_modem(
                qrtr::TYPE_DATA,
                50,
                0x23,
                &status(&[1, 2, 0x0a, 0, 2, 3, 0, 0]),
            ),
        );
        assert_eq!(m.answer(&mut f), Err(KERR_NOT_READY));
        m.handle(
            &mut f,
            &from_modem(
                qrtr::TYPE_DATA,
                50,
                0x23,
                &status(&[1, 2, 2, 0, 2, 3, 0, 0]),
            ),
        );
        assert!(m.status.call.incoming);
        assert_eq!(m.answer(&mut f), Ok(()));
        let (_, pay) = qrtr::decode(f.sent.last().unwrap()).unwrap();
        let a = qmi::Msg::parse(pay).unwrap();
        assert_eq!((a.id, a.tlv(1)), (voice::ANSWER, Some(&[2u8][..])));
    }

    // C6 2026-11-09: a hangup that couldn't reach the modem left the call "dialling" forever and every redial got
    // -14. Now a hangup always ends the call: END is retried each second, and the watchdog ends it after 5 s.
    #[test]
    fn a_hangup_the_modem_never_gets_still_ends_the_call() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(VOICE, 50));
        assert_eq!(m.dial(&mut f, "+37400000012"), Ok(()));
        let (_, _, _, txn) = req(f.sent.last().unwrap());
        let dial_ok = ok(txn, voice::DIAL).u8(0x10, 1).finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 50, 0x23, &dial_ok));
        m.tick(&mut f, 1000);
        f.refuse = usize::MAX; // the modem takes nothing
        assert_eq!(m.hangup(&mut f), Ok(()));
        assert_eq!(m.status.call.state, call::DISCONNECTING);
        m.tick(&mut f, 3000);
        assert_eq!(m.status.call.state, call::DISCONNECTING);
        m.tick(&mut f, 8100); // 5 s after tick first saw it disconnecting
        assert_eq!(m.status.call.state, call::END);
        f.refuse = 0;
        assert_eq!(m.dial(&mut f, "+37400000012"), Ok(()));
    }

    // the END that couldn't go out goes out on a later tick
    #[test]
    fn a_refused_end_is_sent_again() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(VOICE, 50));
        m.dial(&mut f, "+37400000012").unwrap();
        let (_, _, _, txn) = req(f.sent.last().unwrap());
        let dial_ok = ok(txn, voice::DIAL).u8(0x10, 1).finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 50, 0x23, &dial_ok));
        f.refuse = 1;
        m.hangup(&mut f).unwrap();
        let n = f.sent.len();
        m.tick(&mut f, 1500);
        assert_eq!(f.sent.len(), n + 1);
        assert_eq!(req(f.sent.last().unwrap()).2, voice::END);
        m.tick(&mut f, 1600);
        assert_eq!(f.sent.len(), n + 1); // sent: no more retries
    }

    // a dial the modem never reports on ends after 30 s
    #[test]
    fn a_dial_that_goes_nowhere_ends() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(VOICE, 50));
        m.dial(&mut f, "+37400000012").unwrap();
        m.tick(&mut f, 100);
        m.tick(&mut f, 20_000);
        assert_eq!(m.status.call.state, call::ORIGINATION);
        m.tick(&mut f, 31_000);
        assert_eq!(m.status.call.state, call::END);
        assert!(f.logs.iter().any(|l| l.contains("ended here")));
    }

    // C6 2026-11-09: INIT_DRIVER refused during the modem's start-up burst was dropped, and LTE asserted
    #[test]
    fn init_driver_is_retried_until_it_goes_out() {
        let (mut m, mut f) = (modem(), fake());
        m.ipa_up = true;
        f.refuse = 1;
        m.handle(
            &mut f,
            &from_modem(
                qrtr::TYPE_NEW_SERVER,
                PORT_CTRL,
                PORT_CTRL,
                &qrtr::ctrl(4, [ipa::SERVICE, qrtr::instance_word(1, 2), 0, 30]),
            ),
        );
        assert!(f.sent.iter().all(|p| req(p).2 != ipa::INIT_DRIVER));
        m.tick(&mut f, 2000);
        assert_eq!(req(f.sent.last().unwrap()).2, ipa::INIT_DRIVER);
        let n = f.sent.len();
        m.tick(&mut f, 4000);
        assert!(f.sent[n..].iter().all(|p| req(p).2 != ipa::INIT_DRIVER)); // once
    }

    #[test]
    fn keypad_tones_only_in_a_connected_call() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(VOICE, 50));
        assert_eq!(m.dtmf(&mut f, Some(b'5')), Err(KERR_NOT_READY));
        let conversation = qmi::Builder::new(qmi::INDICATION, 1, voice::ALL_CALL_STATUS_IND)
            .tlv(1, &[1, 3, 3, 0, 2, 3, 0, 0])
            .finish();
        m.handle(
            &mut f,
            &from_modem(qrtr::TYPE_DATA, 50, 0x23, &conversation),
        );
        assert_eq!(m.status.call.state, call::CONVERSATION);
        assert_eq!(m.dtmf(&mut f, Some(b'x')), Err(KERR_ARGUMENT));
        assert_eq!(m.dtmf(&mut f, Some(b'#')), Ok(()));
        let (_, pay) = qrtr::decode(f.sent.last().unwrap()).unwrap();
        let q = qmi::Msg::parse(pay).unwrap();
        assert_eq!(
            (q.id, q.tlv(1)),
            (voice::START_CONT_DTMF, Some(&[3u8, b'#'][..]))
        );
        assert_eq!(m.dtmf(&mut f, None), Ok(()));
        let (_, pay) = qrtr::decode(f.sent.last().unwrap()).unwrap();
        let q = qmi::Msg::parse(pay).unwrap();
        assert_eq!((q.id, q.tlv(1)), (voice::STOP_CONT_DTMF, Some(&[3u8][..])));
    }

    #[test]
    fn tftp_write_then_read_refused() {
        let (mut m, mut f) = (modem(), fake());
        let mut wrq = 2u16.to_be_bytes().to_vec();
        wrq.extend_from_slice(b"/readwrite/server_check.txt\0octet\0");
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 32, TFTP_PORT, &wrq));
        let (h, p) = qrtr::decode(f.sent.last().unwrap()).unwrap();
        assert_eq!((h.src_port, p), (FIRST_TRANSFER_PORT, &[0u8, 4, 0, 0][..]));
        let mut d = 3u16.to_be_bytes().to_vec();
        d.extend_from_slice(&1u16.to_be_bytes());
        d.extend_from_slice(b"hi");
        m.handle(
            &mut f,
            &from_modem(qrtr::TYPE_DATA, 32, FIRST_TRANSFER_PORT, &d),
        );
        assert_eq!(
            m.files,
            alloc::vec![(String::from("/readwrite/server_check.txt"), b"hi".to_vec())]
        );
        let mut rrq = 1u16.to_be_bytes().to_vec();
        rrq.extend_from_slice(b"/readwrite/modem_efs\0octet\0");
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 33, TFTP_PORT, &rrq));
        let (_, p) = qrtr::decode(f.sent.last().unwrap()).unwrap();
        assert_eq!(&p[..4], &[0, 5, 0, 1]);
    }

    #[test]
    fn modem_lookup_of_our_rmtfs() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(
            &mut f,
            &from_modem(
                qrtr::TYPE_NEW_LOOKUP,
                40,
                PORT_CTRL,
                &qrtr::ctrl(10, [14, 0, 0, 0]),
            ),
        );
        assert_eq!(f.sent.len(), 2);
        let (h, p) = qrtr::decode(&f.sent[0]).unwrap();
        assert_eq!((h.dst_node, h.dst_port), (0, 40));
        assert_eq!(qrtr::ctrl_words(p), Some((4, [14, 1, NODE, RMTFS_PORT])));
        assert_eq!(
            qrtr::ctrl_words(qrtr::decode(&f.sent[1]).unwrap().1),
            Some((4, [0; 4]))
        );
    }
    fn call_ind(calls: &[u8]) -> Vec<u8> {
        from_modem(
            qrtr::TYPE_DATA,
            77,
            0x23,
            &qmi::Builder::new(qmi::INDICATION, 1, voice::ALL_CALL_STATUS_IND)
                .tlv(1, calls)
                .finish(),
        )
    }

    #[test]
    fn complete_call_list_survives_reorder_and_end_of_one_call() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(VOICE, 77));
        m.handle(
            &mut f,
            &call_ind(&[2, 1, 3, 0, 1, 3, 0, 0, 2, 7, 0, 2, 3, 0, 0]),
        );
        assert_eq!(m.status.calls.len, 2);
        assert_eq!(m.status.call.id, 2);
        m.handle(
            &mut f,
            &call_ind(&[2, 2, 7, 0, 2, 3, 0, 0, 1, 3, 0, 1, 3, 0, 0]),
        );
        assert_eq!(m.status.calls.find(1).unwrap().state, call::CONVERSATION);
        m.handle(
            &mut f,
            &call_ind(&[2, 2, 9, 0, 2, 3, 0, 0, 1, 3, 0, 1, 3, 0, 0]),
        );
        assert_eq!(m.status.call.id, 1);
        assert_eq!(m.status.missed, 1);
        m.handle(
            &mut f,
            &call_ind(&[2, 2, 9, 0, 2, 3, 0, 0, 1, 3, 0, 1, 3, 0, 0]),
        );
        assert_eq!(m.status.missed, 1);
    }

    // a ringing call rejected = busy (UDUB, Manage Calls type 1), END only if the modem refuses that; logged as rejected
    #[test]
    fn reject_is_busy_with_end_fallback() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(VOICE, 77));
        m.handle(&mut f, &call_ind(&[1, 1, 2, 0, 2, 3, 0, 0])); // call 1 incoming
        m.end_call(&mut f, 1).unwrap();
        let (_, _, id, txn) = req(f.sent.last().unwrap());
        assert_eq!((id, sent_tlv(&f, 1)), (voice::MANAGE_CALLS, alloc::vec![1]));
        let refused = qmi::Builder::new(qmi::RESPONSE, txn, id).result(3).finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 77, 0x23, &refused));
        assert_eq!(req(f.sent.last().unwrap()).2, voice::END);
        assert_eq!(sent_tlv(&f, 1), alloc::vec![1]);
        m.handle(&mut f, &call_ind(&[1, 1, 9, 0, 2, 3, 0, 0]));
        assert_eq!(m.log.entries[0].kind, modem_ipc::log::REJECTED);
        assert_eq!(m.status.missed, 0);
        // accepted: no END afterwards
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(VOICE, 77));
        m.handle(&mut f, &call_ind(&[1, 1, 2, 0, 2, 3, 0, 0]));
        m.end_call(&mut f, 1).unwrap();
        let (_, _, id, txn) = req(f.sent.last().unwrap());
        let n = f.sent.len();
        m.handle(
            &mut f,
            &from_modem(qrtr::TYPE_DATA, 77, 0x23, &ok(txn, id).finish()),
        );
        assert_eq!(f.sent.len(), n);
    }

    fn nas_reads(f: &Fake) -> usize {
        f.sent
            .iter()
            .filter(|packet| {
                let (h, payload) = qrtr::decode(packet).unwrap();
                h.src_port == 0x22
                    && qmi::Msg::parse(payload).is_some_and(|m| m.id == nas::GET_SERVING_SYSTEM)
            })
            .count()
    }

    #[test]
    fn final_end_observes_actual_lte_once_per_second_without_radio_changes() {
        let (mut m, mut f) = (modem(), fake());
        m.ipa_up = true;
        m.time_asked = true; // only the serving-system reads are counted below
        m.handle(&mut f, &ns(NAS, 51));
        m.handle(&mut f, &ns(VOICE, 77));
        m.tick(&mut f, 1000);
        m.handle(&mut f, &call_ind(&[1, 1, 3, 0, 1, 3, 0, 0]));
        m.tick(&mut f, 2000);
        m.handle(&mut f, &call_ind(&[1, 1, 9, 0, 1, 3, 0, 0]));
        f.sent.clear();
        m.tick(&mut f, 2000);
        m.tick(&mut f, 2999);
        assert_eq!(nas_reads(&f), 1);
        m.tick(&mut f, 3000);
        assert_eq!(nas_reads(&f), 2);
        let lte = qmi::Builder::new(qmi::INDICATION, 1, nas::GET_SERVING_SYSTEM)
            .tlv(1, &[1, 1, 1, 2, 1, 8])
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 51, 0x22, &lte));
        assert!(m.post_call.is_none());
        m.tick(&mut f, 4000);
        assert_eq!(nas_reads(&f), 2);
        assert!(
            f.sent
                .iter()
                .all(|packet| req(packet).2 == nas::GET_SERVING_SYSTEM)
        );
        assert_eq!(m.status.generation, 0);
        assert!(
            f.logs
                .iter()
                .any(|line| line.contains("LTE confirmed after 1000ms"))
        );
    }

    #[test]
    fn post_call_queries_are_bounded_and_end_of_one_call_does_not_arm_them() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(NAS, 51));
        m.handle(&mut f, &ns(VOICE, 77));
        m.tick(&mut f, 1000);
        m.handle(
            &mut f,
            &call_ind(&[2, 1, 3, 0, 1, 3, 0, 0, 2, 7, 0, 2, 3, 0, 0]),
        );
        m.handle(
            &mut f,
            &call_ind(&[2, 1, 9, 0, 1, 3, 0, 0, 2, 7, 0, 2, 3, 0, 0]),
        );
        assert!(m.post_call.is_none());
        m.tick(&mut f, 2000);
        m.handle(&mut f, &call_ind(&[0]));
        assert!(m.post_call.is_some());
        f.sent.clear();
        for now in (2000..=32_000).step_by(1000) {
            m.tick(&mut f, now);
        }
        assert_eq!(nas_reads(&f), 30);
        assert!(m.post_call.is_none());
        assert!(f.sent.iter().all(|packet| matches!(
            req(packet).2,
            nas::GET_SERVING_SYSTEM | nas::GET_SIGNAL_INFO
        )));
        m.handle(&mut f, &call_ind(&[1, 3, 3, 0, 1, 3, 0, 0]));
        m.handle(&mut f, &call_ind(&[0]));
        assert!(m.post_call.is_some());
        m.handle(&mut f, &call_ind(&[1, 4, 2, 0, 2, 3, 0, 0]));
        assert!(m.post_call.is_none());
    }

    #[test]
    fn routine_poll_does_not_insert_an_off_cadence_post_call_query() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(NAS, 51));
        m.handle(&mut f, &ns(VOICE, 77));
        m.tick(&mut f, 29_500);
        m.handle(&mut f, &call_ind(&[1, 1, 3, 0, 1, 3, 0, 0]));
        m.handle(&mut f, &call_ind(&[0]));
        f.sent.clear();
        m.tick(&mut f, 29_500);
        assert_eq!(nas_reads(&f), 1);
        m.tick(&mut f, 30_000); // the routine 30s poll is due halfway between post-call queries
        m.tick(&mut f, 30_499);
        assert_eq!(nas_reads(&f), 1);
        m.tick(&mut f, 30_500);
        assert_eq!(nas_reads(&f), 2);
    }

    #[test]
    fn airplane_cancels_post_call_observation() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(VOICE, 77));
        m.handle(&mut f, &call_ind(&[1, 1, 3, 0, 1, 3, 0, 0]));
        m.handle(&mut f, &call_ind(&[0]));
        assert!(m.post_call.is_some());
        let _ = m.airplane(&mut f, true);
        assert!(m.post_call.is_none());
    }

    #[test]
    fn manage_error_and_timeout_do_not_change_modem_reported_state() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(VOICE, 77));
        m.handle(&mut f, &call_ind(&[1, 1, 3, 0, 1, 3, 0, 0]));
        m.manage(&mut f, modem_ipc::manage::HOLD, 1).unwrap();
        let (_, _, id, txn) = req(f.sent.last().unwrap());
        assert_eq!(id, voice::MANAGE_CALLS);
        assert!(m.status.pending);
        assert_eq!(m.status.call.state, call::CONVERSATION);
        let failure = qmi::Builder::new(qmi::RESPONSE, txn, id).result(3).finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 77, 0x23, &failure));
        assert!(!m.status.pending);
        assert_eq!(m.status.result, -1003);
        assert_eq!(m.status.call.state, call::CONVERSATION);
        m.manage(&mut f, modem_ipc::manage::HOLD, 1).unwrap();
        m.tick(&mut f, 15_000);
        assert!(!m.status.pending);
        assert_eq!(m.status.result, -33);
    }

    #[test]
    fn reset_retains_ram_efs_intent_and_healthy_adsp_ssctl() {
        let (mut m, mut f) = (modem(), fake());
        m.rmtfs.as_mut().unwrap().images[0][0] = 99;
        m.status.airplane = true;
        m.status.ring.silent = true;
        m.handle(&mut f, &ssctl_ns(2, ssctl::ADSP, 90));
        m.reset(true);
        assert_eq!(m.rmtfs.as_ref().unwrap().images[0][0], 99);
        assert!(m.status.airplane && m.status.ring.silent);
        assert_eq!(m.status.generation, 1);
        f.sent.clear();
        m.handle(&mut f, &ssctl_ns(0, ssctl::MODEM, 91));
        assert_eq!(f.sent.len(), 2); // retained ADSP need not announce again
        assert_eq!(
            f.nodes.iter().rev().take(2).copied().collect::<Vec<_>>(),
            [2, 0]
        );
        m.reset(false);
        assert!(m.servers.is_empty());
        assert!(m.ssctl.iter().all(Option::is_none));
    }

    #[test]
    fn airplane_confirmation_never_claims_online_and_survives_sim_events() {
        let (mut m, mut f) = (modem(), fake());
        m.status.airplane = true;
        m.handle(&mut f, &ns(DMS, 68));
        let (_, _, id, txn) = req(f.sent.last().unwrap());
        assert_eq!(id, DMS_SET_OPERATING_MODE);
        let (_, payload) = qrtr::decode(f.sent.last().unwrap()).unwrap();
        assert_eq!(qmi::Msg::parse(payload).unwrap().tlv(1), Some(&[1][..]));
        m.handle(
            &mut f,
            &from_modem(qrtr::TYPE_DATA, 68, 0x20, &ok(txn, id).finish()),
        );
        assert!(!m.status.radio_online);
        assert!(!m.status.pending);
        m.handle(&mut f, &ns(UIM, 55));
        let c = qmi::Builder::new(qmi::INDICATION, 1, uim::STATUS_IND)
            .tlv(0x10, &[0, 0, 0, 0, 0, 0, 0, 0, 1, 1])
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 55, 0x21, &c));
        m.tick(&mut f, 5000);
        assert!(!m.status.radio_online && m.status.airplane);
        m.reset(true);
        assert!(m.status.airplane);
    }

    #[test]
    fn rejected_pin_updates_attempts_and_is_never_retried() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(UIM, 55));
        m.status.sim = modem_ipc::sim::PIN_REQUIRED;
        assert_eq!(m.verify_pin(&mut f, b"123"), Err(KERR_ARGUMENT));
        m.verify_pin(&mut f, b"1234").unwrap();
        let (_, _, id, txn) = req(f.sent.last().unwrap());
        assert_eq!(id, uim::VERIFY_PIN);
        let (_, payload) = qrtr::decode(f.sent.last().unwrap()).unwrap();
        assert_eq!(qmi::Msg::parse(payload).unwrap().tlv(1), Some(&[6, 0][..]));
        let rejected = qmi::Builder::new(qmi::RESPONSE, txn, id)
            .result(12)
            .tlv(0x10, &[0, 10])
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 55, 0x21, &rejected));
        assert_eq!(m.status.sim, modem_ipc::sim::BLOCKED);
        assert_eq!(m.status.pin_attempts, 0);
        let count = f
            .sent
            .iter()
            .filter(|p| req(p).2 == uim::VERIFY_PIN)
            .count();
        m.tick(&mut f, 60_000);
        assert_eq!(
            f.sent
                .iter()
                .filter(|p| qmi::Msg::parse(qrtr::decode(p).unwrap().1)
                    .is_some_and(|q| q.id == uim::VERIFY_PIN))
                .count(),
            count
        );
        assert_eq!(m.verify_pin(&mut f, b"1234"), Err(KERR_NOT_READY));
    }
    #[test]
    fn cancel_before_delayed_dial_response_sends_end_with_assigned_id() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(VOICE, 77));
        m.dial(&mut f, "+37400000001").unwrap();
        let (_, _, _, txn) = req(f.sent.last().unwrap());
        assert_eq!(m.status.call.id, 0);
        assert!(m.status.pending);
        assert_eq!(m.end_call(&mut f, 0), Ok(()));
        assert!(m.hangup_pending);
        let response = ok(txn, voice::DIAL).u8(0x10, 7).finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 77, 0x23, &response));
        let (_, _, id, _) = req(f.sent.last().unwrap());
        assert_eq!(id, voice::END);
        let (_, p) = qrtr::decode(f.sent.last().unwrap()).unwrap();
        assert_eq!(qmi::Msg::parse(p).unwrap().tlv(1), Some(&[7][..]));
        assert_eq!(m.status.call.state, call::DISCONNECTING);
    }

    #[test]
    fn watchdog_detects_three_lost_poll_cycles_but_not_airplane_idle() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(NAS, 70));
        m.tick(&mut f, 1);
        assert!(!m.watchdog());
        m.tick(&mut f, 90_002);
        assert!(m.watchdog());
        m.status.airplane = true;
        assert!(!m.watchdog());
    }
    #[test]
    fn cancel_after_dial_indication_before_response_sends_end() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(VOICE, 77));
        m.dial(&mut f, "+37400000001").unwrap();
        m.handle(&mut f, &call_ind(&[1, 7, 1, 0, 1, 3, 0, 0]));
        assert!(m.status.pending);
        assert_eq!(m.status.call.id, 7);
        assert_eq!(m.end_call(&mut f, 7), Ok(()));
        assert_eq!(req(f.sent.last().unwrap()).2, voice::END);
    }

    // ---- C7 integration fixes (2026-11-12)

    /// our port for `service`'s client, from the first packet it sent
    fn port_of(f: &Fake, their_port: u32) -> u32 {
        f.sent
            .iter()
            .map(|p| req(p))
            .find(|r| r.1 == their_port)
            .unwrap()
            .0
    }

    #[test]
    fn refused_online_is_asked_three_times_then_registration_means_online() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ns(DMS, 68));
        let dms = port_of(&f, 68);
        m.status.sim = modem_ipc::sim::PRESENT;
        let mut asked = 0;
        for i in 0..10u64 {
            f.sent.clear();
            m.tick(&mut f, 1 + i * 2500);
            let mode: Vec<_> = f
                .sent
                .iter()
                .map(|p| req(p))
                .filter(|r| r.2 == DMS_SET_OPERATING_MODE)
                .collect();
            for (_, _, id, txn) in mode {
                asked += 1;
                // "invalid transition": the modem is already online (C5)
                let refused = qmi::Builder::new(qmi::RESPONSE, txn, id).result(3).finish();
                m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 68, dms, &refused));
            }
        }
        assert_eq!(
            asked, RADIO_REFUSALS as usize,
            "bounded, not a resend every 2 s"
        );
        assert!(!m.status.pending);
        // NAS says registered: the radio is online whatever Set Operating Mode said
        m.handle(&mut f, &ns(NAS, 51));
        let nas_port = port_of(&f, 51);
        let home = qmi::Builder::new(qmi::INDICATION, 1, nas::GET_SERVING_SYSTEM)
            .tlv(1, &[1, 1, 1, 2, 1, 8])
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 51, nas_port, &home));
        assert!(m.status.radio_online);
        // a new intent may be asked for again
        f.sent.clear();
        m.airplane(&mut f, true).unwrap();
        assert!(f.sent.iter().any(|p| req(p).2 == DMS_SET_OPERATING_MODE));
    }

    #[test]
    fn airplane_intent_is_always_recorded() {
        // no SIM and no DMS client yet: the intent still holds (the caller saves it), the radio follows later
        let (mut m, mut f) = (modem(), fake());
        assert_eq!(m.airplane(&mut f, true), Ok(()));
        assert!(m.status.airplane);
        assert_eq!(m.airplane(&mut f, false), Ok(()));
        assert!(!m.status.airplane);
    }

    #[test]
    fn pin_attempts_unknown_until_uim_reports() {
        let mut m = modem();
        assert_eq!(m.status.pin_attempts, 255);
        m.status.pin_attempts = 2;
        m.reset(true);
        assert_eq!(m.status.pin_attempts, 255);
        assert_eq!(Modem::support_only().status.pin_attempts, 255);
    }

    #[test]
    fn lost_modem_keeps_adsp_traffic_and_reset_clears_the_failure() {
        let (mut m, mut f) = (modem(), fake());
        m.handle(&mut f, &ssctl_ns(5, ssctl::ADSP, 90));
        m.handle(&mut f, &ns(DMS, 68));
        m.hello_seen = alloc::vec![0, 5];
        m.outbox.push((qrtr::TYPE_DATA, 1, (5, 7), alloc::vec![1]));
        m.outbox
            .push((qrtr::TYPE_DATA, 1, (MODEM_NODE, 7), alloc::vec![2]));
        m.wlfw = Some(((MODEM_NODE, 83), wlfw::Client::new(WLAN_MSA.0, WLAN_MSA.1)));
        m.lost();
        assert_eq!(m.status.modem, modem_ipc::modem::FAILED);
        assert!(m.radio_failed);
        assert!(m.servers.iter().all(|s| s.node == 5) && !m.servers.is_empty());
        assert_eq!(m.outbox.len(), 1);
        assert_eq!(m.outbox[0].2.0, 5);
        assert_eq!(m.hello_seen, [5]);
        assert!(m.wlfw.is_none(), "the Wi-Fi client goes with the modem");
        assert!(m.rmtfs.is_some(), "RAM EFS survives");
        // a packet from the dead modem is ignored until a recovery resets
        m.handle(&mut f, &ns(VOICE, 77));
        assert!(!m.servers.iter().any(|s| s.node == MODEM_NODE));
        m.reset(true);
        assert!(!m.radio_failed);
        m.handle(&mut f, &ns(VOICE, 77));
        assert!(m.servers.iter().any(|s| s.node == MODEM_NODE));
    }
}
