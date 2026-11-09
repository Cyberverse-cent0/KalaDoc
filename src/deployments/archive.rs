//! Minimal archive writers used by release packaging.
//!
//! [`write_zip`] produces a valid ZIP archive with *stored* (uncompressed)
//! entries — no compression library is pulled in, and the output is
//! deterministic. Gzip-compressed tarballs are delegated to the system
//! `tar` binary (present on every supported CI runner), so this module
//! only owns the ZIP format.

use std::io::{self, Seek, Write};
use std::path::Path;

const CRC_TABLE: [u32; 256] = build_crc_table();

const fn build_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

/// IEEE CRC-32 (the variant used by ZIP and gzip).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = CRC_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

/// One file to place in a ZIP archive.
#[derive(Debug)]
pub struct ZipFile {
    /// Archive-relative path, `/`-separated (e.g. `pkg/kaladoc`).
    pub name: String,
    /// Unix mode including file type (e.g. `0o100755`); stored in the
    /// external attributes so `unzip` restores permissions.
    pub mode: u32,
    pub data: Vec<u8>,
}

impl ZipFile {
    pub fn new(name: impl Into<String>, data: Vec<u8>) -> Self {
        Self {
            name: name.into(),
            mode: 0o100644,
            data,
        }
    }

    pub fn executable(name: impl Into<String>, data: Vec<u8>) -> Self {
        Self {
            name: name.into(),
            mode: 0o100755,
            data,
        }
    }
}

/// DOS date/time for 1980-01-01 00:00:00 — keeps output deterministic.
const DOS_TIME: u16 = 0;
const DOS_DATE: u16 = 33; // 1980-01-01

/// Writes `files` as a stored (uncompressed) ZIP archive at `path`.
///
/// Entries are written in the order given; callers should sort for
/// reproducible output. Returns an error if the archive would exceed
/// the ZIP32 limits (4 GiB / 65535 entries) — release artifacts are far
/// below that.
pub fn write_zip(path: &Path, files: &[ZipFile]) -> io::Result<()> {
    let mut out = io::BufWriter::new(std::fs::File::create(path)?);
    let mut central = Vec::new();

    for file in files {
        let name = file.name.as_bytes();
        if name.len() > u16::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("zip entry name too long: {}", file.name),
            ));
        }
        let offset = out.stream_position()?;
        let crc = crc32(&file.data);
        let size = file.data.len() as u32;

        // Local file header.
        out.write_all(&0x0403_4b50u32.to_le_bytes())?;
        out.write_all(&20u16.to_le_bytes())?; // version needed
        out.write_all(&0u16.to_le_bytes())?; // flags
        out.write_all(&0u16.to_le_bytes())?; // method: stored
        out.write_all(&DOS_TIME.to_le_bytes())?;
        out.write_all(&DOS_DATE.to_le_bytes())?;
        out.write_all(&crc.to_le_bytes())?;
        out.write_all(&size.to_le_bytes())?;
        out.write_all(&size.to_le_bytes())?;
        out.write_all(&(name.len() as u16).to_le_bytes())?;
        out.write_all(&0u16.to_le_bytes())?; // extra length
        out.write_all(name)?;
        out.write_all(&file.data)?;

        // Central directory record (assembled after all locals).
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0u16.to_le_bytes()); // flags
        central.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        central.extend_from_slice(&DOS_TIME.to_le_bytes());
        central.extend_from_slice(&DOS_DATE.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra len
        central.extend_from_slice(&0u16.to_le_bytes()); // comment len
        central.extend_from_slice(&0u16.to_le_bytes()); // disk start
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&(file.mode << 16).to_le_bytes());
        central.extend_from_slice(&(offset as u32).to_le_bytes());
        central.extend_from_slice(name);
    }

    let cd_offset = out.stream_position()?;
    let cd_size = central.len() as u64;
    out.write_all(&central)?;

    // End of central directory record.
    let count = files.len() as u16;
    out.write_all(&0x0605_4b50u32.to_le_bytes())?;
    out.write_all(&0u16.to_le_bytes())?; // disk
    out.write_all(&0u16.to_le_bytes())?; // cd start disk
    out.write_all(&count.to_le_bytes())?;
    out.write_all(&count.to_le_bytes())?;
    out.write_all(&(cd_size as u32).to_le_bytes())?;
    out.write_all(&(cd_offset as u32).to_le_bytes())?;
    out.write_all(&0u16.to_le_bytes())?; // comment length
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le_u16(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([b[at], b[at + 1]])
    }

    fn le_u32(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }

    #[test]
    fn crc32_known_vectors() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// Parses the archive back and checks every entry's name, CRC and
    /// payload — the same checks `unzip -t` performs.
    #[test]
    fn written_zip_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.zip");
        let files = vec![
            ZipFile::executable("pkg/kaladoc", b"#!/bin/sh\necho hi\n".to_vec()),
            ZipFile::new("pkg/README.md", b"# docs\n".to_vec()),
        ];
        write_zip(&path, &files).unwrap();

        let bytes = std::fs::read(&path).unwrap();

        // EOCD is the last 22 bytes.
        let eocd = bytes.len() - 22;
        assert_eq!(le_u32(&bytes, eocd), 0x0605_4b50);
        assert_eq!(le_u16(&bytes, eocd + 10) as usize, files.len());
        let cd_offset = le_u32(&bytes, eocd + 16) as usize;
        let cd_size = le_u32(&bytes, eocd + 12) as usize;
        assert_eq!(cd_offset + cd_size, eocd);

        // Walk the central directory.
        let mut at = cd_offset;
        let mut seen = Vec::new();
        for _ in 0..files.len() {
            assert_eq!(le_u32(&bytes, at), 0x0201_4b50);
            let method = le_u16(&bytes, at + 10);
            assert_eq!(method, 0, "entries must be stored");
            let crc = le_u32(&bytes, at + 16);
            let size = le_u32(&bytes, at + 20) as usize;
            let name_len = le_u16(&bytes, at + 28) as usize;
            let extra_len = le_u16(&bytes, at + 30) as usize;
            let comment_len = le_u16(&bytes, at + 32) as usize;
            let local_offset = le_u32(&bytes, at + 42) as usize;
            let name = String::from_utf8(bytes[at + 46..at + 46 + name_len].to_vec()).unwrap();
            seen.push(name.clone());

            // Local header at local_offset: name len at +26, data after.
            assert_eq!(le_u32(&bytes, local_offset), 0x0403_4b50);
            let l_name_len = le_u16(&bytes, local_offset + 26) as usize;
            let l_extra_len = le_u16(&bytes, local_offset + 28) as usize;
            let data_start = local_offset + 30 + l_name_len + l_extra_len;
            let data = &bytes[data_start..data_start + size];
            assert_eq!(crc32(data), crc, "crc mismatch for {name}");
            assert_eq!(le_u32(&bytes, local_offset + 14), crc);

            at += 46 + name_len + extra_len + comment_len;
        }
        assert_eq!(seen, vec!["pkg/kaladoc", "pkg/README.md"]);
        assert_eq!(at, eocd, "central directory must end at EOCD");
    }

    #[test]
    fn rejects_oversized_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.zip");
        let files = vec![ZipFile::new("x".repeat(70_000), vec![1])];
        assert!(write_zip(&path, &files).is_err());
    }
}
