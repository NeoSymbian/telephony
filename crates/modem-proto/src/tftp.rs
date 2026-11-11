// SPDX-License-Identifier: Apache-2.0
//! TFTP over QRTR (service 4096), what the modem uses for its config files. Each transfer runs on a fresh local
//! port. Writes are kept in RAM (C1: /readwrite/server_check.txt). Reads are served only from the modem's firmware
//! partition, read-only: `/readonly/firmware/image/X` = modem_a `IMAGE/X`, the folder Linux tqftpserv maps to the
//! remoteproc firmware. The modem's WLAN domain loads `wlanmdsp.mbn` that way, with `rsize`/`seek` reads in
//! `wsize`-block windows (Linux kprobe trace, docs wifi.md W0). Every other read is refused, as under Linux in C1.
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

/// A read in progress: a firmware file, served the way tqftpserv's `handle_reader` does.
pub struct Reader {
    pub port: u32,
    pub peer: (u32, u32),
    /// the FAT path on modem_a ("IMAGE/WLANMDSP.MBN")
    pub path: String,
    pub blksize: usize,
    pub wsize: usize,
    /// bytes asked for (0 = not given: tqftpserv then sends one block)
    pub rsize: usize,
    pub seek: usize,
}

/// An RRQ for a file we serve: where it lives and the options the modem asked for.
pub struct ReadRequest {
    pub path: String,
    pub blksize: usize,
    pub wsize: usize,
    pub rsize: usize,
    /// `None` without a seek option
    pub seek: Option<usize>,
    pub timeoutms: Option<u32>,
    pub tsize: bool,
    /// any option at all: answer with an OACK, else with block 1
    pub options: bool,
}

/// What a packet asks the router to do.
pub enum Action {
    /// reply from the service's own port
    Reply(Vec<u8>),
    /// open a transfer port and send this from it
    Start(Transfer, Vec<u8>),
    /// a read of a firmware file: the router looks the file up and calls `start_read`
    Read(ReadRequest),
    Ignore,
}

/// Reads `buf.len()` bytes of firmware file `path` at an offset: how many it got (fewer at the end).
pub type ReadFn<'a> = dyn FnMut(&str, usize, &mut [u8]) -> Result<usize, i32> + 'a;

const FW_PREFIX: &str = "/readonly/firmware/image/";

/// `/readonly/firmware/image/name.ext` -> `IMAGE/NAME.EXT` (an 8.3 name, nothing else)
pub fn firmware_path(name: &str) -> Option<String> {
    let file = name.strip_prefix(FW_PREFIX)?;
    let (base, ext) = file.split_once('.').unwrap_or((file, ""));
    let ok = |s: &str, max: usize| {
        s.len() <= max
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    };
    if base.is_empty() || !ok(base, 8) || !ok(ext, 3) {
        return None;
    }
    Some(alloc::format!("IMAGE/{}", file.to_ascii_uppercase()))
}

fn number(v: &[u8]) -> Option<usize> {
    core::str::from_utf8(v).ok()?.parse().ok()
}

fn read_request(name: &str, s: &[&[u8]]) -> Option<ReadRequest> {
    if !s.get(1).is_some_and(|m| m.eq_ignore_ascii_case(b"octet")) {
        return None;
    }
    let mut r = ReadRequest {
        path: firmware_path(name)?,
        blksize: 512,
        wsize: 1,
        rsize: 0,
        seek: None,
        timeoutms: None,
        tsize: false,
        options: false,
    };
    let mut i = 2; // after name and mode: option name/value pairs
    while i + 1 < s.len() && !s[i].is_empty() {
        let (k, v) = (s[i], s[i + 1]);
        r.options = true;
        match k {
            b"blksize" => r.blksize = number(v).filter(|&n| (8..=65464).contains(&n))?,
            b"wsize" => r.wsize = number(v).filter(|&n| (1..=65535).contains(&n))?,
            b"rsize" => r.rsize = number(v).filter(|&n| n > 0)?,
            b"seek" => r.seek = Some(number(v)?),
            b"timeoutms" => r.timeoutms = Some(number(v)? as u32),
            b"tsize" => r.tsize = true,
            _ => {}
        }
        i += 2;
    }
    Some(r)
}

