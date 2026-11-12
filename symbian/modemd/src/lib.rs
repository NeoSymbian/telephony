// SPDX-License-Identifier: Apache-2.0
//! MODEMD.EXE (docs/telephony.md C4, C5): reads the modem's EFS partitions, boots the modem, runs the apps side of
//! QRTR (modem_proto::Modem) over the kernel's telephony HAL functions (kernel qcom_telhal.cpp, 0x4e0d..0x4e12), and
//! serves the NeoModem IPC API (modem_ipc: status, dial, answer, hang up). neoui's SYSSTART starts it at boot; a
//! second copy (Terminal `modemd`) finds the server name taken and exits.
//! The server answers all the time: the partition reads and the boot wait are steps of the loop, never a long block.
#![no_std]

extern crate alloc;

mod adsp;

use alloc::string::String;
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
use modem_proto::recovery::{self, StopProgress};
use modem_proto::{Modem, Platform};

const GROUP_VARIANT: i32 = 1;
const BOOT: i32 = 0x4e0d;
const STATE: i32 = 0x4e0e;
const SEND: i32 = 0x4e0f;
const RECV: i32 = 0x4e10;
const PART_READ: i32 = 0x4e11;
const RMTFS_MEM: i32 = 0x4e12;
const IPA_START: i32 = 0x4e13;
/// a file of modem_a's FAT16 (kernel qcom_telhal.cpp FwRead): TFTP's /readonly/firmware/image/ (Wi-Fi W0)
const FW_READ: i32 = 0x4e1b;
/// the Wi-Fi firmware's MSA regions to its VMIDs (kernel qcom_telhal.cpp WlanMsa, Wi-Fi W1)
const WLAN_MSA: i32 = 0x4e1c;
/// the copy engines, HTC and WMI on the kernel's own thread (qcom_wcn3990.cpp, Wi-Fi W2)
const WLAN_START: i32 = 0x4e1d;
// C7's HALs (kernel qcom_telhal.cpp): after Wi-Fi's 0x4e1b-0x4e1f, UsbNet's reserved 0x4e20-0x4e22 and Prng 0x4e23
/// stop the modem and/or the ADSP {mask 1 MSS, 2 ADSP}, asynchronously
const RPROC_STOP: i32 = 0x4e24;
/// that stop's progress {phase, error, mss generation, adsp generation}
const RPROC_STATE: i32 = 0x4e25;
/// the ringtone's audio memory for the ADSP (adsp.rs)
pub(crate) const AUDIO_MEM: i32 = 0x4e26;
/// the vibration motor {ms, amplitude}; 0 stops it
const HAPTIC: i32 = 0x4e27;
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
fn state() -> Result<[u32; 4], i32> {
    let mut s = [0u32; 4];
    let r = hal(STATE, s.as_mut_ptr().cast(), core::ptr::null_mut());
    if r < 0 { Err(r) } else { Ok(s) }
}

fn processor_stop_state() -> Result<[u32; 4], i32> {
    let mut s = [0u32; 4];
    let r = hal(RPROC_STATE, s.as_mut_ptr().cast(), core::ptr::null_mut());
    if r < 0 { Err(r) } else { Ok(s) }
}

struct Hal;

impl Platform for Hal {
    fn send(&mut self, node: u32, packet: &[u8]) -> Result<(), i32> {
        if node != MODEM_NODE {
            return adsp::send(adsp::RING_QRTR, packet); // the ADSP's router (C6)
        }
        // No userspace retry sleeps: mandatory packets use the ordered outbox. The synchronous kernel HAL
        // may still wait for a receive intent; this is not a hard real-time send deadline.
        match hal(SEND, packet.as_ptr() as *mut u8, packet.len() as *mut u8) {
            r if r < 0 => Err(r),
            _ => Ok(()),
        }
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
        dprintln!("[modemd t={}ms] {}", time::millis(), args);
    }

