//! Contact sheet / video summary image.
//!
//! [`contact_sheet`] samples a video at `duration / (columns * rows)`
//! intervals via the `fps` filter, scales each frame to
//! `cell_width`x`cell_height`, and tiles them into a single
//! `columns`x`rows` grid image (`tile` filter). Optionally stamps
//! each cell's presentation timestamp with `drawtext` (skipped with
//! a warning when this FFmpeg build lacks the filter).
//!
//! Typical invocation:
//!
//! ```text
//! ffmpeg -i in.mp4 -vf "fps=N/dur,scale=CW:CH[,drawtext=...],tile=COLSxROWS" \
//!        -frames:v 1 -an out.jpg
//! ```

use std::ffi::OsString;

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaInput, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::types::ProbeReport;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::{process, tempdir::TempDir};

/// Options for [`contact_sheet`].
#[derive(Debug, Clone)]
pub struct ContactSheetOptions {
    /// Number of tile columns.
    pub columns: u32,
    /// Number of tile rows.
    pub rows: u32,
    /// Width of each cell in pixels.
    pub cell_width: u32,
    /// Height of each cell in pixels.
    pub cell_height: u32,
    /// Draw each cell's timestamp (`pts` as `HH:MM:SS.mmm`) via the
    /// `drawtext` filter. Requires a build with `drawtext`; when the
    /// filter is absent the sheet is still produced and a warning is
    /// recorded in [`MediaOutput::warnings`].
    pub timestamp: bool,
    /// Output image format extension without dot (`"jpg"`, `"png"`,
    /// ...). Maps to ffmpeg's image2 muxer by extension.
    pub output_format: String,
}

impl Default for ContactSheetOptions {
    fn default() -> Self {
        Self {
            columns: 4,
            rows: 4,
            cell_width: 320,
            cell_height: 180,
            timestamp: false,
            output_format: "jpg".to_string(),
        }
    }
}