/// The first packet of a read of a `size`-byte file: an OACK (tqftpserv's order: blksize, timeoutms, tsize,
/// wsize, rsize, seek) or, without options, block 1.
pub fn start_read(
    q: ReadRequest,
    peer: (u32, u32),
    port: u32,
    size: usize,
    read: &mut ReadFn,
) -> (Reader, Vec<u8>) {
    let r = Reader {
        port,
        peer,
        path: q.path,
        blksize: q.blksize,
        wsize: q.wsize,
        rsize: q.rsize,
        seek: q.seek.unwrap_or(0),
    };
    if !q.options {
        let first = block(&r, 0, read).unwrap_or_else(|_| error(0, "Read error"));
        return (r, first);
    }
    let mut v = OACK.to_be_bytes().to_vec();
    let mut opt = |k: &str, n: usize| {
        v.extend_from_slice(k.as_bytes());
        v.push(0);
        v.extend_from_slice(alloc::format!("{n}").as_bytes());
        v.push(0);
    };
    opt("blksize", r.blksize);
    if let Some(t) = q.timeoutms {
        opt("timeoutms", t as usize);
    }
    if q.tsize {
        opt("tsize", size);
    }
    opt("wsize", r.wsize);
    if r.rsize != 0 {
        opt("rsize", r.rsize);
    }
    if let Some(s) = q.seek {
        opt("seek", s);
    }
    (r, v)
}

/// DATA block `index + 1`: up to blksize bytes at seek + index * blksize, cut to rsize's remainder in its last block
fn block(r: &Reader, index: usize, read: &mut ReadFn) -> Result<Vec<u8>, i32> {
    let mut v = DATA.to_be_bytes().to_vec();
    v.extend_from_slice(&((index + 1) as u16).to_be_bytes());
    v.resize(4 + r.blksize, 0);
    let n = read(&r.path, r.seek + index * r.blksize, &mut v[4..])?;
    let limit = if (index + 1) * r.blksize > r.rsize {
        r.rsize % r.blksize
    } else {
        0
    };
    if limit != 0 && limit > n {
        return Err(-25); // KErrEof: fewer bytes than the modem asked for (tqftpserv rejects this too)
    }
    v.truncate(4 + if limit != 0 { limit } else { n });
    Ok(v)
}

