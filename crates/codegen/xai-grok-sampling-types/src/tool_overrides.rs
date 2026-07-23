//! The `toolOverrides` wire contract for backend-hosted `x_search` / `web_search`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A content-date window for `x_search`: `fromDate` inclusive, `toDate` exclusive at 00:00 UTC of
/// the named day. Both are canonical `YYYY-MM-DD` (camelCase on the wire), validated in
/// [`SearchDateBound::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", try_from = "SearchDateBoundWire")]
#[schemars(deny_unknown_fields)]
pub struct SearchDateBound {
    #[serde(skip_serializing_if = "Option::is_none")]
    from_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to_date: Option<String>,
}

impl SearchDateBound {
    pub fn new(
        from_date: Option<String>,
        to_date: Option<String>,
    ) -> Result<Self, SearchDateBoundError> {
        validate_bound_pair(from_date.as_deref(), to_date.as_deref())?;
        Ok(Self { from_date, to_date })
    }

    pub fn from_date(&self) -> Option<&str> {
        self.from_date.as_deref()
    }

    pub fn to_date(&self) -> Option<&str> {
        self.to_date.as_deref()
    }

    pub fn is_empty(&self) -> bool {
        let SearchDateBound { from_date, to_date } = self;
        from_date.is_none() && to_date.is_none()
    }

    /// Deserialize and validate (via `try_from`), returning the structured `serde_json::Error`.
    pub fn parse(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        Self::deserialize(value)
    }
}

// Deserialize routes through `SearchDateBound::new` via `try_from`, so every ingress is validated.
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SearchDateBoundWire {
    #[serde(default)]
    from_date: Option<String>,
    #[serde(default)]
    to_date: Option<String>,
}

impl TryFrom<SearchDateBoundWire> for SearchDateBound {
    type Error = SearchDateBoundError;

    fn try_from(wire: SearchDateBoundWire) -> Result<Self, Self::Error> {
        Self::new(wire.from_date, wire.to_date)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SearchDateBoundError {
    #[error("{field} {value:?} is not a valid YYYY-MM-DD date")]
    InvalidDate { field: &'static str, value: String },
    #[error("{field} {value:?} is not zero-padded YYYY-MM-DD")]
    NotZeroPadded { field: &'static str, value: String },
    #[error("fromDate must be on or before toDate (got {from} > {to})")]
    InvertedWindow { from: String, to: String },
}

fn validate_bound_date(
    field: &'static str,
    s: &str,
) -> Result<chrono::NaiveDate, SearchDateBoundError> {
    let parsed = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| {
        SearchDateBoundError::InvalidDate {
            field,
            value: s.to_owned(),
        }
    })?;
    // chrono's proleptic calendar admits year 0; reject it so the minimum year is 1.
    if chrono::Datelike::year(&parsed) < 1 {
        return Err(SearchDateBoundError::InvalidDate {
            field,
            value: s.to_owned(),
        });
    }
    if s.len() != 10 || parsed.format("%Y-%m-%d").to_string() != s {
        return Err(SearchDateBoundError::NotZeroPadded {
            field,
            value: s.to_owned(),
        });
    }
    Ok(parsed)
}

fn validate_bound_pair(from: Option<&str>, to: Option<&str>) -> Result<(), SearchDateBoundError> {
    let from_date = from
        .map(|s| validate_bound_date("fromDate", s))
        .transpose()?;
    let to_date = to.map(|s| validate_bound_date("toDate", s)).transpose()?;
    if let (Some(from), Some(to), Some(from_date), Some(to_date)) = (from, to, from_date, to_date)
        && from_date > to_date
    {
        return Err(SearchDateBoundError::InvertedWindow {
            from: from.to_owned(),
            to: to.to_owned(),
        });
    }
    Ok(())
}

/// `x_search` override: the content-date [`SearchDateBound`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct XSearchOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_bound: Option<SearchDateBound>,
}

impl XSearchOptions {
    pub fn is_empty(&self) -> bool {
        let XSearchOptions { date_bound } = self;
        date_bound.as_ref().is_none_or(SearchDateBound::is_empty)
    }

    pub fn to_tool_entry(&self) -> serde_json::Value {
        #[derive(Serialize)]
        #[serde(rename_all = "snake_case")]
        struct XSearchToolEntry<'a> {
            r#type: &'static str,
            #[serde(skip_serializing_if = "Option::is_none")]
            from_date: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            to_date: Option<&'a str>,
        }
        // Destructure so a new field forces a compile error rather than a dropped wire field.
        let XSearchOptions { date_bound } = self;
        let bound = date_bound.as_ref();
        serde_json::to_value(XSearchToolEntry {
            r#type: "x_search",
            from_date: bound.and_then(SearchDateBound::from_date),
            to_date: bound.and_then(SearchDateBound::to_date),
        })
        .expect("XSearchToolEntry is always serializable")
    }
}

/// `web_search` override: a domain allowlist (empty or absent is unbounded).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct WebSearchOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
}

