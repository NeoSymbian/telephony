// SPDX-License-Identifier: Apache-2.0
//! APR, the ADSP's packet protocol (docs/telephony.md C6), and the call-audio sequence over it: the TDM ports the
//! earpiece/speaker amps and the mic sit on, then a voice path the modem's call audio flows through. Each GLINK
//! message on "apr_audio_svc" is one APR packet: a 20-byte header {hdr_field, pkt_size, src svc/domain/port,
//! dest svc/domain/port, token, opcode} and the payload. Commands are acked by APR_BASIC_RSP_RESULT {opcode, status}.
//! Facts (opcodes, layouts, the C1 values) from Linux apr.c, q6core.c, q6afe.c, q6afe-dai.c and the sdm670 q6voice
//! (q6mvm.c, q6cvp.c, q6voice.c), as the phone ran them under C1 Linux; C1's uninitialised bytes are zeros here and
//! its media-format channel-map bug is not copied.
//! One command at a time: the next goes out when the last is acked (or timed out). The codecs are powered between
//! the ports and the voice path (Out::Codecs), because the amps' PLLs lock to the TDM bit clock.

use alloc::vec::Vec;

const DOMAIN_ADSP: u8 = 4;
const DOMAIN_APPS: u8 = 5;
pub const SVC_CORE: u8 = 3;
pub const SVC_AFE: u8 = 4;
pub const SVC_MVM: u8 = 9;
pub const SVC_CVP: u8 = 0x0b;
const HDR_FIELD_SEQ_CMD: u16 = 0x0250;
const HDR: usize = 20;
pub const BASIC_RSP_RESULT: u32 = 0x0001_10e8;

const CORE_GET_STATE: u32 = 0x0001_290c;
const CORE_GET_STATE_RSP: u32 = 0x0001_290d;
const AFE_DEVICE_START: u32 = 0x0001_00e5;
const AFE_DEVICE_STOP: u32 = 0x0001_00e6;
const AFE_SET_PARAM_V2: u32 = 0x0001_00ef;
const AFE_SVC_SET_PARAM: u32 = 0x0001_00f3;
const MVM_CREATE_PASSIVE: u32 = 0x0001_10ff;
const MVM_DUAL_CONTROL: u32 = 0x0001_1327;
const MVM_ATTACH: u32 = 0x0001_123e;
const MVM_DETACH: u32 = 0x0001_123f;
const MVM_START: u32 = 0x0001_1190;
const MVM_STOP: u32 = 0x0001_1192;
const CVP_CREATE_V3: u32 = 0x0001_3169;
const CVP_SET_PARAM_V2: u32 = 0x0001_133d;
const CVP_TOPOLOGY_COMMIT: u32 = 0x0001_3198;
const CVP_ENABLE: u32 = 0x0001_00c6;
const CVP_DISABLE: u32 = 0x0001_10e1;
const DESTROY_SESSION: u32 = 0x0001_003c;

/// The ports: PRIMARY_TDM_TX_0 (the mic, RT5514) and SECONDARY_TDM_RX_0 (the amps), (AFE index, port id, clock id)
pub const MIC: (u16, u16, u32) = (25, 0x9001, 0x200);
pub const SPK: (u16, u16, u32) = (40, 0x9010, 0x202);
const BCLK_HZ: u32 = 6_144_000; // 48 kHz x 8 slots x 16 bits
/// VoiceMMode1: our APR port for the voice sessions, and the modem's session name
const VOICE_PATH: u16 = 6;
const VOICE_SESSION: &[u8] = b"11C05000";
/// the ADSP's built-in voice topologies (TX: single-mic echo cancellation + noise suppression, RX: default), the voice
/// driver's defaults. C1's UCM ids (0x1000fffc, 0x10010f87) commit under Linux but not here (status 1, even with
/// Linux's exact packets, C6 2026-11-09); these commit, enable and attach.
const TOPOLOGY: (u32, u32) = (0x0001_0f71, 0x0001_0f77);
/// Linux waits 3 s for an AFE answer; a voice start can take a while too
const ACK_TIMEOUT_MS: u64 = 3000;
const CORE_TRIES: u32 = 30;

