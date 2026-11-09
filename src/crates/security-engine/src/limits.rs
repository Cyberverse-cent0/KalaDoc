//! Resource-limit configuration and enforcement.
//!
//! [`ResourceLimits`] is the single place upload/processing budgets are
//! declared. It is serde-friendly so limits can come from config files,
//! environment, or per-request overrides; `#[serde(default)]` fills any
//! missing field from [`ResourceLimits::default`], so partial configs
//! are safe.
//!
//! Enforce limits *before* doing expensive work — check sizes before
//! reading bodies, page counts before parsing, output counts before
//! spawning jobs.

use super::error::{SecurityError, SecurityResult};

/// Byte/page/output budgets for uploads and processing operations.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ResourceLimits {
    /// Maximum accepted upload size in bytes.
    pub max_upload_bytes: u64,
    /// Maximum page count for a single document; `None` = unlimited.
    pub max_pages: Option<u32>,
    /// Maximum total bytes an operation may produce across all outputs.
    pub max_operation_bytes: u64,
    /// Maximum number of output artifacts one operation may produce.
    pub max_outputs: u32,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_upload_bytes: 100 * 1024 * 1024, // 100 MiB
            max_pages: Some(10_000),
            max_operation_bytes: 256 * 1024 * 1024, // 256 MiB
            max_outputs: 64,
        }
    }
}

impl ResourceLimits {
    /// Check an upload's byte size against [`Self::max_upload_bytes`].
    ///
    /// # Errors
    /// [`SecurityError::FileTooLarge`] when `len` exceeds the limit.
    pub fn check_size(&self, len: u64) -> SecurityResult<()> {
        if len > self.max_upload_bytes {
            return Err(SecurityError::FileTooLarge {
                size: len,
                max: self.max_upload_bytes,
            });
        }
        Ok(())
    }

    /// Check an operation's total output size against
    /// [`Self::max_operation_bytes`].
    ///
    /// # Errors
    /// [`SecurityError::OperationTooLarge`] when `len` exceeds the limit.
    pub fn check_operation_size(&self, len: u64) -> SecurityResult<()> {
        if len > self.max_operation_bytes {
            return Err(SecurityError::OperationTooLarge {
                size: len,
                max: self.max_operation_bytes,
            });
        }
        Ok(())
    }

    /// Check a document's page count against [`Self::max_pages`].
    /// Always succeeds when `max_pages` is `None`.
    ///
    /// # Errors
    /// [`SecurityError::TooManyPages`] when `pages` exceeds the limit.
    pub fn check_pages(&self, pages: u32) -> SecurityResult<()> {
        if let Some(max) = self.max_pages {
            if pages > max {
                return Err(SecurityError::TooManyPages { pages, max });
            }
        }
        Ok(())
    }

    /// Check a requested output count against [`Self::max_outputs`].
    ///
    /// # Errors
    /// [`SecurityError::TooManyOutputs`] when `count` exceeds the limit.
    pub fn check_outputs(&self, count: u32) -> SecurityResult<()> {
        if count > self.max_outputs {
            return Err(SecurityError::TooManyOutputs {
                count,
                max: self.max_outputs,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_values_are_sane() {
        let l = ResourceLimits::default();
        assert_eq!(l.max_upload_bytes, 100 * 1024 * 1024);
        assert_eq!(l.max_pages, Some(10_000));
        assert!(l.max_operation_bytes >= l.max_upload_bytes);
        assert!(l.max_outputs > 0);
    }

    #[test]
    fn serde_round_trip_and_partial_config() {
        let l = ResourceLimits::default();
        let json = serde_json::to_string(&l).unwrap();
        assert_eq!(serde_json::from_str::<ResourceLimits>(&json).unwrap(), l);

        // Missing fields fall back to defaults.
        let partial: ResourceLimits = serde_json::from_str(r#"{"max_pages": 5}"#).unwrap();
        assert_eq!(partial.max_pages, Some(5));
        assert_eq!(
            partial.max_upload_bytes,
            ResourceLimits::default().max_upload_bytes
        );
    }

    #[test]
    fn check_size_boundaries() {
        let mut l = ResourceLimits::default();
        l.max_upload_bytes = 100;
        assert!(l.check_size(100).is_ok()); // exactly at max
        assert!(l.check_size(0).is_ok());
        assert!(matches!(
            l.check_size(101),
            Err(SecurityError::FileTooLarge {
                size: 101,
                max: 100
            })
        ));
        assert!(matches!(
            l.check_size(u64::MAX),
            Err(SecurityError::FileTooLarge { .. })
        ));
    }

    #[test]
    fn check_operation_size_boundaries() {
        let mut l = ResourceLimits::default();
        l.max_operation_bytes = 50;
        assert!(l.check_operation_size(50).is_ok());
        assert!(matches!(
            l.check_operation_size(51),
            Err(SecurityError::OperationTooLarge { size: 51, max: 50 })
        ));
    }

    #[test]
    fn check_pages_boundaries() {
        let mut l = ResourceLimits::default();
        l.max_pages = Some(10);
        assert!(l.check_pages(10).is_ok());
        assert!(l.check_pages(0).is_ok());
        assert!(matches!(
            l.check_pages(11),
            Err(SecurityError::TooManyPages { pages: 11, max: 10 })
        ));
        l.max_pages = None;
        assert!(l.check_pages(u32::MAX).is_ok());
    }

    #[test]
    fn check_outputs_boundaries() {
        let mut l = ResourceLimits::default();
        l.max_outputs = 3;
        assert!(l.check_outputs(3).is_ok());
        assert!(matches!(
            l.check_outputs(4),
            Err(SecurityError::TooManyOutputs { count: 4, max: 3 })
        ));
    }
}
