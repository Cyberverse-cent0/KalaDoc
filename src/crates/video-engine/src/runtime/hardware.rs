//! Hardware-acceleration discovery: which hwaccels this ffmpeg
//! build lists and which (if any) are plausibly usable on this host.

use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::model::CaptureDevice;
use crate::crates::video_engine::ops::{FFMPEG, PROBE_TIMEOUT, find_tool, run_tool};

/// `ffmpeg -hwaccels` as [`CaptureDevice`] entries of kind
/// `"hwaccel"`. Empty list (not an error) when ffmpeg is missing —
/// callers treat absence as "software only".
pub(crate) fn list_hwaccels() -> VideoResult<Vec<CaptureDevice>> {
    let program = find_tool(FFMPEG)?;
    let args = vec!["-hide_banner".to_string(), "-hwaccels".to_string()];
    let out = run_tool(FFMPEG, &program, &args, PROBE_TIMEOUT)?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("------"))
        .skip(1)
        .filter_map(|line| {
            let name = line.split_whitespace().nth(1)?;
            Some(CaptureDevice {
                kind: "hwaccel".to_string(),
                name: name.to_string(),
                description: None,
            })
        })
        .collect())
}

/// Best-guess usable hwaccel for this host, in preference order:
/// `cuda`, `vaapi`, `videotoolbox`, `qsv`, `amf`, `dxva2`, `d3d11va`,
/// `opencl`. Presence in `-hwaccels` is necessary but not sufficient
/// (drivers/devices may still be absent) — callers must still allow
/// software fallback when an accelerated run fails.
pub(crate) fn detect_hwaccel() -> Option<String> {
    const PREFERENCE: &[&str] = &[
        "cuda",
        "vaapi",
        "videotoolbox",
        "qsv",
        "amf",
        "dxva2",
        "d3d11va",
        "opencl",
    ];
    let listed = list_hwaccels().ok()?;
    let names: Vec<String> = listed.into_iter().map(|d| d.name).collect();
    PREFERENCE
        .iter()
        .find(|p| names.iter().any(|n| n == *p))
        .map(|s| s.to_string())
}
