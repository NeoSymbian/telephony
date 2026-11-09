// SPDX-License-Identifier: Apache-2.0
//! The apps side of the Pixel 3a XL's modem (docs/telephony.md C4, C5): a QRTR router for node 1 with the services
//! the modem needs from it (rmtfs, the service locator, TFTP) and QMI clients for DMS, UIM, NAS and VOICE that keep
//! a [`modem_ipc::Status`] and place, answer and end calls. Pure logic over a [`Platform`] (the kernel's HAL on the
//! phone, a fake in tests), so it is unit-tested on the host.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod apr;
pub mod ipa;
pub mod locator;
pub mod nas;
pub mod qmi;
pub mod qrtr;
pub mod rmtfs;
pub mod ssctl;
pub mod tftp;
pub mod voice;

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
}

pub const NODE: u32 = 1;
const MODEM_NODE: u32 = 0;
const RMTFS_PORT: u32 = 0x10;
const LOCATOR_PORT: u32 = 0x11;
const TFTP_PORT: u32 = 0x12;
const IPA_PORT: u32 = 0x13;
/// our SSCTL client port (the "subsystem up" events, ssctl.rs)
const SSCTL_PORT: u32 = 0x25;
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
    pub rmtfs: rmtfs::Rmtfs,
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
    next_port: u32,
    clients: Vec<Client>,
    /// the nodes whose HELLO we answered (the modem, the ADSP)
    hello_seen: Vec<u32>,
    /// the SSCTL servers (node, port): the modem's, the ADSP's
    ssctl: [Option<(u32, u32)>; 2],
    ssctl_txn: u16,
    online_sent: bool,
    plmn_asked: Option<(u16, u16)>,
    hangup_pending: bool,
    last_poll: u64,
    /// NAS circuit-switched attach, for the log
    cs: u8,
    selection_sent: bool,
}

/// "350000000000012" -> "35xxxxxxxxxxx12": never log a whole IMEI.
pub fn mask(id: &str) -> String {
    id.char_indices()
        .map(|(i, c)| if i < 2 || i + 2 >= id.len() { c } else { 'x' })
        .collect()
}

