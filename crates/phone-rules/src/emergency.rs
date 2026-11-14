// SPDX-License-Identifier: Apache-2.0
//! Emergency numbers (SPS §8): 112 and 911 always, then the SIM's list, or a fallback list when there is none.

use alloc::string::String;

/// In every list.
pub const ALWAYS: [&str; 2] = ["112", "911"];
/// Used when the SIM's emergency list (EF_ECC) can't be read.
pub const FALLBACK: [&str; 6] = ["999", "000", "08", "110", "118", "119"];
/// The longest emergency number we accept.
const MAX_DIGITS: usize = 6;

/// The emergency number in `s`, if it is one: a leading `*31#`/`#31#` is dropped, the string is cut at the first
/// `p`/`w`, one leading `+` is dropped, and what is left must be all digits and exactly a list entry. `sim` is the
/// SIM's list; `None` or empty uses [`FALLBACK`].
pub fn check(s: &str, sim: Option<&[&str]>) -> Option<String> {
    let s = s
        .strip_prefix("*31#")
        .or_else(|| s.strip_prefix("#31#"))
        .unwrap_or(s);
    let s = s.split(['p', 'P', 'w', 'W']).next().unwrap_or("");
    let s = s.strip_prefix('+').unwrap_or(s);
    if s.is_empty() || s.len() > MAX_DIGITS || !s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let extra = match sim {
        Some(list) if !list.is_empty() => list,
        _ => &FALLBACK[..],
    };
    ALWAYS
        .iter()
        .chain(extra)
        .any(|e| *e == s)
        .then(|| String::from(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches() {
        for s in ["112", "+112", "*31#112", "#31#911", "112p1", "112w", "999"] {
            assert!(check(s, None).is_some(), "{s}");
        }
        assert_eq!(check("+112", None).as_deref(), Some("112"));
    }

    #[test]
    fn non_matches() {
        for s in ["112#", "1 12", "1120", "11", "", "+", "0911", "1234567"] {
            assert_eq!(check(s, None), None, "{s}");
        }
    }

    #[test]
    fn sim_list_replaces_fallback() {
        assert_eq!(check("999", Some(&["101"])), None);
        assert!(check("101", Some(&["101"])).is_some());
        assert!(check("112", Some(&["101"])).is_some());
        assert!(check("999", Some(&[])).is_some());
    }
}
