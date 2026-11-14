// SPDX-License-Identifier: Apache-2.0
//! The caller line (SPS §3): what the incoming-call screen, the call screen and the log show for a call.

use alloc::string::String;

/// QMI VOICE remote party presentation (libqmi `QmiVoicePresentation`).
pub mod presentation {
    pub const ALLOWED: u8 = 0;
    pub const RESTRICTED: u8 = 1;
    pub const UNAVAILABLE: u8 = 2;
    pub const PAYPHONE: u8 = 4;
}

/// (main line, second line) for a call with `number`, its contact's `name` (only when exactly one contact matched)
/// and the network's `presentation`. The second line is the number under a name, else empty.
pub fn caller_line(
    name: Option<&str>,
    number: &str,
    presentation: u8,
    emergency: bool,
) -> (String, String) {
    let main = |s: &str| (String::from(s), String::new());
    if emergency {
        return main("Emergency call");
    }
    match presentation {
        presentation::RESTRICTED => return main("Private number"),
        presentation::PAYPHONE => return main("Payphone"),
        _ => {}
    }
    match name.filter(|n| !n.trim().is_empty()) {
        Some(n) => (String::from(n), String::from(number)),
        None if !number.is_empty() => main(number),
        None => main("Unknown number"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(name: Option<&str>, number: &str, p: u8) -> (&'static str, &'static str) {
        let (a, b) = caller_line(name, number, p, false);
        (a.leak(), b.leak())
    }

    #[test]
    fn precedence() {
        assert_eq!(line(Some("Ani"), "+37491", 0), ("Ani", "+37491"));
        assert_eq!(line(None, "+37491", 0), ("+37491", ""));
        assert_eq!(line(Some("Ani"), "", 1), ("Private number", ""));
        assert_eq!(line(None, "", 4), ("Payphone", ""));
        assert_eq!(line(None, "", 2), ("Unknown number", ""));
        assert_eq!(line(Some("  "), "", 0), ("Unknown number", ""));
        assert_eq!(caller_line(Some("Ani"), "112", 0, true).0, "Emergency call");
    }
}
