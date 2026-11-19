//! Lifetime totals over committed model-usage reports.
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ModelUsageSummaryScope {
    Task { task_id: String },
    Session { session_id: String },
    Workspace { workspace_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelUsageSummaryParams {
    pub scope: ModelUsageSummaryScope,
}

pub const MAX_MODEL_USAGE_SUMMARY_BATCH: usize = 64;
pub const MAX_MODEL_USAGE_OWNER_ID_BYTES: usize = 128;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelUsageSummaryBatchParams {
    /// Owner IDs must be 1 to 128 visible ASCII bytes.
    #[schemars(length(min = 1, max = MAX_MODEL_USAGE_SUMMARY_BATCH))]
    pub scopes: Vec<ModelUsageSummaryScope>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelUsageSummaryBatch {
    /// Global event watermark shared by all returned totals.
    pub through_sequence: u64,
    /// Entries align with the request. Missing owners are null; existing
    /// owners with no reports get zero totals. Duplicate scopes are retained.
    #[schemars(length(min = 1, max = MAX_MODEL_USAGE_SUMMARY_BATCH))]
    pub items: Vec<Option<ModelUsageTotals>>,
}

/// An exact integer exposed as a decimal string, including on JavaScript clients.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct UsageCount(
    #[serde(serialize_with = "encode", deserialize_with = "decode")]
    #[schemars(
        with = "String",
        length(min = 1, max = 39),
        regex(pattern = "^(0|[1-9][0-9]*)$")
    )]
    pub u128,
);

fn encode<S: Serializer>(count: &u128, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(count)
}

fn decode<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u128, D::Error> {
    let count = String::deserialize(deserializer)?;
    if count.is_empty()
        || count.len() > 39
        || (count.starts_with('0') && count.len() > 1)
        || !count.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(D::Error::custom("expected an unsigned decimal usage count"));
    }
    count.parse().map_err(D::Error::custom)
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelUsageTotals {
    pub report_count: UsageCount,
    pub input_tokens: UsageCount,
    pub output_tokens: UsageCount,
    pub cached_input_tokens: UsageCount,
    pub cached_input_report_count: UsageCount,
    pub reasoning_output_tokens: UsageCount,
    pub reasoning_output_report_count: UsageCount,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelUsageSummary {
    /// Global event watermark in the same read view as the totals.
    pub through_sequence: u64,
    pub totals: ModelUsageTotals,
}
