//! Immutable mock controls. Decoding supplied values never reads process state.
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use carina_core::provider::Sent;
use carina_core::resource::ResourceId;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum CreateMode {
    #[default]
    Success,
    Untracked,
    Created,
    Pending,
    MalformedPending,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ResumeMode {
    #[default]
    Resumed,
    Resend,
    CreatedReadFailed,
    Discarded,
    Untracked,
    Undetermined,
    MalformedPending,
    Pending,
    Unsupported(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum SentAttributes {
    #[default]
    All,
    Only(BTreeSet<String>),
}

impl SentAttributes {
    pub(crate) fn sent(&self) -> Sent {
        match self {
            Self::All => Sent::AllRequestAttributes,
            Self::Only(keys) => Sent::Only(keys.clone()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResourceSelector {
    Any,
    Address(String),
}

impl ResourceSelector {
    pub fn parse(value: impl Into<String>) -> Self {
        let value = value.into();
        if value == "*" {
            Self::Any
        } else {
            Self::Address(value)
        }
    }

    pub(crate) fn matches(&self, id: &ResourceId) -> bool {
        let Some(identity) = id.identity_str() else {
            return false;
        };
        match self {
            Self::Any => true,
            Self::Address(address) => {
                *address == format!("{}.{}", id.resource_type, identity)
                    || *address == format!("{}.{}.{}", id.provider, id.resource_type, identity)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedDelay {
    // Delays historically match only the short, exact resource address.
    pub address: String,
    pub duration: Duration,
}

impl SelectedDelay {
    pub(crate) fn for_resource(&self, id: &ResourceId) -> Option<Duration> {
        id.identity_str()
            .filter(|identity| self.address == format!("{}.{}", id.resource_type, identity))
            .map(|_| self.duration)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartialResult {
    pub selector: ResourceSelector,
    pub missing_attributes: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MockObservations {
    pub operation_log: Option<PathBuf>,
    pub delete_log: Option<PathBuf>,
    pub update_patch_log: Option<PathBuf>,
    pub update_request_log: Option<PathBuf>,
    pub create_ready: Option<PathBuf>,
    pub read_ready: Option<PathBuf>,
    pub delete_ready: Option<PathBuf>,
    pub read_outcome: Option<PathBuf>,
    pub max_active: Option<PathBuf>,
    pub update_trace: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerListMutation {
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MockBehavior {
    pub create_mode: CreateMode,
    pub resume_mode: ResumeMode,
    pub create_sent: SentAttributes,
    pub create_fail_for: Option<String>,
    pub update_fail_for: Option<ResourceSelector>,
    pub delete_fail_for: Option<ResourceSelector>,
    pub delete_fail_identifier: Option<String>,
    pub delete_timeout_for: Option<ResourceSelector>,
    pub create_delay: Option<SelectedDelay>,
    pub read_delay: Option<SelectedDelay>,
    pub delete_delay: Option<SelectedDelay>,
    pub partial_create: Option<PartialResult>,
    pub partial_update: Option<PartialResult>,
    pub update_delay: Option<Duration>,
    pub update_result_identifier: Option<String>,
    pub server_list_mutation: Option<ServerListMutation>,
    pub observations: MockObservations,
}

pub struct DecodedMockControls {
    pub state_file: Option<PathBuf>,
    pub behavior: MockBehavior,
}

/// Decode the historical controls from a supplied map (full CARINA_MOCK_ keys).
/// Missing/non-Unicode string controls retain the historical defaults. Paths
/// preserve native OS strings. Environment capture belongs to the executable.
pub fn decode_controls(values: &BTreeMap<String, OsString>) -> DecodedMockControls {
    let raw = |key: &str| values.get(&format!("CARINA_MOCK_{key}"));
    let text = |key: &str| raw(key).and_then(|value| value.to_str()).map(str::to_owned);
    let path = |key: &str| raw(key).map(PathBuf::from);
    let millis = |key: &str| {
        text(key)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .map(Duration::from_millis)
    };
    let delay = |selector, duration| {
        Some(SelectedDelay {
            address: text(selector)?,
            duration: millis(duration)?,
        })
    };
    let partial = |selector, missing| {
        Some(PartialResult {
            selector: ResourceSelector::parse(text(selector)?),
            missing_attributes: text(missing)
                .map(|value| split_list(&value))
                .unwrap_or_else(|| vec!["computed".to_owned()]),
        })
    };
    DecodedMockControls {
        state_file: path("STATE_FILE"),
        behavior: MockBehavior {
            create_mode: match text("CREATE_MODE").as_deref() {
                Some("untracked") => CreateMode::Untracked,
                Some("created") => CreateMode::Created,
                Some("pending") => CreateMode::Pending,
                Some("malformed_pending") => CreateMode::MalformedPending,
                Some(_) | None => CreateMode::Success,
            },
            resume_mode: match text("RESUME_MODE").as_deref() {
                Some("resumed") | None => ResumeMode::Resumed,
                Some("resend") => ResumeMode::Resend,
                Some("created_read_failed") => ResumeMode::CreatedReadFailed,
                Some("discarded") => ResumeMode::Discarded,
                Some("untracked") => ResumeMode::Untracked,
                Some("undetermined") => ResumeMode::Undetermined,
                Some("malformed_pending") => ResumeMode::MalformedPending,
                Some("pending") => ResumeMode::Pending,
                Some(unsupported) => ResumeMode::Unsupported(unsupported.to_owned()),
            },
            create_sent: match text("CREATE_SENT") {
                Some(value) if value != "all" => {
                    SentAttributes::Only(split_list(&value).into_iter().collect())
                }
                Some(_) | None => SentAttributes::All,
            },
            create_fail_for: text("CREATE_FAIL_FOR"),
            update_fail_for: text("UPDATE_FAIL_FOR").map(ResourceSelector::parse),
            delete_fail_for: text("DELETE_FAIL_FOR").map(ResourceSelector::parse),
            delete_fail_identifier: text("DELETE_FAIL_IDENTIFIER"),
            delete_timeout_for: text("DELETE_TIMEOUT_FOR").map(ResourceSelector::parse),
            create_delay: delay("CREATE_DELAY_MS_FOR", "CREATE_DELAY_MS"),
            read_delay: delay("READ_DELAY_MS_FOR", "READ_DELAY_MS"),
            delete_delay: delay("DELETE_DELAY_MS_FOR", "DELETE_DELAY_MS"),
            partial_create: partial("PARTIAL_CREATE_FOR", "PARTIAL_CREATE_MISSING"),
            partial_update: partial("PARTIAL_UPDATE_FOR", "PARTIAL_UPDATE_MISSING"),
            update_delay: millis("UPDATE_DELAY_MS"),
            update_result_identifier: text("UPDATE_RESULT_IDENTIFIER"),
            server_list_mutation: text("UPDATE_SERVER_LIST_OUT_OF_BAND_KEY").map(|key| {
                ServerListMutation {
                    key,
                    value: text("UPDATE_SERVER_LIST_OUT_OF_BAND_VALUE")
                        .unwrap_or_else(|| "enabled".to_owned()),
                }
            }),
            observations: MockObservations {
                operation_log: path("OP_LOG"),
                delete_log: path("DELETE_LOG"),
                update_patch_log: path("UPDATE_PATCH_LOG"),
                update_request_log: path("UPDATE_REQUEST_LOG"),
                create_ready: path("CREATE_READY_PATH"),
                read_ready: path("READ_READY_PATH"),
                delete_ready: path("DELETE_READY_PATH"),
                read_outcome: path("READ_OUTCOME_PATH"),
                max_active: path("MAX_ACTIVE_PATH"),
                update_trace: path("UPDATE_TRACE_PATH"),
            },
        },
    }
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_all_mock_controls_from_supplied_values() {
        let raw = [
            ("STATE_FILE", "state.json"),
            ("CREATE_MODE", "created"),
            ("RESUME_MODE", "resend"),
            ("CREATE_SENT", "name,password"),
            ("CREATE_FAIL_FOR", "test.resource.a"),
            ("UPDATE_FAIL_FOR", "*"),
            ("DELETE_FAIL_FOR", "mock.test.resource.a"),
            ("DELETE_FAIL_IDENTIFIER", "old-id"),
            ("DELETE_TIMEOUT_FOR", "test.resource.timeout"),
            ("CREATE_DELAY_MS_FOR", "test.resource.create"),
            ("CREATE_DELAY_MS", "11"),
            ("READ_DELAY_MS_FOR", "test.resource.read"),
            ("READ_DELAY_MS", "12"),
            ("DELETE_DELAY_MS_FOR", "test.resource.delete"),
            ("DELETE_DELAY_MS", "13"),
            ("CREATE_READY_PATH", "create.ready"),
            ("READ_READY_PATH", "read.ready"),
            ("DELETE_READY_PATH", "delete.ready"),
            ("READ_OUTCOME_PATH", "read.outcome"),
            ("PARTIAL_CREATE_FOR", "*"),
            ("PARTIAL_CREATE_MISSING", "computed,name"),
            ("PARTIAL_UPDATE_FOR", "mock.test.resource.a"),
            ("PARTIAL_UPDATE_MISSING", "computed"),
            ("OP_LOG", "operations.log"),
            ("DELETE_LOG", "deletes.log"),
            ("UPDATE_PATCH_LOG", "patch.log"),
            ("UPDATE_REQUEST_LOG", "request.log"),
            ("UPDATE_DELAY_MS", "14"),
            ("MAX_ACTIVE_PATH", "max"),
            ("UPDATE_TRACE_PATH", "trace"),
            ("UPDATE_RESULT_IDENTIFIER", "new-id"),
            ("UPDATE_SERVER_LIST_OUT_OF_BAND_KEY", "extra"),
            ("UPDATE_SERVER_LIST_OUT_OF_BAND_VALUE", "yes"),
        ];
        let values = raw
            .into_iter()
            .map(|(key, value)| (format!("CARINA_MOCK_{key}"), value.into()))
            .collect();
        let DecodedMockControls {
            state_file,
            behavior,
        } = decode_controls(&values);
        assert_eq!(state_file, Some(PathBuf::from("state.json")));
        assert_eq!(
            behavior,
            MockBehavior {
                create_mode: CreateMode::Created,
                resume_mode: ResumeMode::Resend,
                create_sent: SentAttributes::Only(
                    ["name".to_owned(), "password".to_owned()].into()
                ),
                create_fail_for: Some("test.resource.a".to_owned()),
                update_fail_for: Some(ResourceSelector::Any),
                delete_fail_for: Some(ResourceSelector::Address("mock.test.resource.a".to_owned())),
                delete_fail_identifier: Some("old-id".to_owned()),
                delete_timeout_for: Some(ResourceSelector::Address(
                    "test.resource.timeout".to_owned()
                )),
                create_delay: Some(SelectedDelay {
                    address: "test.resource.create".to_owned(),
                    duration: Duration::from_millis(11)
                }),
                read_delay: Some(SelectedDelay {
                    address: "test.resource.read".to_owned(),
                    duration: Duration::from_millis(12)
                }),
                delete_delay: Some(SelectedDelay {
                    address: "test.resource.delete".to_owned(),
                    duration: Duration::from_millis(13)
                }),
                partial_create: Some(PartialResult {
                    selector: ResourceSelector::Any,
                    missing_attributes: vec!["computed".to_owned(), "name".to_owned()]
                }),
                partial_update: Some(PartialResult {
                    selector: ResourceSelector::Address("mock.test.resource.a".to_owned()),
                    missing_attributes: vec!["computed".to_owned()]
                }),
                update_delay: Some(Duration::from_millis(14)),
                update_result_identifier: Some("new-id".to_owned()),
                server_list_mutation: Some(ServerListMutation {
                    key: "extra".to_owned(),
                    value: "yes".to_owned()
                }),
                observations: MockObservations {
                    operation_log: Some("operations.log".into()),
                    delete_log: Some("deletes.log".into()),
                    update_patch_log: Some("patch.log".into()),
                    update_request_log: Some("request.log".into()),
                    create_ready: Some("create.ready".into()),
                    read_ready: Some("read.ready".into()),
                    delete_ready: Some("delete.ready".into()),
                    read_outcome: Some("read.outcome".into()),
                    max_active: Some("max".into()),
                    update_trace: Some("trace".into()),
                },
            }
        );
    }
}
