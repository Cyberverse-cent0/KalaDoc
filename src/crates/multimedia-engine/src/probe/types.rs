//! Serde types mirroring `ffprobe -of json` output.
//!
//! ffprobe emits a large, version-dependent JSON document; every field
//! is `Option`/defaulted because older/newer builds omit keys
//! differently. Consumers must treat `None` as "unknown", never as a
//! sensible default.

use serde::Deserialize;
use std::collections::BTreeMap;

/// ffprobe usually emits counts/sizes as JSON numbers but some builds
/// emit them as strings (e.g. `"channels": "8"`); accept both.
fn opt_u32<'de, D>(d: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumOrStr {
        N(u32),
        S(String),
    }
    Ok(match Option::<NumOrStr>::deserialize(d)? {
        Some(NumOrStr::N(n)) => Some(n),
        Some(NumOrStr::S(s)) => s.trim().parse().ok(),
        None => None,
    })
}

/// Root of `ffprobe -show_format -show_streams -show_chapters -of json`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProbeReport {
    /// Media streams (video, audio, subtitle, data, ...).
    #[serde(default)]
    pub streams: Vec<MediaStream>,
    /// Container-level format information.
    pub format: Option<FormatInfo>,
    /// Chapter markers, when the container has them.
    #[serde(default)]
    pub chapters: Vec<Chapter>,
}

/// One stream entry from `streams[]`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MediaStream {
    /// Stream index within the file.
    #[serde(default, deserialize_with = "opt_u32")]
    pub index: Option<u32>,
    /// Codec identifier (`h264`, `aac`, `mov_text`, ...).
    pub codec_name: Option<String>,
    /// Long codec name.
    pub codec_long_name: Option<String>,
    /// `video`, `audio`, `subtitle`, `data`, ...
    pub codec_type: Option<String>,
    /// Codec profile (`High`, `LC`, ...).
    pub profile: Option<String>,
    /// Pixel format (`yuv420p`, ...). Video only.
    pub pix_fmt: Option<String>,
    /// Width in pixels. Video only.
    #[serde(default, deserialize_with = "opt_u32")]
    pub width: Option<u32>,
    /// Height in pixels. Video only.
    #[serde(default, deserialize_with = "opt_u32")]
    pub height: Option<u32>,
    /// Coded (encoded) width — may differ from display `width`.
    #[serde(default, deserialize_with = "opt_u32")]
    pub coded_width: Option<u32>,
    /// Coded height.
    #[serde(default, deserialize_with = "opt_u32")]
    pub coded_height: Option<u32>,
    /// Sample aspect ratio string (`"1:1"`).
    pub sample_aspect_ratio: Option<String>,
    /// Display aspect ratio string (`"16:9"`).
    pub display_aspect_ratio: Option<String>,
    /// Field order (`progressive`, `tt`, `bb`, ...).
    pub field_order: Option<String>,
    /// Color range (`tv`, `pc`, ...).
    pub color_range: Option<String>,
    /// Color space (`bt709`, `smpte170m`, ...).
    pub color_space: Option<String>,
    /// Color transfer characteristics.
    pub color_transfer: Option<String>,
    /// Color primaries.
    pub color_primaries: Option<String>,
    /// Chroma location.
    pub chroma_location: Option<String>,
    /// Bits per raw sample (bit depth).
    #[serde(default, deserialize_with = "opt_u32")]
    pub bits_per_raw_sample: Option<u32>,
    /// Frame rate string (`"30000/1001"`).
    pub r_frame_rate: Option<String>,
    /// Average frame rate string.
    pub avg_frame_rate: Option<String>,
    /// Time base string (`"1/15360"`).
    pub time_base: Option<String>,
    /// Start time in seconds, as reported.
    pub start_time: Option<String>,
    /// Duration in seconds, as reported (string — ffprobe emits
    /// decimals as strings).
    pub duration: Option<String>,
    /// Stream bit rate in bits/s.
    pub bit_rate: Option<String>,
    /// Number of frames, when known.
    pub nb_frames: Option<String>,
    /// Audio sample rate in Hz.
    pub sample_rate: Option<String>,
    /// Audio channel count.
    #[serde(default, deserialize_with = "opt_u32")]
    pub channels: Option<u32>,
    /// Audio channel layout (`stereo`, `5.1`, ...).
    pub channel_layout: Option<String>,
    /// Bits per audio sample.
    #[serde(default, deserialize_with = "opt_u32")]
    pub bits_per_sample: Option<u32>,
    /// Language tag from stream tags (`eng`, ...).
    pub language: Option<String>,
    /// Stream title from tags.
    pub title: Option<String>,
    /// Stream disposition flags.
    #[serde(default)]
    pub disposition: Disposition,
    /// All stream tags (`language`, `title`, `handler_name`, ...).
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    /// Attached picture indicator (cover art).
    pub disposition_attached_pic: Option<bool>,
}

