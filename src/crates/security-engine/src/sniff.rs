//! Content sniffing via magic bytes.
//!
//! File extensions and caller-declared MIME types are attacker-controlled
//! metadata; the only trustworthy signal available before parsing is the
//! leading byte signature. [`sniff_format`] recognizes the formats this
//! application accepts for upload and returns
//! [`SniffedFormat::Unknown`] for anything else — callers decide whether
//! unknown content is acceptable (see
//! [`crate::crates::security_engine::validate_upload`]).
//!
//! Detection is intentionally conservative: a format is only reported
//! when its signature matches at offset 0. Truncated headers, garbage,
//! and polyglot files whose signature does not lead all return `Unknown`.

use std::fmt;

/// Video container families identified from magic bytes.
///
/// Distinct from [`SniffedFormat`] itself because several video
/// containers share one byte-level signature family (EBML covers both
/// Matroska and WebM); the container carries the disambiguated result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoContainer {
    /// ISO Base Media File Format: `....ftyp` box at offset 4 with a
    /// recognized MP4/MOV major brand (`mp4*`, `isom`, `qt  `, …).
    Mp4,
    /// EBML header (`1A 45 DF A3`) with Matroska DocType evidence, or
    /// EBML with no readable DocType — see [`sniff_format`].
    Matroska,
    /// EBML header with WebM DocType evidence.
    Webm,
    /// `RIFF....AVI ` — Audio Video Interleave.
    Avi,
    /// MPEG transport stream: `0x47` sync byte at each 188-byte packet
    /// boundary.
    MpegTs,
    /// MPEG program stream: `00 00 01 BA` pack header. Also covers
    /// DVD-Video `.vob` files, which are MPEG-PS streams and cannot be
    /// distinguished from `.mpg` by magic bytes alone.
    MpegPs,
    /// Flash Video: `FLV` + version byte `0x01`.
    Flv,
    /// Advanced Systems Format / Windows Media: the 16-byte ASF header
    /// GUID. ASF multiplexes audio and video; audio-only ASF (WMA)
    /// shares this signature and is reported here because the container
    /// family is identical — downstream probing is the arbiter.
    Asf,
    /// MPEG-1/2 elementary video stream: `00 00 01 B3` sequence header
    /// or `00 00 01 00` picture start (`.mpg`/`.mpeg`/`.m2v` family).
    MpegVideo,
    /// MXF (Material eXchange Format): partition-pack UL
    /// `06 0E 2B 34 …`.
    Mxf,
    /// Flash MP4 (`F4V ` major brand in an `ftyp` box).
    F4v,
    /// Ogg container carrying a video codec (Theora, or undetermined
    /// payload — see [`sniff_ogg`]). Ogg audio (Vorbis/Opus/…) is
    /// reported as [`SniffedFormat::Audio`] instead.
    Ogg,
}

/// Audio container families identified from magic bytes.
///
/// Mirrors [`VideoContainer`]: one enum per byte-level signature family,
/// disambiguated only where the leading bytes allow it cheaply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioContainer {
    /// MP3: `ID3` tag header, or an MPEG audio frame sync
    /// (`0xFF` + sync bits with a non-reserved layer field).
    Mp3,
    /// RIFF/WAVE: `RIFF` wrapper with `WAVE` form type at offset 8.
    Wav,
    /// `fLaC` — Free Lossless Audio Codec native stream.
    Flac,
    /// Ogg container carrying an audio codec (Vorbis, Opus, Speex,
    /// FLAC-in-Ogg). See [`sniff_ogg`].
    OggAudio,
    /// AAC ADTS: `0xFF` + ADTS sync pattern (`0xF0`/`0xF1`/`0xF8`/`0xF9`
    /// second byte — sync word plus reserved layer bits).
    Aac,
    /// `FORM` wrapper with `AIFF`/`AIFC` form type at offset 8.
    Aiff,
    /// `.snd` — Sun/NeXT audio.
    Au,
    /// `#!AMR` — Adaptive Multi-Rate narrowband.
    Amr,
    /// `caff` — Core Audio Format (Apple).
    Caf,
    /// `0x0B 0x77` — Dolby AC-3 / E-AC-3 sync word.
    Ac3,
}

/// A document or media format identified from magic bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SniffedFormat {
    /// `%PDF-` — PDF document.
    Pdf,
    /// `\x89PNG` — PNG image.
    Png,
    /// `FF D8 FF` — JPEG image.
    Jpeg,
    /// `PK\x03\x04` — ZIP archive (also covers docx/xlsx/jar family).
    Zip,
    /// `GIF8` — GIF image (87a/89a).
    Gif,
    /// Video container — see [`VideoContainer`] for the detected family.
    Video { container: VideoContainer },
    /// Audio container — see [`AudioContainer`] for the detected family.
    Audio { container: AudioContainer },
    /// No recognized signature.
    Unknown,
}