/// One APR command packet (sent with src_svc = dest_svc = `svc`, as Linux's apr_send_pkt).
pub fn packet(
    svc: u8,
    src_port: u16,
    dest_port: u16,
    token: u32,
    opcode: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut p = Vec::with_capacity(HDR + payload.len());
    p.extend_from_slice(&HDR_FIELD_SEQ_CMD.to_le_bytes());
    p.extend_from_slice(&((HDR + payload.len()) as u16).to_le_bytes());
    p.extend_from_slice(&[svc, DOMAIN_APPS]);
    p.extend_from_slice(&src_port.to_le_bytes());
    p.extend_from_slice(&[svc, DOMAIN_ADSP]);
    p.extend_from_slice(&dest_port.to_le_bytes());
    p.extend_from_slice(&token.to_le_bytes());
    p.extend_from_slice(&opcode.to_le_bytes());
    p.extend_from_slice(payload);
    p
}

/// An inbound packet: its source service and port (a new session's handle), token, opcode and payload.
#[derive(Debug, PartialEq, Eq)]
pub struct Rsp<'a> {
    pub svc: u8,
    pub src_port: u16,
    pub token: u32,
    pub opcode: u32,
    pub payload: &'a [u8],
}

impl<'a> Rsp<'a> {
    pub fn parse(p: &'a [u8]) -> Option<Self> {
        let u16at = |at: usize| Some(u16::from_le_bytes(p.get(at..at + 2)?.try_into().ok()?));
        let u32at = |at: usize| Some(u32::from_le_bytes(p.get(at..at + 4)?.try_into().ok()?));
        let hdr_len = ((u16at(0)? >> 4) & 0xf) as usize * 4;
        if hdr_len < HDR || u16at(2)? as usize != p.len() || p.len() < hdr_len {
            return None;
        }
        Some(Rsp {
            svc: p[4],
            src_port: u16at(6)?,
            token: u32at(12)?,
            opcode: u32at(16)?,
            payload: &p[hdr_len..],
        })
    }

    /// APR_BASIC_RSP_RESULT: (the command's opcode, status; 0 = ok)
    pub fn result(&self) -> Option<(u32, u32)> {
        let w = |at: usize| {
            Some(u32::from_le_bytes(
                self.payload.get(at..at + 4)?.try_into().ok()?,
            ))
        };
        (self.opcode == BASIC_RSP_RESULT).then(|| Some((w(0)?, w(4)?)))?
    }
}

fn le32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn le16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// The in-band set-param header {address 0, mem handle 0, module, param, size, 0} and the data.
fn param(module: u32, param: u32, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    for _ in 0..3 {
        le32(&mut v, 0);
    }
    le32(&mut v, module);
    le32(&mut v, param);
    le16(&mut v, data.len() as u16);
    le16(&mut v, 0);
    v.extend_from_slice(data);
    v
}

/// AFE CLOCK_SET: the port's TDM bit clock (the ADSP is the clock master), attribute INVERT_COUPLE_NO as q6afe-dai.
pub fn afe_clock(port: (u16, u16, u32), on: bool) -> Vec<u8> {
    let mut d = Vec::new();
    le32(&mut d, 1);
    le32(&mut d, port.2);
    le32(&mut d, if on { BCLK_HZ } else { 0 });
    le16(&mut d, 4);
    le16(&mut d, 0);
    le32(&mut d, on as u32);
    let mut pl = Vec::new();
    le32(&mut pl, 12 + d.len() as u32);
    pl.extend_from_slice(&param(0x0001_028f, 0x0001_0290, &d)); // after the u32 size: lsw, msw, handle, module...
    packet(SVC_AFE, 0, 0, port.0 as u32, AFE_SVC_SET_PARAM, &pl)
}

fn afe_port_param(port: (u16, u16, u32), module: u32, id: u32, data: &[u8]) -> Vec<u8> {
    let mut pl = Vec::new();
    le16(&mut pl, port.1);
    le16(&mut pl, 12 + data.len() as u16);
    pl.extend_from_slice(&param(module, id, data));
    packet(SVC_AFE, 0, 0, port.0 as u32, AFE_SET_PARAM_V2, &pl)
}

/// TDM config: 2 channels, 48 kHz, 16-bit, short sync from the ADSP, 8 slots x 16 bits, slots 0-1, data delay 1
pub fn afe_tdm_config(port: (u16, u16, u32)) -> Vec<u8> {
    let mut d = Vec::new();
    for x in [1u32, 2, 48_000, 16] {
        le32(&mut d, x);
    }
    for x in [0u16, 0, 1, 8, 0, 0, 1, 16] {
        le16(&mut d, x);
    }
    le32(&mut d, 0x3);
    afe_port_param(port, 0x0001_020c, 0x0001_029d, &d)
}

