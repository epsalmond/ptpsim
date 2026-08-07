//! Occurrence-scoped PTP/IP fault injection.
//!
//! Every rule whose operation and optional complete parameter list match a
//! command-channel transaction increments its own `seen` counter. A rule is
//! armed after `skip` matching occurrences and remains armed for `count`
//! occurrences, or forever when `count` is absent. At most one rule applies to
//! a transaction: the lowest-id armed rule wins. Counters survive PTP session
//! open/close operations and disappear only when their rule is deleted or the
//! registry is cleared.

use camera_config::WireFraming;
use serde::{Deserialize, Serialize};

pub const MAX_DELAY_MS: u64 = 60_000;
pub const MAX_REPLACEMENT_BYTES: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FaultSelector {
    #[serde(with = "hex_u16")]
    pub operation: u16,
    #[serde(default)]
    pub params: Option<Vec<u32>>,
    #[serde(default)]
    pub skip: u32,
    #[serde(default)]
    pub count: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FaultStage {
    Command,
    Data,
    Response,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DataOrResponse {
    Data,
    Response,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "type",
    deny_unknown_fields
)]
pub enum FaultMutation {
    FailResponse {
        #[serde(with = "hex_u16")]
        response: u16,
    },
    Close {
        stage: FaultStage,
    },
    Delay {
        stage: DataOrResponse,
        ms: u64,
    },
    Suppress {
        stage: DataOrResponse,
    },
    TruncateData {
        keep: u64,
    },
    ReplaceData {
        #[serde(rename = "bytesHex", with = "bytes_hex")]
        bytes: Vec<u8>,
    },
    ReplaceTransactionId {
        transaction_id: u32,
    },
    DataFraming {
        framing: WireFraming,
    },
    PropertyReadback {
        value: i64,
    },
}

impl FaultMutation {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::FailResponse { .. } => "failResponse",
            Self::Close { .. } => "close",
            Self::Delay { .. } => "delay",
            Self::Suppress { .. } => "suppress",
            Self::TruncateData { .. } => "truncateData",
            Self::ReplaceData { .. } => "replaceData",
            Self::ReplaceTransactionId { .. } => "replaceTransactionId",
            Self::DataFraming { .. } => "dataFraming",
            Self::PropertyReadback { .. } => "propertyReadback",
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Delay { ms, .. } if *ms > MAX_DELAY_MS => {
                Err(format!("delay {ms}ms exceeds the {MAX_DELAY_MS}ms maximum"))
            }
            Self::ReplaceData { bytes } if bytes.len() > MAX_REPLACEMENT_BYTES => Err(format!(
                "replacement payload is {} bytes; maximum is {MAX_REPLACEMENT_BYTES}",
                bytes.len()
            )),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FaultSpec {
    #[serde(flatten)]
    pub selector: FaultSelector,
    pub mutation: FaultMutation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultRule {
    pub id: u64,
    pub selector: FaultSelector,
    pub mutation: FaultMutation,
    seen: u32,
    applied: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FaultView {
    pub id: u64,
    #[serde(flatten)]
    pub selector: FaultSelector,
    pub mutation: FaultMutation,
    pub seen: u32,
    pub applied: u32,
    pub exhausted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FaultApplication {
    pub id: u64,
    #[serde(with = "hex_u16")]
    pub operation: u16,
    pub params: Vec<u32>,
    pub kind: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WirePlan {
    None,
    CloseBeforeData,
    CloseBeforeResponse,
    DelayData { ms: u64 },
    DelayResponse { ms: u64 },
    SuppressData,
    SuppressResponse,
    ReplaceTransactionId(u32),
    DataFraming(WireFraming),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedFault {
    pub id: u64,
    pub kind: String,
    pub wire: WirePlan,
}

#[derive(Debug, Clone)]
pub struct FaultSet {
    next_id: u64,
    rules: Vec<FaultRule>,
    last_applied: Option<FaultApplication>,
    applied_mutation: Option<FaultMutation>,
}

impl Default for FaultSet {
    fn default() -> Self {
        Self {
            next_id: 1,
            rules: Vec::new(),
            last_applied: None,
            applied_mutation: None,
        }
    }
}

impl FaultSet {
    /// Install a fault spec. Invalid specs are a caller-facing error, never a
    /// panic (#407): the former infallible `insert` wrapper is gone.
    pub fn try_insert(&mut self, spec: FaultSpec) -> Result<u64, String> {
        spec.mutation.validate()?;
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.rules.push(FaultRule {
            id,
            selector: spec.selector,
            mutation: spec.mutation,
            seen: 0,
            applied: 0,
        });
        Ok(id)
    }

    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.rules.len();
        self.rules.retain(|rule| rule.id != id);
        let removed = before != self.rules.len();
        if removed
            && self
                .last_applied
                .as_ref()
                .is_some_and(|fault| fault.id == id)
        {
            self.last_applied = None;
        }
        removed
    }

    pub fn clear(&mut self) {
        self.rules.clear();
        self.last_applied = None;
        self.applied_mutation = None;
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn list(&self) -> Vec<FaultView> {
        self.rules
            .iter()
            .map(|rule| FaultView {
                id: rule.id,
                selector: rule.selector.clone(),
                mutation: rule.mutation.clone(),
                seen: rule.seen,
                applied: rule.applied,
                exhausted: exhausted(&rule.selector, rule.seen),
            })
            .collect()
    }

    pub fn last_applied(&self) -> Option<FaultApplication> {
        self.last_applied.clone()
    }

    pub fn apply(&mut self, code: u16, params: &[u32]) -> Option<AppliedFault> {
        self.applied_mutation = None;
        let mut selected = None;
        for (index, rule) in self.rules.iter_mut().enumerate() {
            if rule.selector.operation != code
                || rule
                    .selector
                    .params
                    .as_deref()
                    .is_some_and(|expected| expected != params)
            {
                continue;
            }
            let seen_before = rule.seen;
            rule.seen = rule.seen.saturating_add(1);
            if selected.is_none() && armed(&rule.selector, seen_before) {
                selected = Some(index);
            }
        }
        let rule = self.rules.get_mut(selected?)?;
        rule.applied = rule.applied.saturating_add(1);
        let wire = wire_plan(&rule.mutation);
        let application = FaultApplication {
            id: rule.id,
            operation: code,
            params: params.to_vec(),
            kind: rule.mutation.kind().to_string(),
        };
        self.last_applied = Some(application);
        self.applied_mutation = Some(rule.mutation.clone());
        Some(AppliedFault {
            id: rule.id,
            kind: rule.mutation.kind().to_string(),
            wire,
        })
    }

    pub(crate) fn take_applied_mutation(&mut self) -> Option<FaultMutation> {
        self.applied_mutation.take()
    }
}

fn armed(selector: &FaultSelector, seen_before: u32) -> bool {
    if seen_before < selector.skip {
        return false;
    }
    selector
        .count
        .is_none_or(|count| seen_before - selector.skip < count)
}

fn exhausted(selector: &FaultSelector, seen: u32) -> bool {
    selector
        .count
        .is_some_and(|count| seen >= selector.skip.saturating_add(count))
}

fn wire_plan(mutation: &FaultMutation) -> WirePlan {
    match mutation {
        FaultMutation::Close {
            stage: FaultStage::Data,
        } => WirePlan::CloseBeforeData,
        FaultMutation::Close {
            stage: FaultStage::Response,
        } => WirePlan::CloseBeforeResponse,
        FaultMutation::Delay {
            stage: DataOrResponse::Data,
            ms,
        } => WirePlan::DelayData { ms: *ms },
        FaultMutation::Delay {
            stage: DataOrResponse::Response,
            ms,
        } => WirePlan::DelayResponse { ms: *ms },
        FaultMutation::Suppress {
            stage: DataOrResponse::Data,
        } => WirePlan::SuppressData,
        FaultMutation::Suppress {
            stage: DataOrResponse::Response,
        } => WirePlan::SuppressResponse,
        FaultMutation::ReplaceTransactionId { transaction_id } => {
            WirePlan::ReplaceTransactionId(*transaction_id)
        }
        FaultMutation::DataFraming { framing } => WirePlan::DataFraming(*framing),
        FaultMutation::FailResponse { .. }
        | FaultMutation::Close {
            stage: FaultStage::Command,
        }
        | FaultMutation::TruncateData { .. }
        | FaultMutation::ReplaceData { .. }
        | FaultMutation::PropertyReadback { .. } => WirePlan::None,
    }
}

mod hex_u16 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &u16, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("0x{value:04x}"))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u16, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let digits = value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
            .ok_or_else(|| serde::de::Error::custom("expected a 0x-prefixed 16-bit hex string"))?;
        if digits.is_empty() || digits.len() > 4 {
            return Err(serde::de::Error::custom(
                "expected a 0x-prefixed 16-bit hex string",
            ));
        }
        u16::from_str_radix(digits, 16).map_err(serde::de::Error::custom)
    }
}

mod bytes_hex {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut output = String::with_capacity(value.len() * 2);
        for byte in value {
            output.push_str(&format!("{byte:02x}"));
        }
        serializer.serialize_str(&output)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.len() % 2 != 0 {
            return Err(serde::de::Error::custom(
                "bytesHex must contain an even number of hex digits",
            ));
        }
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|digits| {
                let digits = std::str::from_utf8(digits).map_err(serde::de::Error::custom)?;
                u8::from_str_radix(digits, 16).map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(skip: u32, count: Option<u32>, mutation: FaultMutation) -> FaultSpec {
        FaultSpec {
            selector: FaultSelector {
                operation: 0x1015,
                params: Some(vec![53]),
                skip,
                count,
            },
            mutation,
        }
    }

    #[test]
    fn occurrence_window_uses_seen_before_and_exhausts() {
        let mut faults = FaultSet::default();
        faults
            .try_insert(spec(
                2,
                Some(1),
                FaultMutation::FailResponse { response: 0x2019 },
            ))
            .unwrap();
        assert!(faults.apply(0x1015, &[53]).is_none());
        assert!(faults.apply(0x1015, &[53]).is_none());
        assert!(faults.apply(0x1015, &[53]).is_some());
        assert!(faults.apply(0x1015, &[53]).is_none());
        let view = faults.list().pop().unwrap();
        assert_eq!((view.seen, view.applied), (4, 1));
        assert!(view.exhausted);
    }

    #[test]
    fn every_match_advances_but_lowest_armed_id_wins() {
        let mut faults = FaultSet::default();
        let first = faults
            .try_insert(spec(
                0,
                None,
                FaultMutation::Suppress {
                    stage: DataOrResponse::Data,
                },
            ))
            .unwrap();
        faults
            .try_insert(spec(
                0,
                Some(1),
                FaultMutation::FailResponse { response: 0x2005 },
            ))
            .unwrap();
        assert_eq!(faults.apply(0x1015, &[53]).unwrap().id, first);
        let views = faults.list();
        assert_eq!((views[0].seen, views[0].applied), (1, 1));
        assert_eq!((views[1].seen, views[1].applied), (1, 0));
        assert!(views[1].exhausted);
    }

    #[test]
    fn serde_round_trips_every_mutation() {
        let mutations = vec![
            FaultMutation::FailResponse { response: 0x2019 },
            FaultMutation::Close {
                stage: FaultStage::Command,
            },
            FaultMutation::Delay {
                stage: DataOrResponse::Data,
                ms: 25,
            },
            FaultMutation::Suppress {
                stage: DataOrResponse::Response,
            },
            FaultMutation::TruncateData { keep: 3 },
            FaultMutation::ReplaceData {
                bytes: vec![0xde, 0xad],
            },
            FaultMutation::ReplaceTransactionId { transaction_id: 42 },
            FaultMutation::DataFraming {
                framing: WireFraming::Compressed,
            },
            FaultMutation::PropertyReadback { value: -7 },
        ];
        for mutation in mutations {
            let value = serde_json::to_string(&mutation).unwrap();
            assert_eq!(
                serde_json::from_str::<FaultMutation>(&value).unwrap(),
                mutation
            );
        }
    }

    #[test]
    fn selector_and_hex_payload_use_the_public_json_shape() {
        let value = serde_json::to_value(spec(
            0,
            Some(1),
            FaultMutation::ReplaceData {
                bytes: vec![0xde, 0xad, 0xbe, 0xef],
            },
        ))
        .unwrap();
        assert_eq!(value["operation"], "0x1015");
        assert_eq!(value["mutation"]["bytesHex"], "deadbeef");
        assert_eq!(
            serde_json::from_value::<FaultSpec>(value)
                .unwrap()
                .selector
                .operation,
            0x1015
        );
        assert!(serde_json::from_str::<FaultMutation>(
            r#"{"type":"replaceData","bytesHex":"日日"}"#
        )
        .is_err());

        let transaction_id = FaultMutation::ReplaceTransactionId { transaction_id: 42 };
        let value = serde_json::to_value(&transaction_id).unwrap();
        assert_eq!(value["transactionId"], 42);
        assert_eq!(
            serde_json::from_value::<FaultMutation>(value).unwrap(),
            transaction_id
        );
        assert!(serde_json::from_str::<FaultMutation>(
            r#"{"type":"replaceTransactionId","transaction_id":42}"#
        )
        .is_err());
    }
}
