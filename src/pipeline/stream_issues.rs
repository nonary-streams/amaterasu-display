//! Display failures this provider reports first-hand to the launch owner's
//! lifecycle store.
//!
//! Stream callers name their display work with an operation ID bound to the
//! stream: the stream session's `ss-<hex>-display`, NVHTTP's stream key or an
//! ID derived from either. The pipeline remembers that ID for each request it
//! accepts, and every failed completion of such a request becomes one issue.
//! Completions are the funnel: each display path ends in one, so no failure
//! path can be missed. The tick publishes queued issues on the SDK topic.
use crate::model::Operation;
use amaterasu_module_sdk::stream_lifecycle::{LifecycleLayer, StreamIssue, StreamIssueOutbox};
use amaterasu_module_sdk::{Command, ModuleResult, Response};
use serde_json::Value;
use std::collections::BTreeMap;

/// Requests remembered at once. The pipeline queues at most 16 and serves one,
/// so this is only a bound against a lost completion.
const MAX_TRACKED_REQUESTS: usize = 64;
/// Code used when a failed reply carries no identifier-shaped code.
const UNNAMED_FAILURE: &str = "display_request_failed";

/// A stream-scoped request waiting for its completion.
struct StreamRequest {
    phase: &'static str,
    /// Unknown for a restore until its journal names the operation.
    operation: Option<String>,
}

/// One completion the pipeline sent, kept until the tick settles it.
pub(super) struct Completion {
    request: u64,
    failure: Option<String>,
}

impl Completion {
    /// The completion `command` sends, if it is one.
    pub(super) fn observe(command: &Command) -> Option<Self> {
        let Command::Complete { request, payload } = command else {
            return None;
        };
        Some(Self {
            request: *request,
            failure: failure_code(payload),
        })
    }
}

#[derive(Default)]
pub(super) struct StreamIssues {
    enabled: bool,
    requests: BTreeMap<u64, StreamRequest>,
    outbox: StreamIssueOutbox,
}

impl StreamIssues {
    pub(super) fn enable(&mut self) {
        self.enabled = true;
    }

    /// Remember the stream operation an accepted request serves.
    pub(super) fn track(&mut self, request: u64, operation: &Operation) {
        let Some((phase, stream_operation)) = stream_operation(operation) else {
            return;
        };
        if self.requests.len() >= MAX_TRACKED_REQUESTS {
            self.requests.pop_first();
        }
        self.requests.insert(
            request,
            StreamRequest {
                phase,
                operation: stream_operation,
            },
        );
    }

    /// A restore learns its stream operation from the journal it restores.
    pub(super) fn adopt(&mut self, request: u64, operation: Option<&str>) {
        if let (Some(tracked), Some(operation)) = (self.requests.get_mut(&request), operation) {
            tracked.operation = Some(operation.to_owned());
        }
    }

    /// Turn the tick's failed completions into issues.
    pub(super) fn settle(&mut self, completions: Vec<Completion>) {
        for completion in completions {
            let Some(tracked) = self.requests.remove(&completion.request) else {
                continue;
            };
            if let (Some(code), Some(operation)) = (completion.failure, tracked.operation) {
                let issue = display_issue(operation, tracked.phase, &code);
                self.outbox.push(issue);
            }
        }
    }

    /// Publish queued issues. Off by default, so a pipeline under test sends
    /// only the commands its test scripts; then issues are discarded.
    pub(super) fn publish(&mut self, send: &mut impl FnMut(Command) -> ModuleResult<Response>) {
        if !self.enabled {
            self.outbox.drain().for_each(drop);
            return;
        }
        if self.outbox.is_empty() {
            return;
        }
        if let Err(error) = self.outbox.flush_with(send) {
            eprintln!("display stream issue report failed error={error}");
        }
    }
}

/// The issue for a failed display request. Cleanup that cannot put the display
/// back is a failure; a configuration that could not be applied leaves the
/// display as it was and the stream may continue on it.
#[track_caller]
fn display_issue(operation: String, phase: &'static str, code: &str) -> StreamIssue {
    let issue = if let Some(fallback) = fallback(phase) {
        StreamIssue::demoted(operation, LifecycleLayer::Display, phase, code, fallback)
    } else {
        StreamIssue::critical(operation, LifecycleLayer::Display, phase, code)
    };
    match dependency(code) {
        Some(layer) => issue.caused_by(layer),
        None => issue,
    }
}

fn fallback(phase: &str) -> Option<&'static str> {
    match phase {
        "apply" | "scoped_apply" | "scoped_reconfigure" => Some("display_configuration_unchanged"),
        "prepare_hdr_profile" | "apply_hdr_profile" => Some("hdr_profile_unchanged"),
        _ => None,
    }
}

/// Codes that mean a provider this one consulted refused, not this provider.
fn dependency(code: &str) -> Option<LifecycleLayer> {
    match code {
        "target_ownership_unverified" | "retained_mode_unverified" => {
            Some(LifecycleLayer::VirtualDisplay)
        }
        _ => None,
    }
}

/// The phase and stream operation of a request that changes a display for a
/// caller. Reads and status checks are not stream failures.
fn stream_operation(operation: &Operation) -> Option<(&'static str, Option<String>)> {
    let named = |phase, id: &String| Some((phase, Some(id.clone())));
    match operation {
        Operation::Apply {
            operation_id: Some(id),
            ..
        } => named("apply", id),
        Operation::Restore { .. } => Some(("restore", None)),
        Operation::Recover { .. } => Some(("recover", None)),
        Operation::ScopedApply { operation_id, .. } => named("scoped_apply", operation_id),
        Operation::ScopedReconfigure { operation_id, .. } => {
            named("scoped_reconfigure", operation_id)
        }
        Operation::ScopedReconcile { operation_id, .. } => named("scoped_reconcile", operation_id),
        Operation::ScopedRelease { operation_id, .. } => named("scoped_release", operation_id),
        Operation::BorrowTarget { operation_id, .. } => named("borrow_target", operation_id),
        Operation::ReleaseTarget { operation_id, .. } => named("release_target", operation_id),
        Operation::PrepareHdrProfile { operation_id, .. } => {
            named("prepare_hdr_profile", operation_id)
        }
        Operation::ApplyHdrProfile { receipt } => named("apply_hdr_profile", &receipt.operation_id),
        Operation::RestoreHdrProfile { receipt } => {
            named("restore_hdr_profile", &receipt.operation_id)
        }
        _ => None,
    }
}

/// The reply's error code, or `None` when the request succeeded.
fn failure_code(payload: &Value) -> Option<String> {
    if payload["ok"] == true {
        return None;
    }
    let code = payload["error"]["code"]
        .as_str()
        .filter(|code| identifier(code))
        .unwrap_or(UNNAMED_FAILURE);
    Some(code.to_owned())
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
}