impl SniffedFormat {
    /// Conventional file extension including the dot, or `""` for
    /// [`SniffedFormat::Unknown`]. Suitable for building storage names.
    pub fn sniffed_extension(&self) -> &'static str {
        match self {
            Self::Pdf => ".pdf",
            Self::Png => ".png",
            Self::Jpeg => ".jpg",
            Self::Zip => ".zip",
            Self::Gif => ".gif",
            Self::Video { container } => container.sniffed_extension(),
            Self::Audio { container } => container.sniffed_extension(),
            Self::Unknown => "",
        }
    }

    /// Human-readable label for logs and error messages.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Pdf => "PDF",
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Zip => "ZIP",
            Self::Gif => "GIF",
            Self::Video { container } => container.label(),
            Self::Audio { container } => container.label(),
            Self::Unknown => "unknown",
        }
    }

    /// Whether the sniffed format is a video container. Useful for
    /// callers applying video-specific budgets without re-matching.
    pub fn is_video(&self) -> bool {
        matches!(self, Self::Video { .. })
    }

    /// Whether the sniffed format is an audio container. Audio uploads
    /// use the standard document budget (see
    /// [`crate::crates::security_engine::ResourceLimits::upload_cap_for`]) — they are small and
    /// not subject to the video cap.
    pub fn is_audio(&self) -> bool {
        matches!(self, Self::Audio { .. })
    }

    /// Conventional MIME type for the sniffed format.
    pub fn mime(&self) -> &'static str {
        match self {
            Self::Pdf => "application/pdf",
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Zip => "application/zip",
            Self::Gif => "image/gif",
            Self::Video { container } => container.mime(),
            Self::Audio { container } => container.mime(),
            Self::Unknown => "application/octet-stream",
        }
    }
}

impl VideoContainer {
    /// Conventional file extension including the dot.
    pub fn sniffed_extension(&self) -> &'static str {
        match self {
            Self::Mp4 => ".mp4",
            Self::Matroska => ".mkv",
            Self::Webm => ".webm",
            Self::Avi => ".avi",
            Self::MpegTs => ".ts",
            Self::MpegPs => ".mpg",
            Self::Flv => ".flv",
            Self::Asf => ".wmv",
            Self::MpegVideo => ".mpg",
            Self::Mxf => ".mxf",
            Self::F4v => ".f4v",
            Self::Ogg => ".ogv",
        }
    }

    /// Human-readable label for logs and error messages.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Mp4 => "MP4/MOV video",
            Self::Matroska => "Matroska video",
            Self::Webm => "WebM video",
            Self::Avi => "AVI video",
            Self::MpegTs => "MPEG-TS video",
            Self::MpegPs => "MPEG-PS video",
            Self::Flv => "FLV video",
            Self::Asf => "ASF/WMV video",
            Self::MpegVideo => "MPEG video",
            Self::Mxf => "MXF video",
            Self::F4v => "Flash video (F4V)",
            Self::Ogg => "Ogg video",
        }
    }

    /// Conventional MIME type for the container.
    pub fn mime(&self) -> &'static str {
        match self {
            Self::Mp4 => "video/mp4",
            Self::Matroska => "video/x-matroska",
            Self::Webm => "video/webm",
            Self::Avi => "video/x-msvideo",
            Self::MpegTs => "video/mp2t",
            Self::MpegPs | Self::MpegVideo => "video/mpeg",
            Self::Flv => "video/x-flv",
            Self::Asf => "video/x-ms-asf",
            Self::Mxf => "application/mxf",
            Self::F4v => "video/x-f4v",
            Self::Ogg => "video/ogg",
        }
    }
}

