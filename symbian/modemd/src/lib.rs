// SPDX-License-Identifier: Apache-2.0
//! MODEMD.EXE (docs/telephony.md C4, C5): reads the modem's EFS partitions, boots the modem, runs the apps side of
//! QRTR (modem_proto::Modem) over the kernel's telephony HAL functions (kernel qcom_telhal.cpp, 0x4e0d..0x4e12), and
//! serves the NeoModem IPC API (modem_ipc: status, dial, answer, hang up). neoui's SYSSTART starts it at boot; a
//! second copy (Terminal `modemd`) finds the server name taken and exits.
//! The server answers all the time: the partition reads and the boot wait are steps of the loop, never a long block.
#![no_std]

extern crate alloc;

mod adsp;

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::ops::ControlFlow;
use core::pin::pin;
use e32::rt::{self, Either};
use e32::server::{self, Message, Server, Service};
use e32::sys::err;
use e32::sys::ipc::TVersion;
use e32::{dprintln, time};
use modem_proto::modem_ipc::{self, Status, op};
use modem_proto::{Modem, Platform};

const GROUP_VARIANT: i32 = 1;
const BOOT: i32 = 0x4e0d;
const STATE: i32 = 0x4e0e;
const SEND: i32 = 0x4e0f;
const RECV: i32 = 0x4e10;
const PART_READ: i32 = 0x4e11;
const RMTFS_MEM: i32 = 0x4e12;
const IPA_START: i32 = 0x4e13;
/// PartRead's index of vendor_a (the IPA firmware, read-only through ext4ro)
const VENDOR: u32 = 4;
const SECTOR: u64 = 512;
const NOT_READY: i32 = -18;
/// modemst1, modemst2, fsg, fsc (the phone's GPT, docs/telephony.md C0)
const PART_SIZES: [usize; 4] = [0x20_0000, 0x20_0000, 0x20_0000, 0x2_0000];
const RUNNING: u32 = 8;
/// the modem's QRTR node; every other node is behind the ADSP's IPCRTR
const MODEM_NODE: u32 = 0;
const FATAL: u32 = 1;
const BOOT_TIMEOUT_MS: u64 = 60_000;
const STATUS_LOG_MS: u64 = 30_000;
/// the ADSP loads once the modem's start-up traffic has settled (they overlapped badly: C6 2026-11-09)
const ADSP_AFTER_MS: u64 = 20_000;

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
    fn send(&mut self, node: u32, packet: &[u8]) -> Result<(), i32> {
        if node != MODEM_NODE {
            return adsp::send(adsp::RING_QRTR, packet); // the ADSP's router (C6)
        }
        // no receive intent from the modem yet: retry briefly, never long (a 100 s block lost the modem's
        // announcements, UIM among them: C6 2026-11-09)
        for _ in 0..10 {
            let r = hal(SEND, packet.as_ptr() as *mut u8, packet.len() as *mut u8);
            match r {
                NOT_READY => time::sleep(200),
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

enum Phase {
    /// IPA's GSI firmware from vendor_a, then IPA powered and the firmware loaded (C5b)
    Ipa,
    /// reading partition i (one per loop step)
    Read(usize),
    /// the kernel boots the modem; since t0
    Wait(u64),
    Running,
    Failed,
}

struct Daemon {
    phase: Phase,
    images: [Vec<u8>; 4],
    modem: Option<Modem>,
    /// the QRTR receive buffer (8 KB: not on the 8 KB stack)
    rx: Vec<u8>,
    /// before the modem exists (booting / failed)
    status: Status,
    t0: u64,
    last_log: u64,
    fatal_seen: bool,
    /// the kernel runs IPA: LTE allowed, the IPA handshake on (else voice-only)
    ipa_up: bool,
    /// the audio DSP (C6): loaded once the modem runs
    adsp: adsp::Adsp,
    /// the call's audio path and the in-call buttons' settings (C6)
    audio: adsp::CallAudio,
    /// the call state the audio last followed
    audio_call: u8,
    /// a keypad tone ends here (ms): one tap = one 200 ms tone
    dtmf_stop: Option<u64>,
}

impl Daemon {
    fn status(&self) -> Status {
        let mut s = self.modem.as_ref().map_or(self.status, |m| m.status);
        s.audio = self.audio.status();
        s
    }

    fn fail(&mut self, what: &str, r: i32) {
        dprintln!("[modemd] {} failed ({})", what, r);
        self.phase = Phase::Failed;
        self.status.modem = modem_ipc::modem::FAILED;
        if let Some(m) = &mut self.modem {
            m.status.modem = modem_ipc::modem::FAILED;
        }
    }

    /// One step of the work: Err(KErrNotSupported) when this machine has no modem (QEMU/Syborg).
    fn step(&mut self) -> Result<(), i32> {
        match self.phase {
            Phase::Ipa => {
                match ipa_firmware() {
                    Ok((image, mdt_len)) => {
                        let (r, _) = hal_io(
                            IPA_START,
                            Io {
                                a: mdt_len as u32,
                                b: 0,
                                len: image.len() as u32,
                                buf: image.as_ptr() as u32,
                            },
                        );
                        dprintln!(
                            "[modemd] ipa: firmware {} bytes (mdt {}), IPA bring-up -> {}",
                            image.len(),
                            mdt_len,
                            r
                        );
                        self.ipa_up = r == 0;
                    }
                    Err(ext4ro::Error::Io(e)) if e == err::KERR_NOT_SUPPORTED => return Err(e),
                    Err(e) => dprintln!("[modemd] ipa: no firmware from vendor_a ({:?})", e),
                }
                self.phase = Phase::Read(0); // the modem boots either way
            }
            Phase::Read(i) => {
                let n = PART_SIZES[i];
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
                if r == err::KERR_NOT_SUPPORTED {
                    return Err(r);
                }
                if r < 0 {
                    self.fail("partition read", r);
                    return Ok(());
                }
                self.images[i] = v;
                if i + 1 < PART_SIZES.len() {
                    self.phase = Phase::Read(i + 1);
                } else {
                    self.boot();
                }
            }
            Phase::Wait(t0) => {
                let s = state();
                if s[1] != 0 {
                    self.fail("modem boot", s[1] as i32);
                } else if s[0] == RUNNING && s[3] != 0 {
                    let now = time::millis();
                    dprintln!("[modemd] modem running after {} ms; QRTR up", now - t0);
                    self.t0 = now;
                    self.last_log = now;
                    if let Some(m) = &mut self.modem {
                        m.start(&mut Hal);
                    }
                    self.phase = Phase::Running;
                } else if time::millis() - t0 > BOOT_TIMEOUT_MS {
                    self.fail("modem boot (60 s)", NOT_READY);
                }
            }
            Phase::Running => self.run(),
            Phase::Failed => {}
        }
        Ok(())
    }

    /// An audio setting to the kernel's codecs while the call's audio runs (else it waits for the next call).
    fn apply(&mut self, op: u32, arg: u32) -> i32 {
        if !self.audio.apr.running {
            return err::KERR_NONE;
        }
        let r = adsp::audio(op, arg);
        if r < 0 { r } else { err::KERR_NONE }
    }

    fn boot(&mut self) {
        let (r, info) = hal_io(RMTFS_MEM, Io::default());
        if r < 0 {
            self.fail("rmtfs carve-out", r);
            return;
        }
        dprintln!(
            "[modemd] rmtfs buffer {:#x} + {:#x}; booting the modem",
            info.b,
            info.len
        );
        let images = core::mem::take(&mut self.images);
        let mut modem = Modem::new(images, info.b as u64);
        modem.ipa_up = self.ipa_up;
        self.modem = Some(modem);
        let r = hal(BOOT, core::ptr::null_mut(), core::ptr::null_mut());
        if r < 0 {
            self.fail("modem boot start", r);
            return;
        }
        self.phase = Phase::Wait(time::millis());
    }

    fn run(&mut self) {
        let Some(modem) = &mut self.modem else {
            return;
        };
        let buf = &mut self.rx;
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
        if self.adsp.busy() && time::millis() - self.t0 >= ADSP_AFTER_MS {
            self.adsp.step();
        }
        while let Some(n) = adsp::recv(adsp::RING_QRTR, buf) {
            modem.handle(&mut Hal, &buf[..n]);
        }
        // the call's audio follows the call: on once connected (the ADSP up), off when it ends
        let call_state = modem.status.call.state;
        if call_state != self.audio_call {
            self.audio_call = call_state;
            let talking = call_state == modem_ipc::call::CONVERSATION;
            if talking && !self.audio.apr.running && matches!(self.adsp, adsp::Adsp::Up) {
                dprintln!("[modemd] audio: call connected, starting the voice path");
                self.audio.apr.start();
            } else if !talking && self.audio.apr.running {
                dprintln!("[modemd] audio: call over, stopping the voice path");
                self.audio.apr.stop();
            }
        }
        if matches!(self.adsp, adsp::Adsp::Up) {
            self.audio.pump(buf);
        }
        if self.dtmf_stop.is_some_and(|t| time::millis() >= t) {
            self.dtmf_stop = None;
            let _ = modem.dtmf(&mut Hal, None);
        }
        let now = time::millis();
        modem.tick(&mut Hal, now);
        if now - self.last_log >= STATUS_LOG_MS {
            self.last_log = now;
            let s = state();
            let st = modem.status;
            dprintln!(
                "[modemd] {} s: slave-kernel {:#x}, reg {} rat {} bars {}, call state {}",
                (now - self.t0) / 1000,
                s[2],
                st.reg,
                st.rat,
                st.bars,
                st.call.state
            );
            if s[2] & FATAL != 0 && !self.fatal_seen {
                self.fatal_seen = true;
                dprintln!("[modemd] the modem raised FATAL: no service until a reboot");
                modem.lost();
            }
        }
    }
}

/// vendor_a through the kernel's read-only, sector-aligned PartRead.
struct Vendor;

impl ext4ro::Disk for Vendor {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), i32> {
        let first = offset / SECTOR * SECTOR;
        let end = (offset + buf.len() as u64).div_ceil(SECTOR) * SECTOR;
        let mut tmp = vec![0u8; (end - first) as usize];
        let (r, _) = hal_io(
            PART_READ,
            Io {
                a: VENDOR,
                b: first as u32,
                len: tmp.len() as u32,
                buf: tmp.as_mut_ptr() as u32,
            },
        );
        if r < 0 {
            return Err(r);
        }
        let at = (offset - first) as usize;
        buf.copy_from_slice(&tmp[at..at + buf.len()]);
        Ok(())
    }
}

/// IPA's GSI firmware as one ELF image: ipa_fws.mdt (headers + hash) with each ipa_fws.bNN at its p_offset, as
/// Linux's mdt loader reads the split files; and the .mdt's length (the metadata TrustZone checks).
fn ipa_firmware() -> Result<(Vec<u8>, usize), ext4ro::Error> {
    let mut fs = ext4ro::Fs::open(Vendor)?;
    let mdt = fs.find("firmware/ipa_fws.mdt")?;
    let mdt = fs.read(&mdt)?;
    let word =
        |b: &[u8], at: usize| u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize;
    if mdt.len() < 0x34 || &mdt[..4] != b"\x7fELF" {
        return Err(ext4ro::Error::Unsupported);
    }
    let (phoff, phnum) = (
        word(&mdt, 28),
        u16::from_le_bytes([mdt[44], mdt[45]]) as usize,
    );
    if phoff + 32 * phnum > mdt.len() {
        return Err(ext4ro::Error::Unsupported);
    }
    // (p_type, p_offset, p_filesz, p_flags) of program header i
    let ph = |i: usize| {
        let at = phoff + 32 * i;
        (
            word(&mdt, at),
            word(&mdt, at + 4),
            word(&mdt, at + 16),
            word(&mdt, at + 24),
        )
    };
    // only PT_LOAD segments that aren't the hash: those come from .bNN at their p_offset, past the .mdt. The hash
    // segment's p_offset falls inside the .mdt's bytes (its copy is packed there); writing .b01 over it corrupted
    // the metadata TrustZone checks (C5b: init_image refused, -0x300045).
    let loadable = |i: usize| ph(i).0 == 1 && ph(i).3 & (7 << 24) != 2 << 24 && ph(i).2 > 0;
    let len = (0..phnum)
        .filter(|&i| loadable(i))
        .map(|i| ph(i).1 + ph(i).2)
        .fold(mdt.len(), usize::max);
    let mut image = vec![0u8; len];
    image[..mdt.len()].copy_from_slice(&mdt);
    for i in (0..phnum).filter(|&i| loadable(i)) {
        let (_, off, size, _) = ph(i);
        if off < mdt.len() {
            return Err(ext4ro::Error::Unsupported); // would overwrite the metadata
        }
        let seg = fs.find(&alloc::format!("firmware/ipa_fws.b{i:02}"))?;
        let seg = fs.read(&seg)?;
        let n = seg.len().min(size);
        image[off..off + n].copy_from_slice(&seg[..n]);
    }
    Ok((image, mdt.len()))
}

impl Service for Daemon {
    type Session = ();

    fn connect(&mut self, v: TVersion) -> Result<(), i32> {
        if v.major as i8 == modem_ipc::VERSION.0 {
            Ok(())
        } else {
            Err(err::KERR_NOT_SUPPORTED)
        }
    }

    fn message(&mut self, _: &mut (), msg: Message) {
        let r = match msg.function() {
            op::STATUS => msg
                .write8(0, &self.status().to_bytes(), 0)
                .map_or_else(|e| e, |_| err::KERR_NONE),
            op::DIAL => {
                let read = msg.read8_vec(0);
                let r = match (&mut self.modem, &read) {
                    (Some(m), Ok(n)) => match core::str::from_utf8(n) {
                        Ok(n) => m.dial(&mut Hal, n).err().unwrap_or(err::KERR_NONE),
                        Err(_) => err::KERR_ARGUMENT,
                    },
                    (_, Err(e)) => *e,
                    (None, _) => NOT_READY,
                };
                if r != err::KERR_NONE {
                    // what came in, without the number: its length, digit count and any non-digit bytes
                    let (len, digits, other) = read.as_ref().map_or((0, 0, Vec::new()), |n| {
                        let other: Vec<u8> =
                            n.iter().copied().filter(|c| !c.is_ascii_digit()).collect();
                        (n.len(), n.len() - other.len(), other)
                    });
                    dprintln!(
                        "[modemd] dial refused ({}): read {:?}, {} bytes, {} digits, other {:02x?}",
                        r,
                        read.as_ref().map(|_| ()),
                        len,
                        digits,
                        other
                    );
                }
                r
            }
            op::ANSWER => self.modem.as_mut().map_or(NOT_READY, |m| {
                m.answer(&mut Hal).err().unwrap_or(err::KERR_NONE)
            }),
            op::HANGUP => self.modem.as_mut().map_or(NOT_READY, |m| {
                m.hangup(&mut Hal).err().unwrap_or(err::KERR_NONE)
            }),
            // the in-call buttons (C6): remembered for the next call too, applied now if the audio runs
            op::MUTE => {
                self.audio.muted = msg.int(0) != 0;
                self.apply(5, self.audio.muted as u32)
            }
            op::SPEAKER => {
                self.audio.speaker = msg.int(0) != 0;
                self.apply(3, self.audio.speaker as u32)
            }
            op::VOLUME => {
                self.audio.volume = msg.int(0).clamp(0, 10) as u8;
                self.apply(4, self.audio.volume as u32)
            }
            op::DTMF => {
                let k = msg.int(0);
                let key = (k >= 0).then_some(k as u8);
                let r = self.modem.as_mut().map_or(NOT_READY, |m| {
                    m.dtmf(&mut Hal, key).err().unwrap_or(err::KERR_NONE)
                });
                if r == err::KERR_NONE && key.is_some() {
                    self.dtmf_stop = Some(time::millis() + 200);
                }
                r
            }
            _ => err::KERR_NOT_SUPPORTED,
        };
        msg.complete(r);
    }

    fn disconnect(&mut self, _: ()) -> ControlFlow<()> {
        ControlFlow::Continue(())
    }
}

fn main() -> i32 {
    let server = match Server::create(modem_ipc::NAME) {
        Ok(s) => s,
        Err(e) => {
            dprintln!("[modemd] already running ({}): exiting", e);
            return 0;
        }
    };
    dprintln!("[modemd] start: serving NeoModem, reading the EFS partitions");
    let mut d = Daemon {
        phase: Phase::Ipa,
        images: Default::default(),
        modem: None,
        rx: vec![0u8; 0x2000],
        status: Status::default(),
        t0: 0,
        last_log: 0,
        fatal_seen: false,
        ipa_up: false,
        adsp: adsp::Adsp::Idle,
        audio: adsp::CallAudio::new(),
        audio_call: 0,
        dtmf_stop: None,
    };
    rt::block_on(async {
        let mut receive = pin!(server.receive());
        loop {
            // fast while the ADSP loads (its steps) or a call's audio runs; else every 2 s
            let loading = d.adsp.busy() && time::millis() - d.t0 >= ADSP_AFTER_MS;
            let fast = loading || d.audio.busy() || d.audio_call == modem_ipc::call::CONVERSATION;
            let ms = if matches!(d.phase, Phase::Running) && fast {
                20
            } else {
                2000
            };
            match rt::select(receive.as_mut(), time::delay(ms)).await {
                Either::Left(Ok(msg)) => {
                    receive.set(server.receive());
                    let _ = server::dispatch(&mut d, msg);
                }
                Either::Left(Err(e)) => {
                    dprintln!("[modemd] receive failed ({})", e);
                    receive.set(server.receive());
                }
                Either::Right(()) => {
                    if let Err(e) = d.step() {
                        dprintln!("[modemd] no modem on this machine ({}): exiting", e);
                        return e;
                    }
                }
            }
        }
    })
}

e32::main!(main);