/// Generate a contact-sheet image for `input`.
///
/// Writes `input` to a temp file (ffmpeg/ffprobe need seekable paths),
/// probes the duration, builds the `fps`/`scale`/`tile` filtergraph,
/// runs ffmpeg for a single frame, and returns the image as the
/// primary [`MediaFile`](crate::crates::multimedia_engine::MediaFile).
///
/// Errors:
/// - [`MediaError::MissingTool`] when `ffmpeg`/`ffprobe` are absent.
/// - [`MediaError::LimitExceeded`] when the input exceeds
///   `max_input_bytes` or the output exceeds `max_output_bytes`.
/// - [`MediaError::InvalidInput`] for zero dimensions, missing
///   duration, or tool failure on bad media.
pub fn contact_sheet(
    input: &MediaInput,
    options: &ContactSheetOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if options.columns == 0
        || options.rows == 0
        || options.cell_width == 0
        || options.cell_height == 0
    {
        return Err(MediaError::InvalidInput(
            "contact sheet columns, rows and cell dimensions must be non-zero".to_string(),
        ));
    }
    if input.data.is_empty() {
        return Err(MediaError::InvalidInput("empty input".to_string()));
    }
    if let Some(max) = limits.max_input_bytes
        && input.data.len() as u64 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.data.len()
        )));
    }

    let tile_count = u64::from(options.columns) * u64::from(options.rows);
    let cells = tile_count as f64;

    let dir = TempDir::new("contact-sheet")?;
    let ext = sanitize_extension(&input.name);
    let input_path = dir.write(&format!("input{ext}"), &input.data)?;

    let duration = probe_duration(&input_path, limits, rt)?;
    // fps = 1 / (duration / N) = N / duration. ffprobe durations can be
    // 0 or slightly negative on broken files; reject non-positive.
    if !(duration.is_finite() && duration > 0.0) {
        return Err(MediaError::InvalidInput(format!(
            "could not determine a positive duration (got {duration})"
        )));
    }
    let fps = cells / duration;

    // Decide on drawtext before building the graph so a missing filter
    // degrades to a plain sheet instead of failing ffmpeg.
    let mut warnings = Vec::new();
    let mut use_timestamps = false;
    if options.timestamp {
        match rt.capabilities() {
            Ok(caps) if caps.has_filter("drawtext") => use_timestamps = true,
            Ok(_) => warnings.push(
                "drawtext filter unavailable in this FFmpeg build; \
                 timestamps disabled"
                    .to_string(),
            ),
            Err(e) => warnings.push(format!(
                "capability probe failed ({e}); timestamps disabled"
            )),
        }
    }

    let mut filter = format!(
        "fps={fps:.9},scale={}:{}",
        options.cell_width, options.cell_height
    );
    if use_timestamps {
        // pts:hms renders HH:MM:SS.mmm. `\:` keeps the colon inside the
        // expansion from splitting filter options; box+outline keep the
        // text readable on bright frames.
        filter.push_str(
            ",drawtext=text='%{pts\\:hms}':x=4:y=4:fontsize=14:\
             fontcolor=white:borderw=1:bordercolor=black@0.6:\
             box=1:boxcolor=black@0.35:boxborderw=2",
        );
    }
    filter.push_str(&format!(",tile={}x{}", options.columns, options.rows));

    let ext_out = sanitize_extension(&options.output_format);
    let ext_out = ext_out.trim_start_matches('.');
    let out_name = format!("contact_sheet.{ext_out}");
    let out_path = dir.file(&out_name);

    let args: Vec<OsString> = [
        "-y".into(),
        "-v".into(),
        "error".into(),
        "-i".into(),
        input_path.clone().into_os_string(),
        "-vf".into(),
        filter.into(),
        "-map".into(),
        "0:v:0".into(),
        "-frames:v".into(),
        "1".into(),
        "-an".into(),
        "-sn".into(),
        "-dn".into(),
        out_path.clone().into_os_string(),
    ]
    .into_iter()
    .collect();

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, limits)?;
    if !out.stderr.is_empty() {
        // ffmpeg can exit 0 while printing warnings; surface the tail
        // as a non-fatal diagnostic.
        warnings.push(format!("ffmpeg: {}", out.stderr));
    }

    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::InvalidInput(
            "ffmpeg produced an empty contact sheet".to_string(),
        ));
    }

    let mut result = MediaOutput::single(out_name, bytes);
    result.warnings = warnings;
    Ok(result)
}

