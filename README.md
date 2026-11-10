# NeoSymbian telephony

User-side Rust for the cellular modem on the Pixel 3a XL (Qualcomm SDM670), running on the 2010 Symbian user side
(`armv5te-none-eabi`, `no_std`, the [`e32`](https://github.com/NeoSymbian/e32-rs) crate).

The plan, hardware facts and stages are in [docs/telephony.md](https://github.com/NeoSymbian/docs/blob/main/telephony.md).
The kernel transport (SCM, SMEM, SMP2P, GLINK, remote-processor boot) lives in
[kernel](https://github.com/NeoSymbian/kernel) under `baseport/arm64/sdm670_assp`.

Planned crates (stage C4 onward):

| Crate | What |
|---|---|
| `qrtr` | QRTR router and nameserver over the GLINK `IPCRTR` channel |
| `qmi` | QMI encoding, transactions and the DMS/NAS/UIM/VOICE/WMS/WDS clients |
| `rmtfs` | The modem's remote file system service (EFS kept in RAM until gate G4) |
| `pdmapper` | Protection-domain locator for the modem and the ADSP |
| `tqftp` | TFTP-over-QRTR file service for the modem |
| `rprocd` | Starts and watches the remote processors |
| `apr` | The ADSP audio packet router: AFE and the voice path |
| `modemsrv` | The Symbian server apps use for calls, SMS and status |

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
