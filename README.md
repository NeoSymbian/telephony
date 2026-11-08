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