/// Probe the media duration in seconds via `ffprobe` JSON.
///
/// Prefers the container `format.duration`; falls back to the longest
/// stream duration when the format value is absent.
fn probe_duration(path: &std::path::Path, limits: &MediaLimits, rt: &Runtime) -> MediaResult<f64> {
    let mut args: Vec<OsString> = [
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    args.push(path.as_os_str().to_os_string());

    let out = process::run_checked("ffprobe", &rt.ffprobe, &args, None, limits)?;
    let report: ProbeReport = serde_json::from_slice(&out.stdout)
        .map_err(|e| MediaError::InvalidInput(format!("ffprobe JSON parse failed: {e}")))?;

    // ProbeReport already carries format.duration and per-stream
    // duration; reuse it rather than duplicating the JSON shape.
    if let Some(d) = report.format.as_ref().and_then(|f| f.duration_secs()) {
        return Ok(d);
    }
    report
        .streams
        .iter()
        .filter(|s| s.kind() == Some("video"))
        .filter_map(|s| s.duration_secs())
        .fold(None, |acc: Option<f64>, d| {
            Some(acc.map_or(d, |a| a.max(d)))
        })
        .ok_or_else(|| MediaError::InvalidInput("no duration found".to_string()))
}

/// Extract a safe lowercase extension from a file name. Returns `""`
/// when the name has no usable extension.
fn sanitize_extension(name: &str) -> String {
    let base = std::path::Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(name);
    match base.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric()) => {
            format!(".{ext}")
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::discovery;
    use crate::crates::multimedia_engine::runtime::process;

    /// Generate an ~8s 320x180@10fps testsrc clip (video only) via
    /// ffmpeg lavfi. Returns `None` when ffmpeg/libx264 are missing so
    /// tests skip gracefully on minimal builds.
    fn make_clip() -> Option<Vec<u8>> {
        let ffmpeg = discovery::find_tool("ffmpeg")?;
        let dir = TempDir::new("contact-sheet-test").ok()?;
        let out_path = dir.file("clip.mp4");
        let args: Vec<OsString> = [
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x180:rate=10:duration=8",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            out_path.to_str()?,
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let limits = MediaLimits::default();
        let out = process::run("ffmpeg", &ffmpeg, &args, None, &limits).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    fn probe_dims(path: &std::path::Path, rt: &Runtime) -> Option<(u32, u32)> {
        let mut args: Vec<OsString> = [
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        args.push(path.as_os_str().to_os_string());
        let limits = MediaLimits::default();
        let out = process::run("ffprobe", &rt.ffprobe, &args, None, &limits).ok()?;
        if !out.success {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let (w, h) = text.trim().split_once(',')?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    }

    #[test]
    fn contact_sheet_produces_nonempty_jpeg() {
        let Some(data) = make_clip() else {
            eprintln!("skipping: ffmpeg lavfi/libx264 unavailable");
            return;
        };
        let rt = Runtime::discover().expect("ffmpeg/ffprobe on PATH");
        let input = MediaInput {
            name: "clip.mp4".to_string(),
            data,
        };
        let options = ContactSheetOptions {
            columns: 2,
            rows: 2,
            cell_width: 160,
            cell_height: 90,
            timestamp: false,
            output_format: "jpg".to_string(),
        };
        let out =
            contact_sheet(&input, &options, &MediaLimits::default(), &rt).expect("contact_sheet");

        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "contact_sheet.jpg");
        assert!(
            file.bytes.len() > 100,
            "sheet too small: {} bytes",
            file.bytes.len()
        );
        // JPEG SOI marker.
        assert_eq!(&file.bytes[..2], &[0xFF, 0xD8], "not a JPEG");
    }

    #[test]
    fn contact_sheet_tile_dimensions_match_grid() {
        let Some(data) = make_clip() else {
            eprintln!("skipping: ffmpeg lavfi/libx264 unavailable");
            return;
        };
        let rt = Runtime::discover().expect("ffmpeg/ffprobe on PATH");
        let input = MediaInput {
            name: "clip.mp4".to_string(),
            data,
        };
        let options = ContactSheetOptions {
            columns: 3,
            rows: 2,
            cell_width: 120,
            cell_height: 68,
            timestamp: true, // drawtext may be absent — sheet must still work
            output_format: "jpg".to_string(),
        };
        let out =
            contact_sheet(&input, &options, &MediaLimits::default(), &rt).expect("contact_sheet");
        let file = out.primary.expect("primary output");
        assert!(!file.bytes.is_empty());

        // Write the sheet to disk so ffprobe can read its dimensions.
        let dir = TempDir::new("contact-sheet-verify").expect("tempdir");
        let sheet_path = dir.write("sheet.jpg", &file.bytes).expect("write sheet");
        let (w, h) = probe_dims(&sheet_path, &rt).expect("ffprobe dims");
        assert_eq!(w, options.columns * options.cell_width, "width {w}");
        assert_eq!(h, options.rows * options.cell_height, "height {h}");

        // If drawtext was available, timestamps were requested and no
        // warning should mention drawtext; otherwise a warning is fine.
        if rt
            .capabilities()
            .map(|c| c.has_filter("drawtext"))
            .unwrap_or(false)
        {
            assert!(
                !out.warnings.iter().any(|w| w.contains("drawtext")),
                "unexpected drawtext warning: {:?}",
                out.warnings
            );
        }
    }

    #[test]
    fn contact_sheet_rejects_zero_dimensions() {
        let input = MediaInput {
            name: "x.mp4".to_string(),
            data: vec![0u8; 16],
        };
        let options = ContactSheetOptions {
            columns: 0,
            ..ContactSheetOptions::default()
        };
        let rt = Runtime::discover().expect("ffmpeg/ffprobe on PATH");
        let err =
            contact_sheet(&input, &options, &MediaLimits::default(), &rt).expect_err("must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)), "{err:?}");
    }
}
