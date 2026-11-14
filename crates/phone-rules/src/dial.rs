// SPDX-License-Identifier: Apache-2.0
//! What a typed string is (SPS §5, §6): the dialer classifies it before it dials. The checks run in a fixed order
//! and the first that accepts wins: IMEI, emergency, service code (and in-call commands), SIM code, maker code,
//! phone number, and USSD for anything else sent with Call.
//!
//! Deliberate differences from the 2010 parser: an empty forward-to number is refused (3GPP 22.030), and the DTMF
//! part after a number may only hold `0-9 * # + p w`.

use alloc::string::String;
use alloc::vec::Vec;

/// How the string was entered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    /// Call (Send) was pressed; without it only IMEI, SIM and maker codes act, as they're typed.
    pub send: bool,
    /// A call is up (in-call commands `0`..`4`, `1X`, `2X`).
    pub in_call: bool,
}

/// A service code's action (3GPP 22.030 procedure prefix).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// `*`
    Activate,
    /// `#`
    Deactivate,
    /// `*#`
    Interrogate,
    /// `**`
    Register,
    /// `##`
    Erase,
}

/// Caller ID for one call, from a `*31#`/`#31#` prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clir {
    /// `*31#`: send my number for this call (CLIR suppression)
    Show,
    /// `#31#`: hide my number for this call (CLIR invocation)
    Hide,
}

