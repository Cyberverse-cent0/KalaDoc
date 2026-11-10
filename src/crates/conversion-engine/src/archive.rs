//! Minimal read-only ZIP reader for comic-book (CBZ) archives plus a
//! self-contained RFC 1951 DEFLATE decompressor.
//!
//! The crate deliberately has no `zip`/`flate2` dependency, so this
//! module implements just enough of APPNOTE 6.3.x and RFC 1951 to
//! extract stored (method 0) and deflated (method 8) entries from an
//! in-memory archive. The following are rejected with
//! [`ConvertError::InvalidInput`]:
//!
//! - ZIP64 archives (any 0xFFFF/0xFFFFFFFF marker field)
//! - multi-disk archives
//! - entries with the encryption flag set
//! - compression methods other than stored/deflate
//! - truncated or structurally corrupt archives
//!
//! All bounds are checked before slicing; decompressed output is
//! capped at the entry's declared uncompressed size so a corrupt
//! stream cannot expand without limit (callers additionally enforce
//! aggregate archive budgets before calling [`ZipArchive::read_entry`]).

use crate::crates::conversion_engine::error::{ConvertError, ConvertResult};

// --- ZIP structure signatures (little-endian u32 at offset 0) ---------

const SIG_LOCAL_HEADER: u32 = 0x0403_4b50;
const SIG_CENTRAL_HEADER: u32 = 0x0201_4b50;
const SIG_EOCD: u32 = 0x0605_4b50;

/// General-purpose bit flag: entry is encrypted.
const FLAG_ENCRYPTED: u16 = 0x0001;

/// Compression method: stored (no compression).
const METHOD_STORED: u16 = 0;
/// Compression method: DEFLATE.
const METHOD_DEFLATE: u16 = 8;

/// Maximum bytes scanned backwards from EOF for the EOCD record
/// (22-byte fixed part + maximum 64 KiB comment).
const EOCD_SEARCH_WINDOW: usize = 22 + 0xFFFF;

fn invalid(reason: impl Into<String>) -> ConvertError {
    ConvertError::InvalidInput {
        reason: reason.into(),
    }
}

