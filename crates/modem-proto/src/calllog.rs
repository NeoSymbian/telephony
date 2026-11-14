// SPDX-License-Identifier: Apache-2.0
//! The call log (docs/telephony.md C8): MODEMD watches every call of the All Call Status indications and records each
//! one when it ends, as dialled, received (answered), missed, rejected (hung up while it rang) or blocked (rejected by
//! the blocklist before it rang). MODEMD keeps it in `C:\private\e0003001\calls.db` ([`CallLog::encode`]); clients
//! read it with modem_ipc::op::CALL_LOG.

use alloc::vec::Vec;
use modem_ipc::{Call, LogEntry, MAX_LOG, Text, call, log};

const MAGIC: &[u8; 4] = b"NCL2";
/// v2.1 files: 51-byte entries without `why`.
const MAGIC_V1: &[u8; 4] = b"NCL1";
const DAY_MS: i64 = 86_400_000;
/// Blocked calls are kept at most this many days.
const BLOCKED_DAYS: u8 = 7;

/// A call in progress.
#[derive(Clone, Copy, Debug)]
struct Live {
    id: u8,
    utc: i64,
    connected_ms: Option<u64>,
    incoming: bool,
    rejected: bool,
    /// phone_rules::block::why, 0 = not blocked
    blocked: u8,
    number: Text,
    presentation: u8,
}

#[derive(Clone, Debug)]
pub struct CallLog {
    /// newest first
    pub entries: Vec<LogEntry>,
    next_id: u32,
    /// how many days the log keeps a call; 0 = no log
    pub days: u8,
    live: Vec<Live>,
    /// changed since MODEMD last saved it
    pub dirty: bool,
}

impl Default for CallLog {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            next_id: 1,
            days: 30,
            live: Vec::new(),
            dirty: false,
        }
    }
}

impl CallLog {
    /// Watch a new call list (every call the modem reports). `now_ms` is a monotonic clock (durations), `utc_ms` the
    /// wall clock (start times, pruning; 0 = unknown). True when the log changed.
    pub fn observe(&mut self, calls: &[Call], now_ms: u64, utc_ms: i64) -> bool {
        let up = |c: &Call| !matches!(c.state, call::NONE | call::END);
        for c in calls.iter().filter(|c| up(c)) {
            match self.live.iter_mut().find(|l| l.id == c.id) {
                Some(l) => {
                    if l.number.as_str().is_empty() {
                        l.number = c.number;
                    }
                    if c.state == call::CONVERSATION && l.connected_ms.is_none() {
                        l.connected_ms = Some(now_ms);
                    }
                }
                None => self.live.push(Live {
                    id: c.id,
                    utc: utc_ms,
                    connected_ms: (c.state == call::CONVERSATION).then_some(now_ms),
                    incoming: c.incoming,
                    rejected: false,
                    blocked: 0,
                    number: c.number,
                    presentation: c.presentation,
                }),
            }
        }
        let mut changed = false;
        let mut i = 0;
        while i < self.live.len() {
            let l = self.live[i];
            if calls.iter().any(|c| c.id == l.id && up(c)) {
                i += 1;
                continue;
            }
            self.live.swap_remove(i);
            changed |= self.record(l, now_ms);
        }
        if changed {
            self.prune(utc_ms);
        }
        changed
    }

    /// The user hung up call `id` while it rang (or waited): it is logged as rejected, not missed.
    pub fn rejected(&mut self, id: u8) {
        if let Some(l) = self.live.iter_mut().find(|l| l.id == id && l.incoming) {
            l.rejected = l.connected_ms.is_none();
        }
    }

    /// MODEMD rejected call `id` because the blocklist matched (`why`): logged as blocked, never missed.
    pub fn blocked(&mut self, id: u8, why: u8) {
        if let Some(l) = self.live.iter_mut().find(|l| l.id == id && l.incoming) {
            l.rejected = true;
            l.blocked = why.max(1);
        }
    }

    /// Call `id` was blocked.
    pub fn is_blocked(&self, id: u8) -> bool {
        self.live.iter().any(|l| l.id == id && l.blocked != 0)
    }

    /// Call `id` is ringing and the user rejected it (not a missed call).
    pub fn is_rejected(&self, id: u8) -> bool {
        self.live.iter().any(|l| l.id == id && l.rejected)
    }

    fn record(&mut self, l: Live, now_ms: u64) -> bool {
        if self.days == 0 {
            return false;
        }
        let kind = match (l.incoming, l.connected_ms, l.rejected) {
            _ if l.blocked != 0 => log::BLOCKED,
            (false, _, _) => log::DIALLED,
            (true, Some(_), _) => log::RECEIVED,
            (true, None, true) => log::REJECTED,
            (true, None, false) => log::MISSED,
        };
        let duration_s = l
            .connected_ms
            .map_or(0, |c| (now_ms.saturating_sub(c) / 1000) as u32);
        self.entries.insert(
            0,
            LogEntry {
                id: self.next_id,
                utc: l.utc,
                duration_s,
                kind,
                presentation: l.presentation,
                number: l.number,
                why: l.blocked,
            },
        );
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.dirty = true;
        true
    }