/// Slot mapping: channel offsets {0, 4} as C1's machine driver (upstream sdm845 too), the rest unused
pub fn afe_slot_map(port: (u16, u16, u32)) -> Vec<u8> {
    let mut d = Vec::new();
    le32(&mut d, 1);
    le16(&mut d, 2);
    le16(&mut d, 16);
    le32(&mut d, 0);
    for x in [0u16, 4, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff] {
        le16(&mut d, x);
    }
    afe_port_param(port, 0x0001_028a, 0x0001_0297, &d)
}

/// AFE loopback (Dev tools' audio test): the mic's TX port fed straight to the amps' RX port, routing mode default.
/// Ids from Android's msm q6afe (AFE_MODULE_LOOPBACK, AFE_PARAM_ID_LOOPBACK_CONFIG v1): not seen in a capture yet.
pub fn afe_loopback(on: bool) -> Vec<u8> {
    let mut d = Vec::new();
    le32(&mut d, 1);
    le16(&mut d, SPK.1);
    le16(&mut d, 1);
    le16(&mut d, on as u16);
    le16(&mut d, 0);
    afe_port_param(MIC, 0x0001_0205, 0x0001_020b, &d)
}

pub fn afe_start(port: (u16, u16, u32), on: bool) -> Vec<u8> {
    let mut pl = Vec::new();
    le16(&mut pl, port.1);
    le16(&mut pl, 0);
    packet(
        SVC_AFE,
        0,
        0,
        port.0 as u32,
        if on {
            AFE_DEVICE_START
        } else {
            AFE_DEVICE_STOP
        },
        &pl,
    )
}

fn voice(svc: u8, handle: u16, opcode: u32, payload: &[u8]) -> Vec<u8> {
    packet(svc, VOICE_PATH, handle, 0, opcode, payload)
}

pub fn mvm_create() -> Vec<u8> {
    let mut name = [0u8; 20];
    name[..VOICE_SESSION.len()].copy_from_slice(VOICE_SESSION);
    voice(SVC_MVM, 0, MVM_CREATE_PASSIVE, &name)
}

pub fn cvp_create() -> Vec<u8> {
    let mut pl = Vec::new();
    le16(&mut pl, 2); // RX and TX
    le16(&mut pl, MIC.1);
    le32(&mut pl, TOPOLOGY.0);
    le16(&mut pl, SPK.1);
    le32(&mut pl, TOPOLOGY.1);
    le32(&mut pl, 0x0001_135e); // no network calibration
    le32(&mut pl, 0x0001_0f7c); // echo cancellation, internal mixing
    le16(&mut pl, 0xffff); // no echo reference port
    pl.extend_from_slice(&[0; 20]);
    voice(SVC_CVP, 0, CVP_CREATE_V3, &pl)
}

/// CVP set-param (module CVD generic): channel info or endpoint media format, RX (2 ch FL FR) or TX (1 ch FC)
pub fn cvp_param(handle: u16, tx: bool, media: bool) -> Vec<u8> {
    let (ch, map): (u16, &[u8]) = if tx { (1, &[3]) } else { (2, &[1, 2]) };
    let mut d = Vec::new();
    let id = if media {
        le32(&mut d, if tx { MIC.1 } else { SPK.1 } as u32);
        le16(&mut d, ch);
        le16(&mut d, 16);
        le32(&mut d, 48_000);
        if tx { 0x0001_3253 } else { 0x0001_3254 }
    } else {
        le32(&mut d, ch as u32);
        le32(&mut d, 16);
        if tx { 0x0001_328e } else { 0x0001_328f }
    };
    let mut m = [0u8; 8];
    m[..map.len()].copy_from_slice(map);
    d.extend_from_slice(&m);
    let mut pl = Vec::new();
    le32(&mut pl, 0); // in-band
    pl.extend_from_slice(&[0; 8]); // address
    le32(&mut pl, 12 + d.len() as u32);
    le32(&mut pl, 0x0001_316e); // VSS_MODULE_CVD_GENERIC
    le32(&mut pl, id);
    le16(&mut pl, d.len() as u16);
    le16(&mut pl, 0);
    pl.extend_from_slice(&d);
    voice(SVC_CVP, handle, CVP_SET_PARAM_V2, &pl)
}

/// One step of the sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cmd {
    CoreReady,
    Clock((u16, u16, u32)),
    TdmConfig((u16, u16, u32)),
    SlotMap((u16, u16, u32)),
    Port((u16, u16, u32), bool),
    /// not APR: the daemon powers the codecs (true) or stops them
    Codecs(bool),
    MvmCreate,
    DualControl,
    CvpCreate,
    CvpParam(bool, bool),
    TopologyCommit,
    CvpEnable(bool),
    Attach(bool),
    Voice(bool),
    Destroy(u8),
    Loopback(bool),
}

