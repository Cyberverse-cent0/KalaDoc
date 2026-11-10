# Video Engine

`src/crates/video-engine/` provides video/audio inspection and
processing for KalaDoc. Every operation delegates to the external
`ffmpeg` and `ffprobe` command-line tools — the engine shells out, it
does not decode media in-process and **never bundles the binaries**.
Both must be installed on `PATH` (see `.doc/Architecture.md` →
`deployments::require` for per-distro install hints); when either is
missing the engine reports itself `Unavailable`/`Degraded` via
`health()` and operations fail with `MissingTool`.

## Contracts

- Implements `ProcessingEngine` with `name = "video"` and
  `kind = EngineKind::Video`; registered in
  `EngineRegistry::builtin()` alongside the other engines.
- Stateless: `VideoEngine::new()` holds no configuration — every
  operation receives its own `EngineLimits` per call, so the engine is
  trivially `Send + Sync`. Methods are synchronous (callers on an
  async runtime should wrap them in `spawn_blocking`).
- Advertised operations (`capabilities()`): `Inspect`, `Validate`,
  `Thumbnail`, `Trim`, `Transcode`, `Concat`, `ExtractAudio`,
  `Remux`, `Filter`, `Analyze`, `Package`, `ExtractStreams`,
  `Subtitles`, `Slideshow`, `AudioProcess`, `MetadataEdit`,
  `Quality`, `Capture`, `Encode`, plus the generic `Resize`,
  `Rotate`, `Compress`, `Watermark`, and `Convert`.
- Input/output container formats advertised: `mp4`, `mkv`, `mov`,
  `webm`, `avi`.

## Operations catalog

The `ops/` modules implement the work; `engine.rs` exposes ~80 public
methods that delegate to them. Grouped by domain (method names are
the `VideoEngine` API):

### Probe / containers / encode

| Module | Engine methods |
|---|---|
| `probe` | `probe` — `ffprobe -show_format -show_streams -show_chapters`, JSON |
| `remux` | `remux` — stream-copy (`-c copy`) into another container |
| `transcode` | `transcode` — container conversion (`mp4`→H.264/AAC, `webm`→VP9/Opus, `mkv`/`avi`→copy) |
| `encode` | `transcode_profile` (explicit `EncodeProfile`), `encode_presets` |
| `concat` | `concat` — concat demuxer, stream copy when compatible |
| `trim` | `trim` — `-ss`/`-t`, copy or re-encode |
| `extract_audio` | `extract_audio` — `mp3`, `wav`, `aac`, `m4a`, `ogg` |

Encode presets (`encode_presets`) are probed at call time via
`ffmpeg -encoders` and only advertised when the encoder exists:
`h264` (libx264 → libopenh264), `av1` (libaom-av1 → libsvtav1 →
librav1e), `vp9` (libvpx-vp9 + libopus), `prores` (prores_ks +
pcm_s16le), `dnxhd` (dnxhd + pcm_s16le), `ffv1` (ffv1 + flac),
`mpeg2` (mpeg2video + mp2), `mjpeg` (mjpeg + mp2). HEVC is
commented out pending an encoder check.

### Geometry

| Module | Engine methods |
|---|---|
| `geometry` | `resize_video` (fit/cover/contain/stretch), `crop_video`, `pad_video`, `rotate_video`, `flip_video` |
| `geometry_adv` | `letterbox`, `pillarbox`, `autocrop`, `transpose`, `orient` |

### Tempo / time

| Module | Engine methods |
|---|---|
| `tempo` | `set_speed`, `reverse`, `loop_clip`, `freeze_frame` |

### Audio

| Module | Engine methods |
|---|---|
| `audio` | `audio_volume`, `audio_normalize` (EBU R128), `audio_merge`, `audio_concat`, `audio_fade`, `remove_audio`, `channel_convert` |
| `audio_fx` | `audio_eq` (3-band), `audio_denoise`, `audio_compress`, `audio_pitch`, `audio_spectrogram` |
| `audio_mix` | `audio_mix_weighted`, `audio_crossfade`, `audio_delay` |

### Frames / images / slideshow

| Module | Engine methods |
|---|---|
| `frames` | `extract_frames` (`FramesSpec`: every/fps/all/keyframes), `video_to_image_sequence` |
| `contact_sheet` | `contact_sheet` (grid PNG), `scene_keyframes` (one frame per scene change) |
| `slideshow` | `images_to_video`, `video_to_gif`, `video_to_animated_webp`, `video_to_apng` |
| `av_visual` | `audio_to_video` (waveform/spectrum/both), `gif_to_video`, `replace_audio_track` |

