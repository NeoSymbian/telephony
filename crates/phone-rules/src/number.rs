// SPDX-License-Identifier: Apache-2.0
//! Whether two phone numbers belong to the same subscriber, for caller names (SPS §13): the last 7 digits must be
//! equal, and the digits above them (up to 8 more) must agree as far as the shorter number goes, after leading zeros.
//! So `+374 91 234567` matches `091 234567` and `91234567`, but not `+374 51 234567`.

use alloc::string::String;

/// Digits compared exactly, from the right.
pub const LOW: usize = 7;
/// Further digits compared when both numbers have them.
pub const HIGH: usize = 8;

/// The digits of `s`, every other character (`+`, spaces, dashes, `p`, ...) dropped.
pub fn digits(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// (the last [`LOW`] digits left-padded with zeros, up to [`HIGH`] digits above them without leading zeros)
fn split(s: &str) -> Option<([u8; LOW], String)> {
    let d = digits(s);
    if d.is_empty() {
        return None;
    }
    let cut = d.len().saturating_sub(LOW);
    let mut low = [b'0'; LOW];
    low[LOW - (d.len() - cut)..].copy_from_slice(&d.as_bytes()[cut..]);
    let high = &d[cut.saturating_sub(HIGH)..cut];
    Some((low, String::from(high.trim_start_matches('0'))))
}

/// `a` and `b` are the same subscriber's number.
pub fn matches(a: &str, b: &str) -> bool {
    let (Some((la, ha)), Some((lb, hb))) = (split(a), split(b)) else {
        return false;
    };
    if la != lb {
        return false;
    }
    let n = ha.len().min(hb.len());
    ha[ha.len() - n..] == hb[hb.len() - n..]
}

/// The one owner (a contact id, say) whose number matches `number`; `None` when none or several different owners
/// match, so a caller is never shown under the wrong name.
pub fn unique_match<'a, T: PartialEq + Copy>(
    number: &str,
    candidates: impl IntoIterator<Item = (T, &'a str)>,
) -> Option<T> {
    let mut found = None;
    for (owner, n) in candidates {
        if matches(number, n) {
            match found {
                None => found = Some(owner),
                Some(f) if f == owner => {}
                Some(_) => return None,
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_subscriber() {
        assert!(matches("+37491234567", "091234567"));
        assert!(matches("+374 91 23-45-67", "91234567"));
        assert!(matches("0037491234567", "+37491234567"));
        assert!(matches("1234567", "+37491234567"));
        assert!(matches("12345", "0012345"));
    }

    #[test]
    fn different_subscriber() {
        assert!(!matches("+37491234567", "+37451234567"));
        assert!(!matches("+37491234567", "+37491234568"));
        assert!(!matches("", "1234567"));
        assert!(!matches("Private", "1234567"));
    }

    #[test]
    fn unique_only() {
        let book = [(1, "+37491234567"), (1, "091234567"), (2, "+37455000111")];
        assert_eq!(unique_match("91234567", book), Some(1));
        assert_eq!(unique_match("+37455000111", book), Some(2));
        assert_eq!(unique_match("+37455999999", book), None);
        let twins = [(1, "+37491234567"), (2, "091234567")];
        assert_eq!(unique_match("91234567", twins), None);
    }
}
