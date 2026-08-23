//! Consumer-neutral connection activity descriptors (schema §11.23).

use crate::model::SocketRole;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionActivityDescriptor {
    pub id: String,
    pub version: u32,
    pub display_role: ConnectionActivityDisplayRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub default_expected_duration_ms: u32,
    pub interaction_required: bool,
    #[serde(default)]
    pub optional: bool,
    #[serde(flatten)]
    pub binding: ConnectionActivityBinding,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConnectionActivityBinding {
    ExecutorSpan(ExecutorSpanBinding),
    HostCheckpoint(HostCheckpointBinding),
    HostEstablishment(HostEstablishmentBinding),
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

/// A typed host-owned establishment action. Unlike a `hostCheckpoint`, this is
/// executable consumer contract rather than presentation-only progress metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostEstablishmentBinding {
    pub host_establishment: ConnectionActivityHostEstablishment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ConnectionActivityHostEstablishment {
    /// Read the named runtime-scope value and require an exact observed network
    /// identity match. An absent or undisclosed observation does not pass.
    NetworkIdentityExact {
        #[serde(rename = "networkIdentityExact")]
        network_identity_exact: NetworkIdentityExactBinding,
    },
    /// Open and retain the real protocol session on this socket role. This is
    /// the endpoint-reachability proof; it is not a disposable probe.
    RetainedSessionOpen {
        #[serde(rename = "retainedSessionOpen")]
        retained_session_open: RetainedSessionOpenBinding,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NetworkIdentityExactHostEstablishment {
    network_identity_exact: NetworkIdentityExactBinding,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RetainedSessionOpenHostEstablishment {
    retained_session_open: RetainedSessionOpenBinding,
}

impl<'de> Deserialize<'de> for ConnectionActivityHostEstablishment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum HostEstablishment {
            NetworkIdentityExact(NetworkIdentityExactHostEstablishment),
            RetainedSessionOpen(RetainedSessionOpenHostEstablishment),
        }

        Ok(match HostEstablishment::deserialize(deserializer)? {
            HostEstablishment::NetworkIdentityExact(binding) => Self::NetworkIdentityExact {
                network_identity_exact: binding.network_identity_exact,
            },
            HostEstablishment::RetainedSessionOpen(binding) => Self::RetainedSessionOpen {
                retained_session_open: binding.retained_session_open,
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConnectionActivityIdentity {
    ExecutorSpan,
    HostCheckpoint(String),
    HostEstablishment(ConnectionActivityHostEstablishment),
}

impl ConnectionActivityDescriptor {
    pub(crate) fn identity(&self) -> ConnectionActivityIdentity {
        match &self.binding {
            ConnectionActivityBinding::ExecutorSpan(_) => ConnectionActivityIdentity::ExecutorSpan,
            ConnectionActivityBinding::HostCheckpoint(binding) => {
                ConnectionActivityIdentity::HostCheckpoint(binding.host_checkpoint.name.clone())
            }
            ConnectionActivityBinding::HostEstablishment(binding) => {
                ConnectionActivityIdentity::HostEstablishment(binding.host_establishment.clone())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkIdentityExactBinding {
    pub expected_scope: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetainedSessionOpenBinding {
    pub socket_role: SocketRole,
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

#[cfg(test)]
mod tests {
    use super::ConnectionActivityDescriptor;

    #[test]
    fn activity_title_round_trips_yaml() {
        let source = r#"
id: camera.test.titled
version: 1
displayRole: connecting
title: Connecting to the camera
defaultExpectedDurationMs: 1000
interactionRequired: false
hostCheckpoint: { name: titled }
"#;
        let descriptor: ConnectionActivityDescriptor =
            serde_yaml::from_str(source).expect("titled descriptor parses");
        assert_eq!(
            descriptor.title.as_deref(),
            Some("Connecting to the camera")
        );

        let rendered = serde_yaml::to_string(&descriptor).expect("titled descriptor serializes");
        let reparsed: ConnectionActivityDescriptor =
            serde_yaml::from_str(&rendered).expect("serialized descriptor parses");
        assert_eq!(reparsed, descriptor);
    }
}
