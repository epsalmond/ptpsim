//! Consumer-neutral connection activity descriptors (schema §11.23).

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionActivityDescriptor {
    pub id: String,
    pub version: u32,
    pub display_role: ConnectionActivityDisplayRole,
    pub default_expected_duration_ms: u32,
    pub interaction_required: bool,
    #[serde(flatten)]
    pub binding: ConnectionActivityBinding,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConnectionActivityBinding {
    ExecutorSpan(ExecutorSpanBinding),
    HostCheckpoint(HostCheckpointBinding),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutorSpanBinding {
    pub executor_span: ConnectionActivityExecutorSpan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostCheckpointBinding {
    pub host_checkpoint: ConnectionActivityHostCheckpoint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionActivityExecutorSpan {
    pub sequence: ConnectionActivitySequence,
    pub start_step: u32,
    pub end_step_exclusive: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionActivityHostCheckpoint {
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConnectionActivitySequence {
    Steps,
    PostExitReadiness,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionActivityDisplayRole {
    Connecting,
    WaitingForCamera,
    ConfirmingPairing,
    PreparingConnection,
    StartingNetwork,
    JoiningNetwork,
    OpeningSession,
    Unknown(String),
}

impl ConnectionActivityDisplayRole {
    pub fn as_token(&self) -> &str {
        match self {
            Self::Connecting => "connecting",
            Self::WaitingForCamera => "waitingForCamera",
            Self::ConfirmingPairing => "confirmingPairing",
            Self::PreparingConnection => "preparingConnection",
            Self::StartingNetwork => "startingNetwork",
            Self::JoiningNetwork => "joiningNetwork",
            Self::OpeningSession => "openingSession",
            Self::Unknown(raw) => raw,
        }
    }

    fn from_token(raw: String) -> Self {
        match raw.as_str() {
            "connecting" => Self::Connecting,
            "waitingForCamera" => Self::WaitingForCamera,
            "confirmingPairing" => Self::ConfirmingPairing,
            "preparingConnection" => Self::PreparingConnection,
            "startingNetwork" => Self::StartingNetwork,
            "joiningNetwork" => Self::JoiningNetwork,
            "openingSession" => Self::OpeningSession,
            _ => Self::Unknown(raw),
        }
    }
}

impl Serialize for ConnectionActivityDisplayRole {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_token())
    }
}

impl<'de> Deserialize<'de> for ConnectionActivityDisplayRole {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from_token)
    }
}

pub fn valid_activity_id(id: &str) -> bool {
    let mut segments = id.split('.');
    let Some(first) = segments.next() else {
        return false;
    };
    let Some(second) = segments.next() else {
        return false;
    };
    valid_id_segment(first) && valid_id_segment(second) && segments.all(valid_id_segment)
}

fn valid_id_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
