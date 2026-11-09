# Video Engine

`src/crates/video-engine/` provides video/audio inspection and
processing for KalaDoc. Every operation delegates to the external
`ffmpeg` and `ffprobe` command-line tools — the engine shells out, it
does not decode media in-process and **never bundles the binaries**.
Both must be installed on `PATH` (see `.doc/Architecture.md` →
`deployments::require` for per-distro install hints); when either is
missing the engine reports itself `Unavailable` via `health()` and
operations fail with `MissingTool`.

## Contracts

- Implements `ProcessingEngine` with `name = "video"` and
  `kind = EngineKind::Video`; registered in
  `EngineRegistry::builtin()` alongside the other engines.
- Advertised operations: `Inspect` (probe), `Thumbnail`, `Trim`,
  `Transcode`, `Concat`, `ExtractAudio`.
- `EngineLimits` fields honored: `max_duration_secs` (rejects inputs
  longer than the configured duration — checked from `ffprobe` metadata
  before any `ffmpeg` run) and `max_pixels` (bounds decoded frame
  dimensions for thumbnail extraction, a decompression-bomb guard).
  `max_input_bytes` caps the source file size where applicable.

## Operations

| Op | ffmpeg/ffprobe invocation (essence) | Limits enforced |
|---|---|---|
| `probe` | `ffprobe -v error -print_format json -show_format -show_streams <in>` | `max_input_bytes`; duration reported, not gated |
| `thumbnail` | `ffmpeg -ss <t> -i <in> -frames:v 1 -vf scale=... <out.jpg>` | `max_duration_secs` (input), `max_pixels` (output frame), timestamp within duration |
| `trim` | `ffmpeg -ss <start> -t <dur> [-c copy \| re-encode] <in> <out>` | `max_duration_secs` on input and on requested segment |
| `transcode` | `ffmpeg -i <in> [-c:v/-c:a codec opts] <out>` | `max_duration_secs`, `max_input_bytes` |
| `concat` | `ffmpeg -f concat -safe 0 -i list.txt <out>` (stream copy when containers/codecs match, else re-encode) | `max_duration_secs` (sum of inputs) |
| `extract_audio` | `ffmpeg -i <in> -vn -c:a <codec> <out>` | `max_duration_secs` |

Exact flags and output naming are defined by the `ops/` modules; the
table above is the stable contract level.

## Temp-file lifecycle

Every operation that produces bytes materializes `ffmpeg` output in a
unique temp directory (never next to the source, never a fixed name).
The directory is removed when the operation returns — on success,
failure, and timeout alike. Callers receive the output bytes or a
storage-relative path; temp paths never escape into results.

## Error taxonomy

| Variant | Meaning |
|---|---|
| `MissingTool` | `ffmpeg` or `ffprobe` not found on `PATH` (or not executable). Checked up front; no partial runs. |
| `InvalidInput` | Unreadable file, unparseable `ffprobe` output, unsupported container, bad arguments (e.g. trim range outside duration). |
| `LimitExceeded` | An `EngineLimits` bound was hit (`max_duration_secs`, `max_pixels`, `max_input_bytes`); the message names the limit and observed value. |
| `ExternalTool` | The `ffmpeg`/`ffprobe` process ran but exited non-zero or timed out; stderr is captured for diagnostics (truncated, no secrets/paths outside the workspace). |

## CLI and jobs

```
kaladoc video probe          <input> [--output json|yaml]
kaladoc video thumbnail      <input> [--at SECS] [--output-file PATH]
kaladoc video trim           <input> --start SECS [--end SECS] [--reencode] [--output-file PATH]
kaladoc video transcode      <input> <target-ext> [--output-file PATH]
kaladoc video concat         <input1> <input2> [...] --target-ext EXT [--output-file PATH]
kaladoc video extract-audio  <input> <ext> [--output-file PATH]
```

`--output-file` names the produced artifact; without it a derived path
next to the input is used. The global `--output` flag still selects the
result format (`table`/`json`/`yaml`).

Job kinds registered with the job engine
(`application::job_handlers::register_builtin`):

- `video.probe`
- `video.thumbnail`
- `video.transcode`
- `video.trim`

(`concat` and `extract_audio` are CLI-only for now.) Job payloads are
JSON with paths confined to the storage root — absolute paths and `..`
are rejected, same as `pdf.inspect`.

## Honest limitations

- **Keyframe-aligned trims by default.** `trim` uses stream copy
  (`-c copy`) for speed and losslessness; cut points snap to the
  nearest keyframe. Pass the re-encode option for frame-accurate cuts
  at the cost of a generation loss.
- **Concat needs compatible inputs.** The concat demuxer with stream
  copy requires matching codecs/parameters/containers; mismatched
  inputs must be re-encoded first (or use `--reencode`).
- **No GPU / hardware acceleration.** All encode/decode is software
  `ffmpeg`; no `hwaccel`, NVENC/QSV/VideoToolbox paths.
- **Single-threaded orchestration.** One external process per
  operation; no internal parallelism beyond what `ffmpeg` does.
- **Container coverage = ffmpeg's coverage.** Anything `ffmpeg`/
  `ffprobe` on the host supports works; exotic/proprietary codecs
  depend on how the host binary was built.
