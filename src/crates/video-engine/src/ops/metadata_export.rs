//! Media metadata inspection reports and container-tag editing.
//!
//! [`metadata_report`] reads a media buffer with `ffprobe`
//! (`-show_format -show_streams`) and flattens the container tags,
//! stream languages, duration, bitrate, format name and size into a
//! typed [`MetadataReport`]. [`metadata_export`] serializes that
//! report to JSON, XML, CSV or plain text. [`metadata_set`] rewrites
//! container-level tags via an `ffmpeg -c copy` remux (no re-encode),
//! and [`metadata_compare`] diffs the tag maps of two inputs.
//!
//! Only *container* tags are handled: `metadata_set` passes every
//! pair as `-metadata key=value`, which lands on `format.tags`.
//! Stream-level tags (e.g. per-stream `language`) are reported by
//! [`metadata_report`] but cannot be edited here — ffmpeg would need
//! `-metadata:s:<stream>` specifiers, which this op deliberately
//! does not synthesize.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, ext_for_format, failure_detail,
    find_tool, probe_json, read_output, run_tool, tool_failure, write_input,
};

/// Output encodings accepted by [`metadata_export`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// Pretty-printed JSON object.
    Json,
    /// Hand-built XML document (`<?xml ...?><report>...`).
    Xml,
    /// `field,value` CSV rows.
    Csv,
    /// `key: value` plain-text lines.
    Text,
}

/// Flattened view of a media file's metadata, extracted from one
/// `ffprobe -show_format -show_streams` document.
///
/// `title`/`artist`/`album`/`genre`/`date`/`comment`/`encoder` map
/// the well-known ID3/MP4-style container tags; `date` falls back to
/// `creation_time` when `date` is absent. `custom` holds every other
/// container tag as `(key, value)` pairs in sorted key order.
#[derive(Debug, Clone)]
pub struct MetadataReport {
    /// Container `title` tag.
    pub title: Option<String>,
    /// Container `artist` tag.
    pub artist: Option<String>,
    /// Container `album` tag.
    pub album: Option<String>,
    /// Container `genre` tag.
    pub genre: Option<String>,
    /// Container `date` tag, or `creation_time` when `date` is absent.
    pub date: Option<String>,
    /// Container `comment` tag.
    pub comment: Option<String>,
    /// Container `encoder` tag (`encoder_eng` when `encoder` absent).
    pub encoder: Option<String>,
    /// Probed container duration in seconds.
    pub duration_secs: Option<f64>,
    /// Demuxer `format_name` (e.g. `"mov,mp4,m4a,3gp,3g2,mj2"`).
    pub format_name: String,
    /// Probed container bit rate in bits per second.
    pub bitrate_bps: Option<u64>,
    /// Probed container size in bytes (falls back to input length).
    pub size_bytes: u64,
    /// Distinct `language` tag values found on any stream.
    pub languages: Vec<String>,
    /// Container tags outside the well-known set, sorted by key.
    pub custom: Vec<(String, String)>,
}

/// Result of [`metadata_compare`]: keys present on only one side and
/// keys whose values differ.
#[derive(Debug, Clone, Default)]
pub struct MetadataDiff {
    /// Tag keys present in `a` but not `b`.
    pub only_in_a: Vec<String>,
    /// Tag keys present in `b` but not `a`.
    pub only_in_b: Vec<String>,
    /// `(key, value_in_a, value_in_b)` for keys present in both with
    /// different values.
    pub different: Vec<(String, String, String)>,
}

/// Container tag keys [`metadata_report`] lifts into dedicated
/// [`MetadataReport`] fields (or consumes as `date`/`encoder`
/// fallbacks). Everything else lands in [`MetadataReport::custom`].
const KNOWN_TAGS: &[&str] = &[
    "title",
    "artist",
    "album",
    "album_artist",
    "composer",
    "genre",
    "date",
    "creation_time",
    "comment",
    "encoder",
    "encoder_eng",
    "track",
    "disc",
    "copyright",
    "language",
];