impl AudioContainer {
    /// Conventional file extension including the dot.
    pub fn sniffed_extension(&self) -> &'static str {
        match self {
            Self::Mp3 => ".mp3",
            Self::Wav => ".wav",
            Self::Flac => ".flac",
            Self::OggAudio => ".ogg",
            Self::Aac => ".aac",
            Self::Aiff => ".aiff",
            Self::Au => ".au",
            Self::Amr => ".amr",
            Self::Caf => ".caf",
            Self::Ac3 => ".ac3",
        }
    }

    /// Human-readable label for logs and error messages.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Mp3 => "MP3 audio",
            Self::Wav => "WAV audio",
            Self::Flac => "FLAC audio",
            Self::OggAudio => "Ogg audio",
            Self::Aac => "AAC audio",
            Self::Aiff => "AIFF audio",
            Self::Au => "AU audio",
            Self::Amr => "AMR audio",
            Self::Caf => "CAF audio",
            Self::Ac3 => "AC-3 audio",
        }
    }

    /// Conventional MIME type for the container.
    pub fn mime(&self) -> &'static str {
        match self {
            Self::Mp3 => "audio/mpeg",
            Self::Wav => "audio/wav",
            Self::Flac => "audio/flac",
            Self::OggAudio => "audio/ogg",
            Self::Aac => "audio/aac",
            Self::Aiff => "audio/aiff",
            Self::Au => "audio/basic",
            Self::Amr => "audio/amr",
            Self::Caf => "audio/x-caf",
            Self::Ac3 => "audio/ac3",
        }
    }
}

impl fmt::Display for SniffedFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Identify the format of `bytes` from its leading magic bytes.
///
/// Only the first few bytes are inspected; `bytes` may be a head sample
/// of a larger file. Returns [`SniffedFormat::Unknown`] when no known
/// signature matches or the input is too short.
pub fn sniff_format(bytes: &[u8]) -> SniffedFormat {
    if bytes.starts_with(b"%PDF-") {
        SniffedFormat::Pdf
    } else if bytes.starts_with(b"\x89PNG") {
        SniffedFormat::Png
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        SniffedFormat::Jpeg
    } else if bytes.starts_with(b"PK\x03\x04") {
        SniffedFormat::Zip
    } else if bytes.starts_with(b"GIF8") {
        SniffedFormat::Gif
    } else if bytes.starts_with(b"OggS") {
        // Ogg multiplexes audio and video; the codec is only visible in
        // the first page's payload, so this is classified separately.
        sniff_ogg(bytes)
    } else if let Some(container) = sniff_video_container(bytes) {
        SniffedFormat::Video { container }
    } else if let Some(container) = sniff_audio_container(bytes) {
        SniffedFormat::Audio { container }
    } else {
        SniffedFormat::Unknown
    }
}

/// Identify a video container from magic bytes, or `None`.
///
/// Signatures are checked in an order that avoids false positives:
/// the `ftyp` box and RIFF/AVI patterns are unambiguous at their
/// offsets; EBML is disambiguated by scanning the head for DocType
/// strings; MPEG-PS requires its 4-byte pack header; MPEG-TS is the
/// weakest check (see below).
fn sniff_video_container(bytes: &[u8]) -> Option<VideoContainer> {
    if let Some(container) = sniff_iso_bmff(bytes) {
        return Some(container);
    }
    if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Some(ebml_container(bytes));
    }
    // RIFF....AVI — WAV and WebP share the RIFF magic, so the form
    // type at offset 8 must be checked too.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"AVI " {
        return Some(VideoContainer::Avi);
    }
    // FLV: "FLV" magic plus version byte 0x01 at offset 3.
    if bytes.len() >= 4 && &bytes[0..3] == b"FLV" && bytes[3] == 0x01 {
        return Some(VideoContainer::Flv);
    }
    // ASF/WMV header GUID (also covers WMA audio — see enum docs).
    const ASF_GUID: [u8; 16] = [
        0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE,
        0x6C,
    ];
    if bytes.len() >= 16 && bytes[..16] == ASF_GUID {
        return Some(VideoContainer::Asf);
    }
    if bytes.starts_with(&[0x00, 0x00, 0x01, 0xBA]) {
        return Some(VideoContainer::MpegPs);
    }
    // MPEG-1/2 elementary video stream start codes. `00 00 01 00` is a
    // picture start — weaker evidence than the `B3` sequence header,
    // but both only occur at the head of an ES stream.
    if bytes.starts_with(&[0x00, 0x00, 0x01, 0xB3]) || bytes.starts_with(&[0x00, 0x00, 0x01, 0x00])
    {
        return Some(VideoContainer::MpegVideo);
    }
    // MXF partition-pack UL (canonical KLV key prefix).
    const MXF_UL: [u8; 16] = [
        0x06, 0x0E, 0x2B, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0D, 0x01, 0x02, 0x01, 0x01, 0x02, 0x04,
        0x00,
    ];
    if bytes.len() >= 16 && bytes[..16] == MXF_UL {
        return Some(VideoContainer::Mxf);
    }
    if is_mpeg_ts(bytes) {
        return Some(VideoContainer::MpegTs);
    }
    None
}

