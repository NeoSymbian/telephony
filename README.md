# NeoSymbian telephony

User-side Rust for the cellular modem on the Pixel 3a XL (Qualcomm SDM670), running on the 2010 Symbian user side
(`armv5te-none-eabi`, `no_std`, the [`e32`](https://github.com/NeoSymbian/e32-rs) crate).

The plan, hardware facts and stages are in [docs/telephony.md](https://github.com/NeoSymbian/docs/blob/main/telephony.md).
The kernel transport (SCM, SMEM, SMP2P, GLINK, remote-processor boot) lives in
[kernel](https://github.com/NeoSymbian/kernel) under `baseport/arm64/sdm670_assp`.

The existing workspaces contain:

| Crate | Responsibility |
|---|---|
| `modem-ipc` | Version 2 NeoModem contract: calls, command results, SIM/PIN, radio intent, ringing and recovery |
| `modem-proto` | QRTR/QMI services and clients, RAM-backed EFS, complete call indications and serialized APR voice/ringtone/loopback |
| `ext4ro` | Read-only firmware access |
| `symbian/modemd` | MODEMD.EXE: incremental firmware loading, the NeoModem server, notifications and processor recovery |

C7 implementation is on `feat/c7-complete-calls`; phone acceptance is still required. The server and all IPC consumers
must be updated together: v1 sessions are rejected. STATUS is a fixed little-endian snapshot; NOTIFY takes that same
writable descriptor in argument 0 and the last revision in argument 1. Each session may keep one notification
outstanding; CANCEL_NOTIFY and disconnect complete it with cancellation.

The call list is authoritative. Hold/resume/swap/merge report pending and QMI errors without claiming a state change
before the modem reports it. PIN values are bounded, never logged or persisted, and rejected PINs are never retried.
Airplane intent and ring settings are ordinary `C:\private\e0003001\settings.bin` settings; radio EFS still stays in RAM.

Ringtone playback shares the existing APR owner and board path. Its original PCM goes through a bounded kernel-owned
buffer, ASM and ADM; a buffer is republished only after matching DSP completion. Voice starts after ringtone close
and unmap. Wire facts come from primary [q6asm](https://github.com/torvalds/linux/blob/master/sound/soc/qcom/qdsp6/q6asm.c)
and [q6adm](https://github.com/torvalds/linux/blob/master/sound/soc/qcom/qdsp6/q6adm.c) references; QMI facts come from
[libqmi service descriptions](https://github.com/linux-mobile-broadband/libqmi/tree/main/data). No Linux implementation
was copied.

Recovery keeps NeoModem alive while the kernel stops the affected processors, invalidates their transport generation,
and reloads the existing read-only firmware path. RAM EFS survives. Three attempts use 1/3/10-second backoffs; exhausted
recovery remains visible with an explicit retry action. C6 audio ordering is preserved.

The connected-call return-to-LTE defect remains unresolved: existing traces do not establish its cause. No forced
radio cycle is presented as a fix. C7 must not be marked done until device acceptance resolves this and verifies the
new playback, hardware and recovery paths.

Validation uses the existing commands:

```sh
cargo fmt --all --check
cargo clippy --offline -- -D warnings
cargo test --offline
cd symbian
cargo fmt --all --check
cargo arm --workspace --offline
```

Qualcomm firmware is never stored here: it is read from the phone's own partitions at runtime.

## Licence

Apache-2.0 (`SPDX-License-Identifier: Apache-2.0` in every file). Code ported from the BSD-3-Clause linux-msm tools
keeps its original copyright line; [docs/telephony.md](https://github.com/NeoSymbian/docs/blob/main/telephony.md)
lists every outside source.

## Independent ADSP servicing (driver-support prerequisite)

MODEMD remains the only consumer of the ADSP APR and QRTR rings. It starts the ADSP from `vendor_a` before
IPA/EFS/RMTFS preparation. An ADSP-only router serves locator/TFTP without advertising RMTFS or inventing its
physical address; real radio resources attach to the same router later. Radio preparation/boot failure does not
stop ADSP QRTR or call/loopback processing. Pending radio packets are discarded on radio failure so they cannot
block ADSP replies. This does not implement general PCM playback or a separate audio server.

The loop checks a fixed polling deadline before dispatching another IPC request. Receive batches are bounded,
and the userspace transport no longer adds retry sleeps after a failed HAL send. The kernel GLINK HAL can still
wait for a receive intent (up to 200 kernel sleep iterations). The deadline prevents IPC floods from resetting
the polling timer; synchronous transport, firmware, storage and codec HAL calls still need measured phone budgets
before PCM deadlines can be promised.

For the startup-only radio-disabled diagnostic image, build from `symbian/` with
`cargo arm --features radio-disabled`. It skips IPA/EFS/MSS startup and automatic online requests while leaving
ADSP/loopback servicing available. This is a build option, not a runtime airplane-mode API. Default builds retain
normal cellular startup. Validate cold boot without SIM, radio-disabled loopback, forced radio preparation failure
and normal two-way calls on the phone before using this candidate as the installed image.