/// Inspect `input` and return its [`MetadataReport`].
///
/// The buffer is written to a scratch file and read with
/// `ffprobe -v quiet -print_format json -show_format -show_streams
/// -show_chapters`; a non-zero exit on non-media input is
/// [`VideoError::InvalidInput`].
pub(crate) fn metadata_report(input: &[u8]) -> VideoResult<MetadataReport> {
    let input_len = check_input(input)?;
    let dir = TempDir::new("meta-report")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let json = probe_json(&input_path)?;
    parse_report(&json, input_len)
}

/// Parse an ffprobe JSON document into [`MetadataReport`]. Split from
/// [`metadata_report`] so tests can exercise parsing without a
/// subprocess.
fn parse_report(json: &Value, input_len: u64) -> VideoResult<MetadataReport> {
    let format = json.get("format").cloned().unwrap_or(Value::Null);
    let tags = format.get("tags").and_then(Value::as_object);

    let get_tag =
        |key: &str| -> Option<String> { tags.and_then(|t| t.get(key)).and_then(tag_value) };

    // Stream-level language tags (matroska track language, mp4
    // handler names surface differently, but `tags.language` is the
    // common case). Distinct values, first-seen order.
    let mut languages: Vec<String> = Vec::new();
    if let Some(streams) = json.get("streams").and_then(Value::as_array) {
        for stream in streams {
            if let Some(lang) = stream
                .get("tags")
                .and_then(Value::as_object)
                .and_then(|t| t.get("language"))
                .and_then(tag_value)
            {
                if !languages.contains(&lang) {
                    languages.push(lang);
                }
            }
        }
    }
    // Some demuxers put `language` on the format tags instead.
    if let Some(lang) = get_tag("language") {
        if !languages.contains(&lang) {
            languages.push(lang);
        }
    }

    // `custom`: every format tag outside the known set, sorted.
    let mut custom: Vec<(String, String)> = Vec::new();
    if let Some(tags) = tags {
        for (key, value) in tags {
            let lower = key.to_ascii_lowercase();
            if KNOWN_TAGS.contains(&lower.as_str()) {
                continue;
            }
            if let Some(v) = tag_value(value) {
                custom.push((key.clone(), v));
            }
        }
        custom.sort_by(|a, b| a.0.cmp(&b.0));
    }

    Ok(MetadataReport {
        title: get_tag("title"),
        artist: get_tag("artist"),
        album: get_tag("album"),
        genre: get_tag("genre"),
        // `date` is the ID3-style field; `creation_time` is the
        // ISO-8601 variant several muxers write instead.
        date: get_tag("date").or_else(|| get_tag("creation_time")),
        comment: get_tag("comment"),
        encoder: get_tag("encoder").or_else(|| get_tag("encoder_eng")),
        duration_secs: format
            .get("duration")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<f64>().ok()),
        format_name: format
            .get("format_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        bitrate_bps: format
            .get("bit_rate")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<u64>().ok()),
        size_bytes: format
            .get("size")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(input_len),
        languages,
        custom,
    })
}