    fn fw_size(&mut self, path: &str) -> Result<usize, i32> {
        match path.strip_prefix("vendor/") {
            Some(p) => vendor_file(p, |fs, f| Ok(fs_size(fs, f))),
            None => fw_read(path, 0, &mut []),
        }
    }

    fn fw_read(&mut self, path: &str, offset: usize, buf: &mut [u8]) -> Result<usize, i32> {
        match path.strip_prefix("vendor/") {
            Some(p) => vendor_file(p, |fs, f| {
                fs.read_at(f, offset as u64, buf).map_err(ext4_err)
            }),
            None => fw_read(path, offset, buf),
        }
    }

    fn wlan_start(&mut self) -> Result<(), i32> {
        match hal(WLAN_START, core::ptr::null_mut(), core::ptr::null_mut()) {
            r if r < 0 => Err(r),
            _ => Ok(()),
        }
    }

    fn wlan_msa(&mut self, regions: &[modem_proto::wlfw::Region]) -> Result<(), i32> {
        for r in regions {
            let io = Io {
                a: 1,
                b: r.addr as u32,
                len: r.size,
                buf: r.secure as u32,
            };
            let e = hal_io(WLAN_MSA, io).0;
            if e < 0 {
                return Err(e);
            }
        }
        Ok(())
    }

    /// The board data on vendor_a: Pixel 3a XL's `firmware/bdwlan-bonito.bin` (the factory image also has -ROW,
    /// -USA, -JPN, -VZW regional variants and the 3a's -sargo set), else the generic names.
    /// shortcut: the board name and the default (non-regional) file are fixed; pick by device and region before release.
    fn board_data(&mut self, board_id: u32) -> Vec<u8> {
        let names = [
            String::from("firmware/bdwlan-bonito.bin"),
            alloc::format!("firmware/bdwlan.b{:02x}", board_id & 0xff),
            String::from("firmware/bdwlan.bin"),
        ];
        for n in &names {
            let got = vendor_file(n, |fs, f| {
                if f.size > 0x10_0000 {
                    return Err(-9); // KErrOverflow: a board file is tens of KB
                }
                fs.read(f).map_err(ext4_err)
            });
            match got {
                Ok(d) => {
                    dprintln!("[modemd] wlfw: board data from {n}");
                    return d;
                }
                Err(e) => dprintln!("[modemd] wlfw: no {n} ({e})"),
            }
        }
        Vec::new()
    }
}

fn ext4_err(e: ext4ro::Error) -> i32 {
    match e {
        ext4ro::Error::Io(e) => e,
        ext4ro::Error::NotFound => -1, // KErrNotFound
        _ => -20,                      // KErrCorrupt
    }
}

fn fs_size(_: &mut ext4ro::Fs<Vendor>, f: &ext4ro::Inode) -> usize {
    f.size as usize
}

/// The vendor_a file the modem is reading, kept open between its TFTP blocks (one lookup per file, not per block)
struct OpenFile(core::cell::RefCell<Option<(String, ext4ro::Fs<Vendor>, ext4ro::Inode)>>);
// SAFETY: only the daemon's one thread serves TFTP (Platform calls come from its loop).
unsafe impl Sync for OpenFile {}
static OPEN: OpenFile = OpenFile(core::cell::RefCell::new(None));

fn vendor_file<T>(
    path: &str,
    f: impl FnOnce(&mut ext4ro::Fs<Vendor>, &ext4ro::Inode) -> Result<T, i32>,
) -> Result<T, i32> {
    let mut open = OPEN.0.borrow_mut();
    if open.as_ref().is_none_or(|o| o.0 != path) {
        *open = None;
        let mut fs = ext4ro::Fs::open(Vendor).map_err(ext4_err)?;
        let file = fs.find(path).map_err(ext4_err)?;
        *open = Some((String::from(path), fs, file));
    }
    let o = open.as_mut().ok_or(-1)?;
    f(&mut o.1, &o.2)
}

