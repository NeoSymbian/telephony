// SPDX-License-Identifier: Apache-2.0
//! MODEMD.EXE (docs/telephony.md C4): reads the modem's EFS partitions, boots the modem, and runs the apps side of
//! QRTR (modem_proto::Modem) over the kernel's telephony HAL functions (kernel qcom_telhal.cpp, 0x4e0d..0x4e12).
//! Started by hand for now (Terminal: `modemd`); auto-start at boot is decided in C5.
#![no_std]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use e32::{dprintln, time};
use modem_proto::{Modem, Platform};

const GROUP_VARIANT: i32 = 1;
const BOOT: i32 = 0x4e0d;
const STATE: i32 = 0x4e0e;
const SEND: i32 = 0x4e0f;
const RECV: i32 = 0x4e10;
const PART_READ: i32 = 0x4e11;
const RMTFS_MEM: i32 = 0x4e12;
const NOT_READY: i32 = -18;
/// modemst1, modemst2, fsg, fsc (the phone's GPT, docs/telephony.md C0)
const PART_SIZES: [usize; 4] = [0x20_0000, 0x20_0000, 0x20_0000, 0x2_0000];
const RUNNING: u32 = 8;
const FATAL: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Io {
    a: u32,
    b: u32,
    len: u32,
    buf: u32,
}

fn hal(function: i32, a1: *mut u8, a2: *mut u8) -> i32 {
    // SAFETY: the telephony HAL functions read/write only the buffers passed, with the lengths given.
    unsafe { e32::sys::ui::hal_function(GROUP_VARIANT, function, a1, a2) }
}

fn hal_io(function: i32, mut io: Io) -> (i32, Io) {
    let r = hal(function, (&mut io as *mut Io).cast(), core::ptr::null_mut());
    (r, io)
}

/// boot step, error, SMP2P slave-kernel bits, IPCRTR open
fn state() -> [u32; 4] {
    let mut s = [0u32; 4];
    hal(STATE, s.as_mut_ptr().cast(), core::ptr::null_mut());
    s
}

struct Hal;

impl Platform for Hal {
    fn send(&mut self, packet: &[u8]) -> Result<(), i32> {
        for _ in 0..50 {
            let r = hal(SEND, packet.as_ptr() as *mut u8, packet.len() as *mut u8);
            match r {
                NOT_READY => time::sleep(2000), // no receive intent from the modem yet
                r if r < 0 => return Err(r),
                _ => return Ok(()),
            }
        }
        Err(NOT_READY)
    }

    fn rmtfs_read(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), i32> {
        let io = Io {
            a: 1,
            b: offset,
            len: buf.len() as u32,
            buf: buf.as_mut_ptr() as u32,
        };
        match hal_io(RMTFS_MEM, io).0 {
            r if r < 0 => Err(r),
            _ => Ok(()),
        }
    }

    fn rmtfs_write(&mut self, offset: u32, data: &[u8]) -> Result<(), i32> {
        let io = Io {
            a: 2,
            b: offset,
            len: data.len() as u32,
            buf: data.as_ptr() as u32,
        };
        match hal_io(RMTFS_MEM, io).0 {
            r if r < 0 => Err(r),
            _ => Ok(()),
        }
    }

    fn log(&mut self, args: fmt::Arguments) {
        dprintln!("[modemd] {}", args);
    }
}

fn main() -> i32 {
    dprintln!("[modemd] start: reading the EFS partitions");
    let mut images: [Vec<u8>; 4] = Default::default();
    for (i, &n) in PART_SIZES.iter().enumerate() {
        let mut v = vec![0u8; n];
        let (r, _) = hal_io(
            PART_READ,
            Io {
                a: i as u32,
                b: 0,
                len: n as u32,
                buf: v.as_mut_ptr() as u32,
            },
        );
        if r < 0 {
            dprintln!("[modemd] partition {} read failed ({})", i, r);
            return r;
        }
        images[i] = v;
    }
    for (i, img) in images.iter().enumerate() {
        // FNV-1a, to compare with the C0 backups on the Mac (the modem must get exactly these bytes)
        let h = img.iter().fold(0x811c_9dc5u32, |h, &b| {
            (h ^ b as u32).wrapping_mul(0x0100_0193)
        });
        dprintln!("[modemd] partition {} fnv1a {:#010x}", i, h);
    }
    let (r, info) = hal_io(RMTFS_MEM, Io::default());
    if r < 0 {
        dprintln!("[modemd] rmtfs carve-out failed ({})", r);
        return r;
    }
    dprintln!(
        "[modemd] rmtfs buffer {:#x} + {:#x}; booting the modem",
        info.b,
        info.len
    );
    let mut modem = Modem::new(images, info.b as u64);
    modem.trace = true; // bring-up: every QRTR packet in the log
    let r = hal(BOOT, core::ptr::null_mut(), core::ptr::null_mut());
    if r < 0 {
        dprintln!("[modemd] modem boot failed to start ({})", r);
        return r;
    }
    let t0 = time::millis();
    loop {
        let s = state();
        if s[1] != 0 {
            dprintln!(
                "[modemd] modem boot failed at step {} ({})",
                s[0],
                s[1] as i32
            );
            return s[1] as i32;
        }
        if s[0] == RUNNING && s[3] != 0 {
            break;
        }
        if time::millis() - t0 > 60_000 {
            dprintln!("[modemd] modem not up after 60 s (step {})", s[0]);
            return NOT_READY;
        }
        time::sleep(50_000);
    }
    dprintln!(
        "[modemd] modem running after {} ms; QRTR up",
        time::millis() - t0
    );
    modem.start(&mut Hal);
    let mut buf = vec![0u8; 0x2000];
    let mut last = time::millis();
    let mut fatal = false;
    loop {
        loop {
            let r = hal(RECV, buf.as_mut_ptr(), buf.len() as *mut u8);
            if r == NOT_READY {
                break;
            }
            if r < 0 {
                dprintln!("[modemd] receive failed ({})", r);
                break;
            }
            modem.handle(&mut Hal, &buf[..r as usize]);
        }
        let now = time::millis();
        if now - last >= 30_000 {
            last = now;
            let s = state();
            dprintln!(
                "[modemd] {} s: slave-kernel {:#x}, rmtfs {} reads {} writes, {} modem servers",
                (now - t0) / 1000,
                s[2],
                modem.rmtfs.reads,
                modem.rmtfs.writes,
                modem.servers.len()
            );
            if s[2] & FATAL != 0 && !fatal {
                fatal = true;
                dprintln!("[modemd] the modem raised FATAL");
            }
        }
        time::sleep(2000);
    }
}

e32::main!(main);