/// Coerce an ffprobe tag value to `String`. ffprobe normally emits
/// strings, but some demuxers produce numbers; those are stringified
/// rather than dropped.
fn tag_value(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Serialize `input`'s [`MetadataReport`] in `format`.
pub(crate) fn metadata_export(input: &[u8], format: ExportFormat) -> VideoResult<Vec<u8>> {
    let report = metadata_report(input)?;
    let text = match format {
        ExportFormat::Json => serde_json::to_string_pretty(&report_json(&report))
            .map_err(|e| VideoError::ExternalTool {
                tool: "serde_json".to_string(),
                detail: e.to_string(),
            })?
            .into_bytes(),
        ExportFormat::Xml => to_xml(&report).into_bytes(),
        ExportFormat::Csv => to_csv(&report).into_bytes(),
        ExportFormat::Text => to_text(&report).into_bytes(),
    };
    Ok(text)
}

/// Build the JSON document for a report: every struct field plus a
/// `custom` object keyed by tag name.
fn report_json(report: &MetadataReport) -> Value {
    let mut map = Map::new();
    map.insert("title".into(), opt_str(&report.title));
    map.insert("artist".into(), opt_str(&report.artist));
    map.insert("album".into(), opt_str(&report.album));
    map.insert("genre".into(), opt_str(&report.genre));
    map.insert("date".into(), opt_str(&report.date));
    map.insert("comment".into(), opt_str(&report.comment));
    map.insert("encoder".into(), opt_str(&report.encoder));
    map.insert(
        "duration_secs".into(),
        report.duration_secs.map_or(Value::Null, Value::from),
    );
    map.insert(
        "format_name".into(),
        Value::from(report.format_name.as_str()),
    );
    map.insert(
        "bitrate_bps".into(),
        report.bitrate_bps.map_or(Value::Null, Value::from),
    );
    map.insert("size_bytes".into(), Value::from(report.size_bytes));
    map.insert(
        "languages".into(),
        Value::Array(
            report
                .languages
                .iter()
                .map(|l| Value::from(l.as_str()))
                .collect(),
        ),
    );
    let mut custom = Map::new();
    for (k, v) in &report.custom {
        custom.insert(k.clone(), Value::from(v.as_str()));
    }
    map.insert("custom".into(), Value::Object(custom));
    Value::Object(map)
}

fn opt_str(v: &Option<String>) -> Value {
    v.as_deref().map_or(Value::Null, Value::from)
}

/// Ordered `(key, value)` view of every report field, shared by the
/// CSV and text serializers. `None` fields are skipped; `languages`
/// is joined with `; `.
fn field_pairs(report: &MetadataReport) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut push = |key: &str, value: &Option<String>| {
        if let Some(v) = value {
            out.push((key.to_string(), v.clone()));
        }
    };
    push("title", &report.title);
    push("artist", &report.artist);
    push("album", &report.album);
    push("genre", &report.genre);
    push("date", &report.date);
    push("comment", &report.comment);
    push("encoder", &report.encoder);
    out.push(("format_name".to_string(), report.format_name.clone()));
    if let Some(d) = report.duration_secs {
        out.push(("duration_secs".to_string(), d.to_string()));
    }
    if let Some(b) = report.bitrate_bps {
        out.push(("bitrate_bps".to_string(), b.to_string()));
    }
    out.push(("size_bytes".to_string(), report.size_bytes.to_string()));
    if !report.languages.is_empty() {
        out.push(("languages".to_string(), report.languages.join("; ")));
    }
    for (k, v) in &report.custom {
        out.push((format!("custom.{k}"), v.clone()));
    }
    out
}

/// XML-escape `&`, `<`, `>`, `"`, `'` — the five characters that are
/// unsafe in element text and attribute values.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Build the XML export: a `<report>` root with one child element per
/// scalar field, a `<languages>` list, and a `<custom>` section of
/// `<tag name="...">value</tag>` entries.
fn to_xml(report: &MetadataReport) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<report>\n");
    for (key, value) in field_pairs(report) {
        // `custom.`-prefixed keys become `<tag>` entries inside
        // <custom>; everything else is a plain child element.
        if key.starts_with("custom.") {
            continue; // handled below from report.custom directly
        }
        out.push_str(&format!("  <{key}>{}</{key}>\n", xml_escape(&value)));
    }
    if !report.languages.is_empty() {
        out.push_str("  <languages>\n");
        for lang in &report.languages {
            out.push_str(&format!("    <language>{}</language>\n", xml_escape(lang)));
        }
        out.push_str("  </languages>\n");
    }
    if !report.custom.is_empty() {
        out.push_str("  <custom>\n");
        for (k, v) in &report.custom {
            out.push_str(&format!(
                "    <tag name=\"{}\">{}</tag>\n",
                xml_escape(k),
                xml_escape(v)
            ));
        }
        out.push_str("  </custom>\n");
    }
    out.push_str("</report>\n");
    out
}