/// HAL FwRead: `path` ("IMAGE/WLANMDSP.MBN") as a NUL-terminated 32-byte buffer; an empty `buf` asks for the size
fn fw_read(path: &str, offset: usize, buf: &mut [u8]) -> Result<usize, i32> {
    let mut name = [0u8; 32];
    if path.len() >= name.len() {
        return Err(-6); // KErrArgument
    }
    name[..path.len()].copy_from_slice(path.as_bytes());
    let mut io = Io {
        a: 0,
        b: offset as u32,
        len: buf.len() as u32,
        buf: buf.as_mut_ptr() as u32,
    };
    match hal(FW_READ, (&mut io as *mut Io).cast(), name.as_mut_ptr()) {
        r if r < 0 => Err(r),
        r => Ok(r as usize),
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
        /// A watchdog failure leaves stop ownership with the kernel until a terminal acknowledgement.
        watchdog: Option<i32>,
    },
    Failed,
    /// Startup radio-disabled build: ADSP services remain active.
    Disabled,
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
    /// the audio DSP: serviced independently of radio preparation and failures
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
    audio_seen: Option<(Mode, Mode, bool)>,
    haptic_error: i32,
    /// a failed ringtone preparation is retried from here (ms)
    ring_retry_at: u64,
}

impl Daemon {
    fn status(&self) -> Status {
        let mut s = self.modem.as_ref().map_or(self.status, |m| m.status);
        s.audio = self.audio.status();
        s.revision = self.published.revision;
        match self.phase {
            Phase::Recover { watchdog, .. } => {
                s.modem = if let Some(error) = watchdog {
                    s.result = error;
                    modem_ipc::modem::FAILED
                } else {
                    modem_ipc::modem::RECOVERING
                };
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
        // shortcut: the ADSP is never stopped. Its TrustZone PAS shutdown never returned on the phone and froze it
        // (2026-11-12, three times: after a failed modem restart, then from Retry), so every recovery restarts the
        // modem only, whatever the caller asked for; a lost ADSP means no call audio until a reboot. Upgrade when
        // the PAS shutdown hang is understood.
        if mask & 2 != 0 {
            dprintln!(
                "[modemd] recovery: keeping the ADSP (asked mask {}); restarting the modem only",
                mask
            );
        }
        let mask = 1;
        // nothing more goes to the processors being stopped; the ADSP side survives for a modem-only stop
        if let Some(m) = &mut self.modem {
            m.lost();
        }
        if self.recovery_attempt >= 3 {
            self.phase = Phase::Failed;
            return;
        }
        let delay = [1000, 3000, 10_000][usize::from(self.recovery_attempt)];
        self.recovery_attempt += 1;
        self.dtmf_stop = None;
        self.healthy_since = 0;
        let haptic = hal_io(HAPTIC, Io::default()).0;
        if haptic < 0 {
            dprintln!(
                "[modemd t={}ms] recovery vibration stop failed ({})",
                time::millis(),
                haptic
            );
        }
        self.audio.apr.set(Mode::Off);
        self.alert_dismissed = None;
        self.waiting_alert = None;
        let _ = adsp::audio(2, 0);
        self.phase = Phase::Recover {
            mask,
            since: time::millis() + delay,
            issued: false,
            watchdog: None,
        };
        dprintln!(
            "[modemd t={}ms] recovery attempt {} mask {}",
            time::millis(),
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
            m.lost();
        }
        if self.modem.is_some() && self.recovery_attempt < 3 {
            // a modem failure restarts the modem only: the ADSP's TrustZone shutdown (PAS) never returned on the phone
            // and froze it (2026-11-12); only the ADSP's own faults may stop it
            self.recover(1);
        }
    }

    /// One step of the work: Err(KErrNotSupported) when this machine has no modem (QEMU/Syborg).
    fn step(&mut self) -> Result<(), i32> {
        self.adsp.step();
        self.run();
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
                    Err(ext4ro::Error::Io(e))
                        if e == err::KERR_NOT_SUPPORTED && !matches!(self.adsp, adsp::Adsp::Up) =>
                    {
                        return Err(e);
                    }
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
                if r == err::KERR_NOT_SUPPORTED && !matches!(self.adsp, adsp::Adsp::Up) {
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
            // (the top of step() already stepped the ADSP and serviced its rings)
            Phase::Adsp => {
                if !self.adsp.busy() {
                    if matches!(self.adsp, adsp::Adsp::Failed) {
                        // no audio, but the radio still starts (PR #2)
                        dprintln!("[modemd] ADSP unavailable; starting the radio without it");
                    }
                    if cfg!(feature = "radio-disabled") {
                        self.phase = Phase::Disabled;
                    } else if self.modem.as_ref().is_some_and(Modem::radio_attached) {
                        // a recovery restarted the ADSP: the radio storage is still attached, the modem boots again
                        self.boot_modem();
                    } else {
                        self.phase = Phase::Ipa;
                    }
                }
            }
            Phase::Wait(t0) => {
                let s = match state() {
                    Ok(s) => s,
                    Err(e) => {
                        self.fail("modem boot state", e);
                        return Ok(());
                    }
                };
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
            Phase::Recover {
                mask,
                since,
                issued,
                watchdog,
            } => {
                let now = time::millis();
                if now < since {
                    return Ok(());
                }
                if !issued {
                    // A previous worker may still own shutdown, or its state may be unreadable. Neither
                    // permits another stop request. Do not interpret a HAL read failure as an idle worker.
                    if !matches!(processor_stop_state(), Ok([0 | 2 | 3, _, _, _])) {
                        if watchdog.is_none()
                            && now.saturating_sub(since) >= recovery::STOP_WATCHDOG_MS
                        {
                            dprintln!(
                                "[modemd t={}ms] waiting for existing processor stop; quarantined",
                                now
                            );
                            self.phase = Phase::Recover {
                                mask,
                                since,
                                issued,
                                watchdog: Some(err::KERR_TIMED_OUT),
                            };
                        }
                        return Ok(());
                    }
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
                        watchdog: None,
                    };
                } else {
                    match recovery::stop_progress(processor_stop_state(), now.saturating_sub(since))
                    {
                        StopProgress::Pending(error) => {
                            if watchdog.is_none() && error.is_some() {
                                dprintln!(
                                    "[modemd t={}ms] processor stop watchdog {:?}; quarantined, still observing",
                                    now,
                                    error
                                );
                                self.phase = Phase::Recover {
                                    mask,
                                    since,
                                    issued,
                                    watchdog: error,
                                };
                            }
                        }
                        StopProgress::Failed(error) => {
                            self.fail("processor stop completion", error)
                        }
                        StopProgress::Complete(generations) => {
                            dprintln!(
                                "[modemd t={}ms] processor stop acknowledged mask {} generations {}/{} after {}ms",
                                now,
                                mask,
                                generations[0],
                                generations[1],
                                now.saturating_sub(since)
                            );
                            if let Some(m) = &mut self.modem {
                                m.reset(mask & 2 == 0);
                            }
                            self.audio = adsp::CallAudio::new();
                            // fatal_seen stays: a FATAL bit the kernel left set is not a new fault
                            if mask & 2 != 0 {
                                self.adsp = adsp::Adsp::Idle;
                                self.phase = Phase::Adsp;
                            } else {
                                self.boot_modem();
                            }
                        }
                    }
                }
            }
            Phase::Running | Phase::Failed | Phase::Disabled => {}
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
        if info.len < 0x20_0000 {
            self.fail("rmtfs carve-out size", err::KERR_ARGUMENT);
            return;
        }
        let images = core::mem::take(&mut self.images);
        if let Some(modem) = &mut self.modem {
            if let Err(e) = modem.attach_radio(images, info.b as u64) {
                self.fail("radio storage attachment", e);
                return;
            }
            modem.ipa_up = self.ipa_up;
        }
        self.boot_modem();
    }

    fn boot_modem(&mut self) {
        if cfg!(feature = "radio-disabled") {
            self.phase = Phase::Disabled;
            return;
        }
        let r = hal(BOOT, core::ptr::null_mut(), core::ptr::null_mut());
        if r < 0 {
            self.fail("modem boot start", r);
            return;
        }
        self.phase = Phase::Wait(time::millis());
    }

    /// Every step, in every phase (the ADSP serves audio before and without the radio, PR #2). The health checks
    /// and the watchdog only judge a running modem; during a recovery nothing is sent to the processors being stopped.
    fn run(&mut self) {
        if matches!(self.phase, Phase::Recover { .. }) {
            return;
        }
        let running = matches!(self.phase, Phase::Running);
        let st = if running {
            match state() {
                Ok(s) => s,
                Err(e) => {
                    dprintln!(
                        "[modemd t={}ms] modem state unavailable ({})",
                        time::millis(),
                        e
                    );
                    self.recover(3);
                    return;
                }
            }
        } else {
            [0; 4]
        };
        // FATAL counts on its rising edge, observed while running: a bit the kernel leaves set after a stop must not
        // restart the modem again and again (the latch keeps its value outside Running)
        if running && recovery::rising(&mut self.fatal_seen, st[2] & FATAL != 0) {
            let dsp = adsp::processor_state();
            let quiescent = self.audio.apr.quiescent();
            let mask = recovery::recovery_mask(dsp, quiescent);
            dprintln!(
                "[modemd t={}ms] modem fatal; ADSP {:?} APR quiescent {} -> mask {}",
                time::millis(),
                dsp,
                quiescent,
                mask
            );
            self.recover(mask);
            return;
        }
        if running && matches!(self.adsp, adsp::Adsp::Up) {
            // only an ADSP that was up can be lost: a missing one never takes the radio down (PR #2)
            let dsp = adsp::processor_state();
            if !matches!(dsp, Ok(s) if s[2] & FATAL == 0 && s[1] == 0) {
                dprintln!("[modemd t={}ms] ADSP unavailable {:?}", time::millis(), dsp);
                self.recover(3);
                return;
            }
            if self.audio.apr.failed_hard() {
                self.recover(3);
                return;
            }
        }
        let Some(modem) = &mut self.modem else {
            return;
        };
        let buf = &mut self.rx;
        for _ in 0..32 {
            if !matches!(self.phase, Phase::Running) {
                break;
            }
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
        for _ in 0..32 {
            let Some(n) = adsp::recv(adsp::RING_QRTR, buf) else {
                break;
            };
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
                let r = hal_io(
                    HAPTIC,
                    Io {
                        a: 700,
                        b: 70,
                        ..Io::default()
                    },
                )
                .0;
                if self.haptic_at == 0 || r != self.haptic_error {
                    dprintln!("[modemd t={}ms] incoming vibration start -> {}", now, r);
                    self.haptic_error = r;
                }
                self.haptic_at = now + 1500;
            }
        } else if self.haptic_at != 0 {
            let r = hal_io(HAPTIC, Io::default()).0;
            dprintln!("[modemd t={}ms] incoming vibration stop -> {}", now, r);
            self.haptic_error = r;
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
        // a minute of a *running* modem earns new recovery attempts: run() also runs while a failing modem boots,
        // and counting that time reset the attempts and escalated to stopping the ADSP (froze the phone 2026-11-12)
        if running {
            if self.healthy_since == 0 {
                self.healthy_since = now;
            }
            if now.saturating_sub(self.healthy_since) >= 60_000 {
                self.recovery_attempt = 0;
            }
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
        let ring_ready = if ring_wanted
            && self.audio.apr.is() == Mode::Off
            && self.audio.apr.idle()
            && now >= self.ring_retry_at
        {
            match self.audio.prepare_ring() {
                Ok(()) => true,
                Err(e) => {
                    // the vibration still alerts; the tone is tried again in 2 s, not every step
                    modem.status.result = e;
                    self.ring_retry_at = now + 2000;
                    false
                }
            }
        } else {
            ring_wanted
        };
        let wanted = match (
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
        };
        let audio = (wanted, self.audio.apr.is(), self.audio.apr.quiescent());
        if self.audio_seen != Some(audio) {
            dprintln!(
                "[modemd t={}ms] audio wanted {:?} current {:?} quiescent {}",
                now,
                audio.0,
                audio.1,
                audio.2
            );
            self.audio_seen = Some(audio);
        }
        self.audio.apr.set(wanted);
        if matches!(self.adsp, adsp::Adsp::Up) {
            self.audio.pump(buf);
        }
        if self.dtmf_stop.is_some_and(|t| time::millis() >= t) {
            self.dtmf_stop = None;
            let _ = modem.dtmf(&mut Hal, None);
        }
        let now = time::millis();
        modem.tick(&mut Hal, now);
        if matches!(self.phase, Phase::Running) && now - self.last_log >= STATUS_LOG_MS {
            self.last_log = now;
            let s = st;
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
        if running && modem.watchdog() {
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
        let audio_op = matches!(
            msg.function(),
            op::MUTE | op::SPEAKER | op::VOLUME | op::AUDIO_TEST
        );
        if !matches!(
            msg.function(),
            op::STATUS | op::CANCEL_NOTIFY | op::RETRY | op::RING_SETTINGS | op::AIRPLANE
        ) && !matches!(self.phase, Phase::Running)
            && !(audio_op && matches!(self.adsp, adsp::Adsp::Up))
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
                if !matches!(self.phase, Phase::Failed)
                    || self.modem.is_none()
                    || !matches!(processor_stop_state(), Ok([0 | 2 | 3, _, _, _]))
                {
                    err::KERR_IN_USE
                } else {
                    self.recovery_attempt = 0;
                    self.recover(1); // the modem only (recover() never stops the ADSP)
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
            let haptic = hal_io(HAPTIC, Io::default()).0;
            dprintln!(
                "[modemd t={}ms] answered/ended vibration stop -> {}",
                time::millis(),
                haptic
            );
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
    dprintln!("[modemd] start: serving NeoModem, starting ADSP before radio preparation");
    let mut d = Daemon {
        phase: Phase::Adsp,
        images: Default::default(),
        modem: Some({
            let mut router = Modem::support_only();
            router.radio_enabled = !cfg!(feature = "radio-disabled");
            router.trace = cfg!(feature = "qrtr-trace"); // every QRTR packet in the log (Wi-Fi W0)
            router
        }),
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
        audio_seen: None,
        haptic_error: 0,
        ring_retry_at: 0,
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
    // the modem exists from startup (ADSP service first), so the saved intent goes to it before any radio work
    if let Some(m) = &mut d.modem {
        m.status.airplane = d.status.airplane;
        m.status.ring = d.status.ring;
    }
    d.publish();
    rt::block_on(async {
        let mut receive = pin!(server.receive());
        let mut pump =
            modem_proto::schedule::PumpDeadline::new(time::millis(), (LOOP_US / 1000) as u64);
        loop {
            if pump.due(time::millis()) {
                if let Err(e) = d.step() {
                    dprintln!("[modemd] no modem on this machine ({}): exiting", e);
                    return e;
                }
                pump.completed(time::millis());
            }
            // one step every 10 ms: the rings are polled (e32 delays are in microseconds; the old "20 ms / 2 s"
            // were 20 us / 2 ms, C6 2026-11-09)
            match rt::select(
                receive.as_mut(),
                time::delay(pump.wait_us(time::millis()).max(1)),
            )
            .await
            {
                Either::Left(Ok(msg)) => {
                    receive.set(server.receive());
                    let _ = server::dispatch(&mut d, msg);
                }
                Either::Left(Err(e)) => {
                    dprintln!("[modemd] receive failed ({})", e);
                    receive.set(server.receive());
                }
                Either::Right(()) => {}
            }
            d.publish();
        }
    })
}

e32::main!(main);