/// A packet on a read's port: the DATA blocks to send and whether the read is over. An ACK of block `last` asks
/// for the next window; the read ends once rsize is covered or on the modem's ERROR (8 = "End of Transfer" after a
/// size query).
pub fn reader(r: &Reader, b: &[u8], read: &mut ReadFn) -> (Vec<Vec<u8>>, bool) {
    let op = match b.get(..4) {
        Some(o) => u16::from_be_bytes([o[0], o[1]]),
        None => return (Vec::new(), false),
    };
    if op == ERROR {
        return (Vec::new(), true);
    }
    if op != ACK {
        return (alloc::vec![error(4, "Expected ACK opcode")], true);
    }
    let last = u16::from_be_bytes([b[2], b[3]]) as usize;
    if last * r.blksize > r.rsize {
        return (Vec::new(), true);
    }
    let mut out = Vec::new();
    for index in last..last + r.wsize {
        match block(r, index, read) {
            Ok(d) => out.push(d),
            Err(_) => {
                out.push(error(0, "Read error"));
                return (out, true);
            }
        }
        if (index + 1) * r.blksize > r.rsize {
            break;
        }
    }
    (out, false)
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
        RRQ => match read_request(&name, &s) {
            Some(q) => Action::Read(q),
            None => Action::Reply(error(1, "File not found")),
        },
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

    fn rrq(name: &str, opts: &[(&str, &str)]) -> Vec<u8> {
        let mut v = RRQ.to_be_bytes().to_vec();
        for s in [name, "octet"]
            .iter()
            .chain(opts.iter().flat_map(|(k, v)| [k, v]))
        {
            v.extend_from_slice(s.as_bytes());
            v.push(0);
        }
        v
    }

    /// a 20000-byte file whose byte i is i % 251
    fn file(_: &str, at: usize, buf: &mut [u8]) -> Result<usize, i32> {
        let n = buf.len().min(20000usize.saturating_sub(at));
        for (i, b) in buf[..n].iter_mut().enumerate() {
            *b = ((at + i) % 251) as u8;
        }
        Ok(n)
    }

    fn start(opts: &[(&str, &str)]) -> (Reader, Vec<u8>) {
        let Action::Read(q) = request(
            &rrq("/readonly/firmware/image/wlanmdsp.mbn", opts),
            (0, 65),
            0x100,
        ) else {
            panic!("not a read");
        };
        assert_eq!(q.path, "IMAGE/WLANMDSP.MBN");
        start_read(q, (0, 65), 0x100, 20000, &mut file)
    }

    #[test]
    fn size_query_like_the_modem() {
        // C1 trace: blksize 7680, timeoutms 200, tsize 0, wsize 10 -> OACK with the size; the modem then ends it
        let opts = [
            ("blksize", "7680"),
            ("timeoutms", "200"),
            ("tsize", "0"),
            ("wsize", "10"),
        ];
        let (r, oack) = start(&opts);
        assert_eq!(
            oack,
            b"\x00\x06blksize\x007680\x00timeoutms\x00200\x00tsize\x0020000\x00wsize\x0010\x00"
                .to_vec()
        );
        let mut end = ERROR.to_be_bytes().to_vec();
        end.extend_from_slice(&8u16.to_be_bytes());
        end.extend_from_slice(b"End of Transfer\0");
        assert_eq!(reader(&r, &end, &mut file), (Vec::new(), true));
    }

    #[test]
    fn rsize_seek_window() {
        // 10000 bytes from 3000 in 4096-byte blocks, 2 per window: 4096 + 4096, then 1808, then done
        let opts = [
            ("blksize", "4096"),
            ("wsize", "2"),
            ("rsize", "10000"),
            ("seek", "3000"),
        ];
        let (r, oack) = start(&opts);
        assert!(oack.ends_with(b"wsize\x002\x00rsize\x0010000\x00seek\x003000\x00"));
        let (w, done) = reader(&r, &ack(0), &mut file);
        assert!(!done);
        assert_eq!(w.len(), 2);
        assert_eq!(&w[0][..4], &[0, 3, 0, 1]);
        assert_eq!(w[0].len(), 4 + 4096);
        assert_eq!(w[0][4], (3000 % 251) as u8);
        assert_eq!(&w[1][..4], &[0, 3, 0, 2]);
        assert_eq!(w[1][4], ((3000 + 4096) % 251) as u8);
        let (w, done) = reader(&r, &ack(2), &mut file);
        assert!(!done);
        assert_eq!(w.len(), 1);
        assert_eq!(&w[0][..4], &[0, 3, 0, 3]);
        assert_eq!(w[0].len(), 4 + 10000 - 8192);
        assert_eq!(reader(&r, &ack(3), &mut file), (Vec::new(), true));
    }

    #[test]
    fn only_firmware_image_files() {
        for bad in [
            "/readonly/vendor/mbn/mcfg_hw/mbn_hw.dig",
            "/readonly/firmware/image/../x.mbn",
            "/readonly/firmware/image/sub/x.mbn",
            "/readonly/firmware/image/toolongname.mbn",
        ] {
            assert!(
                matches!(request(&rrq(bad, &[]), (0, 1), 0x100), Action::Reply(_)),
                "{bad}"
            );
        }
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