fn u16le(data: &[u8], off: usize) -> Option<u16> {
    let b = data.get(off..off + 2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn u32le(data: &[u8], off: usize) -> Option<u32> {
    let b = data.get(off..off + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// One central-directory record: metadata for a single archive member.
#[derive(Debug, Clone)]
pub struct ZipEntry {
    /// Member name (lossy UTF-8; ZIP names are not guaranteed UTF-8).
    pub name: String,
    /// General-purpose bit flags.
    pub flags: u16,
    /// Compression method (0 = stored, 8 = deflate).
    pub method: u16,
    /// CRC-32 (IEEE) of the uncompressed data.
    pub crc32: u32,
    /// Compressed size in bytes.
    pub compressed_size: u64,
    /// Declared uncompressed size in bytes.
    pub uncompressed_size: u64,
    /// True for directory entries (name ends with `/`).
    pub is_dir: bool,
}

/// A parsed ZIP archive borrowing its source bytes.
#[derive(Debug)]
pub struct ZipArchive<'a> {
    data: &'a [u8],
    entries: Vec<ZipEntry>,
    /// Local-header file offset for each entry (parallel to `entries`).
    local_offsets: Vec<u64>,
}

impl<'a> ZipArchive<'a> {
    /// Parse the end-of-central-directory record and central
    /// directory of `data`. Does not decompress anything.
    pub fn open(data: &'a [u8]) -> ConvertResult<Self> {
        if data.len() < 22 {
            return Err(invalid("input is too small to be a ZIP archive"));
        }

        // Locate the EOCD record by scanning backwards; the signature
        // can appear inside the comment, so the first hit from the end
        // whose comment length reaches EOF is authoritative.
        let window_start = data.len().saturating_sub(EOCD_SEARCH_WINDOW);
        let mut eocd = None;
        for off in (window_start..=data.len() - 22).rev() {
            if u32le(data, off) == Some(SIG_EOCD) {
                let comment_len = u16le(data, off + 20).unwrap_or(0) as usize;
                if off + 22 + comment_len == data.len() {
                    eocd = Some(off);
                    break;
                }
            }
        }
        let eocd = eocd.ok_or_else(|| {
            invalid("no end-of-central-directory record found (not a ZIP archive?)")
        })?;

        // Multi-disk archives are not supported.
        if u16le(data, eocd + 4) != Some(0) || u16le(data, eocd + 6) != Some(0) {
            return Err(invalid("multi-disk ZIP archives are not supported"));
        }
        let entry_count = u16le(data, eocd + 10).unwrap_or(0) as usize;
        if entry_count == 0xFFFF {
            return Err(invalid("ZIP64 archives are not supported"));
        }
        let cd_size = u32le(data, eocd + 12).unwrap_or(0) as usize;
        let cd_offset = u32le(data, eocd + 16).unwrap_or(0) as usize;
        if cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF {
            return Err(invalid("ZIP64 archives are not supported"));
        }
        let cd_end = cd_offset
            .checked_add(cd_size)
            .ok_or_else(|| invalid("corrupt central directory size"))?;
        if cd_end > data.len() {
            return Err(invalid("central directory extends past end of input"));
        }

        let mut entries = Vec::with_capacity(entry_count.min(1024));
        let mut local_offsets = Vec::with_capacity(entry_count.min(1024));
        let mut pos = cd_offset;
        for _ in 0..entry_count {
            if pos + 46 > data.len() || u32le(data, pos) != Some(SIG_CENTRAL_HEADER) {
                return Err(invalid("corrupt central directory entry"));
            }
            let flags = u16le(data, pos + 8).unwrap_or(0);
            let method = u16le(data, pos + 10).unwrap_or(0);
            let crc32 = u32le(data, pos + 16).unwrap_or(0);
            let compressed_size = u32le(data, pos + 20).unwrap_or(0) as u64;
            let uncompressed_size = u32le(data, pos + 24).unwrap_or(0) as u64;
            let name_len = u16le(data, pos + 28).unwrap_or(0) as usize;
            let extra_len = u16le(data, pos + 30).unwrap_or(0) as usize;
            let comment_len = u16le(data, pos + 32).unwrap_or(0) as usize;
            let local_offset = u32le(data, pos + 42).unwrap_or(0) as u64;
            if compressed_size == 0xFFFF_FFFF
                || uncompressed_size == 0xFFFF_FFFF
                || local_offset == 0xFFFF_FFFF
            {
                return Err(invalid("ZIP64 archives are not supported"));
            }
            let name_start = pos + 46;
            let name_end = name_start
                .checked_add(name_len)
                .ok_or_else(|| invalid("corrupt central directory entry"))?;
            if name_end > data.len() {
                return Err(invalid("central directory entry name out of bounds"));
            }
            let name = String::from_utf8_lossy(&data[name_start..name_end]).into_owned();
            entries.push(ZipEntry {
                is_dir: name.ends_with('/'),
                name,
                flags,
                method,
                crc32,
                compressed_size,
                uncompressed_size,
            });
            local_offsets.push(local_offset);
            pos = name_end
                .checked_add(extra_len)
                .and_then(|p| p.checked_add(comment_len))
                .ok_or_else(|| invalid("corrupt central directory entry"))?;
        }

        Ok(Self {
            data,
            entries,
            local_offsets,
        })
    }

    /// Central-directory entries in archive order.
    pub fn entries(&self) -> &[ZipEntry] {
        &self.entries
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the archive has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Decompress and return the full contents of entry `index`.
    ///
    /// Encrypted entries, unsupported compression methods, truncated
    /// data, and CRC mismatches all produce [`ConvertError::InvalidInput`].
    pub fn read_entry(&self, index: usize) -> ConvertResult<Vec<u8>> {
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| invalid(format!("archive entry index {index} out of range")))?;
        if entry.flags & FLAG_ENCRYPTED != 0 {
            return Err(invalid(format!(
                "archive entry \"{}\" is encrypted",
                entry.name
            )));
        }

        // Locate the local header to find where the data starts; the
        // central directory sizes are authoritative for the data span.
        let off = self.local_offsets[index] as usize;
        if off + 30 > self.data.len() || u32le(self.data, off) != Some(SIG_LOCAL_HEADER) {
            return Err(invalid(format!(
                "corrupt local header for archive entry \"{}\"",
                entry.name
            )));
        }
        let name_len = u16le(self.data, off + 26).unwrap_or(0) as usize;
        let extra_len = u16le(self.data, off + 28).unwrap_or(0) as usize;
        let data_start = off
            .checked_add(30)
            .and_then(|p| p.checked_add(name_len))
            .and_then(|p| p.checked_add(extra_len))
            .ok_or_else(|| invalid("corrupt local header"))?;
        let data_end = data_start
            .checked_add(entry.compressed_size as usize)
            .ok_or_else(|| invalid("compressed size overflow"))?;
        let compressed = self.data.get(data_start..data_end).ok_or_else(|| {
            invalid(format!(
                "archive entry \"{}\" data out of bounds",
                entry.name
            ))
        })?;

        let plain = match entry.method {
            METHOD_STORED => compressed.to_vec(),
            METHOD_DEFLATE => inflate(compressed, entry.uncompressed_size as usize)?,
            other => {
                return Err(invalid(format!(
                    "archive entry \"{}\" uses unsupported compression method {other}",
                    entry.name
                )));
            }
        };

        if crc32(&plain) != entry.crc32 {
            return Err(invalid(format!(
                "CRC-32 mismatch for archive entry \"{}\"",
                entry.name
            )));
        }
        Ok(plain)
    }
}

// --- CRC-32 (IEEE 802.3, polynomial 0xEDB88320) ------------------------

const fn crc_table() -> [u32; 256] {
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

static CRC_TABLE: [u32; 256] = crc_table();

pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    !c
}

// --- DEFLATE (RFC 1951) decompressor ------------------------------------

/// Bit reader over the deflate stream, LSB-first per RFC 1951 §3.1.1.
struct BitReader<'a> {
    data: &'a [u8],
    /// Index of the next unread input byte.
    byte: usize,
    /// Bits already loaded from `data`, consumed LSB-first.
    bitbuf: u64,
    /// Number of valid bits currently in `bitbuf`.
    bitcnt: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte: 0,
            bitbuf: 0,
            bitcnt: 0,
        }
    }

    /// Read `n` bits (n <= 32), LSB-first.
    fn bits(&mut self, n: u32) -> Result<u32, ConvertError> {
        debug_assert!(n <= 32);
        while self.bitcnt < n {
            let b = *self
                .data
                .get(self.byte)
                .ok_or_else(|| invalid("unexpected end of deflate stream"))?;
            self.bitbuf |= (b as u64) << self.bitcnt;
            self.byte += 1;
            self.bitcnt += 8;
        }
        let mask = if n == 32 {
            u32::MAX as u64
        } else {
            (1u64 << n) - 1
        };
        let v = (self.bitbuf & mask) as u32;
        self.bitbuf >>= n;
        self.bitcnt -= n;
        Ok(v)
    }

    /// Discard bits up to the next byte boundary.
    fn align_byte(&mut self) -> Result<(), ConvertError> {
        // `bitcnt % 8` is the number of bits of the partially consumed
        // byte still buffered; dropping them lands on a byte edge.
        let drop = self.bitcnt % 8;
        if drop > 0 {
            self.bits(drop)?;
        }
        Ok(())
    }

    /// Copy `n` bytes into `out`. Only valid at a byte boundary.
    fn read_aligned(&mut self, out: &mut Vec<u8>, n: usize) -> Result<(), ConvertError> {
        debug_assert_eq!(self.bitcnt % 8, 0);
        // Drain whole buffered bytes first.
        while self.bitcnt >= 8 {
            out.push((self.bitbuf & 0xFF) as u8);
            self.bitbuf >>= 8;
            self.bitcnt -= 8;
        }
        let end = self
            .byte
            .checked_add(n)
            .ok_or_else(|| invalid("deflate stored-block length overflow"))?;
        let slice = self
            .data
            .get(self.byte..end)
            .ok_or_else(|| invalid("unexpected end of deflate stream"))?;
        out.extend_from_slice(slice);
        self.byte = end;
        Ok(())
    }
}

