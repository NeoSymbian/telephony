// SPDX-License-Identifier: Apache-2.0
//! The ADSP, the audio DSP (docs/telephony.md C6): its firmware from vendor_a streamed to the kernel, which has
//! TrustZone authenticate and start it (HAL AdspLoad, PAS 1), then its two GLINK channels: IPCRTR (QRTR, the same
//! router as the modem's: the audio PD asks our locator for avs/audio) and APR (the audio services).
//! The load runs in steps of the daemon's loop (256 KB each), so NeoModem and the modem keep being served.

use crate::{Io, NOT_READY, Vendor, hal_io};
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;
use e32::{dprintln, time};

const ADSP_LOAD: i32 = 0x4e14;
const ADSP_STATE: i32 = 0x4e15;
pub const CHAN_SEND: i32 = 0x4e16;
pub const CHAN_RECV: i32 = 0x4e17;
/// the kernel's receive rings: the ADSP's IPCRTR and APR
pub const RING_QRTR: u32 = 1;
pub const RING_APR: u32 = 2;
const CHUNK: usize = 0x1_0000;
const STEP_BYTES: usize = 0x4_0000;
const BOOT_TIMEOUT_MS: u64 = 15_000;
/// AdspState step once the ADSP runs and its channels are open (qcom_adsp.cpp)
const STEP_RUNNING: u32 = 3;

/// (p_type, p_filesz, p_memsz, p_flags) of a program header
type Phdr = (u32, u32, u32, u32);

pub struct Load {
    fs: ext4ro::Fs<Vendor>,
    phdrs: Vec<Phdr>,
    seg: usize,
    file: Option<ext4ro::Inode>,
    offset: u64,
    buf: Vec<u8>,
    bytes: u64,
}

pub enum Adsp {
    Idle,
    Loading(alloc::boxed::Box<Load>),
    /// authenticated; waiting for ready and the channels, since t0
    Booting(u64),
    Up,
    Failed,
}

fn adsp_load(op: u32, seg: usize, offset: u32, data: &[u8]) -> i32 {
    hal_io(
        ADSP_LOAD,
        Io {
            a: op | (seg as u32) << 8,
            b: offset,
            len: data.len() as u32,
            buf: data.as_ptr() as u32,
        },
    )
    .0
}

/// step, error, SMP2P slave-kernel bits, open channels (bit 0 IPCRTR, bit 1 APR)
fn state() -> [u32; 4] {
    let mut s = [0u32; 4];
    crate::hal(ADSP_STATE, s.as_mut_ptr().cast(), core::ptr::null_mut());
    s
}

/// The program headers of an ELF32 .mdt (only what the load needs).
fn phdrs(mdt: &[u8]) -> Option<Vec<Phdr>> {
    let w = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(mdt.get(at..at + 4)?.try_into().ok()?))
    };
    if mdt.get(..4)? != b"\x7fELF" {
        return None;
    }
    let (phoff, phnum) = (
        w(28)? as usize,
        u16::from_le_bytes(mdt.get(44..46)?.try_into().ok()?) as usize,
    );
    (0..phnum)
        .map(|i| {
            let at = phoff + 32 * i;
            Some((w(at)?, w(at + 16)?, w(at + 20)?, w(at + 24)?))
        })
        .collect()
}

/// What the kernel copies: PT_LOAD, not the hash segment, with file data (its zero tail the kernel fills).
fn loadable(p: &Phdr) -> bool {
    p.0 == 1 && p.3 & (7 << 24) != 2 << 24 && p.1 > 0 && p.2 > 0
}