/// What the daemon does next.
#[derive(Debug, PartialEq, Eq)]
pub enum Out {
    /// this packet to the ADSP's APR channel
    Send(Vec<u8>),
    /// power the codecs and amps (true) or stop them, then call `codecs_done`
    Codecs(bool),
    Nothing,
}

/// What the audio runs: nothing, a call's voice path, or Dev tools' loopback test.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Off,
    Call,
    Test,
}

/// The audio state machine. Callers only say what they want ([`Audio::set`]); a running sequence is never cut
/// short: once it ends, the current mode is stopped and then the wanted one started. Cutting a stop short left a port
/// running that the next start started again, and the ADSP's AFE stopped answering (C6 2026-11-09).
#[derive(Default)]
pub struct Audio {
    queue: Vec<Cmd>,
    at: usize,
    /// the command in flight: (its opcode or 0 for Codecs, sent at)
    waiting: Option<(u32, u64)>,
    core_tries: u32,
    mvm: u16,
    cvp: u16,
    want: Mode,
    /// the mode the last queued sequence leads to
    is: Mode,
    pub failures: u32,
}

impl Audio {
    pub fn set(&mut self, want: Mode) {
        self.want = want;
    }

    /// The mode running (or being started); Off while stopping.
    pub fn is(&self) -> Mode {
        self.is
    }

    /// Queue the start of `m`: a call in the order of Linux's working call (C6 capture 2026-11-09: the mic's clock
    /// and port, the speaker's clock, the voice sessions and START_VOICE, then the speaker's port, then the codecs:
    /// the amps' PLL locks to the speaker port's bit clock); the test with both ports, the codecs, then the loopback.
    fn begin(&mut self, m: Mode) {
        use Cmd::*;
        let ports = [
            Clock(MIC),
            TdmConfig(MIC),
            SlotMap(MIC),
            Port(MIC, true),
            Clock(SPK),
        ];
        let spk = [TdmConfig(SPK), SlotMap(SPK), Port(SPK, true), Codecs(true)];
        let mut q = alloc::vec![CoreReady];
        q.extend(ports);
        match m {
            Mode::Call => {
                q.extend([
                    MvmCreate,
                    DualControl,
                    CvpCreate,
                    CvpParam(false, false),
                    CvpParam(true, false),
                    CvpParam(false, true),
                    CvpParam(true, true),
                    TopologyCommit,
                    CvpEnable(true),
                    Attach(true),
                    Voice(true),
                ]);
                q.extend(spk);
            }
            Mode::Test => {
                q.extend(spk);
                q.push(Loopback(true));
            }
            Mode::Off => q.clear(),
        }
        self.queue = q;
        self.reset_queue();
        self.core_tries = 0;
        self.is = m;
    }

    /// Queue the stop of what runs: a call as Linux (the codecs, the mic's port, voice off, the speaker's port, the
    /// sessions destroyed MVM then CVP); the test in reverse. The clocks stay on (Linux never turns them off).
    fn end(&mut self) {
        use Cmd::*;
        let mut q = alloc::vec![];
        match self.is {
            Mode::Call => {
                q.extend([
                    Codecs(false),
                    Port(MIC, false),
                    Voice(false),
                    Attach(false),
                    CvpEnable(false),
                    Port(SPK, false),
                ]);
                if self.mvm != 0 {
                    q.push(Destroy(SVC_MVM));
                }
                if self.cvp != 0 {
                    q.push(Destroy(SVC_CVP));
                }
            }
            // the loopback's destination first: with the mic's port stopped first, the speaker port's stop was never
            // answered and the ADSP's AFE stopped answering (C6 2026-11-09)
            Mode::Test => q.extend([
                Loopback(false),
                Codecs(false),
                Port(SPK, false),
                Port(MIC, false),
            ]),
            Mode::Off => {}
        }
        self.queue = q;
        self.reset_queue();
        self.is = Mode::Off;
    }

    fn reset_queue(&mut self) {
        self.at = 0;
        self.waiting = None;
        self.failures = 0;
    }

    pub fn idle(&self) -> bool {
        self.at >= self.queue.len() && self.waiting.is_none()
    }

