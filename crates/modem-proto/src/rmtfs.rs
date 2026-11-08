// SPDX-License-Identifier: Apache-2.0
//! rmtfs, QMI service 14: the modem's EFS partitions (modemst1, modemst2, fsg, fsc), served from RAM images through
//! the shared carve-out (`rmtfs-mem@fc201000`). The modem names a partition by path, asks for the buffer's address,
//! then reads/writes sectors between the partition and buffer offsets.
//! Written from linux-msm rmtfs (`qmi_rmtfs.qmi`, `rmtfs.c`, `storage.c`; BSD-3).
//! shortcut: writes change only the RAM image (gate G4 persists them, docs/telephony.md C12).

use crate::Platform;
use crate::qmi::{Builder, Msg, RESPONSE};

use alloc::vec::Vec;

pub const SERVICE: u32 = 14;
pub const VERSION: u32 = 1;
pub const INSTANCE: u32 = 0;
const OPEN: u16 = 1;
const CLOSE: u16 = 2;
const RW_IOVEC: u16 = 3;
const ALLOC_BUF: u16 = 4;
const GET_DEV_ERROR: u16 = 5;
const SECTOR: usize = 512;
const ERR_INTERNAL: u16 = 1;
const ERR_MALFORMED: u16 = 2;

/// The paths the modem opens, in the order of the images (C1: fs1, fs2, fsg, fsc; fsg_oem_1/2 are refused).
pub const PATHS: [&str; 4] = [
    "/boot/modem_fs1",
    "/boot/modem_fs2",
    "/boot/modem_fsg",
    "/boot/modem_fsc",
];

pub struct Rmtfs {
    pub images: [Vec<u8>; 4],
    pub phys: u64,
    pub reads: u32,
    pub writes: u32,
}

impl Rmtfs {
    pub fn new(images: [Vec<u8>; 4], phys: u64) -> Self {
        Rmtfs {
            images,
            phys,
            reads: 0,
            writes: 0,
        }
    }

    /// One request; returns the response.
    pub fn handle(&mut self, p: &mut dyn Platform, m: &Msg) -> Vec<u8> {
        let r = Builder::new(RESPONSE, m.txn, m.id);
        match m.id {
            OPEN => {
                let path = m.tlv(1).unwrap_or(&[]);
                let path = path.strip_suffix(&[0]).unwrap_or(path);
                match PATHS.iter().position(|q| q.as_bytes() == path) {
                    Some(i) => {
                        p.log(format_args!("rmtfs: open {} -> {}", PATHS[i], i));
                        r.result(0).u32(0x10, i as u32).finish()
                    }
                    None => {
                        let shown = core::str::from_utf8(path).unwrap_or("?");
                        p.log(format_args!("rmtfs: open {shown:?} refused"));
                        r.result(ERR_INTERNAL).finish()
                    }
                }
            }
            CLOSE | GET_DEV_ERROR => {
                if m.id == GET_DEV_ERROR {
                    r.result(0).u8(0x10, 0).finish()
                } else {
                    r.result(0).finish()
                }
            }
            ALLOC_BUF => {
                p.log(format_args!(
                    "rmtfs: alloc {} -> {:#x}",
                    m.u32(2).unwrap_or(0),
                    self.phys
                ));
                r.result(0).u64(0x10, self.phys).finish()
            }
            RW_IOVEC => {
                let err = self.iovec(p, m).err().unwrap_or(0);
                r.result(err).finish()
            }
            _ => r.result(ERR_MALFORMED).finish(),
        }
    }

    fn iovec(&mut self, p: &mut dyn Platform, m: &Msg) -> Result<(), u16> {
        let caller = m.u32(1).ok_or(ERR_MALFORMED)? as usize;
        let write = m.u8(2).ok_or(ERR_MALFORMED)? != 0;
        let list = m.tlv(3).ok_or(ERR_MALFORMED)?;
        let base = self.phys;
        let image = self.images.get_mut(caller).ok_or(ERR_INTERNAL)?;
        let n = *list.first().ok_or(ERR_MALFORMED)? as usize; // a u8 count, then {sector, offset, count} entries
        for e in list.get(1..1 + 12 * n).ok_or(ERR_MALFORMED)?.chunks(12) {
            let w = |i: usize| u32::from_le_bytes(e[i..i + 4].try_into().unwrap()) as usize;
            // "phys_offset" is the buffer's physical address (C1: 0xfc201200 = base + 0x200), not an offset
            let (start, addr, len) = (w(0) * SECTOR, w(4) as u64, w(8) * SECTOR);
            let offset = match addr.checked_sub(base) {
                Some(o) if o + len as u64 <= 0x20_0000 => o as u32,
                _ => {
                    p.log(format_args!(
                        "rmtfs: iovec address {addr:#x} outside the buffer"
                    ));
                    return Err(ERR_INTERNAL);
                }
            };
            if write {
                let mut buf = alloc::vec![0u8; len];
                if let Err(e) = p.rmtfs_read(offset, &mut buf) {
                    p.log(format_args!("rmtfs: buffer read +{offset:#x} failed ({e})"));
                    return Err(ERR_INTERNAL);
                }
                if start + len > image.len() {
                    image.resize(start + len, 0);
                }
                image[start..start + len].copy_from_slice(&buf);
                self.writes += 1;
            } else {
                let mut buf = alloc::vec![0u8; len]; // past the image: zeros, as Linux rmtfs pads
                if start < image.len() {
                    let end = image.len().min(start + len);
                    buf[..end - start].copy_from_slice(&image[start..end]);
                }
                if let Err(e) = p.rmtfs_write(offset, &buf) {
                    p.log(format_args!(
                        "rmtfs: buffer write +{offset:#x} failed ({e})"
                    ));
                    return Err(ERR_INTERNAL);
                }
                self.reads += 1;
            }
            p.log(format_args!(
                "rmtfs: {} {} sector {}+{} at +{:#x}",
                if write { "write" } else { "read" },
                PATHS[caller],
                w(0),
                w(8),
                offset
            ));
        }
        Ok(())
    }
}
