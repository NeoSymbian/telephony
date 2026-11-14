// SPDX-License-Identifier: Apache-2.0
//! The phone's rules, shared by MODEMD and neoui (docs/telephony.md C8). The facts come from the 2010 Symbian phone
//! sources, read and re-expressed in our own code (docs/symbian-phone-sources.md, "SPS"):
//! - [`dial`]: what a typed string is (number, service code, USSD, ...), SPS §5-§6;
//! - [`emergency`]: the emergency numbers, SPS §8;
//! - [`number`]: whether two phone numbers are the same subscriber (caller names), SPS §13;
//! - [`caller`]: the caller line of the call screens and the log, SPS §3.
#![no_std]

extern crate alloc;

pub mod caller;
pub mod dial;
pub mod emergency;
pub mod number;