/// CSV-escape a field: quote when it contains a comma, quote, CR or
/// LF; embedded quotes are doubled per RFC 4180.
fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Build the CSV export: a `field,value` header then one row per
/// report field (custom tags appear as `custom.<key>`).
fn to_csv(report: &MetadataReport) -> String {
    let mut out = String::from("field,value\n");
    for (key, value) in field_pairs(report) {
        out.push_str(&format!("{},{}\n", csv_escape(&key), csv_escape(&value)));
    }
    out
}

/// Build the text export: `key: value` lines, one per report field.
fn to_text(report: &MetadataReport) -> String {
    let mut out = String::new();
    for (key, value) in field_pairs(report) {
        out.push_str(&format!("{key}: {value}\n"));
    }
    out
}

/// The tag-map view used by [`metadata_compare`]: the well-known tag
/// fields plus `custom` pairs, as a sorted map. `date`/`encoder`
/// carry their fallbacks already resolved by [`metadata_report`].
fn tag_map(report: &MetadataReport) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let mut insert = |key: &str, value: &Option<String>| {
        if let Some(v) = value {
            map.insert(key.to_string(), v.clone());
        }
    };
    insert("title", &report.title);
    insert("artist", &report.artist);
    insert("album", &report.album);
    insert("genre", &report.genre);
    insert("date", &report.date);
    insert("comment", &report.comment);
    insert("encoder", &report.encoder);
    for (k, v) in &report.custom {
        map.insert(k.clone(), v.clone());
    }
    map
}

/// Compare the container-tag maps of two media inputs.
///
/// Both buffers are probed via [`metadata_report`]; the diff covers
/// only tag fields — container facts (duration, bitrate, size) are
/// deliberately excluded because remuxing legitimately changes them.
pub(crate) fn metadata_compare(a: &[u8], b: &[u8]) -> VideoResult<MetadataDiff> {
    let ra = metadata_report(a)?;
    let rb = metadata_report(b)?;
    let ma = tag_map(&ra);
    let mb = tag_map(&rb);

    let mut diff = MetadataDiff::default();
    for (k, v) in &ma {
        match mb.get(k) {
            None => diff.only_in_a.push(k.clone()),
            Some(w) if w != v => diff.different.push((k.clone(), v.clone(), w.clone())),
            _ => {}
        }
    }
    for k in mb.keys() {
        if !ma.contains_key(k) {
            diff.only_in_b.push(k.clone());
        }
    }
    Ok(diff)
}

/// Validate a caller-supplied tag key for `-metadata key=value`:
/// non-empty, no `=` (it would split the pair), no `:` (ffmpeg uses
/// `key=value` only at container level and `:` has special meaning in
/// some option syntaxes), no leading `-` (option injection), and no
/// control characters.
fn sanitize_tag_key(key: &str) -> VideoResult<String> {
    let key = key.trim();
    if key.is_empty()
        || key.contains('=')
        || key.contains(':')
        || key.starts_with('-')
        || key.chars().any(|c| c.is_control())
    {
        return Err(VideoError::InvalidInput(format!(
            "invalid metadata key '{key}': keys must be non-empty and contain no '=', ':', \
             leading '-' or control characters"
        )));
    }
    Ok(key.to_string())
}

/// Validate a caller-supplied tag value: NUL bytes cannot cross the
/// exec boundary into ffmpeg's option parser.
fn sanitize_tag_value(value: &str) -> VideoResult<String> {
    if value.contains('\0') {
        return Err(VideoError::InvalidInput(
            "metadata values must not contain NUL bytes".to_string(),
        ));
    }
    Ok(value.to_string())
}

/// Whether a non-zero ffmpeg exit looks like an honest "this
/// container cannot carry these tags" rejection rather than a tool
/// fault (mirrors `remux`'s container-rejection heuristic).
fn is_container_rejection(detail: &str) -> bool {
    let lower = detail.to_lowercase();
    lower.contains("could not find tag")
        || lower.contains("muxer")
        || lower.contains("codec")
        || lower.contains("could not write header")
        || lower.contains("invalid argument")
        || lower.contains("not supported")
}

