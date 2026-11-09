// SPDX-License-Identifier: Apache-2.0
//! A read-only ext4 reader, just enough to find and read files: the superblock, the group descriptors (32- or
//! 64-byte), inodes, extent trees of any depth, and directories scanned linearly (htree directories included: their
//! index blocks look like one empty entry). For the firmware on the Pixel's `vendor_a` (docs/telephony.md C5b), which
//! no NeoSymbian code ever writes. Written from the on-disk layout in the kernel's Documentation/filesystems/ext4
//! (facts).
//! shortcut: no block-mapped (pre-extent) files, inline data or encryption: Android's vendor images use extents.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

/// Byte reads from the partition (on the phone: the kernel's read-only partition HAL).
pub trait Disk {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), i32>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Io(i32),
    NotExt4,
    NotFound,
    /// a layout this reader doesn't handle (block maps, inline data, a bad extent header)
    Unsupported,
}

impl From<i32> for Error {
    fn from(e: i32) -> Self {
        Error::Io(e)
    }
}

const SUPERBLOCK: u64 = 1024;
const MAGIC: u16 = 0xef53;
const INCOMPAT_64BIT: u32 = 0x80;
const EXTENTS_FL: u32 = 0x8_0000;
const INLINE_DATA_FL: u32 = 0x1000_0000;
const EXTENT_MAGIC: u16 = 0xf30a;
const ROOT: u32 = 2;
const MODE_DIR: u16 = 0x4000;

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

pub struct Fs<D: Disk> {
    disk: D,
    pub block_size: u32,
    inodes_per_group: u32,
    inode_size: u32,
    desc_size: u32,
    /// where the group descriptors start (the block after the superblock's)
    gdt: u64,
}

#[derive(Clone, Debug)]
pub struct Inode {
    pub mode: u16,
    pub size: u64,
    flags: u32,
    block: [u8; 60],
}

impl Inode {
    pub fn is_dir(&self) -> bool {
        self.mode & 0xf000 == MODE_DIR
    }
}

impl<D: Disk> Fs<D> {
    pub fn open(mut disk: D) -> Result<Self, Error> {
        let mut sb = [0u8; 1024];
        disk.read(SUPERBLOCK, &mut sb)?;
        if le16(&sb, 0x38) != MAGIC {
            return Err(Error::NotExt4);
        }
        let block_size = 1024u32 << le32(&sb, 0x18);
        let incompat = le32(&sb, 0x60);
        let desc_size = if incompat & INCOMPAT_64BIT != 0 {
            le16(&sb, 0xfe).max(32) as u32
        } else {
            32
        };
        Ok(Fs {
            disk,
            block_size,
            inodes_per_group: le32(&sb, 0x28),
            inode_size: le16(&sb, 0x58) as u32,
            desc_size,
            gdt: (le32(&sb, 0x14) as u64 + 1) * block_size as u64,
        })
    }

    pub fn inode(&mut self, ino: u32) -> Result<Inode, Error> {
        let (group, index) = (
            (ino - 1) / self.inodes_per_group,
            (ino - 1) % self.inodes_per_group,
        );
        let mut d = [0u8; 64];
        let d = &mut d[..self.desc_size as usize];
        self.disk
            .read(self.gdt + group as u64 * self.desc_size as u64, d)?;
        let mut table = le32(d, 0x08) as u64;
        if self.desc_size >= 64 {
            table |= (le32(d, 0x28) as u64) << 32;
        }
        let mut raw = [0u8; 160];
        let n = (self.inode_size as usize).min(raw.len());
        let at = table * self.block_size as u64 + index as u64 * self.inode_size as u64;
        self.disk.read(at, &mut raw[..n])?;
        let mut block = [0u8; 60];
        block.copy_from_slice(&raw[0x28..0x28 + 60]);
        Ok(Inode {
            mode: le16(&raw, 0),
            size: le32(&raw, 4) as u64 | (le32(&raw, 0x6c) as u64) << 32,
            flags: le32(&raw, 0x20),
            block,
        })
    }

    /// The physical block of logical block `lblk`, walking the extent tree; None in a hole or an unwritten extent.
    fn map(&mut self, inode: &Inode, lblk: u32) -> Result<Option<u64>, Error> {
        if inode.flags & EXTENTS_FL == 0 || inode.flags & INLINE_DATA_FL != 0 {
            return Err(Error::Unsupported);
        }
        let mut node: Vec<u8> = inode.block.to_vec();
        loop {
            if le16(&node, 0) != EXTENT_MAGIC {
                return Err(Error::Unsupported);
            }
            let (entries, depth) = (le16(&node, 2) as usize, le16(&node, 6));
            let entry = |i: usize| &node[12 + 12 * i..24 + 12 * i];
            // the last entry starting at or before lblk
            let Some(i) = (0..entries)
                .take_while(|&i| node.len() >= 24 + 12 * i)
                .filter(|&i| le32(entry(i), 0) <= lblk)
                .last()
            else {
                return Ok(None);
            };
            let e = entry(i);
            if depth == 0 {
                let (first, len) = (le32(e, 0), le16(e, 4) as u32);
                if len > 32768 {
                    return Ok(None); // unwritten (preallocated): reads as zeros
                }
                if lblk >= first + len {
                    return Ok(None);
                }
                let start = (le16(e, 6) as u64) << 32 | le32(e, 8) as u64;
                return Ok(Some(start + (lblk - first) as u64));
            }
            let leaf = (le16(e, 8) as u64) << 32 | le32(e, 4) as u64;
            node = vec![0; self.block_size as usize];
            self.disk.read(leaf * self.block_size as u64, &mut node)?;
        }
    }

