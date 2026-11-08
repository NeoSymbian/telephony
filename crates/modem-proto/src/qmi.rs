// SPDX-License-Identifier: Apache-2.0
//! QMI over QRTR: a 7-byte header {type, txn, message id, length} and TLVs {type u8, length u16, value}.
//! Message ids and TLV layouts come from linux-msm's .qmi files (BSD-3) and libqmi's JSON service descriptions.

use alloc::vec::Vec;

pub const REQUEST: u8 = 0;
pub const RESPONSE: u8 = 2;
pub const INDICATION: u8 = 4;
pub const RESULT_TLV: u8 = 0x02;

#[derive(Debug, PartialEq, Eq)]
pub struct Msg<'a> {
    pub ty: u8,
    pub txn: u16,
    pub id: u16,
    pub body: &'a [u8],
}

impl<'a> Msg<'a> {
    pub fn parse(b: &'a [u8]) -> Option<Self> {
        let len = u16::from_le_bytes(b.get(5..7)?.try_into().ok()?) as usize;
        Some(Msg {
            ty: *b.first()?,
            txn: u16::from_le_bytes(b.get(1..3)?.try_into().ok()?),
            id: u16::from_le_bytes(b.get(3..5)?.try_into().ok()?),
            body: b.get(7..7 + len)?,
        })
    }

    /// The value of TLV `ty`, if present.
    pub fn tlv(&self, ty: u8) -> Option<&'a [u8]> {
        let mut b = self.body;
        while b.len() >= 3 {
            let len = u16::from_le_bytes([b[1], b[2]]) as usize;
            let v = b.get(3..3 + len)?;
            if b[0] == ty {
                return Some(v);
            }
            b = &b[3 + len..];
        }
        None
    }

    pub fn u32(&self, ty: u8) -> Option<u32> {
        Some(u32::from_le_bytes(self.tlv(ty)?.get(..4)?.try_into().ok()?))
    }

    pub fn u8(&self, ty: u8) -> Option<u8> {
        self.tlv(ty)?.first().copied()
    }

    /// The standard result TLV: Ok(()) for success, Err(error code) otherwise.
    pub fn result(&self) -> Option<Result<(), u16>> {
        let r = self.tlv(RESULT_TLV)?;
        let result = u16::from_le_bytes(r.get(..2)?.try_into().ok()?);
        let error = u16::from_le_bytes(r.get(2..4)?.try_into().ok()?);
        Some(if result == 0 { Ok(()) } else { Err(error) })
    }
}

/// Builds one message: `Builder::new(RESPONSE, txn, id).result(0).u32(0x10, 3).finish()`.
pub struct Builder(Vec<u8>);

impl Builder {
    pub fn new(ty: u8, txn: u16, id: u16) -> Self {
        let mut v = Vec::with_capacity(64);
        v.push(ty);
        v.extend_from_slice(&txn.to_le_bytes());
        v.extend_from_slice(&id.to_le_bytes());
        v.extend_from_slice(&[0, 0]);
        Builder(v)
    }

    pub fn tlv(mut self, ty: u8, value: &[u8]) -> Self {
        self.0.push(ty);
        self.0
            .extend_from_slice(&(value.len() as u16).to_le_bytes());
        self.0.extend_from_slice(value);
        self
    }

    pub fn u8(self, ty: u8, v: u8) -> Self {
        self.tlv(ty, &[v])
    }

    pub fn u16(self, ty: u8, v: u16) -> Self {
        self.tlv(ty, &v.to_le_bytes())
    }

    pub fn u32(self, ty: u8, v: u32) -> Self {
        self.tlv(ty, &v.to_le_bytes())
    }

    pub fn u64(self, ty: u8, v: u64) -> Self {
        self.tlv(ty, &v.to_le_bytes())
    }

    /// The result TLV: 0 = success; otherwise failure (1) with `error`.
    pub fn result(self, error: u16) -> Self {
        let r: u16 = if error == 0 { 0 } else { 1 };
        let mut v = [0u8; 4];
        v[..2].copy_from_slice(&r.to_le_bytes());
        v[2..].copy_from_slice(&error.to_le_bytes());
        self.tlv(RESULT_TLV, &v)
    }

    pub fn finish(mut self) -> Vec<u8> {
        let len = (self.0.len() - 7) as u16;
        self.0[5..7].copy_from_slice(&len.to_le_bytes());
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_parse() {
        let b = Builder::new(RESPONSE, 7, 0x23)
            .result(0)
            .u32(0x10, 3)
            .finish();
        let m = Msg::parse(&b).unwrap();
        assert_eq!((m.ty, m.txn, m.id), (RESPONSE, 7, 0x23));
        assert_eq!(m.result(), Some(Ok(())));
        assert_eq!(m.u32(0x10), Some(3));
        assert_eq!(m.tlv(0x11), None);
    }

    #[test]
    fn failure_result() {
        let b = Builder::new(RESPONSE, 1, 1).result(0x30).finish();
        assert_eq!(Msg::parse(&b).unwrap().result(), Some(Err(0x30)));
    }

    #[test]
    fn truncated_is_none() {
        assert_eq!(Msg::parse(&[0, 1, 0, 0x23, 0, 9, 0, 1]), None);
    }
}