/// Canonical Huffman table per RFC 1951 §3.2.2.
struct Huffman {
    /// Number of codes of each length (index 0 unused).
    counts: [u16; 16],
    /// Symbols ordered by (code length, symbol value).
    symbols: Vec<u16>,
}

impl Huffman {
    fn build(lengths: &[u8]) -> Result<Self, ConvertError> {
        let mut counts = [0u16; 16];
        for &len in lengths {
            if len > 15 {
                return Err(invalid("deflate code length exceeds 15 bits"));
            }
            if len != 0 {
                counts[len as usize] += 1;
            }
        }
        // Kraft inequality check: reject over-subscribed trees.
        let mut left: i32 = 1;
        for len in 1..=15 {
            left <<= 1;
            left -= counts[len] as i32;
            if left < 0 {
                return Err(invalid("deflate Huffman code is over-subscribed"));
            }
        }
        let total: usize = counts[1..].iter().map(|&c| c as usize).sum();
        let mut offs = [0usize; 17];
        for len in 1..=15 {
            offs[len + 1] = offs[len] + counts[len] as usize;
        }
        let mut symbols = vec![0u16; total];
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                symbols[offs[len as usize]] = sym as u16;
                offs[len as usize] += 1;
            }
        }
        Ok(Self { counts, symbols })
    }

    fn decode(&self, br: &mut BitReader) -> Result<u16, ConvertError> {
        if self.symbols.is_empty() {
            return Err(invalid("deflate symbol decoded with empty Huffman table"));
        }
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..=15usize {
            code |= br.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(invalid("invalid deflate Huffman code"))
    }
}