    /// The whole file (`size` bytes; holes read as zeros).
    pub fn read(&mut self, inode: &Inode) -> Result<Vec<u8>, Error> {
        let bs = self.block_size as u64;
        let mut out = vec![0u8; inode.size as usize];
        for (n, chunk) in out.chunks_mut(bs as usize).enumerate() {
            if let Some(p) = self.map(inode, n as u32)? {
                self.disk.read(p * bs, chunk)?;
            }
        }
        Ok(out)
    }

    /// The inode number of `name` in the directory `dir`.
    pub fn lookup(&mut self, dir: &Inode, name: &str) -> Result<u32, Error> {
        let data = self.read(dir)?;
        for block in data.chunks(self.block_size as usize) {
            let mut at = 0;
            while at + 8 <= block.len() {
                let (ino, rec_len, name_len) = (
                    le32(block, at),
                    le16(block, at + 4) as usize,
                    block[at + 6] as usize,
                );
                if rec_len < 8 {
                    break;
                }
                if ino != 0 && block.get(at + 8..at + 8 + name_len) == Some(name.as_bytes()) {
                    return Ok(ino);
                }
                at += rec_len;
            }
        }
        Err(Error::NotFound)
    }

    /// The inode at `path` ("firmware/ipa_fws.mdt", from the filesystem's root).
    pub fn find(&mut self, path: &str) -> Result<Inode, Error> {
        let mut inode = self.inode(ROOT)?;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if !inode.is_dir() {
                return Err(Error::NotFound);
            }
            let ino = self.lookup(&inode, part)?;
            inode = self.inode(ino)?;
        }
        Ok(inode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};

    struct Image(File);
    impl Disk for Image {
        fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), i32> {
            self.0.seek(SeekFrom::Start(offset)).map_err(|_| -1)?;
            self.0.read_exact(buf).map_err(|_| -1)
        }
    }

    /// A raw (unsparsed) Android vendor image named by EXT4RO_IMAGE: never committed (it holds Qualcomm firmware),
    /// so the test is skipped without it.
    fn image() -> Option<Fs<Image>> {
        let path = std::env::var("EXT4RO_IMAGE").ok()?;
        Some(Fs::open(Image(File::open(path).ok()?)).unwrap())
    }

    #[test]
    fn vendor_image_has_the_ipa_firmware() {
        let Some(mut fs) = image() else {
            eprintln!("EXT4RO_IMAGE not set: skipped");
            return;
        };
        let mdt = fs.find("firmware/ipa_fws.mdt").unwrap();
        let data = fs.read(&mdt).unwrap();
        assert_eq!(&data[..4], b"\x7fELF");
        for n in 0..5 {
            let seg = fs.find(&std::format!("firmware/ipa_fws.b{n:02}")).unwrap();
            assert_eq!(fs.read(&seg).unwrap().len() as u64, seg.size);
        }
        assert!(matches!(
            fs.find("firmware/no_such_file"),
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn not_ext4() {
        struct Zeros;
        impl Disk for Zeros {
            fn read(&mut self, _: u64, buf: &mut [u8]) -> Result<(), i32> {
                buf.fill(0);
                Ok(())
            }
        }
        assert_eq!(Fs::open(Zeros).err(), Some(Error::NotExt4));
    }
}

/// Copies the IPA firmware out of EXT4RO_IMAGE into EXT4RO_OUT, to check the reader against another source by hand.
#[cfg(test)]
#[test]
fn dump_ipa_firmware() {
    let (Ok(img), Ok(out)) = (std::env::var("EXT4RO_IMAGE"), std::env::var("EXT4RO_OUT")) else {
        return;
    };
    struct Image(std::fs::File);
    impl Disk for Image {
        fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), i32> {
            use std::io::{Read, Seek, SeekFrom};
            self.0.seek(SeekFrom::Start(offset)).map_err(|_| -1)?;
            self.0.read_exact(buf).map_err(|_| -1)
        }
    }
    let mut fs = Fs::open(Image(std::fs::File::open(img).unwrap())).unwrap();
    for name in ["mdt", "b00", "b01", "b02", "b03", "b04"] {
        let i = fs.find(&std::format!("firmware/ipa_fws.{name}")).unwrap();
        std::fs::write(std::format!("{out}/ipa_fws.{name}"), fs.read(&i).unwrap()).unwrap();
    }
}
