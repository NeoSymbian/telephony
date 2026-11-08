// SPDX-License-Identifier: Apache-2.0
//! The apps side of the Pixel 3a XL's modem (docs/telephony.md C4): a QRTR router for node 1 with the services the
//! modem needs from it (rmtfs, the service locator, TFTP) and a small QMI client. Pure logic over a [`Platform`]
//! (the kernel's HAL on the phone, a fake in tests), so it is unit-tested on the host.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod locator;
pub mod qmi;
pub mod qrtr;
pub mod rmtfs;
pub mod tftp;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use qrtr::{Header, NODE_BCAST, PORT_CTRL};

/// What the router needs from the system.
pub trait Platform {
    /// One whole QRTR packet to the modem (GLINK IPCRTR).
    fn send(&mut self, packet: &[u8]) -> Result<(), i32>;
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
const CLIENT_PORT: u32 = 0x20;
const FIRST_TRANSFER_PORT: u32 = 0x100;
/// (service, version, instance, our port)
const SERVICES: [(u32, u32, u32, u32); 3] = [
    (rmtfs::SERVICE, rmtfs::VERSION, rmtfs::INSTANCE, RMTFS_PORT),
    (
        locator::SERVICE,
        locator::VERSION,
        locator::INSTANCE,
        LOCATOR_PORT,
    ),
    (tftp::SERVICE, tftp::VERSION, tftp::INSTANCE, TFTP_PORT),
];
const DMS: u32 = 2;
const UIM: u32 = 11;
const DMS_GET_REVISION: u16 = 0x23;
const DMS_GET_IDS: u16 = 0x25;
const UIM_GET_CARD_STATUS: u16 = 0x2f;

/// A server the modem announced: service, version|instance word, node, port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Server {
    pub service: u32,
    pub instance: u32,
    pub node: u32,
    pub port: u32,
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
    transfers: Vec<tftp::Transfer>,
    next_port: u32,
    txn: u16,
    hello_seen: bool,
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
            transfers: Vec::new(),
            next_port: FIRST_TRANSFER_PORT,
            txn: 0,
            hello_seen: false,
        }
    }

    fn send(
        &mut self,
        p: &mut dyn Platform,
        ty: u32,
        src_port: u32,
        dst: (u32, u32),
        payload: &[u8],
    ) {
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
        if let Err(e) = p.send(&qrtr::encode(&h, payload)) {
            p.log(format_args!(
                "qrtr: send type {ty} to {}:{} failed ({e})",
                dst.0, dst.1 as i32
            ));
        }
    }

    fn ctrl(&mut self, p: &mut dyn Platform, ty: u32, dst: (u32, u32), words: [u32; 4]) {
        self.send(p, ty, PORT_CTRL, dst, &qrtr::ctrl(ty, words));
    }

    fn announce(&mut self, p: &mut dyn Platform, dst: (u32, u32)) {
        for (service, version, instance, port) in SERVICES {
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
        self.ctrl(p, qrtr::TYPE_HELLO, (MODEM_NODE, PORT_CTRL), [0; 4]);
        self.announce(p, (MODEM_NODE, PORT_CTRL));
    }

    fn query(&mut self, p: &mut dyn Platform, server: Server, id: u16) {
        self.txn = self.txn.wrapping_add(1);
        let req = qmi::Builder::new(qmi::REQUEST, self.txn, id).finish();
        self.send(
            p,
            qrtr::TYPE_DATA,
            CLIENT_PORT,
            (server.node, server.port),
            &req,
        );
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
        match h.ty {
            qrtr::TYPE_DATA => {
                self.data(p, &h, payload);
                if h.confirm_rx != 0 {
                    self.send(
                        p,
                        qrtr::TYPE_RESUME_TX,
                        h.dst_port,
                        from,
                        &qrtr::ctrl(qrtr::TYPE_RESUME_TX, [NODE, h.dst_port, 0, 0]),
                    );
                }
            }
            qrtr::TYPE_HELLO if !self.hello_seen => {
                self.hello_seen = true;
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
                p.log(format_args!(
                    "qrtr: server {service} v{} i{} at {node}:{port}",
                    instance & 0xff,
                    instance >> 8
                ));
                if service == DMS {
                    self.query(p, s, DMS_GET_REVISION);
                    self.query(p, s, DMS_GET_IDS);
                } else if service == UIM {
                    self.query(p, s, UIM_GET_CARD_STATUS);
                }
            }
            qrtr::TYPE_NEW_LOOKUP => {
                // the remote looks up our services: matching NEW_SERVERs, then an empty one ends the list
                let Some((_, [service, instance, _, _])) = qrtr::ctrl_words(payload) else {
                    return;
                };
                for (s, v, i, port) in SERVICES {
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
            CLIENT_PORT => self.client(p, payload),
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

    fn client(&mut self, p: &mut dyn Platform, payload: &[u8]) {
        let Some(m) = qmi::Msg::parse(payload) else {
            return;
        };
        if m.ty != qmi::RESPONSE {
            return;
        }
        let text = |t: Option<&[u8]>| t.map(|b| String::from_utf8_lossy(b).into_owned());
        match (m.id, m.result()) {
            (DMS_GET_REVISION, Some(Ok(()))) => {
                self.revision = text(m.tlv(0x01));
                p.log(format_args!(
                    "dms: revision {}",
                    self.revision.as_deref().unwrap_or("?")
                ));
            }
            (DMS_GET_IDS, Some(Ok(()))) => {
                self.imei = text(m.tlv(0x11));
                p.log(format_args!(
                    "dms: IMEI {}",
                    self.imei.as_deref().map(mask).unwrap_or_default()
                ));
            }
            (UIM_GET_CARD_STATUS, Some(Ok(()))) => {
                // card status: 4 x u16 session indexes, a u8 card count, then per card its state first
                self.card = m.tlv(0x10).and_then(|t| t.get(9)).copied();
                let state = match self.card {
                    Some(0) => "absent",
                    Some(1) => "present",
                    Some(2) => "error",
                    _ => "?",
                };
                p.log(format_args!("uim: card {state}"));
            }
            (id, r) => p.log(format_args!("qmi: response {id:#x} {r:?}")),
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
        mem: Vec<u8>,
        logs: Vec<String>,
    }

    impl Platform for Fake {
        fn send(&mut self, packet: &[u8]) -> Result<(), i32> {
            self.sent.push(packet.to_vec());
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
    fn confirm_rx_gets_resume_tx() {
        let (mut m, mut f) = (modem(), fake());
        let q = qmi::Builder::new(qmi::REQUEST, 1, 0x24).finish();
        let mut pkt = from_modem(qrtr::TYPE_DATA, 30, LOCATOR_PORT, &q);
        pkt[16] = 1;
        m.handle(&mut f, &pkt);
        let (h, p) = qrtr::decode(f.sent.last().unwrap()).unwrap();
        assert_eq!((h.ty, h.dst_port), (qrtr::TYPE_RESUME_TX, 30));
        assert_eq!(qrtr::ctrl_words(p), Some((7, [NODE, LOCATOR_PORT, 0, 0])));
    }

    #[test]
    fn dms_and_uim_answers() {
        let (mut m, mut f) = (modem(), fake());
        let ns = |s: u32, port: u32| {
            from_modem(
                qrtr::TYPE_NEW_SERVER,
                PORT_CTRL,
                PORT_CTRL,
                &qrtr::ctrl(4, [s, 1, 0, port]),
            )
        };
        m.handle(&mut f, &ns(DMS, 68));
        assert_eq!(f.sent.len(), 2);
        let (h, p) = qrtr::decode(&f.sent[0]).unwrap();
        assert_eq!(
            (h.src_port, h.dst_port, qmi::Msg::parse(p).unwrap().id),
            (CLIENT_PORT, 68, DMS_GET_REVISION)
        );
        let rev = qmi::Builder::new(qmi::RESPONSE, 1, DMS_GET_REVISION)
            .result(0)
            .tlv(1, b"MPSS.AT.4.0.2")
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 68, CLIENT_PORT, &rev));
        assert_eq!(m.revision.as_deref(), Some("MPSS.AT.4.0.2"));
        let ids = qmi::Builder::new(qmi::RESPONSE, 2, DMS_GET_IDS)
            .result(0)
            .tlv(0x11, b"351234567890123")
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 68, CLIENT_PORT, &ids));
        assert!(f.logs.iter().any(|l| l == "dms: IMEI 35xxxxxxxxxxx23"));
        assert!(!f.logs.iter().any(|l| l.contains("351234567890123")));
        m.handle(&mut f, &ns(UIM, 55));
        let card = qmi::Builder::new(qmi::RESPONSE, 3, UIM_GET_CARD_STATUS)
            .result(0)
            .tlv(0x10, &[0, 0, 0, 0, 0, 0, 0, 0, 1, 1])
            .finish();
        m.handle(&mut f, &from_modem(qrtr::TYPE_DATA, 55, CLIENT_PORT, &card));
        assert_eq!(m.card, Some(1));
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