impl Clir {
    pub fn prefix(self) -> &'static str {
        match self {
            Clir::Show => "*31#",
            Clir::Hide => "#31#",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `*#06#`, typed (no Call)
    Imei,
    /// an emergency number (normalised, e.g. `+112` -> `112`)
    Emergency(String),
    /// a supplementary service procedure; absent optional parameters are empty strings
    Ss {
        action: Action,
        code: String,
        params: Vec<String>,
    },
    /// an in-call command: `cmd` 0..=4, with `call` for `1X` (end call X) and `2X` (hold all but X)
    InCall { cmd: u8, call: Option<u8> },
    /// `**04*`, `**042*`, `**05*`, `**052*` with valid PINs/PUK (typed)
    SimCode { code: String, params: Vec<String> },
    /// a maker code (typed, ends in `#`); `bad_pin` for a malformed SIM code
    Maker(&'static str),
    /// a phone number to dial; `dtmf` is sent as tones once the call connects
    Number {
        clir: Option<Clir>,
        number: String,
        dtmf: String,
    },
    /// sent to the network as USSD
    Ussd,
    /// nothing to do (yet)
    None,
}

pub fn classify(s: &str, f: Flags) -> Kind {
    if s.is_empty() {
        return Kind::None;
    }
    if !f.send && s == "*#06#" {
        return Kind::Imei;
    }
    if f.send
        && let Some(e) = crate::emergency::check(s, None)
    {
        return Kind::Emergency(e);
    }
    if f.send {
        if let Some(k) = service_code(s) {
            return k;
        }
        if f.in_call
            && let Some(k) = in_call(s)
        {
            return k;
        }
    } else {
        if let Some(k) = sim_code(s) {
            return k;
        }
        if let Some(k) = maker(s) {
            return k;
        }
        return Kind::None;
    }
    number(s, f).unwrap_or(Kind::Ussd)
}

/// The service codes: (code, allowed syntax for activate, deactivate, interrogate, register, erase); 0 = not allowed.
/// Syntax ids are those of [`params`].
const SERVICES: &[(&str, [u8; 5])] = &[
    ("002", [2, 2, 0, 3, 2]), // all forwarding
    ("004", [2, 2, 0, 3, 2]), // all conditional forwarding
    ("21", [2, 2, 2, 4, 2]),  // forward unconditional
    ("61", [2, 2, 2, 3, 2]),  // forward on no reply
    ("62", [2, 2, 2, 4, 2]),  // forward when not reachable
    ("67", [2, 2, 2, 4, 2]),  // forward when busy
    ("33", [5, 5, 1, 0, 0]),  // bar all outgoing
    ("331", [5, 5, 1, 0, 0]), // bar outgoing international
    ("332", [5, 5, 1, 0, 0]), // bar outgoing international except home
    ("35", [5, 5, 1, 0, 0]),  // bar all incoming
    ("351", [5, 5, 1, 0, 0]), // bar incoming when roaming
    ("330", [0, 5, 0, 0, 0]), // all barring
    ("333", [0, 5, 0, 0, 0]), // all outgoing barring
    ("353", [0, 5, 0, 0, 0]), // all incoming barring
    ("43", [7, 7, 7, 0, 0]),  // call waiting
    ("30", [0, 0, 1, 0, 0]),  // CLIP
    ("31", [0, 0, 1, 0, 0]),  // CLIR
    ("76", [0, 0, 1, 0, 0]),  // COLP
    ("77", [0, 0, 1, 0, 0]),  // COLR
    ("300", [0, 0, 1, 0, 0]), // CNAP
    ("03", [6, 0, 0, 6, 0]),  // barring password
];

/// Basic service codes a procedure may name (3GPP 22.030 annex C).
fn basic_service(s: &str) -> bool {
    matches!(
        s.parse::<u8>(),
        Ok(10..=13 | 16 | 19..=22 | 24..=27 | 50..=65 | 70..=85 | 89)
    ) && s.len() == 2
}

fn sc_number(s: &str) -> bool {
    let d = s.strip_prefix('+').unwrap_or(s);
    (1..=38).contains(&d.len()) && d.bytes().all(|c| c.is_ascii_digit())
}

fn timer(s: &str) -> bool {
    matches!(s, "5" | "10" | "15" | "20" | "25" | "30")
}

fn password(s: &str) -> bool {
    s.len() == 4 && s.bytes().all(|c| c.is_ascii_digit())
}

/// The parameters of a procedure's remainder (after the service code) under syntax `id`.
fn params(rest: &str, id: u8) -> Option<Vec<String>> {
    let body = rest.strip_suffix('#')?;
    let fields: Vec<&str> = if body.is_empty() {
        Vec::new()
    } else {
        body.strip_prefix('*')?.split('*').collect()
    };
    let out = |v: &[&str]| Some(v.iter().map(|s| String::from(*s)).collect());
    // syntax 3 has (number, basic service, timer), 4 (number, basic service): absent ones are empty
    let padded = |v: &[&str]| {
        let mut o: Vec<String> = v.iter().map(|s| String::from(*s)).collect();
        o.resize(if id == 3 { 3 } else { 2 }, String::new());
        Some(o)
    };
    match (id, fields.as_slice()) {
        (1, []) => out(&[]),
        (2 | 7, []) => out(&[""]),
        (2, ["", bs]) | (7, [bs]) if basic_service(bs) => out(&[bs]),
        (3 | 4, [n]) if sc_number(n) => padded(&[n]),
        (3 | 4, [n, bs]) if sc_number(n) && basic_service(bs) => padded(&[n, bs]),
        (3, [n, bs, t]) if sc_number(n) && (bs.is_empty() || basic_service(bs)) && timer(t) => {
            out(&[n, bs, t])
        }
        (5, [pw]) if password(pw) => out(&[pw, ""]),
        (5, [pw, bs]) if password(pw) && basic_service(bs) => out(&[pw, bs]),
        (6, [g, a, b, c])
            if (g.is_empty() || *g == "330") && password(a) && password(b) && password(c) =>
        {
            out(&[g, a, b, c])
        }
        _ => None,
    }
}

fn service_code(s: &str) -> Option<Kind> {
    let run = s.bytes().take_while(|c| *c == b'*' || *c == b'#').count();
    let action = match &s[..run] {
        "*" => Action::Activate,
        "#" => Action::Deactivate,
        "*#" => Action::Interrogate,
        "**" => Action::Register,
        "##" => Action::Erase,
        _ => return None,
    };
    let after = &s[run..];
    let n = after.bytes().take_while(u8::is_ascii_digit).count();
    if n == 0 || n > 5 {
        return None;
    }
    let (code, rest) = after.split_at(n);
    let (_, syntax) = SERVICES.iter().find(|(c, _)| *c == code)?;
    let slot = |a: Action| syntax[a as usize];
    let try_as = |a: Action| match slot(a) {
        0 => None,
        id => params(rest, id).map(|params| Kind::Ss {
            action: a,
            code: String::from(code),
            params,
        }),
    };
    // An activation with a number is a registration (`*21*number#`).
    try_as(action).or_else(|| {
        (action == Action::Activate)
            .then(|| try_as(Action::Register))
            .flatten()
    })
}

fn in_call(s: &str) -> Option<Kind> {
    let b = s.as_bytes();
    match b {
        [c @ b'0'..=b'4'] => Some(Kind::InCall {
            cmd: c - b'0',
            call: None,
        }),
        [c @ (b'1' | b'2'), x @ b'0'..=b'9'] => Some(Kind::InCall {
            cmd: c - b'0',
            call: Some(x - b'0'),
        }),
        _ => None,
    }
}

fn sim_code(s: &str) -> Option<Kind> {
    let (code, rest) = ["**042*", "**052*", "**04*", "**05*"]
        .iter()
        .find_map(|p| s.strip_prefix(p).map(|r| (&p[2..p.len() - 1], r)))?;
    let pin = |p: &str| (4..=8).contains(&p.len()) && p.bytes().all(|c| c.is_ascii_digit());
    let puk = |p: &str| p.len() == 8 && p.bytes().all(|c| c.is_ascii_digit());
    let ok = rest.strip_suffix('#').and_then(|body| {
        let f: Vec<&str> = body.split('*').collect();
        let valid = match (code.starts_with("04"), f.as_slice()) {
            (true, [a, b, c]) => pin(a) && pin(b) && pin(c) && b == c,
            (false, [k, b, c]) => puk(k) && pin(b) && pin(c) && b == c,
            _ => false,
        };
        valid.then(|| f.iter().map(|x| String::from(*x)).collect::<Vec<_>>())
    });
    Some(match ok {
        Some(params) => Kind::SimCode {
            code: String::from(code),
            params,
        },
        None => Kind::Maker("bad_pin"),
    })
}

fn maker(s: &str) -> Option<Kind> {
    let name = match s {
        "*#0000#" => "version",
        "*#7780#" => "reset",
        "*#7370#" => "deep_reset",
        "*#2820#" => "bt_address",
        "*#62209526#" => "wlan_mac",
        "*#92702689#" => "life_timer",
        _ => return None,
    };
    Some(Kind::Maker(name))
}

fn number(s: &str, f: Flags) -> Option<Kind> {
    let pauses = s.bytes().any(|c| matches!(c, b'p' | b'P' | b'w' | b'W'));
    if !pauses {
        if s.len() >= 2 && s.ends_with('#') {
            return None;
        }
        if s.len() <= 2 {
            let b = s.as_bytes();
            let one_x = b.len() == 2 && b[0] == b'1' && b[1].is_ascii_digit() && !f.in_call;
            if !one_x {
                return None;
            }
        }
    }
    let (clir, body) = if let Some(r) = s.strip_prefix("*31#") {
        (Some(Clir::Show), r)
    } else if let Some(r) = s.strip_prefix("#31#") {
        (Some(Clir::Hide), r)
    } else {
        (None, s)
    };
    let plus = usize::from(body.starts_with('+'));
    let run = body[plus..]
        .bytes()
        .take_while(|c| c.is_ascii_digit() || *c == b'*' || *c == b'#')
        .count();
    if run == 0 || run > 80 {
        return None;
    }
    let (number, dtmf) = body.split_at(plus + run);
    if !dtmf
        .bytes()
        .all(|c| c.is_ascii_digit() || b"*#+pPwW".contains(&c))
    {
        return None;
    }
    Some(Kind::Number {
        clir,
        number: String::from(number),
        dtmf: dtmf.to_ascii_lowercase(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const SEND: Flags = Flags {
        send: true,
        in_call: false,
    };
    const TYPED: Flags = Flags {
        send: false,
        in_call: false,
    };
    const IN_CALL: Flags = Flags {
        send: true,
        in_call: true,
    };

    fn ss(action: Action, code: &str, params: &[&str]) -> Kind {
        Kind::Ss {
            action,
            code: code.into(),
            params: params.iter().map(|p| String::from(*p)).collect(),
        }
    }

    fn num(clir: Option<Clir>, number: &str, dtmf: &str) -> Kind {
        Kind::Number {
            clir,
            number: number.into(),
            dtmf: dtmf.into(),
        }
    }

    /// docs/symbian-phone-sources.md §5, vectors 1-26.
    #[test]
    fn sps_vectors() {
        use Action::*;
        let cases: Vec<(&str, Flags, Kind)> = vec![
            ("*#06#", TYPED, Kind::Imei),
            ("*#06#", SEND, Kind::Ussd),
            ("112", IN_CALL, Kind::Emergency("112".into())),
            ("+112", SEND, Kind::Emergency("112".into())),
            (
                "*31#0401234567",
                SEND,
                num(Some(Clir::Show), "0401234567", ""),
            ),
            (
                "#31#+358401234567p123",
                SEND,
                num(Some(Clir::Hide), "+358401234567", "p123"),
            ),
            (
                "*21*+358401234567#",
                SEND,
                ss(Register, "21", &["+358401234567", ""]),
            ),
            (
                "**61*0401234567*11*20#",
                SEND,
                ss(Register, "61", &["0401234567", "11", "20"]),
            ),
            (
                "**61*0401234567**25#",
                SEND,
                ss(Register, "61", &["0401234567", "", "25"]),
            ),
            ("**61*0401234567*11*35#", SEND, Kind::Ussd),
            ("*43#", SEND, ss(Activate, "43", &[""])),
            ("*#43#", SEND, ss(Interrogate, "43", &[""])),
            ("##002#", SEND, ss(Erase, "002", &[""])),
            ("*33*1234#", SEND, ss(Activate, "33", &["1234", ""])),
            ("*33*123#", SEND, Kind::Ussd),
            (
                "**03*330*1111*2222*2222#",
                SEND,
                ss(Register, "03", &["330", "1111", "2222", "2222"]),
            ),
            (
                "**04*1234*5678*5678#",
                TYPED,
                Kind::SimCode {
                    code: "04".into(),
                    params: vec!["1234".into(), "5678".into(), "5678".into()],
                },
            ),
            ("**04*12*5678*5678#", TYPED, Kind::Maker("bad_pin")),
            ("*#7370#", TYPED, Kind::Maker("deep_reset")),
            ("1", IN_CALL, Kind::InCall { cmd: 1, call: None }),
            (
                "12",
                IN_CALL,
                Kind::InCall {
                    cmd: 1,
                    call: Some(2),
                },
            ),
            ("12", SEND, num(None, "12", "")),
            ("5", IN_CALL, Kind::Ussd),
            ("0", SEND, Kind::Ussd),
            ("*100#", SEND, Kind::Ussd),
            ("*30#", SEND, Kind::Ussd),
        ];
        for (i, (s, f, want)) in cases.into_iter().enumerate() {
            assert_eq!(classify(s, f), want, "vector {} {s:?}", i + 1);
        }
    }

    #[test]
    fn ours() {
        // empty forward-to number refused (3GPP), so it falls to USSD
        assert_eq!(classify("**21*#", SEND), Kind::Ussd);
        // DTMF part limited to the dial characters
        assert_eq!(
            classify("+37491234567p12", SEND),
            num(None, "+37491234567", "p12")
        );
        assert_eq!(
            classify("091234567W1#", SEND),
            num(None, "091234567", "w1#")
        );
        assert_eq!(classify("0912 34567", SEND), Kind::Ussd);
        assert_eq!(classify("", SEND), Kind::None);
        assert_eq!(classify("0912", TYPED), Kind::None);
        assert_eq!(
            classify("**052*12345678*1234*1234#", TYPED).clone(),
            Kind::SimCode {
                code: "052".into(),
                params: vec!["12345678".into(), "1234".into(), "1234".into()],
            }
        );
        assert_eq!(classify("*#2820#", SEND), Kind::Ussd);
        assert_eq!(Clir::Hide.prefix(), "#31#");
    }
}