    fn packet(&self, c: Cmd) -> Vec<u8> {
        use Cmd::*;
        match c {
            CoreReady => packet(SVC_CORE, 0, 0, 0, CORE_GET_STATE, &[]),
            Clock(p) => afe_clock(p, true),
            TdmConfig(p) => afe_tdm_config(p),
            SlotMap(p) => afe_slot_map(p),
            Port(p, on) => afe_start(p, on),
            MvmCreate => mvm_create(),
            DualControl => voice(SVC_MVM, self.mvm, MVM_DUAL_CONTROL, &[1]),
            CvpCreate => cvp_create(),
            CvpParam(tx, media) => cvp_param(self.cvp, tx, media),
            TopologyCommit => voice(SVC_CVP, self.cvp, CVP_TOPOLOGY_COMMIT, &[]),
            CvpEnable(on) => voice(
                SVC_CVP,
                self.cvp,
                if on { CVP_ENABLE } else { CVP_DISABLE },
                &[],
            ),
            Attach(on) => voice(
                SVC_MVM,
                self.mvm,
                if on { MVM_ATTACH } else { MVM_DETACH },
                &self.cvp.to_le_bytes(),
            ),
            Voice(on) => voice(
                SVC_MVM,
                self.mvm,
                if on { MVM_START } else { MVM_STOP },
                &[],
            ),
            Destroy(svc) => voice(
                svc,
                if svc == SVC_MVM { self.mvm } else { self.cvp },
                DESTROY_SESSION,
                &[],
            ),
            Loopback(on) => afe_loopback(on),
            Codecs(_) => Vec::new(),
        }
    }

    /// The next thing to do at `now` (ms); `log` hears about failures and timeouts.
    pub fn poll(&mut self, now: u64, log: &mut dyn FnMut(core::fmt::Arguments)) -> Out {
        if let Some((op, t)) = self.waiting {
            if now.saturating_sub(t) < ACK_TIMEOUT_MS {
                return Out::Nothing;
            }
            log(format_args!("apr: no answer to {:#x}", op));
            self.failed(log);
        }
        if self.idle() && self.want != self.is {
            if self.is == Mode::Off {
                log(format_args!("apr: starting {:?}", self.want));
                self.begin(self.want);
            } else {
                log(format_args!("apr: stopping {:?}", self.is));
                self.end();
            }
        }
        let Some(&c) = self.queue.get(self.at) else {
            return Out::Nothing;
        };
        if let Cmd::Codecs(on) = c {
            self.waiting = Some((0, now));
            return Out::Codecs(on);
        }
        let p = self.packet(c);
        self.waiting = Some((u32::from_le_bytes([p[16], p[17], p[18], p[19]]), now));
        Out::Send(p)
    }

    /// The daemon powered (or stopped) the codecs: on with the sequence.
    pub fn codecs_done(&mut self, ok: bool, log: &mut dyn FnMut(core::fmt::Arguments)) {
        if matches!(self.waiting, Some((0, _))) {
            self.waiting = None;
            if !ok {
                log(format_args!(
                    "apr: the codecs failed; the voice path goes on"
                ));
                self.failures += 1;
            }
            self.at += 1;
        }
    }

    /// A failed or unanswered command: a session create ends the sequence (nothing to address), else on.
    fn failed(&mut self, log: &mut dyn FnMut(core::fmt::Arguments)) {
        self.failures += 1;
        self.waiting = None;
        match self.queue.get(self.at) {
            Some(Cmd::MvmCreate) | Some(Cmd::CvpCreate) => {
                log(format_args!("apr: no voice session; stopping the sequence"));
                self.at = self.queue.len();
            }
            Some(Cmd::CoreReady) if self.core_tries < CORE_TRIES => self.core_tries += 1, // again next poll
            _ => self.at += 1,
        }
    }