### Extraction / subtitles

| Module | Engine methods |
|---|---|
| `extract` | `extract_streams` — subtitles, attachments, cover art, chapters (JSON sidecar), data streams |
| `subtitles` | `extract_subtitles` (srt/ass/vtt), `convert_subtitles`, `shift_subtitles`, `burn_subtitles` (with `SubtitleStyle`) |
| `thumbnail` | `thumbnail` — single PNG frame at a timestamp |

### Filters / overlays / stream ops

| Module | Engine methods |
|---|---|
| `filters` | `apply_filters` (raw `-vf`/`-af` chains), `filter_presets` |
| `overlay` | `overlay_image`, `draw_text`, `chroma_key` |
| `stream_ops` | `select_streams`, `merge_av`, `set_metadata`, `strip_metadata`, `set_cover_art` |

Note: `filter_presets()` currently returns an empty catalog (the
preset table is a stub); `apply_filters` itself works with any
ffmpeg filter chain. `kaladoc video filters` therefore lists nothing
today.

### Analysis / loudness / quality

| Module | Engine methods |
|---|---|
| `analysis` | `analyze` (`MediaAnalysis`: black/freeze/silence/scene/metrics), `scene_timestamps`, `detect_black_frames`, `detect_freeze_frames`, `detect_silence` |
| `loudness` | `measure_loudness` (`LoudnessReport`, EBU R128), `normalize_loudness`, `audio_stats` |
| `quality` | `compare_quality` — PSNR/SSIM/VMAF (`QualityReport`) |

### Packaging / capture / runtime

| Module | Engine methods |
|---|---|
| `packaging` | `package_hls` (`index.m3u8` + segments), `package_dash` (`manifest.mpd` + segments), `faststart` |
| `capture` | `list_devices`, `list_protocols`, `list_hwaccels`, `detect_hwaccel` |
| `runtime` | `media_capabilities` (`MediaCapabilities`: encoders/decoders/filters/muxers/demuxers/protocols/hwaccels), `ffmpeg_version`, `ffprobe_version` |

### Compiled but not yet exposed

