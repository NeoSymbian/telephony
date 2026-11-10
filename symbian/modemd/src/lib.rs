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
use modem_proto::apr::Mode;
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
const RPROC_STOP: i32 = 0x4e1a;
const RPROC_STATE: i32 = 0x4e1b;
const HAPTIC: i32 = 0x4e1d;
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
/// the daemon's step period
const LOOP_US: i32 = 10_000;
const STATUS_LOG_MS: u64 = 30_000;

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
                NOT_READY => time::sleep(20_000), // e32 sleeps in microseconds: 20 ms, at most 200 ms in all
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
    /// the ADSP loads and comes up before the modem boots (C6): loading it during the modem's start-up lost the
    /// modem's announcements, and the modem hears "adsp up" (SSCTL) as soon as both run
    Adsp,
    /// the kernel boots the modem; since t0
    Wait(u64),
    Running,
    Recover {
        mask: u32,
        since: u64,
        issued: bool,
    },
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
    /// a keypad tone ends here (ms): one tap = one 200 ms tone
    dtmf_stop: Option<u64>,
    subscribers: Vec<(u32, Message)>,
    next_session: u32,
    published: Status,
    recovery_attempt: u8,
    haptic_at: u64,
    healthy_since: u64,
    alert_dismissed: Option<u8>,
    waiting_alert: Option<u8>,
}

impl Daemon {
    fn status(&self) -> Status {
        let mut s = self.modem.as_ref().map_or(self.status, |m| m.status);
        s.audio = self.audio.status();
        s.revision = self.published.revision;
        match self.phase {
            Phase::Recover { .. } => {
                s.modem = modem_ipc::modem::RECOVERING;
                s.reg = modem_ipc::reg::NONE;
                s.bars = 0;
            }
            Phase::Failed => s.modem = modem_ipc::modem::FAILED,
            _ => {}
        }
        s
    }

    fn publish(&mut self) {
        let mut next = self.status();
        if next == self.published {
            return;
        }
        next.revision = self.published.revision.wrapping_add(1);
        self.published = next;
        for (_, msg) in self.subscribers.drain(..) {
            let r = msg.write8(0, &next.to_bytes(), 0).map_or_else(|e| e, |_| 0);
            msg.complete(r);
        }
    }

    fn cancel_notify(&mut self, id: u32) {
        if let Some(i) = self.subscribers.iter().position(|(s, _)| *s == id) {
            self.subscribers.remove(i).1.complete(-3);
        }
    }

    fn recover(&mut self, mask: u32) {
        if matches!(self.phase, Phase::Recover { .. }) {
            return;
        }
        if self.recovery_attempt >= 3 {
            self.phase = Phase::Failed;
            return;
        }
        let delay = [1000, 3000, 10_000][usize::from(self.recovery_attempt)];
        self.recovery_attempt += 1;
        self.dtmf_stop = None;
        self.healthy_since = 0;
        let _ = hal_io(HAPTIC, Io::default());
        self.audio.apr.set(Mode::Off);
        self.alert_dismissed = None;
        self.waiting_alert = None;
        let _ = adsp::audio(2, 0);
        self.phase = Phase::Recover {
            mask,
            since: time::millis() + delay,
            issued: false,
        };
        dprintln!(
            "[modemd] recovery attempt {} mask {}",
            self.recovery_attempt,
            mask
        );
    }

    fn save_settings(&self) -> Result<(), i32> {
        use e32::fs::{File, Fs, mode};
        let s = self.status();
        let fs = Fs::connect()?;
        for path in ["C:\\private\\", "C:\\private\\e0003001\\"] {
            if let Err(e) = fs.mkdir(path) {
                if e != err::KERR_ALREADY_EXISTS {
                    return Err(e);
                }
            }
        }
        let f = File::replace(
            &fs,
            "C:\\private\\e0003001\\settings.bin",
            mode::SHARE_EXCLUSIVE,
        )?;
        f.write(&[
            b'C',
            b'7',
            1,
            s.airplane as u8,
            s.ring.volume,
            s.ring.silent as u8,
            s.ring.vibrate as u8,
        ])?;
        Ok(())
    }

