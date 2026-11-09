//! Resource limits for pipeline runs.
//!
//! [`PipelineLimits`] bounds the *pipeline itself* — how many steps may
//! run, how large each step's input may be, and how many bytes the
//! whole run may materialize cumulatively. Page-count and output-count
//! budgets are forwarded to the wrapped [`PdfEngine`](crate::crates::pdf_engine::PdfEngine),
//! which enforces them inside its ops.
//!
//! Like [`ResourceLimits`](crate::crates::security_engine::ResourceLimits),
//! this type is serde-friendly with `#[serde(default)]` so partial
//! configs are safe.

use serde::{Deserialize, Serialize};

/// Budgets enforced by [`PipelineEngine::run_pipeline`](crate::crates::pipeline_engine::PipelineEngine::run_pipeline).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PipelineLimits {
    /// Maximum size of the byte stream any single step may consume, in
    /// bytes. Also used as the `max_bytes` cap for `security.validate`
    /// steps that do not override it via params.
    pub max_input_bytes: u64,
    /// Maximum total bytes materialized across the entire run: the
    /// input plus every byte-producing step's output, summed. Guards
    /// against amplification attacks (e.g. repeated transforms).
    pub max_total_bytes: u64,
    /// Maximum number of steps a definition may contain.
    pub max_steps: u32,
    /// Page-count budget forwarded to the PDF engine
    /// ([`EngineLimits::max_pages`](crate::crates::core::engine::EngineLimits)).
    /// `None` = unlimited.
    pub max_pages: Option<u32>,
    /// Output-count budget forwarded to the PDF engine
    /// ([`EngineLimits::max_outputs`](crate::crates::core::engine::EngineLimits)).
    /// `None` = unlimited.
    pub max_outputs: Option<u32>,
}

impl Default for PipelineLimits {
    fn default() -> Self {
        Self {
            // Match the security engine's default upload cap.
            max_input_bytes: 100 * 1024 * 1024, // 100 MiB
            // Generous cumulative cap: inputs plus intermediate and
            // final outputs. Still bounded so a pipeline cannot
            // materialize unbounded memory.
            max_total_bytes: 512 * 1024 * 1024, // 512 MiB
            max_steps: 64,
            max_pages: Some(10_000),
            max_outputs: Some(64),
        }
    }
}

impl PipelineLimits {
    /// Check a single step's input size against `max_input_bytes`.
    ///
    /// # Errors
    /// `Err` with a descriptive message when `len` exceeds the limit.
    pub fn check_step_input(&self, len: u64) -> Result<(), String> {
        if len > self.max_input_bytes {
            return Err(format!(
                "step input of {len} bytes exceeds max_input_bytes ({})",
                self.max_input_bytes
            ));
        }
        Ok(())
    }

    /// Check cumulative materialized bytes against `max_total_bytes`.
    ///
    /// # Errors
    /// `Err` with a descriptive message when `total` exceeds the limit.
    pub fn check_total(&self, total: u64) -> Result<(), String> {
        if total > self.max_total_bytes {
            return Err(format!(
                "pipeline materialized {total} bytes, exceeding max_total_bytes ({})",
                self.max_total_bytes
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_round_trip_and_partial_config() {
        let l = PipelineLimits::default();
        let json = serde_json::to_string(&l).unwrap();
        assert_eq!(serde_json::from_str::<PipelineLimits>(&json).unwrap(), l);

        let partial: PipelineLimits = serde_json::from_str(r#"{"max_steps": 4}"#).unwrap();
        assert_eq!(partial.max_steps, 4);
        assert_eq!(
            partial.max_input_bytes,
            PipelineLimits::default().max_input_bytes
        );
    }

    #[test]
    fn boundary_checks() {
        let mut l = PipelineLimits::default();
        l.max_input_bytes = 100;
        l.max_total_bytes = 250;
        assert!(l.check_step_input(100).is_ok());
        assert!(l.check_step_input(101).is_err());
        assert!(l.check_total(250).is_ok());
        assert!(l.check_total(251).is_err());
    }
}