/// Length codes 257..=285 → (base length, extra bits). RFC 1951 §3.2.5.
const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// Distance codes 0..=29 → (base distance, extra bits). RFC 1951 §3.2.5.
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Order in which code-length-code lengths are transmitted (RFC 1951 §3.2.7).
const CL_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Push a byte/literal, enforcing the output cap.
fn push_limited(out: &mut Vec<u8>, byte: u8, max_out: usize) -> Result<(), ConvertError> {
    if out.len() >= max_out {
        return Err(invalid("deflate output exceeds declared uncompressed size"));
    }
    out.push(byte);
    Ok(())
}

/// Decode one deflate block body (everything after the 3-bit header)
/// until the end-of-block symbol. Returns when symbol 256 is read.
fn decode_block(
    br: &mut BitReader,
    lit: &Huffman,
    dist: &Huffman,
    out: &mut Vec<u8>,
    max_out: usize,
) -> Result<(), ConvertError> {
    loop {
        let sym = lit.decode(br)?;
        match sym {
            0..=255 => push_limited(out, sym as u8, max_out)?,
            256 => return Ok(()),
            257..=285 => {
                let li = (sym - 257) as usize;
                let mut length = LEN_BASE[li] as usize;
                let extra = LEN_EXTRA[li];
                if extra > 0 {
                    length += br.bits(extra as u32)? as usize;
                }
                if length == 0 || length > 258 {
                    return Err(invalid("invalid deflate length code"));
                }
                let dsym = dist.decode(br)? as usize;
                if dsym >= DIST_BASE.len() {
                    return Err(invalid("invalid deflate distance code"));
                }
                let mut distance = DIST_BASE[dsym] as usize;
                let dextra = DIST_EXTRA[dsym];
                if dextra > 0 {
                    distance += br.bits(dextra as u32)? as usize;
                }
                if distance == 0 || distance > out.len() {
                    return Err(invalid("deflate distance beyond start of output"));
                }
                if out.len() + length > max_out {
                    return Err(invalid("deflate output exceeds declared uncompressed size"));
                }
                // LZ77 copy; overlapping ranges repeat byte-by-byte.
                for _ in 0..length {
                    let b = out[out.len() - distance];
                    out.push(b);
                }
            }
            _ => return Err(invalid("invalid deflate literal/length symbol")),
        }
    }
}

/// Parse a dynamic-Huffman block header and return the literal/length
/// and distance tables.
fn dynamic_tables(br: &mut BitReader) -> Result<(Huffman, Huffman), ConvertError> {
    let hlit = br.bits(5)? as usize + 257;
    let hdist = br.bits(5)? as usize + 1;
    let hclen = br.bits(4)? as usize + 4;
    if hlit > 286 || hdist > 32 {
        return Err(invalid("invalid deflate dynamic header sizes"));
    }

    let mut cl_lengths = [0u8; 19];
    for i in 0..hclen {
        cl_lengths[CL_ORDER[i]] = br.bits(3)? as u8;
    }
    let cl_huff = Huffman::build(&cl_lengths)?;

    let total = hlit + hdist;
    let mut lengths = vec![0u8; total];
    let mut i = 0usize;
    while i < total {
        let sym = cl_huff.decode(br)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(invalid("deflate repeat with no previous length"));
                }
                let prev = lengths[i - 1];
                let rep = 3 + br.bits(2)? as usize;
                if i + rep > total {
                    return Err(invalid("deflate code lengths overflow"));
                }
                for slot in lengths.iter_mut().take(i + rep).skip(i) {
                    *slot = prev;
                }
                i += rep;
            }
            17 | 18 => {
                let (base, extra) = if sym == 17 { (3usize, 3u32) } else { (11, 7) };
                let rep = base + br.bits(extra)? as usize;
                if i + rep > total {
                    return Err(invalid("deflate code lengths overflow"));
                }
                i += rep; // lengths already zero
            }
            _ => return Err(invalid("invalid deflate code-length symbol")),
        }
    }

    let lit_lengths = &lengths[..hlit];
    // The literal/length alphabet must contain an end-of-block code.
    if lit_lengths.iter().all(|&l| l == 0) || lit_lengths.get(256).copied().unwrap_or(0) == 0 {
        return Err(invalid("deflate stream missing end-of-block code"));
    }
    let lit = Huffman::build(lit_lengths)?;
    let dist = Huffman::build(&lengths[hlit..])?;
    Ok((lit, dist))
}