impl WebSearchOptions {
    pub fn is_empty(&self) -> bool {
        let WebSearchOptions { allowed_domains } = self;
        allowed_domains
            .as_ref()
            .is_none_or(|domains| domains.is_empty())
    }
}

/// The resolved per-tool overrides, and the shape echoed back for attestation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct ToolOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_search: Option<XSearchOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<WebSearchOptions>,
}

impl ToolOverrides {
    pub fn parse(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        Self::deserialize(value)
    }

    pub fn is_empty(&self) -> bool {
        let ToolOverrides {
            x_search,
            web_search,
        } = self;
        x_search.as_ref().is_none_or(XSearchOptions::is_empty)
            && web_search.as_ref().is_none_or(WebSearchOptions::is_empty)
    }
}

/// A tri-state per-turn patch field: absent leaves, `null` clears, a value sets. Pair with
/// [`crate::serde_helpers::double_option`].
pub type ClearableField<T> = Option<Option<T>>;

/// The ingress-only per-turn patch: each tool is a tri-state [`ClearableField`] applied by
/// [`Self::apply`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct ToolOverridesUpdate {
    #[serde(default, deserialize_with = "crate::serde_helpers::double_option")]
    pub x_search: ClearableField<XSearchOptions>,
    #[serde(default, deserialize_with = "crate::serde_helpers::double_option")]
    pub web_search: ClearableField<WebSearchOptions>,
}

impl ToolOverridesUpdate {
    pub fn parse(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        Self::deserialize(value)
    }

    /// Fold a per-turn update onto `base`: an object sets, `null` clears, absent or empty leaves.
    pub fn apply(self, base: Option<ToolOverrides>) -> Option<ToolOverrides> {
        let ToolOverridesUpdate {
            x_search,
            web_search,
        } = self;
        let base = base.unwrap_or_default();
        let next = ToolOverrides {
            x_search: merge_field(x_search, base.x_search, XSearchOptions::is_empty),
            web_search: merge_field(web_search, base.web_search, WebSearchOptions::is_empty),
        };
        (!next.is_empty()).then_some(next)
    }
}

/// Normalize an override option: empty carries no constraint, so it reads as absent. The one home
/// for this rule, shared by `merge_field` and `apply_tool_overrides`.
pub(crate) fn drop_empty<T>(opt: Option<T>, is_empty: impl Fn(&T) -> bool) -> Option<T> {
    opt.filter(|value| !is_empty(value))
}

/// Fold one tri-state field onto its base: an object sets, `null` clears, absent or empty leaves.
fn merge_field<T>(
    update: ClearableField<T>,
    base: Option<T>,
    is_empty: impl Fn(&T) -> bool,
) -> Option<T> {
    match update {
        // An object sets, but an empty one carries no instruction, so it falls back to the base.
        Some(Some(value)) => drop_empty(Some(value), is_empty).or(base),
        Some(None) => None,
        None => base,
    }
}