/// Classify an Ogg stream (`OggS` capture pattern already matched) as
/// audio or video by inspecting the first page's payload.
///
/// Ogg page layout: 27-byte header (`OggS` + version + header type +
/// granule + serial + sequence + CRC + segment count), then a segment
/// table of `nsegs` bytes, then the payload. Codec-identifying magic
/// sits at the start of the first payload packet:
///
/// - Vorbis: `0x01 "vorbis"`; Opus: `"OpusHead"`; Speex: `"Speex   "`;
///   FLAC-in-Ogg: `0x7F "FLAC"` → [`AudioContainer::OggAudio`]
/// - Theora: `0x80 "theora"`; Skeleton: `"fishead"` →
///   [`VideoContainer::Ogg`]
///
/// Best-effort: when the head sample is too short to reach the payload,
/// or the codec is unrecognized, the stream defaults to
/// [`AudioContainer::OggAudio`] — Ogg audio is the more common case and
/// the media engine's decoder remains the real arbiter. Documented
/// ambiguity, not a security decision: both outcomes are accepted media.
fn sniff_ogg(bytes: &[u8]) -> SniffedFormat {
    let audio = || SniffedFormat::Audio {
        container: AudioContainer::OggAudio,
    };
    // Need the full fixed page header plus at least one segment-table
    // byte to locate the payload.
    if bytes.len() < 28 {
        return audio();
    }
    let nsegs = bytes[26] as usize;
    let payload_start = 27usize.saturating_add(nsegs);
    let Some(payload) = bytes.get(payload_start..) else {
        return audio();
    };
    let head = &payload[..payload.len().min(16)];
    if head.starts_with(b"\x01vorbis")
        || head.starts_with(b"OpusHead")
        || head.starts_with(b"Speex")
        || head.starts_with(b"fLaC")
        || head.starts_with(b"\x7fFLAC")
    {
        audio()
    } else if head.starts_with(b"\x80theora") || head.starts_with(b"fishead") {
        SniffedFormat::Video {
            container: VideoContainer::Ogg,
        }
    } else {
        // Unrecognized codec: default to the audio family (see docs).
        audio()
    }
}

/// ISO-BMFF `ftyp` box: 4-byte size, `ftyp` at offset 4, major brand at
/// 8..12. Only recognized MP4/MOV/F4V brand families are accepted — an
/// `ftyp` box with an unknown brand (e.g. `heic`) is reported as
/// unknown rather than guessed at as video.
fn sniff_iso_bmff(bytes: &[u8]) -> Option<VideoContainer> {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return None;
    }
    let brand = &bytes[8..12];
    if brand == b"F4V " {
        return Some(VideoContainer::F4v);
    }
    if brand.starts_with(b"mp4")
        || matches!(
            brand,
            b"isom"
                | b"iso2"
                | b"iso3"
                | b"iso4"
                | b"iso5"
                | b"iso6"
                | b"iso8"
                | b"iso9"
                | b"avc1"
                | b"dash"
                | b"qt  "
                | b"M4V "
                | b"M4A "
        )
    {
        Some(VideoContainer::Mp4)
    } else {
        None
    }
}

/// Identify an audio container from magic bytes, or `None`.
///
/// Checked after the video sniffer: the only shared signature family
/// is RIFF (AVI video vs WAVE audio, disambiguated by the form type at
/// offset 8) and the ASF GUID (kept under video — see
/// [`VideoContainer::Asf`]).
///
/// Ordering inside this function matters: the ADTS AAC pattern is a
/// subset of the MPEG audio frame-sync space, so AAC is tested first;
/// `ID3` precedes both since a tagged MP3's first frame is not at
/// offset 0.
fn sniff_audio_container(bytes: &[u8]) -> Option<AudioContainer> {
    if bytes.starts_with(b"ID3") {
        return Some(AudioContainer::Mp3);
    }
    if bytes.starts_with(b"fLaC") {
        return Some(AudioContainer::Flac);
    }
    // RIFF....WAVE — mirrors the AVI check in `sniff_video_container`.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE" {
        return Some(AudioContainer::Wav);
    }
    // FORM....AIFF/AIFC — AIFF's RIFF-like wrapper uses "FORM".
    if bytes.len() >= 12
        && bytes.starts_with(b"FORM")
        && (&bytes[8..12] == b"AIFF" || &bytes[8..12] == b"AIFC")
    {
        return Some(AudioContainer::Aiff);
    }
    if bytes.starts_with(b".snd") {
        return Some(AudioContainer::Au);
    }
    if bytes.starts_with(b"#!AMR") {
        return Some(AudioContainer::Amr);
    }
    if bytes.starts_with(b"caff") {
        return Some(AudioContainer::Caf);
    }
    // Dolby AC-3/E-AC-3 sync word.
    if bytes.starts_with(&[0x0B, 0x77]) {
        return Some(AudioContainer::Ac3);
    }
    if bytes.len() >= 2 && bytes[0] == 0xFF {
        // ADTS AAC: 12-bit sync `0xFFF` followed by layer bits `00`
        // (second byte `0xF0`/`0xF1`/`0xF8`/`0xF9` — i.e. `(b & 0xF6)
        // == 0xF0`). Checked before the MP3 frame sync because ADTS is
        // the narrower pattern.
        if bytes[1] & 0xF6 == 0xF0 {
            return Some(AudioContainer::Aac);
        }
        // MPEG audio frame sync: `0xFF` + top 3 bits set, with a
        // non-reserved layer field (`(b & 0x06) != 0` excludes ADTS and
        // other layer-00 streams). JPEG (`FF D8 FF`) and ADTS were
        // already claimed by earlier sniffer branches.
        if bytes[1] & 0xE0 == 0xE0 && bytes[1] & 0x06 != 0 {
            return Some(AudioContainer::Mp3);
        }
    }
    None
}

