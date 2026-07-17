//! Deterministic manifest action catalog and fail-before-effects resolution.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    ActionEffect, ActionParameterKind, ActionRole, ActionVerb, CameraManifest, ResponderMutation,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionCatalog {
    pub revision: String,
    pub actions: Vec<ActionCatalogEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionCatalogEntry {
    pub action_id: String,
    pub connection: String,
    pub mode: String,
    pub supported_roles: Vec<ActionRole>,
    pub parameters: Vec<ActionRoleParameters>,
    pub triggers: Vec<ActionEffect>,
    pub availability: ActionAvailability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionRoleParameters {
    pub role: ActionRole,
    pub parameters: Vec<ActionCatalogParameter>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionCatalogParameter {
    pub name: String,
    pub kind: ActionCatalogParameterKind,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionCatalogParameterKind {
    U32,
    U64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionAvailability {
    Available,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionArgument {
    pub name: String,
    pub value: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionInvocationRequest {
    pub catalog_revision: String,
    pub action_id: String,
    pub connection: String,
    pub mode: String,
    pub role: ActionRole,
    #[serde(default)]
    pub parameters: Vec<ActionArgument>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedActionInvocation {
    pub action: ActionVerb,
    pub role: ActionRole,
    pub parameters: BTreeMap<String, u64>,
    pub responder_mutation: Option<ResponderMutation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ActionResolutionError {
    #[error("stale catalog revision")]
    StaleRevision,
    #[error("unknown connection {0:?}")]
    UnknownConnection(String),
    #[error("unknown action {action:?} on connection {connection:?}")]
    UnknownAction { connection: String, action: String },
    #[error("action requires mode {expected:?}, got {actual:?}")]
    WrongMode { expected: String, actual: String },
    #[error("action does not support role {0:?}")]
    WrongRole(ActionRole),
    #[error("duplicate parameter {0:?}")]
    DuplicateParameter(String),
    #[error("missing parameter {0:?}")]
    MissingParameter(String),
    #[error("extra parameter {0:?}")]
    ExtraParameter(String),
    #[error("parameter {name:?} value {value} is outside its declaration")]
    InvalidParameter { name: String, value: u64 },
}

impl ActionResolutionError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::StaleRevision => "staleCatalogRevision",
            Self::UnknownConnection(_) => "unknownConnection",
            Self::UnknownAction { .. } => "unknownAction",
            Self::WrongMode { .. } => "wrongMode",
            Self::WrongRole(_) => "wrongRole",
            Self::DuplicateParameter(_) => "duplicateParameter",
            Self::MissingParameter(_) => "missingParameter",
            Self::ExtraParameter(_) => "extraParameter",
            Self::InvalidParameter { .. } => "invalidParameter",
        }
    }
}

impl CameraManifest {
    pub fn action_catalog(&self) -> ActionCatalog {
        let mut actions = Vec::new();
        for (connection_id, connection) in &self.connections {
            for (verb, action) in &connection.actions {
                let mut supported_roles = Vec::new();
                let mut parameters = Vec::new();
                if let Some(initiator) = &action.initiator {
                    supported_roles.push(ActionRole::Initiator);
                    parameters.push(ActionRoleParameters {
                        role: ActionRole::Initiator,
                        parameters: initiator
                            .params
                            .iter()
                            .map(|name| ActionCatalogParameter {
                                name: name.clone(),
                                kind: ActionCatalogParameterKind::U64,
                                required: true,
                                default: None,
                                min: None,
                                max: None,
                            })
                            .collect(),
                    });
                }
                if let Some(responder) = &action.responder {
                    supported_roles.push(ActionRole::Responder);
                    parameters.push(ActionRoleParameters {
                        role: ActionRole::Responder,
                        parameters: responder
                            .params
                            .iter()
                            .map(|parameter| ActionCatalogParameter {
                                name: parameter.name.clone(),
                                kind: match parameter.kind {
                                    ActionParameterKind::U32 => ActionCatalogParameterKind::U32,
                                },
                                required: parameter.default.is_none(),
                                default: parameter.default.map(u64::from),
                                min: parameter.min.map(u64::from),
                                max: parameter.max.map(u64::from),
                            })
                            .collect(),
                    });
                }
                actions.push(ActionCatalogEntry {
                    action_id: verb.as_str().to_string(),
                    connection: connection_id.clone(),
                    mode: action.mode.clone(),
                    supported_roles,
                    parameters,
                    triggers: action.triggers.clone(),
                    availability: ActionAvailability::Available,
                });
            }
        }
        let revision = catalog_revision(&actions);
        ActionCatalog { revision, actions }
    }

    pub fn resolve_action_invocation(
        &self,
        request: &ActionInvocationRequest,
    ) -> Result<ResolvedActionInvocation, ActionResolutionError> {
        let catalog = self.action_catalog();
        if request.catalog_revision != catalog.revision {
            return Err(ActionResolutionError::StaleRevision);
        }
        let connection = self
            .connections
            .get(&request.connection)
            .ok_or_else(|| ActionResolutionError::UnknownConnection(request.connection.clone()))?;
        let verb = request.action_id.parse::<ActionVerb>().map_err(|_| {
            ActionResolutionError::UnknownAction {
                connection: request.connection.clone(),
                action: request.action_id.clone(),
            }
        })?;
        let action =
            connection
                .actions
                .get(&verb)
                .ok_or_else(|| ActionResolutionError::UnknownAction {
                    connection: request.connection.clone(),
                    action: request.action_id.clone(),
                })?;
        if !action.mode.is_empty()
            && request.mode != action.mode
            && !request.mode.starts_with(&format!("{}/", action.mode))
        {
            return Err(ActionResolutionError::WrongMode {
                expected: action.mode.clone(),
                actual: request.mode.clone(),
            });
        }
        let declaration = catalog
            .actions
            .iter()
            .find(|entry| {
                entry.connection == request.connection && entry.action_id == request.action_id
            })
            .and_then(|entry| {
                entry
                    .parameters
                    .iter()
                    .find(|parameters| parameters.role == request.role)
            })
            .ok_or(ActionResolutionError::WrongRole(request.role))?;

        let mut supplied = BTreeMap::new();
        for argument in &request.parameters {
            if supplied
                .insert(argument.name.clone(), argument.value)
                .is_some()
            {
                return Err(ActionResolutionError::DuplicateParameter(
                    argument.name.clone(),
                ));
            }
        }
        let expected = declaration
            .parameters
            .iter()
            .map(|parameter| parameter.name.as_str())
            .collect::<BTreeSet<_>>();
        if let Some(extra) = supplied
            .keys()
            .find(|name| !expected.contains(name.as_str()))
        {
            return Err(ActionResolutionError::ExtraParameter(extra.clone()));
        }
        for parameter in &declaration.parameters {
            if !supplied.contains_key(&parameter.name) {
                if let Some(default) = parameter.default {
                    supplied.insert(parameter.name.clone(), default);
                } else {
                    return Err(ActionResolutionError::MissingParameter(
                        parameter.name.clone(),
                    ));
                }
            }
            let value = supplied[&parameter.name];
            if (parameter.kind == ActionCatalogParameterKind::U32 && value > u32::MAX as u64)
                || parameter.min.is_some_and(|min| value < min)
                || parameter.max.is_some_and(|max| value > max)
            {
                return Err(ActionResolutionError::InvalidParameter {
                    name: parameter.name.clone(),
                    value,
                });
            }
        }
        Ok(ResolvedActionInvocation {
            action: verb,
            role: request.role,
            parameters: supplied,
            responder_mutation: action
                .responder
                .as_ref()
                .filter(|_| request.role == ActionRole::Responder)
                .map(|binding| binding.mutation.clone()),
        })
    }
}

fn catalog_revision(actions: &[ActionCatalogEntry]) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(actions).expect("action catalog serializes"))
    )
}
