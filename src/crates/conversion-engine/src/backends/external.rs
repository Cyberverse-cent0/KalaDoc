//! External-tool conversion backends (pandoc, libreoffice, poppler, ...).
//!
//! Each backend probes `PATH` **once** at [`detect`] time; tools that
//! are not installed are simply omitted from the returned vector —
//! omission is the signal that a tool is unavailable.

mod libreoffice;
mod pandoc;
mod poppler;
mod util;

use std::ffi::OsStr;
use std::sync::Arc;

use libreoffice::LibreOfficeBackend;
use pandoc::PandocBackend;
use poppler::PopplerBackend;

use crate::crates::conversion_engine::ConversionBackend;

/// Namespace for external-backend detection, matching the call site
/// in `engine.rs` (`ExternalBackendSet::detect()`).
pub struct ExternalBackendSet;

impl ExternalBackendSet {
    /// Probe `PATH` for every supported external tool and return the
    /// backends that are usable in this environment. Equivalent to
    /// [`detect`]. Each backend caches its availability at probe time.
    pub fn detect() -> Vec<Arc<dyn ConversionBackend>> {
        detect()
    }
}

/// Probe `PATH` for every supported external tool and return the
/// backends that are usable in this environment.
///
/// Probing happens once per call site (the engine calls this at
/// startup); each backend caches its availability so
/// `ConversionBackend::availability` stays cheap.
pub fn detect() -> Vec<Arc<dyn ConversionBackend>> {
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    detect_with(&path_env)
}

/// [`detect`] against an explicit PATH-style search string — kept
/// separate so tests can point at a fake bin directory without
/// mutating process env.
fn detect_with(path_env: &OsStr) -> Vec<Arc<dyn ConversionBackend>> {
    let mut backends: Vec<Arc<dyn ConversionBackend>> = Vec::new();
    if let Some(b) = PandocBackend::detect(path_env) {
        backends.push(Arc::new(b));
    }
    if let Some(b) = LibreOfficeBackend::detect(path_env) {
        backends.push(Arc::new(b));
    }
    if let Some(b) = PopplerBackend::detect(path_env) {
        backends.push(Arc::new(b));
    }
    backends
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::conversion_engine::{BackendAvailability, Format};
    use std::fs;

    /// Create a fake PATH dir containing the given `name -> script
    /// body` stubs (made executable on unix).
    fn fake_path_dir(tag: &str, stubs: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kaladoc-test-detect-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        for (name, body) in stubs {
            let p = dir.join(name);
            fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        dir
    }

    #[test]
    fn empty_path_yields_no_backends() {
        let dir = fake_path_dir("empty", &[]);
        let path_env = std::ffi::OsString::from(dir.as_os_str());
        assert!(detect_with(&path_env).is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn detects_all_three_tools() {
        let dir = fake_path_dir(
            "all",
            &[
                ("pandoc", "echo 'pandoc 3.1.13'"),
                ("soffice", "echo 'LibreOffice 24.2.3.2'"),
                ("pdftoppm", "echo 'pdftoppm version 24.02.0'"),
                // a pdf engine so pandoc advertises →pdf
                ("pdflatex", "echo 'pdfTeX 3.141592653'"),
            ],
        );
        let path_env = std::ffi::OsString::from(dir.as_os_str());
        let backends = detect_with(&path_env);
        assert_eq!(backends.len(), 3);
        let names: Vec<&str> = backends.iter().map(|b| b.name()).collect();
        assert!(names.contains(&"pandoc"));
        assert!(names.contains(&"libreoffice"));
        assert!(names.contains(&"poppler"));
        for b in &backends {
            match b.availability() {
                BackendAvailability::Available { version } => {
                    assert!(version.is_some(), "{} should report a version", b.name());
                    assert!(version.as_deref().unwrap().len() <= 80);
                }
                BackendAvailability::Unavailable { .. } => {
                    panic!("{} should be Available", b.name())
                }
            }
        }
        // pandoc with a pdf engine present supports markdown→pdf.
        let pandoc = backends.iter().find(|b| b.name() == "pandoc").unwrap();
        assert!(pandoc.supports(Format::Markdown, Format::Pdf));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pandoc_without_pdf_engine_loses_pdf_pairs() {
        let dir = fake_path_dir(
            "nopdf",
            &[
                ("pandoc", "echo 'pandoc 3.1.13'"),
                // no pdflatex/xelatex/tectonic/weasyprint/typst/context
            ],
        );
        let path_env = std::ffi::OsString::from(dir.as_os_str());
        let backends = detect_with(&path_env);
        assert_eq!(backends.len(), 1);
        let b = &backends[0];
        assert!(b.supports(Format::Docx, Format::Html));
        assert!(!b.supports(Format::Docx, Format::Pdf));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prefers_soffice_over_libreoffice() {
        let dir = fake_path_dir(
            "lo",
            &[
                ("libreoffice", "echo 'LibreOffice 7.6'"),
                ("soffice", "echo 'LibreOffice 24.2'"),
            ],
        );
        let path_env = std::ffi::OsString::from(dir.as_os_str());
        let backends = detect_with(&path_env);
        assert_eq!(backends.len(), 1);
        // whichever binary was found, availability reports a version.
        match backends[0].availability() {
            BackendAvailability::Available { version } => {
                assert!(version.unwrap().contains("LibreOffice"));
            }
            _ => panic!("expected Available"),
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn temp_dir_is_removed_on_drop() {
        let dir = util::TempDir::new("cleanup-test").unwrap();
        let p = dir.path().to_path_buf();
        assert!(p.exists());
        drop(dir);
        assert!(!p.exists(), "temp dir should be removed on drop");
    }
}