    fn fail(&mut self, what: &str, r: i32) {
        dprintln!("[modemd] {} failed ({})", what, r);
        self.phase = Phase::Failed;
        self.status.modem = modem_ipc::modem::FAILED;
        if let Some(m) = &mut self.modem {
            m.status.modem = modem_ipc::modem::FAILED;
        }
        if self.modem.is_some() && self.recovery_attempt < 3 {
            self.recover(3);
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
            Phase::Adsp => {
                self.adsp.step();
                if let Some(m) = &mut self.modem {
                    while let Some(n) = adsp::recv(adsp::RING_QRTR, &mut self.rx) {
                        m.handle(&mut Hal, &self.rx[..n]);
                    }
                }
                if matches!(self.adsp, adsp::Adsp::Failed) {
                    self.fail("ADSP boot", NOT_READY);
                } else if !self.adsp.busy() {
                    self.boot_modem();
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
            Phase::Recover {
                mask,
                since,
                issued,
            } => {
                let now = time::millis();
                if now < since {
                    return Ok(());
                }
                if !issued {
                    let r = hal_io(
                        RPROC_STOP,
                        Io {
                            a: mask,
                            ..Io::default()
                        },
                    )
                    .0;
                    if r < 0 {
                        self.fail("processor stop", r);
                        return Ok(());
                    }
                    self.phase = Phase::Recover {
                        mask,
                        since: now,
                        issued: true,
                    };
                } else {
                    let mut st = [0u32; 4];
                    let r = hal(RPROC_STATE, st.as_mut_ptr().cast(), core::ptr::null_mut());
                    if r < 0 || st[0] == 3 || now.saturating_sub(since) > 15_000 {
                        self.fail(
                            "processor stop completion",
                            if r < 0 { r } else { st[1] as i32 },
                        );
                    } else if st[0] == 2 {
                        if let Some(m) = &mut self.modem {
                            m.reset(mask & 2 == 0);
                        }
                        self.audio = adsp::CallAudio::new();
                        self.fatal_seen = false;
                        if mask & 2 != 0 {
                            self.adsp = adsp::Adsp::Idle;
                            self.phase = Phase::Adsp;
                        } else {
                            self.boot_modem();
                        }
                    }
                }
            }
            Phase::Failed => {}
        }
        Ok(())
    }

    /// An audio setting to the kernel's codecs while the call's audio runs (else it waits for the next call).
    fn apply(&mut self, op: u32, arg: u32) -> i32 {
        if self.audio.apr.is() != Mode::Call {
            return err::KERR_NONE;
        }
        let r = adsp::audio(op, arg);
        if r < 0 {
            // The board driver stops amps on a failed control. Invalidate the live
            // path immediately and rebuild both processors, preserving RAM EFS.
            self.audio.apr.codec_fault();
            self.recover(3);
            r
        } else {
            err::KERR_NONE
        }
    }

    /// Dev tools' audio test (no call): 1 loudspeaker, 2 earpiece, the mic looped back to the amps; 0 stops it.
    /// Pressing the other route while it runs only switches the amps.
    fn audio_test(&mut self, what: i32) -> i32 {
        if !matches!(self.adsp, adsp::Adsp::Up) {
            return NOT_READY;
        }
        if self.talking() {
            return err::KERR_IN_USE;
        }
        let test = match what {
            0 => None,
            1 | 2 => Some(what == 1),
            _ => return err::KERR_ARGUMENT,
        };
        dprintln!("[modemd] audio test {:?} (loudspeaker)", test);
        if let (Some(spk), Mode::Test) = (test, self.audio.apr.is()) {
            let r = adsp::audio(3, spk as u32);
            if r < 0 {
                return r;
            }
        }
        self.audio.test = test;
        err::KERR_NONE
    }

    /// A call is connected (its audio runs or is wanted).
    fn talking(&self) -> bool {
        self.modem.as_ref().is_some_and(|m| {
            m.status
                .calls
                .as_slice()
                .iter()
                .any(|c| !matches!(c.state, modem_ipc::call::NONE | modem_ipc::call::END))
                || call_audio(m.status.call.state, self.audio.apr.is())
        })
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
        modem.status.airplane = self.status.airplane;
        modem.status.ring = self.status.ring;
        modem.status.pin_attempts = 255;
        self.modem = Some(modem);
        self.phase = Phase::Adsp;
    }

    fn boot_modem(&mut self) {
        let r = hal(BOOT, core::ptr::null_mut(), core::ptr::null_mut());
        if r < 0 {
            self.fail("modem boot start", r);
            return;
        }
        self.phase = Phase::Wait(time::millis());
    }

    fn run(&mut self) {
        let st = state();
        let dsp = adsp::processor_state();
        if st[2] & FATAL != 0 {
            self.recover(if self.audio.apr.quiescent() { 1 } else { 3 });
            return;
        }
        if dsp[2] & FATAL != 0 || dsp[1] != 0 {
            self.recover(3);
            return;
        }
        if self.audio.apr.failed_hard() {
            self.recover(3);
            return;
        }
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
        while let Some(n) = adsp::recv(adsp::RING_QRTR, buf) {
            modem.handle(&mut Hal, &buf[..n]);
        }
        // the audio follows what is wanted now: a connected call's voice path, else the test, else nothing (the
        // state machine finishes a running sequence first)
        let talking = if modem.status.calls.len > 0 {
            modem
                .status
                .calls
                .as_slice()
                .iter()
                .any(|c| call_audio(c.state, self.audio.apr.is()))
        } else {
            call_audio(modem.status.call.state, self.audio.apr.is())
        };
        if self.alert_dismissed.is_some_and(|id| {
            !modem.status.calls.as_slice().iter().any(|c| {
                c.id == id
                    && matches!(
                        c.state,
                        modem_ipc::call::INCOMING | modem_ipc::call::WAITING
                    )
            })
        }) || (!modem.status.pending && modem.status.result < 0)
        {
            self.alert_dismissed = None;
        }
        let ringing = modem.status.calls.as_slice().iter().any(|c| {
            c.incoming && c.state == modem_ipc::call::INCOMING && self.alert_dismissed != Some(c.id)
        });
        let now = time::millis();
        if ringing && !talking && !modem.status.airplane && modem.status.ring.vibrate {
            if now >= self.haptic_at {
                let _ = hal_io(
                    HAPTIC,
                    Io {
                        a: 700,
                        b: 70,
                        ..Io::default()
                    },
                );
                self.haptic_at = now + 1500;
            }
        } else if self.haptic_at != 0 {
            let _ = hal_io(HAPTIC, Io::default());
            self.haptic_at = 0;
        }
        let waiting = modem
            .status
            .calls
            .as_slice()
            .iter()
            .find(|c| c.state == modem_ipc::call::WAITING)
            .map(|c| c.id);
        if waiting != self.waiting_alert {
            self.waiting_alert = waiting;
            if waiting.is_some() && talking && !modem.status.airplane && modem.status.ring.vibrate {
                let r = hal_io(
                    HAPTIC,
                    Io {
                        a: 150,
                        b: 60,
                        ..Io::default()
                    },
                )
                .0;
                if r < 0 {
                    dprintln!("[modemd] waiting-call vibration failed ({})", r);
                }
            }
        }
        if self.healthy_since == 0 {
            self.healthy_since = now;
        }
        if now.saturating_sub(self.healthy_since) >= 60_000 {
            self.recovery_attempt = 0;
        }
        if talking {
            self.audio.test = None;
        }
        self.audio.ring = modem.status.ring;
        let ring_wanted = ringing
            && !talking
            && !modem.status.airplane
            && !modem.status.ring.silent
            && modem.status.ring.volume > 0;
        let ring_ready = if ring_wanted && self.audio.apr.is() == Mode::Off && self.audio.apr.idle()
        {
            match self.audio.prepare_ring() {
                Ok(()) => true,
                Err(e) => {
                    modem.status.result = e;
                    false
                }
            }
        } else {
            ring_wanted
        };
        self.audio.apr.set(
            match (
                matches!(self.adsp, adsp::Adsp::Up),
                talking,
                self.audio.test,
                ring_ready,
            ) {
                (false, _, _, _) => Mode::Off,
                (true, true, _, _) => Mode::Call,
                (true, false, _, true) => Mode::Ring,
                (true, false, Some(_), false) => Mode::Test,
                (true, false, None, false) => Mode::Off,
            },
        );
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
        }
        if modem.watchdog() {
            self.recover(3);
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
    type Session = u32;

    fn connect(&mut self, v: TVersion) -> Result<u32, i32> {
        if v.major as i8 == modem_ipc::VERSION.0 {
            self.next_session = self.next_session.wrapping_add(1).max(1);
            Ok(self.next_session)
        } else {
            Err(err::KERR_NOT_SUPPORTED)
        }
    }

    fn message(&mut self, session: &mut u32, msg: Message) {
        if msg.function() == op::NOTIFY {
            if self.subscribers.iter().any(|(s, _)| s == session) || self.subscribers.len() >= 16 {
                msg.complete(err::KERR_IN_USE);
                return;
            }
            if msg.int(1) as u32 == self.published.revision {
                self.subscribers.push((*session, msg));
                return;
            }
            let r = msg
                .write8(0, &self.published.to_bytes(), 0)
                .map_or_else(|e| e, |_| 0);
            msg.complete(r);
            return;
        }
        if !matches!(
            msg.function(),
            op::STATUS | op::CANCEL_NOTIFY | op::RETRY | op::RING_SETTINGS
        ) && !matches!(self.phase, Phase::Running)
        {
            msg.complete(NOT_READY);
            return;
        }
        let target = msg.int(0);
        if matches!(msg.function(), op::ANSWER | op::HANGUP) && !(0..=255).contains(&target) {
            msg.complete(err::KERR_ARGUMENT);
            return;
        }

        let r = match msg.function() {
            op::STATUS => msg
                .write8(0, &self.status().to_bytes(), 0)
                .map_or_else(|e| e, |_| err::KERR_NONE),
            op::DIAL => {
                let read = msg.des_len(0).and_then(|n| {
                    if n <= 32 {
                        msg.read8_vec(0)
                    } else {
                        Err(err::KERR_ARGUMENT)
                    }
                });
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
                m.answer_call(&mut Hal, target as u8)
                    .err()
                    .unwrap_or(err::KERR_NONE)
            }),
            op::HANGUP => self.modem.as_mut().map_or(NOT_READY, |m| {
                m.end_call(&mut Hal, target as u8)
                    .err()
                    .unwrap_or(err::KERR_NONE)
            }),
            // the in-call buttons (C6): remembered for the next call too, applied now if the audio runs
            op::MUTE => {
                let want = msg.int(0) != 0;
                let r = self.apply(5, want as u32);
                if r == 0 {
                    self.audio.muted = want;
                }
                r
            }
            op::SPEAKER => {
                let want = msg.int(0) != 0;
                let r = self.apply(3, want as u32);
                if r == 0 {
                    self.audio.speaker = want;
                }
                r
            }
            op::VOLUME => {
                let want = msg.int(0);
                if !(0..=10).contains(&want) {
                    err::KERR_ARGUMENT
                } else {
                    let r = self.apply(4, want as u32);
                    if r == 0 {
                        self.audio.volume = want as u8;
                    }
                    r
                }
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
            op::AUDIO_TEST => self.audio_test(msg.int(0)),
            op::CANCEL_NOTIFY => {
                self.cancel_notify(*session);
                0
            }
            op::MANAGE => {
                let id = msg.int(1);
                if !(0..=255).contains(&id) {
                    err::KERR_ARGUMENT
                } else {
                    self.modem.as_mut().map_or(NOT_READY, |m| {
                        m.manage(&mut Hal, msg.int(0), id as u8).err().unwrap_or(0)
                    })
                }
            }
            op::AIRPLANE => {
                let r = self.modem.as_mut().map_or(NOT_READY, |m| {
                    m.airplane(&mut Hal, msg.int(0) != 0).err().unwrap_or(0)
                });
                if r == 0 {
                    self.save_settings().err().unwrap_or(0)
                } else {
                    r
                }
            }
            op::VERIFY_PIN => match msg.des_len(0).and_then(|n| {
                if (4..=8).contains(&n) {
                    msg.read8_vec(0)
                } else {
                    Err(err::KERR_ARGUMENT)
                }
            }) {
                Ok(mut pin) => {
                    let r = self.modem.as_mut().map_or(NOT_READY, |m| {
                        m.verify_pin(&mut Hal, &pin).err().unwrap_or(0)
                    });
                    pin.fill(0);
                    r
                }
                Err(e) => e,
            },
            op::RING_SETTINGS => {
                if !(0..=10).contains(&msg.int(0)) {
                    err::KERR_ARGUMENT
                } else {
                    let ring = modem_ipc::Ring {
                        volume: msg.int(0) as u8,
                        silent: msg.int(1) != 0,
                        vibrate: msg.int(2) != 0,
                    };
                    self.status.ring = ring;
                    if let Some(m) = &mut self.modem {
                        m.status.ring = ring;
                    }
                    self.save_settings().err().unwrap_or(0)
                }
            }
            op::RETRY => {
                if !matches!(self.phase, Phase::Failed) || self.modem.is_none() {
                    err::KERR_IN_USE
                } else {
                    self.recovery_attempt = 0;
                    self.recover(3);
                    0
                }
            }
            _ => err::KERR_NOT_SUPPORTED,
        };
        if r == 0 && matches!(msg.function(), op::ANSWER | op::HANGUP) {
            self.alert_dismissed = self.modem.as_ref().map(|m| {
                if target == 0 {
                    m.status.call.id
                } else {
                    target as u8
                }
            });
            let _ = hal_io(HAPTIC, Io::default());
            self.haptic_at = 0;
        }
        msg.complete(r);
    }

    fn disconnect(&mut self, session: u32) -> ControlFlow<()> {
        self.cancel_notify(session);
        ControlFlow::Continue(())
    }
}

/// The call's voice path runs from "connected" until the modem says the call ended (END/NONE), through hold and
/// disconnecting, as Linux (it stops on "terminated"). Stopping it at "disconnecting" overlapped the modem's own
/// teardown with the ADSP: the ADSP stopped answering and the modem stayed on 3G (C6 2026-11-09).
fn call_audio(state: u8, is: Mode) -> bool {
    use modem_ipc::call;
    state == call::CONVERSATION
        || (is == Mode::Call && matches!(state, call::HOLD | call::DISCONNECTING))
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
        dtmf_stop: None,
        subscribers: Vec::new(),
        next_session: 0,
        published: Status::default(),
        recovery_attempt: 0,
        haptic_at: 0,
        healthy_since: 0,
        alert_dismissed: None,
        waiting_alert: None,
    };
    // Ordinary C: settings only; modem EFS remains RAM-backed (G4).
    if let Ok(fs) = e32::fs::Fs::connect() {
        if let Ok(f) = e32::fs::File::open(
            &fs,
            "C:\\private\\e0003001\\settings.bin",
            e32::fs::mode::READ | e32::fs::mode::SHARE_READERS_ONLY,
        ) {
            let mut b = [0; 7];
            if f.read_at(0, &mut b) == Ok(7) && b[..3] == [b'C', b'7', 1] && b[4] <= 10 {
                d.status.airplane = b[3] != 0;
                d.status.ring = modem_ipc::Ring {
                    volume: b[4],
                    silent: b[5] != 0,
                    vibrate: b[6] != 0,
                };
            }
        }
    }
    d.publish();
    rt::block_on(async {
        let mut receive = pin!(server.receive());
        loop {
            // one step every 10 ms: the rings are polled (e32 delays are in microseconds; the old "20 ms / 2 s"
            // were 20 us / 2 ms, C6 2026-11-09)
            match rt::select(receive.as_mut(), time::delay(LOOP_US)).await {
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
            d.publish();
        }
    })
}

e32::main!(main);