impl Adsp {
    /// The .mdt to the kernel (init_image); then the segments follow in steps.
    fn begin() -> Result<Load, i32> {
        let io = |e: ext4ro::Error| match e {
            ext4ro::Error::Io(e) => e,
            _ => -20, // KErrCorrupt
        };
        let mut fs = ext4ro::Fs::open(Vendor).map_err(io)?;
        let mdt = fs.find("firmware/adsp.mdt").map_err(io)?;
        let mdt = fs.read(&mdt).map_err(io)?;
        let phdrs = phdrs(&mdt).ok_or(-20)?;
        let r = adsp_load(0, 0, 0, &mdt);
        if r < 0 {
            return Err(r);
        }
        Ok(Load {
            fs,
            phdrs,
            seg: 0,
            file: None,
            offset: 0,
            buf: vec![0u8; CHUNK],
            bytes: 0,
        })
    }

    /// One step: start, a slice of the segments, or the wait for the ADSP to come up.
    pub fn step(&mut self) {
        match self {
            Adsp::Idle => match Self::begin() {
                Ok(l) => {
                    dprintln!(
                        "[modemd] adsp: loading the firmware ({} program headers)",
                        l.phdrs.len()
                    );
                    *self = Adsp::Loading(alloc::boxed::Box::new(l));
                }
                Err(e) => {
                    dprintln!("[modemd] adsp: no start ({})", e);
                    *self = Adsp::Failed;
                }
            },
            Adsp::Loading(l) => match l.more() {
                Ok(false) => {}
                Ok(true) => {
                    let r = adsp_load(2, 0, 0, &[]);
                    dprintln!(
                        "[modemd] adsp: {} bytes loaded; authenticate and start -> {}",
                        l.bytes,
                        r
                    );
                    *self = if r < 0 {
                        Adsp::Failed
                    } else {
                        Adsp::Booting(time::millis())
                    };
                }
                Err(e) => {
                    dprintln!(
                        "[modemd] adsp: load failed at segment {} offset {} ({})",
                        l.seg,
                        l.offset,
                        e
                    );
                    *self = Adsp::Failed;
                }
            },
            Adsp::Booting(t0) => {
                let s = state();
                if s[1] != 0 {
                    dprintln!(
                        "[modemd] adsp: boot failed (step {}, error {})",
                        s[0],
                        s[1] as i32
                    );
                    *self = Adsp::Failed;
                } else if s[0] >= STEP_RUNNING && s[3] == 3 {
                    dprintln!(
                        "[modemd] adsp: running after {} ms; IPCRTR and APR open",
                        time::millis() - *t0
                    );
                    *self = Adsp::Up;
                } else if time::millis() - *t0 > BOOT_TIMEOUT_MS {
                    dprintln!(
                        "[modemd] adsp: not up after 15 s (step {}, slave-kernel {:#x})",
                        s[0],
                        s[2]
                    );
                    *self = Adsp::Failed;
                }
            }
            Adsp::Up | Adsp::Failed => {}
        }
    }

    /// Loading: the daemon polls fast.
    pub fn busy(&self) -> bool {
        matches!(self, Adsp::Idle | Adsp::Loading(_) | Adsp::Booting(_))
    }
}

impl Load {
    /// Up to STEP_BYTES more of the segments; true when all are in.
    fn more(&mut self) -> Result<bool, i32> {
        let io = |e: ext4ro::Error| match e {
            ext4ro::Error::Io(e) => e,
            _ => -20,
        };
        let mut sent = 0;
        while sent < STEP_BYTES {
            while self.seg < self.phdrs.len() && !loadable(&self.phdrs[self.seg]) {
                self.seg += 1;
            }
            if self.seg >= self.phdrs.len() {
                return Ok(true);
            }
            let filesz = self.phdrs[self.seg].1 as u64;
            if self.file.is_none() {
                let f = self
                    .fs
                    .find(&format!("firmware/adsp.b{:02}", self.seg))
                    .map_err(io)?;
                if f.size < filesz {
                    return Err(-20);
                }
                self.file = Some(f);
            }
            let f = self.file.clone().ok_or(NOT_READY)?;
            let want = (filesz - self.offset).min(CHUNK as u64) as usize;
            let n = self
                .fs
                .read_at(&f, self.offset, &mut self.buf[..want])
                .map_err(io)?;
            if n != want {
                return Err(-20);
            }
            let r = adsp_load(1, self.seg, self.offset as u32, &self.buf[..n]);
            if r < 0 {
                return Err(r);
            }
            self.offset += n as u64;
            self.bytes += n as u64;
            sent += n;
            if self.offset >= filesz {
                self.seg += 1;
                self.file = None;
                self.offset = 0;
            }
        }
        Ok(false)
    }
}

