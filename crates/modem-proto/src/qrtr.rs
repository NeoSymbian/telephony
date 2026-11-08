// SPDX-License-Identifier: Apache-2.0
//! QRTR, the Qualcomm IPC router: packet header v1 and control packets.
//! Written from the layout in linux-msm qrtr (`include/libqrtr.h`, BSD-3) and Linux `net/qrtr`.

use alloc::vec::Vec;

pub const TYPE_DATA: u32 = 1;
pub const TYPE_HELLO: u32 = 2;
pub const TYPE_BYE: u32 = 3;
pub const TYPE_NEW_SERVER: u32 = 4;
pub const TYPE_DEL_SERVER: u32 = 5;
pub const TYPE_DEL_CLIENT: u32 = 6;
pub const TYPE_RESUME_TX: u32 = 7;
pub const TYPE_NEW_LOOKUP: u32 = 10;
pub const PORT_CTRL: u32 = 0xffff_fffe;
pub const NODE_BCAST: u32 = 0xffff_ffff;
pub const HEADER_LEN: usize = 32;

/// The v1 header: version 1, then these 7 little-endian words.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Header {
    pub ty: u32,
    pub src_node: u32,
    pub src_port: u32,
    pub confirm_rx: u32,
    pub size: u32,
    pub dst_node: u32,
    pub dst_port: u32,
}

pub fn encode(h: &Header, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(HEADER_LEN + payload.len() + 3);
    for w in [
        1,
        h.ty,
        h.src_node,
        h.src_port,
        h.confirm_rx,
        payload.len() as u32,
        h.dst_node,
        h.dst_port,
    ] {
        v.extend_from_slice(&w.to_le_bytes());
    }
    v.extend_from_slice(payload);
    while v.len() % 4 != 0 {
        v.push(0);
    }
    v
}

pub fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// A whole packet: its header and its payload (`size` bytes), or None if it is not a v1 packet.
pub fn decode(b: &[u8]) -> Option<(Header, &[u8])> {
    if le32(b, 0)? != 1 {
        return None;
    }
    let h = Header {
        ty: le32(b, 4)?,
        src_node: le32(b, 8)?,
        src_port: le32(b, 12)?,
        confirm_rx: le32(b, 16)?,
        size: le32(b, 20)?,
        dst_node: le32(b, 24)?,
        dst_port: le32(b, 28)?,
    };
    let payload = b.get(HEADER_LEN..HEADER_LEN + h.size as usize)?;
    Some((h, payload))
}

/// A control packet's payload: the command and up to 4 words (server: service, instance, node, port;
/// client: node, port).
pub fn ctrl(cmd: u32, words: [u32; 4]) -> Vec<u8> {
    let mut v = Vec::with_capacity(20);
    v.extend_from_slice(&cmd.to_le_bytes());
    for w in words {
        v.extend_from_slice(&w.to_le_bytes());
    }
    v
}

pub fn ctrl_words(p: &[u8]) -> Option<(u32, [u32; 4])> {
    Some((
        le32(p, 0)?,
        [
            le32(p, 4).unwrap_or(0),
            le32(p, 8).unwrap_or(0),
            le32(p, 12).unwrap_or(0),
            le32(p, 16).unwrap_or(0),
        ],
    ))
}

/// QRTR announces a service's version and instance in one word.
pub fn instance_word(version: u32, instance: u32) -> u32 {
    instance << 8 | version
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_padding() {
        let h = Header {
            ty: TYPE_DATA,
            src_node: 1,
            src_port: 14,
            dst_node: 0,
            dst_port: 9,
            ..Default::default()
        };
        let p = encode(&h, &[1, 2, 3]);
        assert_eq!(p.len(), 36);
        let (d, pay) = decode(&p).unwrap();
        assert_eq!(d.size, 3);
        assert_eq!(pay, &[1, 2, 3]);
        assert_eq!(d.dst_port, 9);
    }

    #[test]
    fn the_modems_new_server_from_c3() {
        // phone log 2026-11-08: "version 1 type 4 src 0:-2 confirm 0 size 20 dst 1:-2"
        let mut b = Vec::new();
        for w in [
            1u32,
            4,
            0,
            PORT_CTRL,
            0,
            20,
            1,
            PORT_CTRL,
            4,
            2,
            instance_word(1, 0),
            0,
            68,
        ] {
            b.extend_from_slice(&w.to_le_bytes());
        }
        let (h, p) = decode(&b).unwrap();
        assert_eq!(
            (h.ty, h.src_node, h.src_port),
            (TYPE_NEW_SERVER, 0, PORT_CTRL)
        );
        assert_eq!(ctrl_words(p), Some((4, [2, 1, 0, 68])));
    }
}