/// Set container-level metadata tags on `input`, returning the
/// remuxed media bytes (`ffmpeg -map 0 -c copy -metadata k=v ...`).
///
/// Tags are applied in argument order; ffmpeg lets later `-metadata`
/// pairs override earlier ones for the same key. The output container
/// is chosen from the probed `format_name` via [`ext_for_format`] —
/// the same container family, so `-c copy` almost always succeeds;
/// when the muxer refuses the tags the ffmpeg error is surfaced as
/// [`VideoError::InvalidInput`].
///
/// Only container tags are written. Stream-level metadata (e.g.
/// per-stream `language`) is out of scope: it would require
/// `-metadata:s:<n>` specifiers this op does not build.
pub(crate) fn metadata_set(
    input: &[u8],
    tags: &[(String, String)],
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    if tags.is_empty() {
        return Err(VideoError::InvalidInput(
            "metadata_set requires at least one (key, value) tag".to_string(),
        ));
    }
    let pairs: Vec<(String, String)> = tags
        .iter()
        .map(|(k, v)| Ok((sanitize_tag_key(k)?, sanitize_tag_value(v)?)))
        .collect::<VideoResult<_>>()?;

    let program = find_tool(FFMPEG)?;

    // Probe once: limits, media validation, format name for the
    // scratch input extension and the output container.
    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;
    let format_name = info.format_name.as_deref().unwrap_or_default();
    let out_ext = ext_for_format(format_name).ok_or_else(|| {
        VideoError::InvalidInput(format!(
            "cannot determine an output container for format_name '{format_name}'"
        ))
    })?;
    let input_ext = ext_for_format(format_name).unwrap_or("bin");

    let dir = TempDir::new("meta-set")?;
    let input_path = write_input(&dir, &format!("input.{input_ext}"), input)?;
    let out_path = dir.file(&format!("out.{out_ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        // Copy every stream bit-for-bit; only container tags change.
        "-map".to_string(),
        "0".to_string(),
        "-c".to_string(),
        "copy".to_string(),
    ];
    for (k, v) in &pairs {
        args.push("-metadata".to_string());
        args.push(format!("{k}={v}"));
    }
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        let detail = failure_detail(&output, "ffmpeg exited unsuccessfully");
        if is_container_rejection(&detail) {
            return Err(VideoError::InvalidInput(format!(
                "container '{out_ext}' rejected the metadata tags: {detail}"
            )));
        }
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, h264_encoder};
    use std::fs;
    use std::sync::OnceLock;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// Tiny 1s H.264+AAC mp4 with `title`/`artist`/`genre` container
    /// tags, built once per test process.
    fn fixture() -> Option<&'static Vec<u8>> {
        static FIXTURE: OnceLock<Option<Vec<u8>>> = OnceLock::new();
        FIXTURE
            .get_or_init(|| {
                let program = find_tool(FFMPEG).ok()?;
                let enc = h264_encoder()?;
                let dir = TempDir::new("meta-test").ok()?;
                let out = dir.file("fixture.mp4");
                let args = vec![
                    "-y".to_string(),
                    "-v".to_string(),
                    "error".to_string(),
                    "-f".to_string(),
                    "lavfi".to_string(),
                    "-i".to_string(),
                    "testsrc=duration=1:size=160x120:rate=10".to_string(),
                    "-f".to_string(),
                    "lavfi".to_string(),
                    "-i".to_string(),
                    "sine=frequency=440:duration=1".to_string(),
                    "-c:v".to_string(),
                    enc.to_string(),
                    "-pix_fmt".to_string(),
                    "yuv420p".to_string(),
                    "-c:a".to_string(),
                    "aac".to_string(),
                    "-metadata".to_string(),
                    "title=Test Media".to_string(),
                    "-metadata".to_string(),
                    "artist=Test Artist".to_string(),
                    "-metadata".to_string(),
                    "genre=Test Genre".to_string(),
                    "-shortest".to_string(),
                    out.to_string_lossy().into_owned(),
                ];
                let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
                if !output.status.success() {
                    return None;
                }
                fs::read(out).ok()
            })
            .as_ref()
    }

    #[test]
    fn report_extracts_container_tags() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = fixture() else { return };
        let report = metadata_report(fixture).unwrap();
        assert_eq!(report.title.as_deref(), Some("Test Media"));
        assert_eq!(report.artist.as_deref(), Some("Test Artist"));
        assert_eq!(report.genre.as_deref(), Some("Test Genre"));
        assert!(
            report.format_name.to_ascii_lowercase().contains("mp4")
                || report.format_name.to_ascii_lowercase().contains("mov"),
            "unexpected format_name {:?}",
            report.format_name
        );
        assert!(report.size_bytes > 0);
        assert!(report.duration_secs.unwrap_or(0.0) > 0.0);
    }

    #[test]
    fn export_json_contains_title() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = fixture() else { return };
        let out = metadata_export(fixture, ExportFormat::Json).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"title\""), "json missing title: {text}");
        // Round-trip: it must parse back to a JSON object.
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["title"], "Test Media");
    }

    #[test]
    fn export_xml_is_well_formed_enough() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = fixture() else { return };
        let out = metadata_export(fixture, ExportFormat::Xml).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("<?xml"), "xml missing declaration: {text}");
        assert!(text.contains("<report>"));
        assert!(text.contains("<title>Test Media</title>"));
    }

    #[test]
    fn export_csv_and_text_contain_fields() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = fixture() else { return };
        let csv = String::from_utf8(metadata_export(fixture, ExportFormat::Csv).unwrap()).unwrap();
        assert!(csv.starts_with("field,value"), "csv header: {csv}");
        assert!(csv.contains("title"), "csv missing title: {csv}");
        let text =
            String::from_utf8(metadata_export(fixture, ExportFormat::Text).unwrap()).unwrap();
        assert!(
            text.contains("title: Test Media"),
            "text missing title: {text}"
        );
    }

    #[test]
    fn metadata_set_updates_title() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = fixture() else { return };
        let out = metadata_set(
            fixture,
            &[
                ("title".to_string(), "Foo".to_string()),
                ("comment".to_string(), "bar,baz".to_string()),
            ],
            &EngineLimits::default(),
        )
        .unwrap();
        let report = metadata_report(&out).unwrap();
        assert_eq!(report.title.as_deref(), Some("Foo"));
        assert_eq!(report.comment.as_deref(), Some("bar,baz"));
    }

    #[test]
    fn metadata_set_rejects_bad_keys() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = fixture() else { return };
        for bad in ["a=b", "-x", "k:ey", "", "bad\u{7}key"] {
            assert!(
                matches!(
                    metadata_set(
                        fixture,
                        &[(bad.to_string(), "v".to_string())],
                        &EngineLimits::default()
                    ),
                    Err(VideoError::InvalidInput(_))
                ),
                "key {bad:?} should be rejected"
            );
        }
        assert!(matches!(
            metadata_set(
                fixture,
                &[("ok".to_string(), "nul\u{0}byte".to_string())],
                &EngineLimits::default()
            ),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            metadata_set(fixture, &[], &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn metadata_compare_reports_differences() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = fixture() else { return };
        // Self-compare: no differences.
        let same = metadata_compare(fixture, fixture).unwrap();
        assert!(same.only_in_a.is_empty());
        assert!(same.only_in_b.is_empty());
        assert!(same.different.is_empty());

        // Mutated copy: title/comment change.
        let edited = metadata_set(
            fixture,
            &[
                ("title".to_string(), "Changed".to_string()),
                ("comment".to_string(), "new".to_string()),
            ],
            &EngineLimits::default(),
        )
        .unwrap();
        let diff = metadata_compare(fixture, &edited).unwrap();
        assert!(
            diff.different
                .iter()
                .any(|(k, a, b)| k == "title" && a == "Test Media" && b == "Changed"),
            "expected title diff, got {:?}",
            diff.different
        );
        assert!(diff.only_in_b.iter().any(|k| k == "comment"));
    }

    #[test]
    fn report_rejects_garbage_input() {
        if !tools_available() {
            return;
        }
        let garbage = b"this is definitely not a media file".to_vec();
        assert!(matches!(
            metadata_report(&garbage),
            Err(VideoError::InvalidInput(_))
        ));
    }
}