impl MediaStream {
    /// Convenience: codec type lowercased (`"video"`, `"audio"`, ...).
    pub fn kind(&self) -> Option<&str> {
        self.codec_type.as_deref()
    }

    /// Whether this stream is flagged as the default.
    pub fn is_default(&self) -> bool {
        self.disposition.default.unwrap_or(0) != 0
    }

    /// Duration parsed to seconds, when present and parseable.
    pub fn duration_secs(&self) -> Option<f64> {
        self.duration.as_deref().and_then(|d| d.parse().ok())
    }

    /// Frame rate parsed to f64 (first component of `num/den`).
    pub fn frame_rate(&self) -> Option<f64> {
        parse_rate(
            self.r_frame_rate
                .as_deref()
                .or(self.avg_frame_rate.as_deref())?,
        )
    }
}

/// Parse `"30000/1001"` or `"25"` into frames per second.
fn parse_rate(s: &str) -> Option<f64> {
    let (num, den) = s.split_once('/').unwrap_or((s, "1"));
    let n: f64 = num.trim().parse().ok()?;
    let d: f64 = den.trim().parse().ok()?;
    if d == 0.0 { None } else { Some(n / d) }
}

/// Stream disposition flags (which stream players pick by default).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Disposition {
    /// Default stream for its type.
    pub default: Option<i64>,
    /// Dub track.
    pub dub: Option<i64>,
    /// Original language/content.
    pub original: Option<i64>,
    /// Commentary.
    pub comment: Option<i64>,
    /// Lyrics.
    pub lyrics: Option<i64>,
    /// Karaoke.
    pub karaoke: Option<i64>,
    /// Forced subtitles.
    pub forced: Option<i64>,
    /// Hearing-impaired captions.
    pub hearing_impaired: Option<i64>,
    /// Visual-impaired descriptions.
    pub visual_impaired: Option<i64>,
    /// Clean effects (no dialogue/noise).
    pub clean_effects: Option<i64>,
    /// Attached picture (cover art).
    pub attached_pic: Option<i64>,
    /// Timed thumbnails.
    pub timed_thumbnails: Option<i64>,
    /// Captions.
    pub captions: Option<i64>,
    /// Descriptions.
    pub descriptions: Option<i64>,
    /// Metadata track.
    pub metadata: Option<i64>,
    /// Dependent stream (would not make sense alone).
    pub dependent: Option<i64>,
    /// Still image.
    pub still_image: Option<i64>,
}

/// Container/format-level info from `format{}`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FormatInfo {
    /// Format name(s), comma-separated (`"mov,mp4,m4a,3gp,3g2,mj2"`).
    pub format_name: Option<String>,
    /// Long format name.
    pub format_long_name: Option<String>,
    /// Number of streams.
    #[serde(default, deserialize_with = "opt_u32")]
    pub nb_streams: Option<u32>,
    /// Duration in seconds (string).
    pub duration: Option<String>,
    /// File size in bytes (string).
    pub size: Option<String>,
    /// Overall bit rate (string).
    pub bit_rate: Option<String>,
    /// Start time (string).
    pub start_time: Option<String>,
    /// Probe score (higher = more confident detection).
    #[serde(default, deserialize_with = "opt_u32")]
    pub probe_score: Option<u32>,
    /// Format tags (title, artist, encoder, ...).
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

impl FormatInfo {
    /// Duration parsed to seconds.
    pub fn duration_secs(&self) -> Option<f64> {
        self.duration.as_deref().and_then(|d| d.parse().ok())
    }
    /// Size parsed to bytes.
    pub fn size_bytes(&self) -> Option<u64> {
        self.size.as_deref().and_then(|s| s.parse().ok())
    }
    /// Bit rate parsed.
    pub fn bit_rate(&self) -> Option<u64> {
        self.bit_rate.as_deref().and_then(|s| s.parse().ok())
    }
    /// Primary container name (first of `format_name` list).
    pub fn primary_name(&self) -> Option<&str> {
        self.format_name
            .as_deref()
            .and_then(|n| n.split(',').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

/// One chapter from `chapters[]`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Chapter {
    /// Chapter index.
    #[serde(default, deserialize_with = "opt_u32")]
    pub id: Option<u32>,
    /// Start time in seconds (string).
    pub start_time: Option<String>,
    /// End time in seconds (string).
    pub end_time: Option<String>,
    /// Chapter tags (`title`, ...).
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

impl Chapter {
    /// Start parsed to seconds.
    pub fn start_secs(&self) -> Option<f64> {
        self.start_time.as_deref().and_then(|t| t.parse().ok())
    }
    /// End parsed to seconds.
    pub fn end_secs(&self) -> Option<f64> {
        self.end_time.as_deref().and_then(|t| t.parse().ok())
    }
    /// Chapter title tag, if present.
    pub fn title(&self) -> Option<&str> {
        self.tags.get("title").map(String::as_str)
    }
}
