//! Request bodies for the simulator service's control HTTP routes. They live
//! here, beside `StateOverlay` and `FaultSpec`, so the service and its clients
//! (such as `camera-simctl`) share one definition of each wire shape.

use camera_config::{ActionArgument, ActionRole};
use serde::{Deserialize, Serialize};

/// Body of `POST /actions/<id>`. The action id comes from the path and the
/// connection from the running service, so neither appears here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionRunRequest {
    pub catalog_revision: String,
    pub mode: String,
    pub role: ActionRole,
    #[serde(default)]
    pub parameters: Vec<ActionArgument>,
}

/// Body of `POST /control/advance-property`. `code` is a hex property code
/// such as `0x500D`; the service parses it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdvancePropertyRequest {
    pub code: String,
}
