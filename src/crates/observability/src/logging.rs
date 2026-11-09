//! Global `tracing` subscriber setup.
//!
//! [`init_tracing`] installs a `tracing_subscriber::fmt` subscriber
//! once per process. Subscriber installation is global and
//! once-only, so the function is idempotent-safe: the first call
//! installs and returns `Ok(true)`; every later call returns
//! `Ok(false)` instead of failing. Library code only — binaries
//! (e.g. the `kaladoc` CLI) decide when and with which level to call
//! it.
//!
//! Filter precedence: `RUST_LOG` wins when set and valid; otherwise
//! the level passed by the caller is used.

use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

/// Errors from building the log filter or subscriber.
#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    /// Neither `RUST_LOG` nor the caller-supplied level parsed as a
    /// valid `tracing` filter directive.
    #[error("invalid log filter (level '{level}'): {source}")]
    InvalidFilter {
        /// The level string that failed to parse.
        level: String,
        /// Underlying parse error from `tracing-subscriber`.
        source: tracing_subscriber::filter::ParseError,
    },
}

/// Build the effective [`EnvFilter`]: `RUST_LOG` if set and valid,
/// else `level`. Exposed separately so callers/tests can inspect the
/// filter without installing a global subscriber.
pub fn build_env_filter(level: &str) -> Result<EnvFilter, LoggingError> {
    EnvFilter::try_from_default_env().or_else(|_| {
        EnvFilter::try_new(level).map_err(|source| LoggingError::InvalidFilter {
            level: level.to_string(),
            source,
        })
    })
}

/// Install the global tracing subscriber. Returns `Ok(true)` when
/// this call installed it, `Ok(false)` when a subscriber was already
/// set (by an earlier call or by the embedding application).
///
/// Format is `fmt` compact on stderr — conventional for CLI
/// diagnostics so stdout stays clean for `--output json`.
pub fn init_tracing(level: &str) -> Result<bool, LoggingError> {
    install(build_env_filter(level)?)
}

/// Same as [`init_tracing`] but takes a [`LevelFilter`] instead of a
/// string. `RUST_LOG` still takes precedence when set.
pub fn init_tracing_with_level_filter(level: LevelFilter) -> Result<bool, LoggingError> {
    // `LevelFilter`'s `Display` yields names like "INFO"/"OFF" that the
    // `EnvFilter` directive parser accepts case-insensitively.
    install(build_env_filter(&level.to_string())?)
}

fn install(filter: EnvFilter) -> Result<bool, LoggingError> {
    // try_init's only failure mode is "a global subscriber is already
    // set" — map it to the distinguishable already-installed signal.
    match tracing_subscriber::fmt()
        .with_env_filter(filter)
        .compact()
        .with_writer(std::io::stderr)
        .try_init()
    {
        Ok(()) => Ok(true),
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_env_filter_accepts_level_names() {
        for level in ["trace", "debug", "info", "warn", "error", "off"] {
            assert!(build_env_filter(level).is_ok(), "level {level}");
        }
    }

    #[test]
    fn build_env_filter_accepts_directives() {
        assert!(build_env_filter("kaladoc=debug,info").is_ok());
    }

    #[test]
    fn build_env_filter_rejects_garbage() {
        // Bare words are valid `EnvFilter` syntax (a target name at
        // TRACE), so an invalid *level* is needed to force a parse
        // error: the part after `=` must be a real level.
        assert!(matches!(
            build_env_filter("kaladoc=not-a-level"),
            Err(LoggingError::InvalidFilter { .. })
        ));
    }

    // Subscriber installation is process-global and once-only, so
    // this test performs both calls itself: whichever order the test
    // binary runs tests in, the first call may already have installed
    // (e.g. via init_tracing in another test), but the *second*
    // consecutive call must always report already-installed.
    #[test]
    fn init_tracing_is_idempotent() {
        let first = init_tracing("warn").expect("valid level");
        let second = init_tracing("debug").expect("valid level");
        assert!(
            !second,
            "second init_tracing call must report already-installed"
        );
        // `first` is true only if no other test installed a subscriber
        // first; both outcomes are valid, so assert nothing about it.
        let _ = first;
    }

    #[test]
    fn init_tracing_propagates_filter_errors() {
        assert!(matches!(
            init_tracing("kaladoc=not-a-level"),
            Err(LoggingError::InvalidFilter { .. })
        ));
    }
}
