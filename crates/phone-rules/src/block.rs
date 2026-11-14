// SPDX-License-Identifier: Apache-2.0
//! The blocklist (docs/telephony.md C8, "Blocking"): which incoming calls MODEMD rejects as busy before they ring.
//! A rule is a whole number (compared like caller names, [`number::matches`]) or a prefix of the digits; two switches
//! block hidden callers and callers not in the contacts. Contacts are known to MODEMD only as [`known_key`] hashes the
//! Phone app sends. Emergency numbers are never blocked.

use crate::{caller::presentation, emergency, number};
use alloc::string::String;
use alloc::vec::Vec;

/// Why a call was blocked (the call log's `why` byte).
pub mod why {
    pub const NONE: u8 = 0;
    pub const NUMBER: u8 = 1;
    pub const PREFIX: u8 = 2;
    pub const PRIVATE: u8 = 3;
    pub const UNKNOWN: u8 = 4;
}

/// Most rules in a list.
pub const MAX_RULES: usize = 100;
/// Most known-number hashes.
pub const MAX_KNOWN: usize = 2000;
/// Longest rule text.
pub const MAX_TEXT: usize = 32;
const MAGIC: &[u8; 4] = b"NBL1";

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Rule {
    /// The rule matches every number whose digits start with `number`'s digits.
    pub prefix: bool,
    pub calls: bool,
    pub messages: bool,
    pub number: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct List {
    pub rules: Vec<Rule>,
    /// Block callers who hide their number (or whose number the network doesn't give).
    pub private: bool,
    /// Block callers whose number matches no contact.
    pub unknown: bool,
    /// [`known_key`] of every contact number, sorted.
    pub known: Vec<u32>,
}

/// FNV-1a of the last [`number::LOW`] digits, zero-padded: equal for numbers [`number::matches`] could pair.
pub fn known_key(n: &str) -> Option<u32> {
    let d = number::digits(n);
    if d.is_empty() {
        return None;
    }
    let tail = &d.as_bytes()[d.len().saturating_sub(number::LOW)..];
    let pad = number::LOW - tail.len();
    let mut h: u32 = 0x811c_9dc5;
    for &b in core::iter::repeat_n(&b'0', pad).chain(tail) {
        h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    Some(h)
}

impl List {
    /// Why an incoming call from `n` with the network's `presentation` is blocked ([`why`]), or `None`.
    pub fn check_call(&self, n: &str, pres: u8) -> Option<u8> {
        let digits = number::digits(n);
        if !digits.is_empty() && emergency::check(&digits, None).is_some() {
            return None;
        }
        let rule = |prefix: bool| {
            self.rules
                .iter()
                .filter(|r| r.calls && r.prefix == prefix)
                .any(|r| {
                    if prefix {
                        let p = number::digits(&r.number);
                        !p.is_empty() && digits.starts_with(&p)
                    } else {
                        number::matches(n, &r.number)
                    }
                })
        };
        if rule(false) {
            return Some(why::NUMBER);
        }
        if rule(true) {
            return Some(why::PREFIX);
        }
        let hidden = digits.is_empty()
            || matches!(pres, presentation::RESTRICTED | presentation::UNAVAILABLE);
        if hidden {
            return self.private.then_some(why::PRIVATE);
        }
        if self.unknown && known_key(n).is_some_and(|k| self.known.binary_search(&k).is_err()) {
            return Some(why::UNKNOWN);
        }
        None
    }

    /// Replace the known hashes (sorted, deduplicated, capped at [`MAX_KNOWN`]).
    pub fn set_known(&mut self, mut keys: Vec<u32>) {
        keys.sort_unstable();
        keys.dedup();
        keys.truncate(MAX_KNOWN);
        self.known = keys;
    }

    /// `"NBL1"`, flags (1 private, 2 unknown), rule count, rules {flags (1 prefix, 2 calls, 4 messages), len, text},
    /// known count (u16 LE), known keys (u32 LE). `with_known: false` writes no keys (the app's BLOCK_GET/SET).
    pub fn encode(&self, with_known: bool) -> Vec<u8> {
        let mut out = Vec::from(&MAGIC[..]);
        out.push(u8::from(self.private) | u8::from(self.unknown) << 1);
        let rules = &self.rules[..self.rules.len().min(MAX_RULES)];
        out.push(rules.len() as u8);
        for r in rules {
            out.push(u8::from(r.prefix) | u8::from(r.calls) << 1 | u8::from(r.messages) << 2);
            let t = &r.number.as_bytes()[..r.number.len().min(MAX_TEXT)];
            out.push(t.len() as u8);
            out.extend_from_slice(t);
        }
        let known: &[u32] = if with_known { &self.known } else { &[] };
        out.extend_from_slice(&(known.len() as u16).to_le_bytes());
        for k in known {
            out.extend_from_slice(&k.to_le_bytes());
        }
        out
    }

    /// The list in `b`, or `None` if it isn't one.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let mut at = 0;
        let mut take = |n: usize| {
            let s = b.get(at..at + n)?;
            at += n;
            Some(s)
        };
        if take(4)? != MAGIC {
            return None;
        }
        let flags = take(1)?[0];
        let count = usize::from(take(1)?[0]);
        if count > MAX_RULES {
            return None;
        }
        let mut rules = Vec::with_capacity(count);
        for _ in 0..count {
            let f = take(1)?[0];
            let len = usize::from(take(1)?[0]);
            if len > MAX_TEXT {
                return None;
            }
            let number = String::from(core::str::from_utf8(take(len)?).ok()?);
            rules.push(Rule {
                prefix: f & 1 != 0,
                calls: f & 2 != 0,
                messages: f & 4 != 0,
                number,
            });
        }
        let n = usize::from(u16::from_le_bytes(take(2)?.try_into().ok()?));
        if n > MAX_KNOWN {
            return None;
        }
        let mut known = Vec::with_capacity(n);
        for _ in 0..n {
            known.push(u32::from_le_bytes(take(4)?.try_into().ok()?));
        }
        let mut list = List {
            rules,
            private: flags & 1 != 0,
            unknown: flags & 2 != 0,
            known: Vec::new(),
        };
        list.set_known(known);
        Some(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(number: &str, prefix: bool) -> Rule {
        Rule {
            prefix,
            calls: true,
            messages: true,
            number: String::from(number),
        }
    }

    #[test]
    fn numbers_and_prefixes() {
        let list = List {
            rules: alloc::vec![rule("+374 28 123456", false), rule("+1 900", true)],
            ..List::default()
        };
        assert_eq!(list.check_call("028123456", 0), Some(why::NUMBER));
        assert_eq!(list.check_call("+19005550101", 0), Some(why::PREFIX));
        assert_eq!(list.check_call("+37491000000", 0), None);
        let mut msgs_only = list.clone();
        msgs_only.rules.iter_mut().for_each(|r| r.calls = false);
        assert_eq!(msgs_only.check_call("028123456", 0), None);
    }

    #[test]
    fn private_and_unknown() {
        let mut list = List {
            private: true,
            unknown: true,
            ..List::default()
        };
        list.set_known(alloc::vec![known_key("+37491234567").unwrap()]);
        assert_eq!(
            list.check_call("", presentation::RESTRICTED),
            Some(why::PRIVATE)
        );
        assert_eq!(
            list.check_call("", presentation::UNAVAILABLE),
            Some(why::PRIVATE)
        );
        assert_eq!(list.check_call("091234567", 0), None);
        assert_eq!(list.check_call("+37455000000", 0), Some(why::UNKNOWN));
        list.private = false;
        assert_eq!(list.check_call("", presentation::RESTRICTED), None);
    }

    #[test]
    fn emergency_never_blocked() {
        let list = List {
            rules: alloc::vec![rule("1", true)],
            unknown: true,
            ..List::default()
        };
        assert_eq!(list.check_call("112", 0), None);
        assert_eq!(list.check_call("123456789", 0), Some(why::PREFIX));
    }

    #[test]
    fn round_trip() {
        let mut list = List {
            rules: alloc::vec![rule("+1 900", true)],
            private: true,
            ..List::default()
        };
        list.set_known(alloc::vec![3, 1, 2, 2]);
        assert_eq!(list.known, [1, 2, 3]);
        assert_eq!(List::decode(&list.encode(true)), Some(list.clone()));
        let rules_only = List::decode(&list.encode(false)).unwrap();
        assert!(rules_only.known.is_empty() && rules_only.rules == list.rules);
        assert_eq!(List::decode(b"NBL0"), None);
        assert_eq!(List::decode(&list.encode(true)[..7]), None);
    }
}