/// EBML covers both Matroska (.mkv) and WebM; they are the same
/// container family distinguished only by the DocType element inside
/// the header. Sniffing scans the head window for the DocType strings
/// `webm`/`matroska`. When neither is visible (truncated or
/// non-canonical header), the file is still an EBML stream, so it is
/// conservatively reported as [`VideoContainer::Matroska`] — callers
/// must not treat the container label as proof of WebM; the video
/// engine's decoder is the real arbiter. This ambiguity is inherent:
/// security never trusts the filename extension to disambiguate.
fn ebml_container(bytes: &[u8]) -> VideoContainer {
    let window = &bytes[..bytes.len().min(64)];
    let has_matroska = window.windows(b"matroska".len()).any(|w| w == b"matroska");
    let has_webm = window.windows(b"webm".len()).any(|w| w == b"webm");
    if has_webm && !has_matroska {
        VideoContainer::Webm
    } else {
        // Matroska evidence, or neither string visible: report the
        // broader Matroska family rather than guessing WebM.
        VideoContainer::Matroska
    }
}

/// MPEG transport stream: `0x47` sync byte at offset 0 and, when the
/// buffer reaches the next packet boundary, at offset 188. When the
/// head sample is shorter than 189 bytes the second sync cannot be
/// checked, so a lone `0x47` is accepted — a deliberately weak check
/// documented here; downstream decoding is the real verification.
fn is_mpeg_ts(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes[0] != 0x47 {
        return false;
    }
    bytes.len() < 189 || bytes[188] == 0x47
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_each_format() {
        assert_eq!(sniff_format(b"%PDF-1.7\n..."), SniffedFormat::Pdf);
        assert_eq!(sniff_format(b"\x89PNG\r\n\x1a\n..."), SniffedFormat::Png);
        assert_eq!(
            sniff_format(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]),
            SniffedFormat::Jpeg
        );
        assert_eq!(sniff_format(b"PK\x03\x04\x14\x00"), SniffedFormat::Zip);
        assert_eq!(sniff_format(b"GIF89a...."), SniffedFormat::Gif);
        assert_eq!(sniff_format(b"GIF87a...."), SniffedFormat::Gif);
    }

    #[test]
    fn detects_video_containers() {
        // MP4/MOV: size-prefixed `ftyp` box at offset 4, brand at 8..12.
        let mp4 = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom";
        assert_eq!(
            sniff_format(mp4),
            SniffedFormat::Video {
                container: VideoContainer::Mp4
            }
        );
        let mov = b"\x00\x00\x00\x14ftypqt  \x00\x00\x00\x00qt  ";
        assert_eq!(
            sniff_format(mov),
            SniffedFormat::Video {
                container: VideoContainer::Mp4
            }
        );
        let isom = b"\x00\x00\x00\x20ftypisom\x00\x00\x02\x00isomiso2";
        assert_eq!(
            sniff_format(isom),
            SniffedFormat::Video {
                container: VideoContainer::Mp4
            }
        );

        // Matroska/WebM: EBML magic, DocType string distinguishes them.
        let mkv = b"\x1a\x45\xdf\xa3\x93\x42\x82\x88matroska\x42\x87\x81\x02\x42\xf7\x81\x01";
        assert_eq!(
            sniff_format(mkv),
            SniffedFormat::Video {
                container: VideoContainer::Matroska
            }
        );
        let webm = b"\x1a\x45\xdf\xa3\x9f\x42\x82\x84webm\x42\x87\x81\x04\x42\xf7\x81\x01";
        assert_eq!(
            sniff_format(webm),
            SniffedFormat::Video {
                container: VideoContainer::Webm
            }
        );
        // EBML header with no visible DocType → conservatively Matroska.
        let bare_ebml = b"\x1a\x45\xdf\xa3\x01\x00\x00\x00\x00\x00\x00\x00";
        assert_eq!(
            sniff_format(bare_ebml),
            SniffedFormat::Video {
                container: VideoContainer::Matroska
            }
        );

        // AVI: RIFF wrapper with "AVI " form type at offset 8.
        let avi = b"RIFF\x00\x00\x00\x00AVI LIST\x00\x00\x00\x00hdrl";
        assert_eq!(
            sniff_format(avi),
            SniffedFormat::Video {
                container: VideoContainer::Avi
            }
        );

        // MPEG-PS pack header.
        let ps = b"\x00\x00\x01\xba\x44\x00\x04\x00\x04\x01\x89\xc3\xf8";
        assert_eq!(
            sniff_format(ps),
            SniffedFormat::Video {
                container: VideoContainer::MpegPs
            }
        );

        // MPEG-TS: 0x47 sync at packet boundaries 0 and 188.
        let mut ts = vec![0u8; 376];
        ts[0] = 0x47;
        ts[188] = 0x47;
        assert_eq!(
            sniff_format(&ts),
            SniffedFormat::Video {
                container: VideoContainer::MpegTs
            }
        );
        // Short head (< 189 bytes): lone sync byte accepted, documented.
        assert_eq!(
            sniff_format(&[0x47, 0x40, 0x00, 0x10]),
            SniffedFormat::Video {
                container: VideoContainer::MpegTs
            }
        );
        // Sync at 0 but broken second sync → not TS.
        let mut bad_ts = vec![0u8; 376];
        bad_ts[0] = 0x47;
        bad_ts[188] = 0x00;
        assert_eq!(sniff_format(&bad_ts), SniffedFormat::Unknown);
    }

    #[test]
    fn detects_extended_video_containers() {
        // FLV: "FLV" + version byte 0x01.
        assert_eq!(
            sniff_format(b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00"),
            SniffedFormat::Video {
                container: VideoContainer::Flv
            }
        );
        // Wrong version byte → not FLV.
        assert_eq!(sniff_format(b"FLV\x02...."), SniffedFormat::Unknown);

        // ASF/WMV header GUID.
        let asf = b"\x30\x26\xb2\x75\x8e\x66\xcf\x11\xa6\xd9\x00\xaa\x00\x62\xce\x6c\x07\x01";
        assert_eq!(
            sniff_format(asf),
            SniffedFormat::Video {
                container: VideoContainer::Asf
            }
        );

        // MPEG-1/2 elementary video stream start codes.
        assert_eq!(
            sniff_format(b"\x00\x00\x01\xb3\x2d\x01\xe0\x24"),
            SniffedFormat::Video {
                container: VideoContainer::MpegVideo
            }
        );
        assert_eq!(
            sniff_format(b"\x00\x00\x01\x00\x00\x0f\xff\xff"),
            SniffedFormat::Video {
                container: VideoContainer::MpegVideo
            }
        );

        // MXF partition-pack UL.
        let mxf = b"\x06\x0e\x2b\x34\x02\x05\x01\x01\x0d\x01\x02\x01\x01\x02\x04\x00\x00";
        assert_eq!(
            sniff_format(mxf),
            SniffedFormat::Video {
                container: VideoContainer::Mxf
            }
        );

        // F4V: ftyp box with Flash major brand.
        let f4v = b"\x00\x00\x00\x14ftypF4V \x00\x00\x00\x00F4V isom";
        assert_eq!(
            sniff_format(f4v),
            SniffedFormat::Video {
                container: VideoContainer::F4v
            }
        );
    }

    #[test]
    fn detects_ogg_audio_and_video() {
        // Build a minimal Ogg page: 27-byte header + segment table + payload.
        let mut ogg_vorbis = b"OggS".to_vec();
        ogg_vorbis.extend_from_slice(&[0x00, 0x02]); // version, BOS header
        ogg_vorbis.extend_from_slice(&[0u8; 8]); // granule
        ogg_vorbis.extend_from_slice(&[1, 0, 0, 0]); // serial
        ogg_vorbis.extend_from_slice(&[0, 0, 0, 0]); // seq
        ogg_vorbis.extend_from_slice(&[0, 0, 0, 0]); // crc
        let payload = b"\x01vorbis\x00\x00\x00\x00\x00\x00\x00\x00";
        ogg_vorbis.push(1); // one segment
        ogg_vorbis.extend_from_slice(payload);
        assert_eq!(
            sniff_format(&ogg_vorbis),
            SniffedFormat::Audio {
                container: AudioContainer::OggAudio
            }
        );

        // Opus in Ogg.
        let mut ogg_opus = ogg_vorbis.clone();
        let pos = 27 + 1;
        ogg_opus.truncate(pos);
        ogg_opus.extend_from_slice(b"OpusHead\x01\x02\x00\x00\x00");
        assert_eq!(
            sniff_format(&ogg_opus),
            SniffedFormat::Audio {
                container: AudioContainer::OggAudio
            }
        );

        // Theora in Ogg → video.
        let mut ogg_theora = ogg_vorbis.clone();
        ogg_theora.truncate(27 + 1);
        ogg_theora.extend_from_slice(b"\x80theora\x03\x02\x00\x00");
        assert_eq!(
            sniff_format(&ogg_theora),
            SniffedFormat::Video {
                container: VideoContainer::Ogg
            }
        );

        // Truncated page (no payload reachable) → best-effort audio.
        assert_eq!(
            sniff_format(b"OggS\x00\x02"),
            SniffedFormat::Audio {
                container: AudioContainer::OggAudio
            }
        );
    }

    #[test]
    fn detects_audio_containers() {
        // MP3 via ID3 tag.
        assert_eq!(
            sniff_format(b"ID3\x04\x00\x00\x00\x00\x00\x00"),
            SniffedFormat::Audio {
                container: AudioContainer::Mp3
            }
        );
        // MP3 via frame sync (MPEG-1 Layer III: 0xFF 0xFB).
        assert_eq!(
            sniff_format(&[0xFF, 0xFB, 0x90, 0x00]),
            SniffedFormat::Audio {
                container: AudioContainer::Mp3
            }
        );
        assert_eq!(
            sniff_format(&[0xFF, 0xF3, 0x40, 0x00]),
            SniffedFormat::Audio {
                container: AudioContainer::Mp3
            }
        );
        // WAV: RIFF + WAVE form type.
        assert_eq!(
            sniff_format(b"RIFF\x24\x00\x00\x00WAVEfmt \x10\x00\x00\x00"),
            SniffedFormat::Audio {
                container: AudioContainer::Wav
            }
        );
        // FLAC.
        assert_eq!(
            sniff_format(b"fLaC\x00\x00\x00\x22"),
            SniffedFormat::Audio {
                container: AudioContainer::Flac
            }
        );
        // AAC ADTS (0xFF 0xF1) — must not be claimed as MP3.
        assert_eq!(
            sniff_format(&[0xFF, 0xF1, 0x50, 0x80, 0x02, 0x1F, 0xFC]),
            SniffedFormat::Audio {
                container: AudioContainer::Aac
            }
        );
        assert_eq!(
            sniff_format(&[0xFF, 0xF9, 0x50, 0x80]),
            SniffedFormat::Audio {
                container: AudioContainer::Aac
            }
        );
        // AIFF / AIFC.
        assert_eq!(
            sniff_format(b"FORM\x00\x00\x10\x00AIFFCOMM"),
            SniffedFormat::Audio {
                container: AudioContainer::Aiff
            }
        );
        assert_eq!(
            sniff_format(b"FORM\x00\x00\x10\x00AIFCCOMM"),
            SniffedFormat::Audio {
                container: AudioContainer::Aiff
            }
        );
        // AU, AMR, CAF, AC-3.
        assert_eq!(
            sniff_format(b".snd\x00\x00\x00\x18"),
            SniffedFormat::Audio {
                container: AudioContainer::Au
            }
        );
        assert_eq!(
            sniff_format(b"#!AMR\n"),
            SniffedFormat::Audio {
                container: AudioContainer::Amr
            }
        );
        assert_eq!(
            sniff_format(b"caff\x00\x01\x00\x00"),
            SniffedFormat::Audio {
                container: AudioContainer::Caf
            }
        );
        assert_eq!(
            sniff_format(&[0x0B, 0x77, 0x00, 0x00]),
            SniffedFormat::Audio {
                container: AudioContainer::Ac3
            }
        );
    }

    #[test]
    fn rejects_audio_lookalikes() {
        // RIFF+AVI is video, RIFF+WEBP stays unknown — WAVE is the only
        // RIFF form type claimed as audio.
        assert_eq!(
            sniff_format(b"RIFF\x00\x00\x00\x00AVI LIST"),
            SniffedFormat::Video {
                container: VideoContainer::Avi
            }
        );
        assert_eq!(
            sniff_format(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            SniffedFormat::Unknown
        );
        // MPEG-PS pack header is video, not MPEG-ES.
        assert_eq!(
            sniff_format(b"\x00\x00\x01\xba\x44\x00\x04\x00"),
            SniffedFormat::Video {
                container: VideoContainer::MpegPs
            }
        );
        // Frame-sync-shaped byte with reserved layer bits is not MP3.
        assert_eq!(
            sniff_format(&[0xFF, 0xF1, 0x00]),
            SniffedFormat::Audio {
                container: AudioContainer::Aac
            }
        );
        assert_eq!(sniff_format(&[0xFF, 0xE0, 0x00]), SniffedFormat::Unknown);
        // "ID" without the '3' is not an ID3 tag.
        assert_eq!(sniff_format(b"ID\x03\x00"), SniffedFormat::Unknown);
        // FORM with a non-audio form type.
        assert_eq!(
            sniff_format(b"FORM\x00\x00\x10\x008SVX"),
            SniffedFormat::Unknown
        );
    }

    #[test]
    fn rejects_non_media_lookalikes() {
        // RIFF is shared across containers — "AVI " is video, "WAVE" is
        // audio, anything else (e.g. WebP) stays unknown.
        assert_eq!(
            sniff_format(b"RIFF\x00\x00\x00\x00WAVEfmt "),
            SniffedFormat::Audio {
                container: AudioContainer::Wav
            }
        );
        assert_eq!(
            sniff_format(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            SniffedFormat::Unknown
        );
        // ftyp box with unrecognized brand is not claimed as video.
        assert_eq!(
            sniff_format(b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00mif1heic"),
            SniffedFormat::Unknown
        );
        // ftyp magic at wrong offset.
        assert_eq!(
            sniff_format(b"\x00\x00ftypmp42\x00\x00\x00\x00"),
            SniffedFormat::Unknown
        );
        // Truncated signatures.
        assert_eq!(sniff_format(b"\x1a\x45\xdf"), SniffedFormat::Unknown);
        assert_eq!(sniff_format(b"RIFF\x00\x00"), SniffedFormat::Unknown);
        assert_eq!(sniff_format(b"\x00\x00\x01"), SniffedFormat::Unknown);
    }

    #[test]
    fn rejects_unknown_and_truncated() {
        assert_eq!(sniff_format(b""), SniffedFormat::Unknown);
        assert_eq!(sniff_format(b"%PDF"), SniffedFormat::Unknown); // missing '-'
        assert_eq!(sniff_format(b"%pdF-"), SniffedFormat::Unknown); // case-sensitive
        assert_eq!(sniff_format(b"\x89PN"), SniffedFormat::Unknown); // truncated sig
        assert_eq!(sniff_format(b"PK\x03"), SniffedFormat::Unknown);
        assert_eq!(sniff_format(b"MZ\x90\x00"), SniffedFormat::Unknown); // PE executable
        assert_eq!(sniff_format(b"#!/bin/sh\necho hi"), SniffedFormat::Unknown);
        assert_eq!(sniff_format(&[0xFF, 0xD8]), SniffedFormat::Unknown); // truncated JPEG
        assert_eq!(
            sniff_format(b"random garbage bytes"),
            SniffedFormat::Unknown
        );
    }

    #[test]
    fn extension_and_label_helpers() {
        assert_eq!(SniffedFormat::Pdf.sniffed_extension(), ".pdf");
        assert_eq!(SniffedFormat::Jpeg.sniffed_extension(), ".jpg");
        assert_eq!(SniffedFormat::Unknown.sniffed_extension(), "");
        assert_eq!(SniffedFormat::Gif.label(), "GIF");
        assert_eq!(SniffedFormat::Unknown.to_string(), "unknown");
        let webm = SniffedFormat::Video {
            container: VideoContainer::Webm,
        };
        assert_eq!(webm.sniffed_extension(), ".webm");
        assert_eq!(webm.label(), "WebM video");
        assert!(webm.is_video());
        assert!(!SniffedFormat::Pdf.is_video());
        let mp3 = SniffedFormat::Audio {
            container: AudioContainer::Mp3,
        };
        assert_eq!(mp3.sniffed_extension(), ".mp3");
        assert_eq!(mp3.label(), "MP3 audio");
        assert!(mp3.is_audio());
        assert!(!mp3.is_video());
        assert!(!SniffedFormat::Pdf.is_audio());
        // Serde round-trips through lowercase names.
        assert_eq!(
            serde_json::to_string(&SniffedFormat::Pdf).unwrap(),
            "\"pdf\""
        );
        assert_eq!(
            serde_json::from_str::<SniffedFormat>("\"jpeg\"").unwrap(),
            SniffedFormat::Jpeg
        );
    }
}