    /// Drop what is older than `days` (when the clock is known) and beyond [`MAX_LOG`].
    fn prune(&mut self, utc_ms: i64) {
        let before = self.entries.len();
        if utc_ms > 0 {
            let oldest = |days: u8| utc_ms - i64::from(days) * DAY_MS;
            let (all, blocked) = (oldest(self.days), oldest(self.days.min(BLOCKED_DAYS)));
            self.entries.retain(|e| {
                e.utc == 0 || e.utc >= if e.kind == log::BLOCKED { blocked } else { all }
            });
        }
        self.entries.truncate(MAX_LOG);
        self.dirty |= self.entries.len() != before;
    }

    /// modem_ipc::op::CALL_SETTINGS: 0 empties the log and stops it.
    pub fn set_days(&mut self, days: u8, utc_ms: i64) -> bool {
        let before = (self.days, self.entries.len());
        self.days = days;
        if days == 0 {
            self.entries.clear();
        }
        self.prune(utc_ms);
        let changed = before != (self.days, self.entries.len());
        self.dirty |= changed;
        changed
    }

    /// modem_ipc::op::CLEAR_LOG: entry `id` (0 = all) of `kind` (0 = any). True when something went.
    pub fn clear(&mut self, id: u32, kind: u8) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|e| !((id == 0 || e.id == id) && (kind == 0 || e.kind == kind)));
        let changed = self.entries.len() != before;
        self.dirty |= changed;
        changed
    }

    /// The entries as modem_ipc::op::CALL_LOG sends them: as many whole entries as fit `room` bytes.
    pub fn to_wire(&self, room: usize) -> Vec<u8> {
        let n = self.entries.len().min(room / LogEntry::SIZE);
        let mut b = Vec::with_capacity(n * LogEntry::SIZE);
        for e in &self.entries[..n] {
            b.extend_from_slice(&e.to_bytes());
        }
        b
    }

    /// `calls.db`: "NCL2", next id (u32), days, count, then the entries.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(10 + self.entries.len() * LogEntry::SIZE);
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&self.next_id.to_le_bytes());
        b.push(self.days);
        b.push(self.entries.len() as u8);
        b.extend_from_slice(&self.to_wire(usize::MAX));
        b
    }

    /// A saved log ("NCL2", or the v2.1 "NCL1"); `None` for a file that isn't one (MODEMD then starts empty).
    pub fn decode(b: &[u8]) -> Option<Self> {
        let size = match b.get(..4)? {
            m if m == MAGIC => LogEntry::SIZE,
            m if m == MAGIC_V1 => LogEntry::SIZE - 1,
            _ => return None,
        };
        let next_id = u32::from_le_bytes(b.get(4..8)?.try_into().ok()?).max(1);
        let days = *b.get(8)?;
        let n = usize::from(*b.get(9)?).min(MAX_LOG);
        let entries = (0..n)
            .map(|i| {
                let mut e = [0u8; LogEntry::SIZE];
                e[..size].copy_from_slice(b.get(10 + i * size..10 + (i + 1) * size)?);
                LogEntry::from_bytes(&e)
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            entries,
            next_id,
            days,
            ..Self::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: u8, state: u8, incoming: bool, number: &str) -> Call {
        Call {
            id,
            state,
            incoming,
            number: Text::new(number),
            presentation: 0,
        }
    }

    const T0: i64 = 1_794_513_000_000;

    #[test]
    fn dialled_with_duration() {
        let mut l = CallLog::default();
        assert!(!l.observe(&[c(1, call::ORIGINATION, false, "+37491")], 0, T0));
        l.observe(&[c(1, call::ALERTING, false, "+37491")], 3_000, T0);
        l.observe(&[c(1, call::CONVERSATION, false, "+37491")], 5_000, T0);
        assert!(l.observe(&[c(1, call::END, false, "+37491")], 65_500, T0 + 65_500));
        let e = l.entries[0];
        assert_eq!(
            (e.id, e.kind, e.duration_s, e.utc),
            (1, log::DIALLED, 60, T0)
        );
        assert_eq!(e.number.as_str(), "+37491");
        assert!(l.dirty);
    }

    #[test]
    fn received_missed_rejected() {
        let mut l = CallLog::default();
        // answered, then the call leaves the list without an END
        l.observe(&[c(1, call::INCOMING, true, "")], 0, T0);
        l.observe(&[c(1, call::CONVERSATION, true, "+1")], 1_000, T0);
        l.observe(&[], 11_000, T0);
        // rang out
        l.observe(&[c(2, call::INCOMING, true, "+2")], 20_000, T0);
        l.observe(&[c(2, call::END, true, "+2")], 30_000, T0);
        // rejected
        l.observe(&[c(1, call::INCOMING, true, "+3")], 40_000, T0);
        l.rejected(1);
        l.observe(&[c(1, call::DISCONNECTING, true, "+3")], 41_000, T0);
        l.observe(&[c(1, call::END, true, "+3")], 41_500, T0);
        let kinds: Vec<(u32, u8, u32, &str)> = l
            .entries
            .iter()
            .map(|e| (e.id, e.kind, e.duration_s, e.number.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                (3, log::REJECTED, 0, "+3"),
                (2, log::MISSED, 0, "+2"),
                (1, log::RECEIVED, 10, "+1"),
            ]
        );
    }

    #[test]
    fn two_calls_and_waiting() {
        let mut l = CallLog::default();
        l.observe(&[c(1, call::CONVERSATION, false, "+1")], 0, T0);
        l.observe(
            &[
                c(1, call::CONVERSATION, false, "+1"),
                c(2, call::WAITING, true, "+2"),
            ],
            1_000,
            T0,
        );
        l.observe(
            &[
                c(1, call::HOLD, false, "+1"),
                c(2, call::CONVERSATION, true, "+2"),
            ],
            2_000,
            T0,
        );
        l.observe(&[c(1, call::HOLD, false, "+1")], 32_000, T0);
        l.observe(&[], 40_000, T0);
        let e: Vec<(u8, u32)> = l.entries.iter().map(|e| (e.kind, e.duration_s)).collect();
        assert_eq!(e, [(log::DIALLED, 40), (log::RECEIVED, 30)]);
    }

    #[test]
    fn days_prune_clear_and_file() {
        let mut l = CallLog::default();
        for i in 0..70u8 {
            l.observe(&[c(1, call::INCOMING, true, "+1")], 0, T0 + i64::from(i));
            l.observe(&[], 0, T0 + i64::from(i));
        }
        assert_eq!(l.entries.len(), MAX_LOG);
        assert_eq!(l.entries[0].id, 70);
        let back = CallLog::decode(&l.encode()).unwrap();
        assert_eq!(
            (back.entries.clone(), back.next_id, back.days),
            (l.entries.clone(), 71, 30)
        );
        assert!(CallLog::decode(b"NCL0").is_none());
        // a v2.1 file: 51-byte entries
        let mut v1 = Vec::from(&b"NCL1"[..]);
        v1.extend_from_slice(&[5, 0, 0, 0, 30, 1]);
        v1.extend_from_slice(&l.entries[0].to_bytes()[..LogEntry::SIZE - 1]);
        let old = CallLog::decode(&v1).unwrap();
        assert_eq!((old.entries[0], old.next_id), (l.entries[0], 5));
        assert_eq!(l.to_wire(3 * LogEntry::SIZE + 7).len(), 3 * LogEntry::SIZE);

        assert!(l.clear(70, 0));
        assert!(!l.clear(70, 0));
        assert!(!l.clear(0, log::DIALLED));
        // a day later with a one-day log: everything is older
        assert!(l.set_days(1, T0 + 2 * DAY_MS));
        assert!(l.entries.is_empty());
        l.observe(&[c(1, call::INCOMING, true, "+1")], 0, T0);
        assert!(l.set_days(0, T0));
        assert!(!l.observe(&[], 0, T0), "no log: nothing recorded");
    }

    #[test]
    fn blocked_is_logged_with_why_and_kept_a_week() {
        let mut l = CallLog::default();
        l.observe(&[c(1, call::INCOMING, true, "+19005550101")], 0, T0);
        l.blocked(1, 2);
        assert!(l.is_blocked(1) && l.is_rejected(1));
        l.observe(&[c(1, call::END, true, "+19005550101")], 500, T0);
        l.observe(&[c(2, call::INCOMING, true, "+2")], 600, T0);
        l.observe(&[], 700, T0);
        let e = l.entries[1];
        assert_eq!((e.kind, e.why, e.duration_s), (log::BLOCKED, 2, 0));
        assert_eq!(l.entries[0].kind, log::MISSED);
        // eight days later the blocked call is gone, the missed one (30-day log) stays
        l.observe(&[c(3, call::INCOMING, true, "+3")], 800, T0 + 8 * DAY_MS);
        l.observe(&[], 900, T0 + 8 * DAY_MS);
        let kinds: Vec<u8> = l.entries.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [log::MISSED, log::MISSED]);
    }
}