    /// One packet from the ADSP's APR channel.
    pub fn handle(&mut self, packet: &[u8], log: &mut dyn FnMut(core::fmt::Arguments)) {
        let Some(r) = Rsp::parse(packet) else {
            log(format_args!("apr: bad packet ({} bytes)", packet.len()));
            return;
        };
        let Some((op, _)) = self.waiting else {
            return; // an event, or a late answer
        };
        let current = self.queue.get(self.at).copied();
        if current == Some(Cmd::CoreReady) {
            let state = r
                .payload
                .get(..4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
            match (r.opcode, r.result()) {
                (CORE_GET_STATE_RSP, _) if state.unwrap_or(0) != 0 => {}
                (CORE_GET_STATE_RSP, _) => {
                    self.waiting = None; // not ready yet: ask again
                    self.core_tries += 1;
                    if self.core_tries >= CORE_TRIES {
                        log(format_args!(
                            "apr: the ADSP's core never got ready; going on"
                        ));
                        self.at += 1;
                    }
                    return;
                }
                (_, Some((o, 3))) if o == CORE_GET_STATE => {} // unsupported: ready (as Linux)
                _ => return,
            }
            self.waiting = None;
            self.at += 1;
            return;
        }
        let Some((acked, status)) = r.result() else {
            return;
        };
        if acked != op {
            return;
        }
        if status != 0 {
            log(format_args!("apr: {:?} -> status {}", current, status));
            self.failed(log);
            return;
        }
        log(format_args!(
            "apr: {:?} ok",
            current.unwrap_or(Cmd::CoreReady)
        ));
        match current {
            Some(Cmd::MvmCreate) => self.mvm = r.src_port,
            Some(Cmd::CvpCreate) => self.cvp = r.src_port,
            Some(Cmd::Destroy(SVC_MVM)) => self.mvm = 0,
            Some(Cmd::Destroy(_)) => self.cvp = 0,
            _ => {}
        }
        self.waiting = None;
        self.at += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect()
    }

    // the bytes C1's Linux sent in a working call (APR kprobe capture, C6 2026-11-09; zeros where it left stack garbage)
    #[test]
    fn afe_packets_match_c1() {
        assert_eq!(
            afe_clock(SPK, true),
            hex(
                "50 02 44 00 04 05 00 00 04 04 00 00 28 00 00 00 f3 00 01 00 20 00 00 00 00 00 00 00 00 00 00 00 \
                 00 00 00 00 8f 02 01 00 90 02 01 00 14 00 00 00 01 00 00 00 02 02 00 00 00 c0 5d 00 04 00 00 00 \
                 01 00 00 00"
            )
        );
        assert_eq!(
            afe_tdm_config(SPK),
            hex(
                "50 02 54 00 04 05 00 00 04 04 00 00 28 00 00 00 ef 00 01 00 10 90 30 00 00 00 00 00 00 00 00 00 \
                 00 00 00 00 0c 02 01 00 9d 02 01 00 24 00 00 00 01 00 00 00 02 00 00 00 80 bb 00 00 10 00 00 00 \
                 00 00 00 00 01 00 08 00 00 00 00 00 01 00 10 00 03 00 00 00"
            )
        );
        assert_eq!(
            afe_slot_map(MIC),
            hex(
                "50 02 4c 00 04 05 00 00 04 04 00 00 19 00 00 00 ef 00 01 00 01 90 28 00 00 00 00 00 00 00 00 00 \
                 00 00 00 00 8a 02 01 00 97 02 01 00 1c 00 00 00 01 00 00 00 02 00 10 00 00 00 00 00 00 00 04 00 \
                 ff ff ff ff ff ff ff ff ff ff ff ff"
            )
        );
        assert_eq!(
            afe_start(MIC, true),
            hex("50 02 18 00 04 05 00 00 04 04 00 00 19 00 00 00 e5 00 01 00 01 90 00 00")
        );
    }

    #[test]
    fn voice_packets_match_c1() {
        assert_eq!(
            mvm_create(),
            hex(
                "50 02 28 00 09 05 06 00 09 04 00 00 00 00 00 00 ff 10 01 00 31 31 43 30 35 30 30 30 00 00 00 00 \
                 00 00 00 00 00 00 00 00"
            )
        );
        assert_eq!(
            cvp_create(),
            hex(
                "50 02 40 00 0b 05 06 00 0b 04 00 00 00 00 00 00 69 31 01 00 02 00 01 90 71 0f 01 00 10 90 77 0f \
                 01 00 5e 13 01 00 7c 0f 01 00 ff ff 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00"
            )
        );
        assert_eq!(
            cvp_param(0xbbbb, false, true),
            hex(
                "50 02 44 00 0b 05 06 00 0b 04 bb bb 00 00 00 00 3d 13 01 00 00 00 00 00 00 00 00 00 00 00 00 00 \
                 20 00 00 00 6e 31 01 00 54 32 01 00 14 00 00 00 10 90 00 00 02 00 10 00 80 bb 00 00 01 02 00 00 \
                 00 00 00 00"
            )
        );
        assert_eq!(
            cvp_param(0xbbbb, true, false),
            hex(
                "50 02 40 00 0b 05 06 00 0b 04 bb bb 00 00 00 00 3d 13 01 00 00 00 00 00 00 00 00 00 00 00 00 00 \
                 1c 00 00 00 6e 31 01 00 8e 32 01 00 10 00 00 00 01 00 00 00 10 00 00 00 03 00 00 00 00 00 00 00"
            )
        );
    }

    /// The ADSP's ack of `p`: APR_BASIC_RSP_RESULT {opcode, status} from `src_port`.
    fn ack(p: &[u8], src_port: u16, status: u32) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&[0x50, 0x01, 28, 0, p[4], 4]);
        r.extend_from_slice(&src_port.to_le_bytes());
        r.extend_from_slice(&[p[4], 5, 6, 0]);
        r.extend_from_slice(&p[12..16]);
        r.extend_from_slice(&BASIC_RSP_RESULT.to_le_bytes());
        r.extend_from_slice(&p[16..20]);
        r.extend_from_slice(&status.to_le_bytes());
        r
    }

    /// Poll `a` for 100 ms of fake time from `t0`, answering every command (MVM handle 0x11, CVP 0x22, the core
    /// ready): the opcodes sent and the codec steps asked for.
    fn drive(a: &mut Audio, t0: u64) -> (Vec<u32>, Vec<bool>) {
        let mut log =
            |l: core::fmt::Arguments| assert!(!alloc::format!("{l}").contains("no answer"));
        let (mut ops, mut codecs) = (Vec::new(), Vec::new());
        for now in t0..t0 + 100 {
            match a.poll(now, &mut log) {
                Out::Send(p) => {
                    let op = u32::from_le_bytes([p[16], p[17], p[18], p[19]]);
                    ops.push(op);
                    if op == CORE_GET_STATE {
                        let mut r =
                            packet(SVC_CORE, 0, 0, 0, CORE_GET_STATE_RSP, &1u32.to_le_bytes());
                        r[0] = 0x50;
                        r[1] = 0x01;
                        a.handle(&r, &mut log);
                    } else {
                        let handle = match op {
                            MVM_CREATE_PASSIVE => 0x11,
                            CVP_CREATE_V3 => 0x22,
                            _ => u16::from_le_bytes([p[10], p[11]]),
                        };
                        a.handle(&ack(&p, handle, 0), &mut log);
                    }
                }
                Out::Codecs(on) => {
                    codecs.push(on);
                    a.codecs_done(true, &mut log);
                }
                Out::Nothing => {}
            }
        }
        (ops, codecs)
    }

    #[test]
    fn a_call_starts_and_stops_in_order() {
        let mut a = Audio::default();
        a.set(Mode::Call);
        let (ops, codecs) = drive(&mut a, 0);
        assert!(a.idle() && a.is() == Mode::Call);
        assert_eq!((a.mvm, a.cvp), (0x11, 0x22));
        assert_eq!(codecs, [true]);
        assert_eq!(ops.first(), Some(&CORE_GET_STATE));
        // Linux's order: the mic's port before the voice sessions, the speaker's after START_VOICE
        let voice = ops.iter().position(|&o| o == MVM_START).unwrap();
        let starts: Vec<_> = (0..ops.len())
            .filter(|&i| ops[i] == AFE_DEVICE_START)
            .collect();
        assert_eq!(starts.len(), 2);
        assert!(starts[0] < ops.iter().position(|&o| o == MVM_CREATE_PASSIVE).unwrap());
        assert_eq!(starts[1], ops.len() - 1);
        assert!(voice < starts[1]);
        assert_eq!(ops.len(), 20);
        a.set(Mode::Off);
        let (ops, codecs) = drive(&mut a, 100);
        assert_eq!(
            ops,
            [
                AFE_DEVICE_STOP,
                MVM_STOP,
                MVM_DETACH,
                CVP_DISABLE,
                AFE_DEVICE_STOP,
                DESTROY_SESSION,
                DESTROY_SESSION
            ]
        );
        assert_eq!(codecs, [false]);
        assert_eq!((a.mvm, a.cvp), (0, 0));
        assert!(a.idle() && a.is() == Mode::Off);
    }

    // Dev tools' audio test: both ports and the codecs, then the AFE loopback mic -> amps; stopped in reverse
    #[test]
    fn the_loopback_test_starts_and_stops() {
        assert_eq!(
            afe_loopback(true),
            hex(
                "50 02 3c 00 04 05 00 00 04 04 00 00 19 00 00 00 ef 00 01 00 01 90 18 00 00 00 00 00 00 00 00 00 \
                 00 00 00 00 05 02 01 00 0b 02 01 00 0c 00 00 00 01 00 00 00 10 90 01 00 01 00 00 00"
            )
        );
        let mut a = Audio::default();
        a.set(Mode::Test);
        let (ops, codecs) = drive(&mut a, 0);
        assert!(a.idle() && a.is() == Mode::Test);
        assert_eq!(codecs, [true]);
        assert_eq!(ops.len(), 10); // core, 2 clocks, 2 x (config, slots, start), loopback
        assert_eq!(ops.iter().filter(|&&o| o == AFE_DEVICE_START).count(), 2);
        assert_eq!(ops.last(), Some(&AFE_SET_PARAM_V2));
        assert!(!ops.contains(&MVM_CREATE_PASSIVE));
        a.set(Mode::Off);
        let (ops, codecs) = drive(&mut a, 100);
        assert_eq!(ops, [AFE_SET_PARAM_V2, AFE_DEVICE_STOP, AFE_DEVICE_STOP]);
        // the loopback's destination (the speaker port) stops before its source
        use Cmd::*;
        assert_eq!(
            a.queue,
            [
                Loopback(false),
                Codecs(false),
                Port(SPK, false),
                Port(MIC, false)
            ]
        );
        assert_eq!(codecs, [false]);
        assert!(a.idle() && a.is() == Mode::Off);
    }

    // Stop then start at once (Dev tools' Stop, Speaker): nothing to do; and a start wanted mid-stop waits for the
    // whole stop, so no port is started twice
    #[test]
    fn a_new_mode_waits_for_the_running_sequence() {
        let mut a = Audio::default();
        a.set(Mode::Test);
        drive(&mut a, 0);
        a.set(Mode::Off);
        a.set(Mode::Test);
        let (ops, _) = drive(&mut a, 100);
        assert_eq!(a.is(), Mode::Test);
        assert!(ops.is_empty()); // wanted again before anything ran: the test just keeps running
        let mut log = |_: core::fmt::Arguments| {};
        a.set(Mode::Off);
        let Out::Send(p) = a.poll(200, &mut log) else {
            panic!("the stop begins")
        };
        a.set(Mode::Test); // mid-stop: no effect until the stop is done
        assert_eq!(a.is(), Mode::Off);
        a.handle(&ack(&p, 0, 0), &mut log);
        let (ops, _) = drive(&mut a, 300);
        assert_eq!(
            &ops[..3],
            &[AFE_DEVICE_STOP, AFE_DEVICE_STOP, CORE_GET_STATE]
        );
        assert_eq!(a.is(), Mode::Test);
    }

    // A call wanted during the test: the test stops first, then the call starts
    #[test]
    fn a_call_during_the_test_stops_the_test_first() {
        let mut a = Audio::default();
        a.set(Mode::Test);
        drive(&mut a, 0);
        a.set(Mode::Call);
        let (ops, codecs) = drive(&mut a, 100);
        assert_eq!(
            &ops[..4],
            &[
                AFE_SET_PARAM_V2,
                AFE_DEVICE_STOP,
                AFE_DEVICE_STOP,
                CORE_GET_STATE
            ]
        );
        assert_eq!(ops.last(), Some(&AFE_DEVICE_START));
        assert!(ops.contains(&MVM_START));
        assert_eq!(codecs, [false, true]);
        assert_eq!(a.is(), Mode::Call);
    }

    #[test]
    fn a_failed_session_create_ends_the_sequence() {
        let mut a = Audio::default();
        let mut log = |_: core::fmt::Arguments| {};
        a.begin(Mode::Call);
        a.want = Mode::Call;
        a.at = a.queue.iter().position(|c| *c == Cmd::MvmCreate).unwrap();
        let Out::Send(p) = a.poll(0, &mut log) else {
            panic!()
        };
        a.handle(&ack(&p, 0, 2), &mut log);
        assert!(a.idle());
        assert_eq!(a.failures, 1);
        assert_eq!(a.poll(1, &mut log), Out::Nothing); // no retry loop
    }

    #[test]
    fn an_unanswered_command_times_out_and_the_sequence_goes_on() {
        let mut a = Audio::default();
        let mut n = 0;
        let mut log = |_: core::fmt::Arguments| n += 1;
        a.begin(Mode::Call);
        a.want = Mode::Call;
        a.at = 1; // past the core check
        assert!(matches!(a.poll(0, &mut log), Out::Send(_)));
        assert_eq!(a.poll(2000, &mut log), Out::Nothing);
        assert!(matches!(a.poll(3000, &mut log), Out::Send(_)));
        assert_eq!((a.at, a.failures), (2, 1));
    }
}
