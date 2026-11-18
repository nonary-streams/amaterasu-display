use super::*;
use amaterasu_module_sdk::stream_lifecycle::{
    IssueDisposition, LifecycleLayer, StreamIssue, StreamReport,
};

/// An apply that names both one policy and a list fails before any effect.
fn rejected_apply(operation: &str) -> Value {
    let policy = policy("monitor.one", "1920x1080", "60");
    json!({"action":"apply","configuration":configuration(),"operation_id":operation,
        "mode_policy":policy,"mode_policies":[policy]})
}

/// Serve one request and return every command the tick sent.
fn serve(pipeline: &mut Pipeline<FakePlatform>, payload: Value) -> Vec<Command> {
    let mut input = Some(payload);
    let mut commands = Vec::new();
    pipeline
        .tick(|command| {
            let response = match &command {
                Command::Poll {} => {
                    input
                        .take()
                        .map_or(Response::Pending, |payload| Response::Request {
                            id: 7,
                            caller: "stream-session".into(),
                            initiator: "nvhttp".into(),
                            service: SERVICE.into(),
                            payload,
                            remaining_ms: 30_000,
                        })
                }
                _ => Response::Ok,
            };
            commands.push(command);
            Ok(response)
        })
        .unwrap();
    commands
}

fn published(commands: &[Command]) -> Vec<StreamIssue> {
    commands
        .iter()
        .filter_map(|command| match command {
            Command::Publish { topic, payload } if topic == "amaterasu.stream.report/1" => {
                assert_eq!(payload["kind"], "issue");
                match serde_json::from_value(payload.clone()).unwrap() {
                    StreamReport::Issue(issue) => Some(issue),
                    StreamReport::Binding(_) => None,
                }
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_failed_stream_apply_is_published_under_the_callers_operation() {
    let mut pipeline = Pipeline::new(FakePlatform::default(), true);
    pipeline.enable_stream_issue_reports();
    let issues = published(&serve(&mut pipeline, rejected_apply("ss-2a-display")));
    assert_eq!(issues.len(), 1);
    let issue = &issues[0];
    assert_eq!(issue.operation, "ss-2a-display");
    assert_eq!(issue.layer, LifecycleLayer::Display);
    assert_eq!(issue.phase, "apply");
    assert_eq!(issue.code, "invalid_request");
    assert_eq!(issue.disposition, IssueDisposition::DemotedWarning);
    assert_eq!(
        issue.fallback.as_deref(),
        Some("display_configuration_unchanged")
    );
    assert!(issue.caused_by.is_none());
    assert!(
        issue
            .site
            .replace('\\', "/")
            .contains("pipeline/stream_issues.rs:"),
        "{}",
        issue.site
    );
}

#[test]
fn nothing_is_published_until_enabled_or_without_an_operation() {
    let mut pipeline = Pipeline::new(FakePlatform::default(), true);
    assert!(published(&serve(&mut pipeline, rejected_apply("ss-2b-display"))).is_empty());

    pipeline.enable_stream_issue_reports();
    let mut anonymous = rejected_apply("unused");
    anonymous.as_object_mut().unwrap().remove("operation_id");
    assert!(published(&serve(&mut pipeline, anonymous)).is_empty());
}