/// Fixed-Huffman literal/length and distance tables (RFC 1951 §3.2.6).
fn fixed_tables() -> Result<(Huffman, Huffman), ConvertError> {
    let mut lit_lengths = [0u8; 288];
    for (i, l) in lit_lengths.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let dist_lengths = [5u8; 32];
    Ok((
        Huffman::build(&lit_lengths)?,
        Huffman::build(&dist_lengths)?,
    ))
}

/// Decompress a raw DEFLATE stream (no zlib wrapper), producing at
/// most `max_out` bytes. Errors on truncation, corrupt codes, or
/// output exceeding `max_out`.
fn inflate(input: &[u8], max_out: usize) -> ConvertResult<Vec<u8>> {
    let mut br = BitReader::new(input);
    let mut out: Vec<u8> = Vec::new();
    let mut fixed: Option<(Huffman, Huffman)> = None;

    loop {
        let bfinal = br.bits(1)?;
        let btype = br.bits(2)?;
        match btype {
            0 => {
                // Stored block: align, then LEN/NLEN + raw bytes.
                br.align_byte()?;
                let len = br.bits(16)? as usize;
                let nlen = br.bits(16)? as usize;
                if len ^ nlen != 0xFFFF {
                    return Err(invalid("deflate stored block LEN/NLEN mismatch"));
                }
                if out.len() + len > max_out {
                    return Err(invalid("deflate output exceeds declared uncompressed size"));
                }
                br.read_aligned(&mut out, len)?;
            }
            1 => {
                if fixed.is_none() {
                    fixed = Some(fixed_tables()?);
                }
                let (lit, dist) = fixed.as_ref().unwrap();
                decode_block(&mut br, lit, dist, &mut out, max_out)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut br)?;
                decode_block(&mut br, &lit, &dist, &mut out, max_out)?;
            }
            _ => return Err(invalid("reserved deflate block type")),
        }
        if bfinal == 1 {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrap raw member data into a minimal single-entry ZIP (stored
    /// method) and return the archive bytes.
    fn stored_zip(name: &str, data: &[u8]) -> Vec<u8> {
        let name_b = name.as_bytes();
        let crc = crc32(data);
        let mut out = Vec::new();
        // Local file header.
        out.extend_from_slice(&SIG_LOCAL_HEADER.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&METHOD_STORED.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0u16.to_le_bytes()); // date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name_b);
        out.extend_from_slice(data);
        // Central directory.
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&SIG_CENTRAL_HEADER.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version made by
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&METHOD_STORED.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra
        out.extend_from_slice(&0u16.to_le_bytes()); // comment
        out.extend_from_slice(&0u16.to_le_bytes()); // disk
        out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        out.extend_from_slice(&0u32.to_le_bytes()); // local offset
        out.extend_from_slice(name_b);
        let cd_size = out.len() as u32 - cd_offset;
        // EOCD.
        out.extend_from_slice(&SIG_EOCD.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // entries on disk
        out.extend_from_slice(&1u16.to_le_bytes()); // total entries
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        out
    }

    /// Wrap `data` as a single-entry ZIP using a *deflated* member.
    /// `deflated` must be a raw RFC 1951 stream.
    fn deflate_zip(name: &str, deflated: &[u8], plain: &[u8]) -> Vec<u8> {
        let name_b = name.as_bytes();
        let crc = crc32(plain);
        let mut out = Vec::new();
        out.extend_from_slice(&SIG_LOCAL_HEADER.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&METHOD_DEFLATE.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(deflated.len() as u32).to_le_bytes());
        out.extend_from_slice(&(plain.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name_b);
        out.extend_from_slice(deflated);
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&SIG_CENTRAL_HEADER.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&METHOD_DEFLATE.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(deflated.len() as u32).to_le_bytes());
        out.extend_from_slice(&(plain.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(name_b);
        let cd_size = out.len() as u32 - cd_offset;
        out.extend_from_slice(&SIG_EOCD.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// Build a raw deflate stream made only of stored (uncompressed)
    /// blocks — legal method-8 data that exercises `inflate`'s
    /// stored-block path without needing a compressor.
    fn deflate_stored(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut chunks = data.chunks(65535).peekable();
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            // Block header: BFINAL(1) + BTYPE=00, byte-aligned, then
            // LEN/NLEN little-endian and raw bytes.
            out.push(if last { 0x01 } else { 0x00 });
            let len = chunk.len() as u16;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(chunk);
        }
        if data.is_empty() {
            // Empty final stored block.
            out.push(0x01);
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0xFFFFu16.to_le_bytes());
        }
        out
    }

    #[test]
    fn crc32_known_vector() {
        // CRC-32/IEEE of "123456789" is 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn stored_zip_roundtrip() {
        let data = b"hello comic book world";
        let zip = stored_zip("page1.txt", data);
        let archive = ZipArchive::open(&zip).unwrap();
        assert_eq!(archive.len(), 1);
        assert_eq!(archive.entries()[0].name, "page1.txt");
        assert_eq!(archive.read_entry(0).unwrap(), data);
    }

    #[test]
    fn deflate_stored_blocks_roundtrip() {
        let data = b"deflate me please, deflate me please, deflate me!";
        let stream = deflate_stored(data);
        let out = inflate(&stream, data.len() + 16).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn deflate_stored_empty() {
        let out = inflate(&deflate_stored(b""), 16).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn deflate_output_cap_enforced() {
        let data = b"some payload bytes";
        let stream = deflate_stored(data);
        let err = inflate(&stream, data.len() - 1).unwrap_err();
        assert!(matches!(err, ConvertError::InvalidInput { .. }));
    }

    /// Raw RFC 1951 streams produced by zlib (wbits=-15) — covers
    /// fixed/dynamic Huffman paths that stored blocks cannot reach.
    #[test]
    fn deflate_real_zlib_vectors() {
        // zlib level 9 of "Hello Hello Hello Hello, this is a deflate
        // test payload!" (56 bytes).
        let hello = hex_bytes(
            "f348cdc9c957f0402775144a32328b158028512125352d27b12455a124b5b844a120b132273f31451100",
        );
        let out = inflate(&hello, 128).unwrap();
        assert_eq!(
            out,
            b"Hello Hello Hello Hello, this is a deflate test payload!"
        );

        // Highly repetitive 192-byte input compresses to a tiny
        // fixed-Huffman stream.
        let rep = hex_bytes("4b4c4a4e1cca0800");
        let expected = b"abcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabc".repeat(4);
        let out = inflate(&rep, 256).unwrap();
        assert_eq!(out, expected);
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn deflate_zip_roundtrip() {
        let data = b"compressed comic page bytes";
        let stream = deflate_stored(data);
        let zip = deflate_zip("page1.png", &stream, data);
        let archive = ZipArchive::open(&zip).unwrap();
        assert_eq!(archive.read_entry(0).unwrap(), data);
    }

    #[test]
    fn rejects_non_zip() {
        let err = ZipArchive::open(b"this is not a zip file at all..............").unwrap_err();
        assert!(matches!(err, ConvertError::InvalidInput { .. }));
    }

    #[test]
    fn rejects_truncated_zip() {
        let zip = stored_zip("a.txt", b"data");
        let truncated = &zip[..zip.len() - 10];
        assert!(ZipArchive::open(truncated).is_err());
    }

    #[test]
    fn crc_mismatch_detected() {
        let mut zip = stored_zip("a.txt", b"data");
        // Corrupt one payload byte (local data starts after the fixed
        // 30-byte header + name; find it via the EOCD math: local header
        // is at offset 0, data at 30 + name_len).
        let data_off = 30 + "a.txt".len();
        zip[data_off] ^= 0xFF;
        let archive = ZipArchive::open(&zip).unwrap();
        assert!(archive.read_entry(0).is_err());
    }
}
