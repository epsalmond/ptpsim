use thiserror::Error;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("io error reading manifest: {0}")]
    Io(#[from] std::io::Error),
    #[error("YAML parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("unsupported schema {found:?}; this build understands {expected:?}")]
    Schema { found: String, expected: String },
}

/// A non-fatal finding from [`crate::CameraManifest::validate`]. Lints never
/// block loading — an unresolved `evidence:` id (e.g. a private client application doc not
/// shipped publicly) is a warning, by design.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lint {
    pub severity: Severity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Info,
}

impl Lint {
    pub fn warn(message: impl Into<String>) -> Self {
        Lint { severity: Severity::Warning, message: message.into() }
    }
}