/// One message from kernel ring `ring` into `buf`: its length, or None when the ring is empty.
pub fn recv(ring: u32, buf: &mut [u8]) -> Option<usize> {
    let (r, _) = hal_io(
        CHAN_RECV,
        Io {
            a: ring,
            b: 0,
            len: buf.len() as u32,
            buf: buf.as_mut_ptr() as u32,
        },
    );
    if r == NOT_READY {
        return None;
    }
    if r < 0 {
        dprintln!("[modemd] adsp: receive on ring {} failed ({})", ring, r);
        return None;
    }
    Some(r as usize)
}

/// One message to the ADSP on ring `ring` (KErrNotReady until it gave a receive intent: retried briefly).
pub fn send(ring: u32, data: &[u8]) -> Result<(), i32> {
    for _ in 0..20 {
        let (r, _) = hal_io(
            CHAN_SEND,
            Io {
                a: ring,
                b: 0,
                len: data.len() as u32,
                buf: data.as_ptr() as u32,
            },
        );
        match r {
            NOT_READY => time::sleep(50),
            r if r < 0 => return Err(r),
            _ => return Ok(()),
        }
    }
    Err(NOT_READY)
}

const AUDIO: i32 = 0x4e18;

/// HAL Audio (the kernel's board codecs, a64_audio.cpp): 1 start(route), 2 stop, 3 route, 4 volume, 5 mic mute.
pub fn audio(op: u32, arg: u32) -> i32 {
    hal_io(
        AUDIO,
        Io {
            a: op,
            b: arg,
            len: 0,
            buf: 0,
        },
    )
    .0
}

/// The call's audio (C6): the APR sequence (modem_proto::apr) with its codecs step, and the buttons' settings.
pub struct CallAudio {
    pub apr: modem_proto::apr::Audio,
    pub muted: bool,
    pub speaker: bool,
    pub volume: u8,
}

impl CallAudio {
    pub fn new() -> Self {
        CallAudio {
            apr: Default::default(),
            muted: false,
            speaker: false,
            volume: 10,
        }
    }

    /// Answers from the ADSP, then whatever the sequence does next (a packet out, or the codecs).
    pub fn pump(&mut self, buf: &mut [u8]) {
        let mut log = |a: core::fmt::Arguments| dprintln!("[modemd] {}", a);
        while let Some(n) = recv(RING_APR, buf) {
            self.apr.handle(&buf[..n], &mut log);
        }
        loop {
            match self.apr.poll(time::millis(), &mut log) {
                modem_proto::apr::Out::Send(p) => {
                    if let Err(e) = send(RING_APR, &p) {
                        dprintln!("[modemd] apr: send failed ({})", e);
                    }
                    return; // its answer comes on a later step
                }
                modem_proto::apr::Out::Codecs(on) => {
                    let r = if on {
                        audio(4, self.volume as u32);
                        audio(5, self.muted as u32);
                        audio(1, self.speaker as u32)
                    } else {
                        audio(2, 0)
                    };
                    dprintln!(
                        "[modemd] audio: codecs {} -> {}",
                        if on { "on" } else { "off" },
                        r
                    );
                    self.apr.codecs_done(r >= 0, &mut log);
                }
                modem_proto::apr::Out::Nothing => return,
            }
        }
    }

    /// The sequence is mid-way: the daemon polls fast.
    pub fn busy(&self) -> bool {
        !self.apr.idle()
    }

    pub fn status(&self) -> modem_proto::modem_ipc::Audio {
        modem_proto::modem_ipc::Audio {
            on: self.apr.running,
            muted: self.muted,
            speaker: self.speaker,
            volume: self.volume,
        }
    }
}
