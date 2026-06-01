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

/// Errors raised by [`crate::ConfigStore::from_manufacturer_index`] (the loader for
/// the new manufacturer-index / family-inheritance shape used by the BLE-MVP).
/// Fail-fast: any error aborts the entire load — there is no partial-success path
/// (plan §11.10).
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("YAML parse error in manufacturer index: {0}")]
    IndexParse(serde_yaml::Error),
    #[error("YAML parse error in model body '{id}': {err}")]
    BodyParse { id: String, err: serde_yaml::Error },
    #[error("model '{id}' declared in index but no body supplied")]
    MissingModelBody { id: String },
    #[error("model '{model_id}' inherits from unknown family '{family_id}'")]
    UnknownFamily { model_id: String, family_id: String },
    #[error("validation failed at {path}: {message}")]
    Validation { path: String, message: String },
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
        Lint {
            severity: Severity::Warning,
            message: message.into(),
        }
    }
}
