// SPDX-License-Identifier: Apache-2.0
//! TFTP over QRTR (service 4096), what the modem uses for its config files. Each transfer runs on a fresh local
//! port. Reads are refused ("not found": under Linux in C1 every one the modem tried failed and calls still worked);
//! writes are kept in RAM (C1: /readwrite/server_check.txt).
//! Written from linux-msm tqftpserv (`tqftpserv.c`, BSD-3) and RFC 1350/2347/2348.

use alloc::string::String;
use alloc::vec::Vec;

pub const SERVICE: u32 = 4096;
pub const VERSION: u32 = 1;
pub const INSTANCE: u32 = 0;
const RRQ: u16 = 1;
const WRQ: u16 = 2;
const DATA: u16 = 3;
const ACK: u16 = 4;
const ERROR: u16 = 5;
const OACK: u16 = 6;

pub struct Transfer {
    pub port: u32,
    pub peer: (u32, u32),
    pub name: String,
    pub blksize: usize,
    pub next: u16,
    pub data: Vec<u8>,
}

/// What a packet asks the router to do.
pub enum Action {
    /// reply from the service's own port
    Reply(Vec<u8>),
    /// open a transfer port and send this from it
    Start(Transfer, Vec<u8>),
    Ignore,
}

pub fn error(code: u16, msg: &str) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&ERROR.to_be_bytes());
    v.extend_from_slice(&code.to_be_bytes());
    v.extend_from_slice(msg.as_bytes());
    v.push(0);
    v
}

fn ack(block: u16) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&ACK.to_be_bytes());
    v.extend_from_slice(&block.to_be_bytes());
    v
}

fn strings(b: &[u8]) -> Vec<&[u8]> {
    b.split(|&c| c == 0).collect()
}

/// A request on the service port. `port` is the local port a new transfer would get.
pub fn request(b: &[u8], from: (u32, u32), port: u32) -> Action {
    let op = match b.get(..2) {
        Some(o) => u16::from_be_bytes([o[0], o[1]]),
        None => return Action::Ignore,
    };
    let s = strings(&b[2..]);
    let name = String::from_utf8_lossy(s.first().copied().unwrap_or(&[])).into_owned();
    match op {
        RRQ => Action::Reply(error(1, "File not found")),
        WRQ => {
            let mut blksize = 512;
            let mut oack = Vec::new();
            oack.extend_from_slice(&OACK.to_be_bytes());
            let mut i = 2; // after name and mode: option name/value pairs
            while i + 1 < s.len() && !s[i].is_empty() {
                let (k, v) = (s[i], s[i + 1]);
                let value: &[u8] = match k {
                    b"blksize" => {
                        blksize = core::str::from_utf8(v)
                            .ok()
                            .and_then(|x| x.parse().ok())
                            .unwrap_or(512);
                        v
                    }
                    b"wsize" => b"1", // one block per ACK
                    b"tsize" | b"timeoutms" => v,
                    _ => {
                        i += 2;
                        continue; // options we don't know are not acknowledged
                    }
                };
                oack.extend_from_slice(k);
                oack.push(0);
                oack.extend_from_slice(value);
                oack.push(0);
                i += 2;
            }
            let first = if oack.len() > 2 { oack } else { ack(0) };
            Action::Start(
                Transfer {
                    port,
                    peer: from,
                    name,
                    blksize,
                    next: 1,
                    data: Vec::new(),
                },
                first,
            )
        }
        _ => Action::Reply(error(4, "Illegal TFTP operation")),
    }
}

/// A packet on a transfer port: the reply (if any) and whether the transfer is complete.
pub fn transfer(t: &mut Transfer, b: &[u8]) -> (Option<Vec<u8>>, bool) {
    let op = match b.get(..4) {
        Some(o) => u16::from_be_bytes([o[0], o[1]]),
        None => return (None, false),
    };
    let block = u16::from_be_bytes([b[2], b[3]]);
    match op {
        DATA if block == t.next => {
            let payload = &b[4..];
            t.data.extend_from_slice(payload);
            t.next = t.next.wrapping_add(1);
            (Some(ack(block)), payload.len() < t.blksize)
        }
        DATA => (Some(ack(t.next.wrapping_sub(1))), false), // a repeat: ack what we have
        ERROR => (None, true),
        _ => (None, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrq(name: &str, opts: &[(&str, &str)]) -> Vec<u8> {
        let mut v = WRQ.to_be_bytes().to_vec();
        for s in [name, "octet"]
            .iter()
            .copied()
            .chain(opts.iter().flat_map(|(k, v)| [*k, *v]))
        {
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        v
    }

    #[test]
    fn read_is_refused() {
        let mut r = RRQ.to_be_bytes().to_vec();
        r.extend_from_slice(b"/readonly/vendor/mbn/mcfg_hw/mbn_hw.dig\0octet\0");
        match request(&r, (0, 32), 0x5000) {
            Action::Reply(e) => assert_eq!(&e[..4], &[0, 5, 0, 1]),
            _ => panic!(),
        }
    }

    #[test]
    fn write_server_check_txt() {
        let req = wrq(
            "/readwrite/server_check.txt",
            &[("blksize", "7680"), ("wsize", "10"), ("unlink", "0")],
        );
        let Action::Start(mut t, first) = request(&req, (0, 32), 0x5000) else {
            panic!()
        };
        assert_eq!(&first[..2], &[0, 6]);
        assert!(first.windows(8).any(|w| w == b"wsize\x001\x00"));
        assert!(!first.windows(6).any(|w| w == b"unlink"));
        let mut d = DATA.to_be_bytes().to_vec();
        d.extend_from_slice(&1u16.to_be_bytes());
        d.extend_from_slice(b"ok");
        let (reply, done) = transfer(&mut t, &d);
        assert_eq!(reply.unwrap(), alloc::vec![0, 4, 0, 1]);
        assert!(done);
        assert_eq!(t.data, b"ok");
    }

    #[test]
    fn no_options_starts_with_ack_0() {
        let Action::Start(_, first) = request(&wrq("/readwrite/x", &[]), (0, 1), 0x5000) else {
            panic!()
        };
        assert_eq!(first, alloc::vec![0, 4, 0, 0]);
    }
}