These `ops/` modules are declared in `ops/mod.rs` and compile, but no
`VideoEngine` method, CLI subcommand, or job kind reaches them yet
(`audio_time` is explicitly `#[allow(dead_code)]` "until VideoEngine
dispatches to them"):

- `audio_time` — `audio_reverse`, `audio_loop`, `audio_speed`,
  `audio_trim`, `audio_channels`, `audio_convert`
- `interpolate` — `interpolate_video` (frame interpolation)
- `proxy` — `generate_proxy` (`ProxyKind` variants)
- `sync` — `av_sync_report`, `av_sync_fix`, `av_sync_auto`
- `waveform` — `audio_waveform_image` (`WaveformParams`)

Additionally, `ops/` contains files that are **not declared in
`ops/mod.rs` and are not compiled**: `concat_filter.rs`,
`metadata_export.rs`, `preview.rs`, `stabilize.rs`, `validate.rs`.
They are work-in-progress on disk only and provide no functionality.

## Limits

`EngineLimits` fields honored by video ops (all optional, per call):

| Limit | Enforced by |
|---|---|
| `max_input_bytes` | Every op via `check_limits`; `probe` is bounded only by the hard cap |
| `max_duration_secs` | Ops on timed media — checked from `ffprobe` metadata before any `ffmpeg` run |
| `max_pixels` | Raster/frame-producing ops (thumbnail, frames) — decompression-bomb guard on probed width × height |
| `max_outputs` | Multi-file producers: `extract_frames`, `extract_streams`, HLS/DASH packaging (defaults apply when unset, e.g. 10 000 frames, 500 package files) |
| `max_pages` | Not applicable to video (document-oriented field) |

Hard engine bounds (not configurable):

- `MAX_INPUT_BYTES` = **32 GiB** sanity cap on any single input
  buffer (`LimitExceeded` beyond it).
- `COMMAND_TIMEOUT` = **600 s** per `ffmpeg` invocation;
  `PROBE_TIMEOUT` = **60 s** for `ffprobe`/`--version`/`-encoders`
  probes. Timeouts surface as `ExternalTool`.
- Job handlers additionally cap `path` payloads at
  `DEFAULT_VIDEO_MAX_BYTES` = **4 GiB**.

## Security model

- **No shell.** All invocations are `Command::new(path).args(...)` —
  no shell interpolation; filter strings pass through as single
  argv elements to ffmpeg's own parser.
- **Private temp dirs.** Every op writes inputs/outputs into a
  uniquely-named `TempDir` under the system temp dir
  (`kaladoc-video-<tag>-<pid>-<nanos>-<seq>`), removed on drop on
  success, failure, and timeout. Temp paths never escape into
  results.
- **Tool resolution.** `ffmpeg`/`ffprobe` are located by scanning
  `PATH` for an executable file — `MissingTool` if absent; results
  are never approximated.
- **Process hygiene.** stdout/stderr are drained on helper threads
  (no pipe deadlock); children are killed on timeout; `stdin` is
  null. Non-zero exits report a trimmed stderr tail (≤ 500 chars).
- **Job confinement.** `video.*` job payloads take a `path`
  **relative to the storage root**; absolute paths and `..` are
  rejected, same as `pdf.inspect`.
- **Content sniffing.** `security_engine::sniff_format` identifies
  video containers from magic bytes (MP4/MOV `ftyp`, Matroska, WebM,
  AVI, MPEG-TS/PS, FLV, ASF/WMV, MXF, F4V, Ogg, MPEG elementary) —
  used by `document.*` handlers for format reporting and size
  budgets (`SniffedFormat::is_video`).
- **Encoder honesty.** Codec availability is verified via
  `ffmpeg -encoders` (cached per process); unsupported targets fail
  with an explicit `ExternalTool` detail instead of a mid-run ffmpeg
  error.

## ffmpeg requirements

- `ffmpeg` and `ffprobe` on `PATH`; versions are reported by
  `kaladoc video versions` / `media_capabilities`.
- **Encoder-availability caveat.** Container coverage equals the
  host ffmpeg build's coverage. H.264 output requires `libx264` or
  `libopenh264`; WebM requires `libvpx-vp9` + `libopus`; encode
  presets and capabilities are discovered at runtime, so a minimal
  ffmpeg build (e.g. Debian's) supports fewer presets than a full
  build. Hardware acceleration is *detected*
  (`detect_hwaccel`, `list_hwaccels`, `media_capabilities`) but ops
  do not request it — see limitations.

## Job kinds

Registered by `application::job_handlers::register_builtin`
(`src/crates/application/job_handlers.rs`). All `video.*` payloads
are JSON; `path` is confined to the storage root and `max_bytes`
defaults to 4 GiB. Single-file ops write `<path>.<marker><ext>` or a
`.json` report next to the input; multi-file ops write under
`<path>.<name>/` directories.

| Kind | Payload (essence) | Output |
|---|---|---|
| `video.probe` | `{path, max_bytes?}` | `<path>.probe.json` |
| `video.thumbnail` | `{path, at_secs?, max_bytes?}` | `<path>.thumb.png` |
| `video.transcode` | `{path, target_ext, max_bytes?}` | `<path>.<target_ext>` |
| `video.trim` | `{path, start_secs, end_secs?, reencode?, max_bytes?}` | `<path>.trim<ext>` |
| `video.remux` | `{path, target_ext, max_bytes?}` | `<path>.<target_ext>` |
| `video.extract_frames` | `{path, every_secs?, fps?, all?, keyframes_only?, format?, max_bytes?}` | `<path>.frames/` |
| `video.subtitles_extract` | `{path, track?, format?, max_bytes?}` | `<path>.subs.<fmt>` |
| `video.analyze` | `{path, max_bytes?}` | `<path>.analysis.json` |
| `video.package_hls` | `{path, max_bytes?}` | `<path>.hls/` |
| `video.metadata_set` | `{path, tags:{k:v}, max_bytes?}` | `<path>.tagged<ext>` |
| `video.metadata_strip` | `{path, max_bytes?}` | `<path>.stripped<ext>` |
| `video.extract_streams` | `{path, subtitles?, attachments?, cover_art?, chapters?, data?, subtitle_format?, max_bytes?}` | `<path>.streams/` |
| `video.audio_volume` | `{path, gain_db, max_bytes?}` | `<path>.vol<ext>` |
| `video.contact_sheet` | `{path, cols?, rows?, thumb_width?, interval_secs?, max_bytes?}` | `<path>.sheet.png` |
| `video.speed` | `{path, factor, max_bytes?}` | `<path>.speed<ext>` |
| `video.reverse` | `{path, max_bytes?}` | `<path>.reverse<ext>` |
| `video.loop` | `{path, count, max_bytes?}` | `<path>.loop<ext>` |
| `video.freeze` | `{path, at_secs, duration_secs, max_bytes?}` | `<path>.freeze<ext>` |
| `video.resize` | `{path, width, height, fit?, max_bytes?}` | `<path>.resize<ext>` |
| `video.crop` | `{path, x, y, w, h, max_bytes?}` | `<path>.crop<ext>` |
| `video.rotate` | `{path, degrees, max_bytes?}` | `<path>.rotate<ext>` |
| `video.flip` | `{path, horizontal?, max_bytes?}` | `<path>.flip<ext>` |
| `video.gif` | `{path, fps?, width?, max_bytes?}` | `<path>.gif` |
| `video.slideshow` | `{paths:[...], fps?, duration_per_image?, target_ext, max_bytes?}` | `slideshow.<target_ext>` next to first input |
| `video.normalize` | `{path, target_lufs?, max_bytes?}` | `<path>.norm<ext>` |
| `video.loudness` | `{path, max_bytes?}` | `<path>.loudness.json` |
| `video.scene_frames` | `{path, threshold?, max_bytes?}` | `<path>.scenes/` |
| `video.merge_av` | `{video_path, audio_path, target_ext?, max_bytes?}` | `<video_path>.merged<ext>` |

Not job-exposed (CLI-only): `concat`, `extract_audio`, and the
geometry/audio/subtitle/filter/overlay/packaging/runtime commands
listed below. `video.merge_av` is the only two-input kind.

## CLI reference

`kaladoc video <subcommand>` (`src/crates/application/cli_mcp_server/
cli/command/video.rs`; clap args in `cli/args.rs`). Mutating commands
take `--output-file PATH`; without it a derived path next to the input
is used. `--output-dir` selects directories for multi-file outputs.
The global `--output json|yaml|table` flag selects the result format
for report-style commands.

```
# Inspection
kaladoc video probe            <file>
kaladoc video versions
kaladoc video capabilities     # JSON capability snapshot
kaladoc video devices
kaladoc video protocols
kaladoc video hwaccels

# Core media ops
kaladoc video thumbnail        <file> [--at SECS] [--output-file PATH]
kaladoc video trim             <file> --start SECS [--end SECS] [--reencode] [--output-file PATH]
kaladoc video transcode        <file> <target-ext> [--output-file PATH]
kaladoc video concat           <file>... --target-ext EXT [--output-file PATH]
kaladoc video extract-audio    <file> <ext> [--output-file PATH]
kaladoc video remux            <file> <target-ext> [--output-file PATH]
kaladoc video encode           <file> <profile> [ext] [--output-file PATH]
kaladoc video presets

# Geometry
kaladoc video resize           <file> --width W --height H [--fit fit|cover|contain|stretch]
kaladoc video crop             <file> --x X --y Y --width W --height H
kaladoc video pad              <file> [--left N --right N --top N --bottom N] [--color COLOR]
kaladoc video rotate           <file> --degrees DEG
kaladoc video flip             <file> [--vertical]
kaladoc video letterbox        <file> --width W --height H
kaladoc video pillarbox        <file> --width W --height H
kaladoc video autocrop         <file>
kaladoc video transpose        <file> [--counter-clockwise]
kaladoc video orient           <file> --orientation landscape|portrait

# Tempo
kaladoc video speed            <file> --factor F
kaladoc video reverse          <file>
kaladoc video loop             <file> --count N
kaladoc video freeze           <file> --at SECS --duration SECS

# Audio
kaladoc video volume           <file> --gain-db DB
kaladoc video normalize        <file> [--lufs LUFS]
kaladoc video audio-merge      <file>...
kaladoc video audio-concat     <file>... <ext>
kaladoc video fade             <file> [--in SECS] [--out SECS]
kaladoc video remove-audio     <file>
kaladoc video channels         <file> --count N
kaladoc video eq               <file> --bass DB --treble DB [--mid DB]
kaladoc video denoise          <file> --strength F
kaladoc video compress-audio   <file> --threshold DB --ratio R
kaladoc video pitch            <file> --semitones N
kaladoc video spectrogram      <file> [--width W --height H] --output-file PATH
kaladoc video mix              <file>... --weights 0.5,0.5 [--no-normalize]
kaladoc video crossfade        <file>... --duration SECS
kaladoc video delay            <file> --ms MS

# Frames / images
kaladoc video frames           <file> (--every SECS | --fps F | --all | --keyframes) [--format png|jpg] [--output-dir DIR]
kaladoc video image-sequence   <file> [--format png|jpg] [--output-dir DIR]
kaladoc video contact-sheet    <file> --cols C --rows R [--thumb-width W] [--interval SECS]
kaladoc video scene-frames     <file> [--threshold F] [--output-dir DIR]
kaladoc video slideshow        <file>... --fps F <ext> [--duration SECS]
kaladoc video to-gif           <file> [--fps F] [--width W]
kaladoc video to-webp          <file> [--fps F]
kaladoc video to-apng          <file> [--fps F]
kaladoc video audio-to-video   <file> [--style waveform|spectrum|both] --width W --height H
kaladoc video gif-to-video     <file> <ext>
kaladoc video replace-audio    <video> <audio>

# Extraction / subtitles
kaladoc video extract-streams  <file> [--subtitles] [--attachments] [--cover] [--chapters] [--data] [--subtitle-format srt|ass|vtt] [--output-dir DIR]
kaladoc video subtitles        <file> [--track N] [--format srt|ass|vtt]
kaladoc video subs-convert     <file> [--from FMT] --to srt|ass|vtt
kaladoc video subs-shift       <file> --offset SECS [--format FMT]
kaladoc video burn-subs        <file> <subs-file> [--style JSON]

# Filters / overlays / stream ops
kaladoc video filter           <file> [--vf CHAIN] [--af CHAIN]
kaladoc video filters          # preset list (currently empty)
kaladoc video overlay          <file> <image> --x X --y Y [--opacity F]
kaladoc video text             <file> --text T --x X --y Y [--size N] [--color 0xRRGGBB]
kaladoc video chroma-key       <foreground> <background> --color COLOR [--similarity F] [--blend F]
kaladoc video select-streams   <file> <ext> [--no-video] [--audio 0,1] [--subs 0]
kaladoc video merge-av         <video> <audio> [ext]
kaladoc video set-metadata     <file> --tag key=value [--tag ...]
kaladoc video strip-metadata   <file> [ext]
kaladoc video cover-art        <file> <image>

# Analysis / quality
kaladoc video analyze          <file>
kaladoc video scenes           <file> [--threshold F]
kaladoc video black-frames     <file>
kaladoc video freeze-frames    <file>
kaladoc video silences         <file>
kaladoc video loudness         <file>
kaladoc video normalize-lufs   <file> [--lufs LUFS]
kaladoc video audio-stats      <file>
kaladoc video quality          <reference> <distorted> [--metrics psnr,ssim,vmaf]

# Packaging
kaladoc video hls              <file> [--output-dir DIR]
kaladoc video dash             <file> [--output-dir DIR]
kaladoc video faststart        <file>
```

## Error taxonomy

| Variant | Meaning |
|---|---|
| `MissingTool` | `ffmpeg` or `ffprobe` not found on `PATH` (or not executable). Checked up front; no partial runs. |
| `InvalidInput` | Empty/unreadable file, unparseable `ffprobe` output, unsupported container, bad arguments (e.g. trim range outside duration, disallowed extension). |
| `LimitExceeded` | An `EngineLimits` bound or the 32 GiB hard cap was hit; the message names the limit and observed value. |
| `ExternalTool` | The `ffmpeg`/`ffprobe` process ran but exited non-zero, timed out (600 s / 60 s), could not be spawned, or lacked a required encoder; stderr tail (≤ 500 chars) is captured for diagnostics. |
| `Io` | Tempdir creation, input write, or output read failed outside the tool itself. |
| `Json` | `ffprobe` JSON output could not be parsed. |

## Honest limitations

- **Keyframe-aligned trims by default.** `trim` uses stream copy
  (`-c copy`) for speed and losslessness; cut points snap to the
  nearest keyframe. Pass `--reencode` for frame-accurate cuts at the
  cost of a generation loss.
- **Concat needs compatible inputs.** The concat demuxer with stream
  copy requires matching codecs/parameters/containers; mismatched
  inputs must be re-encoded first.
- **No GPU / hardware acceleration in ops.** All encode/decode is
  software `ffmpeg`; `hwaccel` is only *enumerated* (`list_hwaccels`,
  `detect_hwaccel`, `media_capabilities`) — no op passes an
  `hwaccel` flag.
- **Synchronous, one process per op.** Engine methods block on a
  single external process; no internal parallelism beyond what
  `ffmpeg` does, and each call gets a fresh 600 s timeout.
- **In-memory buffers.** Inputs and outputs are `&[u8]`/`Vec<u8>`
  (plus temp files for tool I/O); there is no streaming API, so very
  large jobs pay full memory cost on both sides.
- **Container coverage = ffmpeg's coverage.** Anything `ffmpeg`/
  `ffprobe` on the host supports works; exotic/proprietary codecs
  depend on how the host binary was built. `filter_presets` is an
  empty stub; `ops/{preview,stabilize,validate,metadata_export,
  concat_filter}.rs` exist on disk but are not wired into the build.