impl Modem {
    pub fn new(images: [Vec<u8>; 4], rmtfs_phys: u64) -> Self {
        Modem {
            trace: false,
            rmtfs: rmtfs::Rmtfs::new(images, rmtfs_phys),
            servers: Vec::new(),
            files: Vec::new(),
            revision: None,
            imei: None,
            card: None,
            status: Status::default(),
            transfers: Vec::new(),
            next_port: FIRST_TRANSFER_PORT,
            clients: Vec::new(),
            hello_seen: Vec::new(),
            ssctl: [None; 2],
            ssctl_txn: 0,
            online_sent: false,
            plmn_asked: None,
            hangup_pending: false,
            last_poll: 0,
            cs: 0,
            selection_sent: false,
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

    fn ctrl(&mut self, p: &mut dyn Platform, ty: u32, dst: (u32, u32), words: [u32; 4]) {
        self.send(p, ty, PORT_CTRL, dst, &qrtr::ctrl(ty, words));
    }

    /// Our servers: IPA's only while the kernel runs IPA.
    fn services(&self) -> impl Iterator<Item = (u32, u32, u32, u32)> {
        let ipa_up = self.ipa_up;
        SERVICES.into_iter().filter(move |s| ipa_up || s.0 != IPA)
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
        self.send(p, qrtr::TYPE_DATA, port, dst, &b.finish())
    }

    /// The first requests once the modem announces a service we are a client of.
    fn client_up(&mut self, p: &mut dyn Platform, s: Server) {
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
                self.request(p, DMS, DMS_GET_REVISION, &[]);
                self.request(p, DMS, DMS_GET_IDS, &[]);
                self.maybe_online(p);
            }
            UIM => {
                self.request(p, UIM, UIM_GET_CARD_STATUS, &[]);
            }
            NAS => {
                self.request(p, NAS, nas::REGISTER_INDICATIONS, &nas::INDICATIONS);
                self.request(p, NAS, nas::GET_SERVING_SYSTEM, &[]);
                self.request(p, NAS, nas::GET_SIGNAL_INFO, &[]);
            }
            VOICE => {
                self.request(p, VOICE, voice::INDICATION_REGISTER, &voice::INDICATIONS);
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
        for (dst, name) in [(modem, "adsp"), (adsp, "modem")] {
            self.ssctl_txn = self.ssctl_txn.wrapping_add(1).max(1);
            let mut b = qmi::Builder::new(qmi::REQUEST, self.ssctl_txn, ssctl::SUBSYS_EVENT_REQ);
            for (t, v) in ssctl::event_tlvs(name) {
                b = b.tlv(t, &v);
            }
            self.send(p, qrtr::TYPE_DATA, SSCTL_PORT, dst, &b.finish());
        }
        p.log(format_args!(
            "ssctl: told the modem \"adsp up\" and the adsp \"modem up\""
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
        self.send(p, qrtr::TYPE_DATA, IPA_PORT, dst, &ind);
        p.log(format_args!("ipa: handshake complete"));
    }

    /// The radio on (DMS Set Operating Mode online) once there is a SIM: ModemManager does the same on enable.
    fn maybe_online(&mut self, p: &mut dyn Platform) {
        if !self.online_sent
            && self.status.sim == modem_ipc::sim::PRESENT
            && self.request(p, DMS, DMS_SET_OPERATING_MODE, &[(0x01, &[0])])
        {
            self.online_sent = true;
        }
    }

    /// The periodic work: NAS serving system and signal every 30 s. `now_ms` is any monotonic clock.
    pub fn tick(&mut self, p: &mut dyn Platform, now_ms: u64) {
        if self.ipa_init_due && now_ms.saturating_sub(self.ipa_init_at) >= 2000 {
            self.ipa_init_at = now_ms; // every 2 s until it goes out (a refused send takes up to 2 s)
            self.ipa_init_due = !self.init_driver(p);
        }
        if now_ms.saturating_sub(self.last_poll) < POLL_MS {
            return;
        }
        self.last_poll = now_ms;
        self.request(p, NAS, nas::GET_SERVING_SYSTEM, &[]);
        self.request(p, NAS, nas::GET_SIGNAL_INFO, &[]);
    }

    /// The modem died (SMP2P fatal): nothing it reported holds any more. The UI shows "No service".
    /// shortcut: no restart (SSR) yet; a reboot brings the modem back.
    pub fn lost(&mut self) {
        self.status = Status {
            modem: modem_ipc::modem::FAILED,
            sim: self.status.sim,
            ..Status::default()
        };
        self.clients.clear();
    }

    /// Place a call (modem_ipc::op::DIAL). KErrArgument for a number [`voice::dialable`] refuses, KErrInUse while a
    /// call is up, KErrNotReady before VOICE is there.
    pub fn dial(&mut self, p: &mut dyn Platform, number: &str) -> Result<(), i32> {
        if !voice::dialable(number) {
            return Err(KERR_ARGUMENT);
        }
        if !matches!(self.status.call.state, call::NONE | call::END) {
            return Err(KERR_IN_USE);
        }
        if !self.request(p, VOICE, voice::DIAL, &[(0x01, number.as_bytes())]) {
            return Err(KERR_NOT_READY);
        }
        self.hangup_pending = false;
        self.status.call = Call {
            id: 0,
            state: call::ORIGINATION,
            incoming: false,
            number: Text::new(number),
        };
        p.log(format_args!("voice: dial {}", mask(number)));
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

    /// End, reject or cancel the call (modem_ipc::op::HANGUP). A dial the modem hasn't numbered yet ends as soon as
    /// it has its id.
    pub fn hangup(&mut self, p: &mut dyn Platform) -> Result<(), i32> {
        let c = self.status.call;
        if matches!(c.state, call::NONE | call::END) {
            return Err(KERR_NOT_READY);
        }
        if c.id == 0 {
            self.hangup_pending = true;
            return Ok(());
        }
        self.request(p, VOICE, voice::END, &[(0x01, &[c.id])])
            .then_some(())
            .ok_or(KERR_NOT_READY)
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
            None => self.request(p, VOICE, voice::STOP_CONT_DTMF, &[(0x01, &[c.id])]),
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
        let from = (h.src_node, h.src_port);
        // flow control: a sender marks a packet confirm_rx and waits for RESUME_TX before it sends that port more.
        // Every type, control included: unconfirmed NEW_SERVERs stalled the modem's announcements, and DMS or UIM
        // never arrived (C5, 2026-11-09). Linux confirms whatever a socket receives.
        if h.confirm_rx != 0 && h.ty != qrtr::TYPE_RESUME_TX {
            self.send(
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
            }
            qrtr::TYPE_NEW_LOOKUP => {
                // the remote looks up our services: matching NEW_SERVERs, then an empty one ends the list
                let Some((_, [service, instance, _, _])) = qrtr::ctrl_words(payload) else {
                    return;
                };
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
                    self.send(p, qrtr::TYPE_DATA, IPA_PORT, from, &resp);
                }
                self.ipa_complete(p);
            }
            RMTFS_PORT | LOCATOR_PORT => {
                let Some(m) = qmi::Msg::parse(payload) else {
                    return;
                };
                let resp = if h.dst_port == RMTFS_PORT {
                    self.rmtfs.handle(p, &m)
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
                self.send(p, qrtr::TYPE_DATA, h.dst_port, from, &resp);
            }
            SSCTL_PORT => {
                let Some(m) = qmi::Msg::parse(payload) else {
                    return;
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
                    self.send(p, qrtr::TYPE_DATA, TFTP_PORT, from, &r);
                }
                tftp::Action::Start(t, first) => {
                    p.log(format_args!("tftp: write {:?}", t.name));
                    self.next_port += 1;
                    let port = t.port;
                    self.transfers.push(t);
                    self.send(p, qrtr::TYPE_DATA, port, from, &first);
                }
                tftp::Action::Ignore => {}
            },
            port if self.clients.iter().any(|c| c.port == port) => self.client(p, port, payload),
            port => {
                let Some(i) = self.transfers.iter().position(|t| t.port == port) else {
                    return;
                };
                let (reply, done) = tftp::transfer(&mut self.transfers[i], payload);
                if let Some(r) = reply {
                    self.send(p, qrtr::TYPE_DATA, port, from, &r);
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
                self.response(p, service, &m);
            }
            qmi::INDICATION => self.indication(p, service, &m),
            _ => {}
        }
    }

    fn response(&mut self, p: &mut dyn Platform, service: u32, m: &qmi::Msg) {
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
            (DMS, DMS_SET_OPERATING_MODE) => {
                self.status.modem = modem_ipc::modem::ONLINE;
                p.log(format_args!("dms: online"));
            }
            (UIM, UIM_GET_CARD_STATUS) => {
                // card status: 4 x u16 session indexes, a u8 card count, then per card its state first
                self.card = m.tlv(0x10).and_then(|t| t.get(9)).copied();
                let (sim, state) = match self.card {
                    Some(0) => (modem_ipc::sim::ABSENT, "absent"),
                    Some(1) => (modem_ipc::sim::PRESENT, "present"),
                    Some(2) => (modem_ipc::sim::ERROR, "error"),
                    _ => (modem_ipc::sim::UNKNOWN, "?"),
                };
                self.status.sim = sim;
                p.log(format_args!("uim: card {state}"));
                self.maybe_online(p);
            }
            (NAS, nas::GET_SERVING_SYSTEM) => self.serving(p, m),
            (NAS, nas::GET_SIGNAL_INFO) => self.signal(m),
            (NAS, nas::SET_SYSTEM_SELECTION) => p.log(format_args!("nas: voice-only mode set")),
            (NAS, nas::GET_SYSTEM_SELECTION) => p.log(format_args!(
                "nas: mode preference {:#x}",
                nas::mode_preference(m).unwrap_or(0)
            )),
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
            (VOICE, voice::ALL_CALL_STATUS_IND) => {
                let Some(c) = voice::all_call_status(m) else {
                    return;
                };
                let old = self.status.call;
                let number = match c.number {
                    Some(n) => Text::new(&n),
                    None if old.id == c.id || old.id == 0 => old.number,
                    None => Text::default(),
                };
                self.status.call = Call {
                    id: c.id,
                    state: c.state,
                    incoming: c.incoming,
                    number,
                };
                if (old.id, old.state) != (c.id, c.state) {
                    p.log(format_args!(
                        "voice: call {} state {} {} {}",
                        c.id,
                        c.state,
                        if c.incoming { "from" } else { "to" },
                        mask(number.as_str())
                    ));
                }
                if self.hangup_pending && c.id != 0 {
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
        if registered {
            // online, whether or not our Set Operating Mode was needed (C5: "invalid transition" when it already was)
            self.status.modem = modem_ipc::modem::ONLINE;
            // voice-only once registered: sent as soon as NAS appeared, the modem refused it (QMI error 25) and its
            // SIM service never came up (C5, 2026-11-09)
            if !self.ipa_up && !self.selection_sent {
                self.selection_sent = true;
                self.request(p, NAS, nas::SET_SYSTEM_SELECTION, &nas::VOICE_ONLY);
                self.request(p, NAS, nas::GET_SYSTEM_SELECTION, &[]);
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
            "{dir} data {}:{src} -> {}:{dst} {} B qmi type {} id {:#x} txn {}{confirm}",
            h.src_node,
            h.dst_node,
            payload.len(),
            m.ty,
            m.id,
            m.txn
        )),
        None => p.log(format_args!(
            "{dir} data {}:{src} -> {}:{dst} {} B{confirm}",
            h.src_node,
            h.dst_node,
            payload.len()
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
        let q = qmi::Builder::new(qmi::REQUEST, 3, 0x20)
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
        let ssctl = |node: u32, instance: u32, port: u32| {
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
        };
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
        assert_eq!(m.rmtfs.reads, 1);
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
        assert!(m.rmtfs.images[1][3 * 512..4 * 512].iter().all(|&b| b == 9));
        assert_eq!(m.rmtfs.writes, 1);
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
            (0x21, 55, UIM_GET_CARD_STATUS, 1)
        );
        // a response on a transaction we never sent is not ours
        let stray = ok(9, UIM_GET_CARD_STATUS)
            .tlv(0x10, &[0, 0, 0, 0, 0, 0, 0, 0, 1, 1])
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 55, 0x21, &stray));
        assert_eq!(m.card, None);
        let card = ok(1, UIM_GET_CARD_STATUS)
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
        // registered: voice-only (once), then the name
        let n = f.sent.len();
        let sel = &f.sent[n - 3];
        assert_eq!(req(sel).2, nas::SET_SYSTEM_SELECTION);
        let (_, pay) = qrtr::decode(sel).unwrap();
        let sel = qmi::Msg::parse(pay).unwrap();
        assert_eq!(
            (sel.tlv(0x11), sel.tlv(0x18), sel.tlv(0x17)),
            (Some(&[0x0c, 0][..]), Some(&[0u8; 4][..]), Some(&[0u8][..]))
        );
        assert_eq!(req(&f.sent[n - 2]).2, nas::GET_SYSTEM_SELECTION);
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
}
