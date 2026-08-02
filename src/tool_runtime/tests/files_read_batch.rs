//! Bounded batch `read_file` tests.
//!
//! These exercise the batch input/output protocol end to end: input Schema
//! mutual exclusion, item-count bounds, ordered output regardless of completion
//! order, per-item partial failure, the shared overall deadline, the
//! serialized-output budget, and the single-session-event invariant. The
//! single-file form's behavior is covered by `files.rs`; this module only adds
//! the batch form on top of the same single-range helper.

use super::super::file_read_batch::ReadFileItem;
use super::super::*;
use super::support::*;
use crate::shell_protocol::ShellAgentPollRequest;
use serde_json::json;
use std::time::Duration;

#[test]
fn batch_input_schema_mutual_exclusion_and_bounds() {
    use crate::tool_runtime::ToolCall;
    // Only `path`: parses as the single-file form.
    let call =
        ToolCall::from_tool_name("read_file", json!({"project": "p", "path": "a.rs"})).unwrap();
    assert!(matches!(
        call,
        ToolCall::ReadFile {
            path: Some(_),
            items: None,
            ..
        }
    ));

    // Only `items`: parses as the batch form.
    let call = ToolCall::from_tool_name(
        "read_file",
        json!({"project": "p", "items": [{"path": "a.rs"}, {"path": "b.rs"}]}),
    )
    .unwrap();
    assert!(matches!(
        call,
        ToolCall::ReadFile {
            path: None,
            items: Some(_),
            ..
        }
    ));

    // Both `path` and `items`: rejected.
    let err = ToolCall::from_tool_name(
        "read_file",
        json!({"project": "p", "path": "a.rs", "items": [{"path": "b.rs"}]}),
    )
    .unwrap_err();
    assert!(err.contains("mutually exclusive"), "{err}");

    // Neither: rejected.
    let err = ToolCall::from_tool_name("read_file", json!({"project": "p"})).unwrap_err();
    assert!(err.contains("exactly one of path or items"), "{err}");

    // Empty items: rejected.
    let err =
        ToolCall::from_tool_name("read_file", json!({"project": "p", "items": []})).unwrap_err();
    assert!(err.contains("at least one"), "{err}");

    // 17 items: rejected (max 16).
    let mut many = Vec::new();
    for i in 0..17 {
        many.push(json!({"path": format!("f{i}.rs")}));
    }
    let err =
        ToolCall::from_tool_name("read_file", json!({"project": "p", "items": many})).unwrap_err();
    assert!(err.contains("at most 16"), "{err}");

    // Batch with top-level start_line/limit: rejected.
    let err = ToolCall::from_tool_name(
        "read_file",
        json!({"project": "p", "items": [{"path": "a.rs"}], "start_line": 1}),
    )
    .unwrap_err();
    assert!(err.contains("start_line/limit"), "{err}");

    // Item without path: rejected.
    let err = ToolCall::from_tool_name(
        "read_file",
        json!({"project": "p", "items": [{"limit": 5}]}),
    )
    .unwrap_err();
    assert!(err.contains("path"), "{err}");
}

#[test]
fn batch_input_schema_declares_mutual_exclusion() {
    let specs = registered_tool_specs();
    let read_file = specs.iter().find(|s| s.name == "read_file").unwrap();
    let props = read_file.input_schema["properties"].as_object().unwrap();
    assert!(props.contains_key("path"));
    assert!(props.contains_key("items"));
    let one_of = read_file.input_schema["oneOf"].as_array().unwrap();
    assert_eq!(one_of.len(), 2);
    let items_schema = &props["items"];
    assert_eq!(items_schema["maxItems"], 16);
    assert_eq!(items_schema["minItems"], 1);
    // The batch item sub-schema requires a path.
    assert_eq!(items_schema["items"]["required"][0], "path");
}

/// Read a batch via the runtime's internal executor (bypassing HTTP). The
/// executor enqueues every `file_read` request up front; tests complete each
/// request by running it locally, then await the batch result.
async fn run_batch_and_complete_all(
    runtime: &ToolRuntime,
    client_id: &str,
    project: &str,
    items: Vec<ReadFileItem>,
    with_line_numbers: bool,
) -> ToolResult {
    let runtime_for_task = runtime.clone();
    let project_for_task = project.to_string();
    let task = tokio::spawn(async move {
        runtime_for_task
            .read_file_batch(project_for_task.to_string(), items, Some(with_line_numbers))
            .await
    });
    // Poll and complete every queued file_read request until the batch task
    // finishes. The executor enqueues all requests in phase 1 before awaiting,
    // so polling must keep going until the task resolves (a transient None poll
    // between enqueues must not stop the loop).
    loop {
        let req = runtime
            .shell_clients
            .poll(ShellAgentPollRequest {
                client_id: client_id.to_string(),
                agent_instance_id: "inst".to_string(),
                projects: None,
            })
            .await
            .unwrap();
        match req {
            Some(req) => {
                complete_agent_request_by_running_locally(runtime, client_id, req).await;
            }
            None => {
                if task.is_finished() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }
    }
    task.await.unwrap()
}

fn test_read_item(path: &str, start: Option<usize>, limit: Option<usize>) -> ReadFileItem {
    ReadFileItem {
        path: path.to_string(),
        start_line: start,
        limit,
    }
}

#[tokio::test]
async fn batch_reads_two_ranges_in_request_order() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("a.rs"),
        (1..=10)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("b.rs"),
        (1..=5)
            .map(|i| format!("b-{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let runtime = runtime_with_agent_project("batch-ok");
    let project = register_agent_project_at_path(&runtime, "batch-ok", "proj", tmp.path()).await;

    let result = run_batch_and_complete_all(
        &runtime,
        "batch-ok",
        &project,
        vec![
            test_read_item("a.rs", Some(1), Some(2)),
            test_read_item("b.rs", Some(3), Some(2)),
        ],
        false,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["mode"], "batch");
    assert_eq!(result.output["requested_count"], 2);
    assert_eq!(result.output["returned_count"], 2);
    assert_eq!(result.output["succeeded_count"], 2);
    assert_eq!(result.output["failed_count"], 0);
    let items = result.output["items"].as_array().unwrap();
    assert_eq!(items[0]["index"], 0);
    assert_eq!(items[0]["success"], true);
    assert_eq!(items[0]["output"]["text"], "line-1\nline-2");
    assert_eq!(items[1]["index"], 1);
    assert_eq!(items[1]["success"], true);
    assert_eq!(items[1]["output"]["text"], "b-3\nb-4");
    assert!(result.output["output_truncated"].as_bool().unwrap() == false);
}

#[tokio::test]
async fn batch_partial_failure_returns_successful_batch() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("exists.rs"), "hello\n").unwrap();
    let runtime = runtime_with_agent_project("batch-partial");
    let project =
        register_agent_project_at_path(&runtime, "batch-partial", "proj", tmp.path()).await;

    let result = run_batch_and_complete_all(
        &runtime,
        "batch-partial",
        &project,
        vec![
            test_read_item("exists.rs", None, None),
            test_read_item("missing.rs", None, None),
        ],
        false,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    let items = result.output["items"].as_array().unwrap();
    assert_eq!(items[0]["success"], true);
    assert_eq!(items[1]["success"], false);
    assert_eq!(items[1]["output"]["reason_code"], "not_found");
    assert_eq!(result.output["succeeded_count"], 1);
    assert_eq!(result.output["failed_count"], 1);
}

#[tokio::test]
async fn batch_all_items_fail_still_returns_valid_batch() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = runtime_with_agent_project("batch-allfail");
    let project =
        register_agent_project_at_path(&runtime, "batch-allfail", "proj", tmp.path()).await;

    let result = run_batch_and_complete_all(
        &runtime,
        "batch-allfail",
        &project,
        vec![
            test_read_item("missing1.rs", None, None),
            test_read_item("missing2.rs", None, None),
        ],
        false,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["failed_count"], 2);
    assert_eq!(result.output["succeeded_count"], 0);
    assert_eq!(result.output["returned_count"], 2);
}

#[tokio::test]
async fn batch_same_file_two_ranges_both_return() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("multi.rs"),
        (1..=10)
            .map(|i| format!("m-{i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let runtime = runtime_with_agent_project("batch-same");
    let project = register_agent_project_at_path(&runtime, "batch-same", "proj", tmp.path()).await;

    let result = run_batch_and_complete_all(
        &runtime,
        "batch-same",
        &project,
        vec![
            test_read_item("multi.rs", Some(1), Some(2)),
            test_read_item("multi.rs", Some(5), Some(2)),
        ],
        false,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    let items = result.output["items"].as_array().unwrap();
    assert_eq!(items[0]["output"]["text"], "m-1\nm-2");
    assert_eq!(items[1]["output"]["text"], "m-5\nm-6");
    assert_eq!(items[0]["output"]["sha256"], items[1]["output"]["sha256"]);
}

#[tokio::test]
async fn batch_order_preserved_when_completion_order_differs() {
    // Complete requests in reverse order (last enqueued first) to prove the
    // output is reassembled by request index, not completion order.
    let tmp = tempfile::tempdir().unwrap();
    for i in 0..6 {
        std::fs::write(
            tmp.path().join(format!("f{i}.rs")),
            format!("file-{i} content\n"),
        )
        .unwrap();
    }
    let runtime = runtime_with_agent_project("batch-order");
    let project = register_agent_project_at_path(&runtime, "batch-order", "proj", tmp.path()).await;

    let runtime_for_task = runtime.clone();
    let project_for_task = project.clone();
    let task = tokio::spawn(async move {
        let items = (0..6)
            .map(|i| test_read_item(&format!("f{i}.rs"), None, None))
            .collect::<Vec<_>>();
        runtime_for_task
            .read_file_batch(project_for_task.to_string(), items, Some(false))
            .await
    });
    // Collect all 6 requests (ignoring transient None polls until the executor
    // has enqueued every request in phase 1), then complete them in reverse
    // order to prove output order is by request index, not completion order.
    let mut requests = Vec::new();
    for _ in 0..2000 {
        let req = runtime
            .shell_clients
            .poll(ShellAgentPollRequest {
                client_id: "batch-order".to_string(),
                agent_instance_id: "inst".to_string(),
                projects: None,
            })
            .await
            .unwrap();
        if let Some(req) = req {
            requests.push(req);
            if requests.len() == 6 {
                break;
            }
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(requests.len(), 6, "expected 6 enqueued reads");
    for req in requests.into_iter().rev() {
        complete_agent_request_by_running_locally(&runtime, "batch-order", req).await;
    }
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    let items = result.output["items"].as_array().unwrap();
    for (i, item) in items.iter().enumerate() {
        assert_eq!(item["index"], i);
        assert_eq!(item["output"]["text"], format!("file-{i} content"));
    }
}

#[tokio::test]
async fn batch_single_session_event_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "aaa\n").unwrap();
    std::fs::write(tmp.path().join("b.rs"), "bbb\n").unwrap();
    let runtime = runtime_with_agent_project("batch-sess");
    let bootstrap = bootstrap_auth_context();
    let project = register_agent_project_at_path_with_auth(
        &runtime,
        "batch-sess",
        "proj",
        tmp.path(),
        &bootstrap,
    )
    .await;
    let session = runtime.sessions.start_session(None, None);
    let session_id = session.session_id.clone();

    let runtime_for_task = runtime.clone();
    let project_for_task = project.clone();
    let session_for_task = session_id.clone();
    let auth_for_task = bootstrap.clone();
    let task = tokio::spawn(async move {
        runtime_for_task
            .dispatch_with_auth(
                ToolCall::ReadFile {
                    project: project_for_task,
                    path: None,
                    items: Some(vec![
                        test_read_item("a.rs", None, None),
                        test_read_item("b.rs", None, None),
                    ]),
                    session_id: Some(session_for_task),
                    start_line: None,
                    limit: None,
                    with_line_numbers: None,
                },
                Some(&auth_for_task),
            )
            .await
    });
    for _ in 0..200 {
        let req = runtime
            .shell_clients
            .poll(ShellAgentPollRequest {
                client_id: "batch-sess".to_string(),
                agent_instance_id: "inst".to_string(),
                projects: None,
            })
            .await
            .unwrap();
        let Some(req) = req else {
            break;
        };
        complete_agent_request_by_running_locally(&runtime, "batch-sess", req).await;
    }
    let result = task.await.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["session_recorded"], true);
    let summary = runtime.sessions.summary(&session_id, Some(10)).unwrap();
    assert_eq!(
        summary.counts.tool_calls, 1,
        "batch must record exactly one read_file tool call"
    );
}

#[tokio::test]
async fn batch_deadline_cancels_and_times_out_pending_items() {
    // Enqueue a batch but never complete the reads; the shared overall deadline
    // must fire, cancel the requests, and yield per-item `timeout` failures
    // without leaving anything parked.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "a\n").unwrap();
    let runtime = runtime_with_agent_project("batch-timeout");
    let project =
        register_agent_project_at_path(&runtime, "batch-timeout", "proj", tmp.path()).await;

    let runtime_for_task = runtime.clone();
    let project_for_task = project.clone();
    let task = tokio::spawn(async move {
        runtime_for_task
            .read_file_batch(
                project_for_task.to_string(),
                vec![test_read_item("a.rs", None, None)],
                Some(false),
            )
            .await
    });
    // Give the executor a chance to enqueue, then wait well past the 32s deadline.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let result = tokio::time::timeout(Duration::from_secs(60), task)
        .await
        .unwrap_or_else(|_| panic!("batch did not resolve within the overall deadline"))
        .unwrap();
    assert!(result.success, "{:?}", result.error);
    let items = result.output["items"].as_array().unwrap();
    assert_eq!(items[0]["success"], false);
    assert_eq!(items[0]["output"]["reason_code"], "timeout");
    // No request should remain pending after the deadline.
    let queued = runtime
        .shell_clients
        .poll(ShellAgentPollRequest {
            client_id: "batch-timeout".to_string(),
            agent_instance_id: "inst".to_string(),
            projects: None,
        })
        .await
        .unwrap();
    assert!(
        queued.is_none(),
        "deadline left a pending request: {queued:?}"
    );
}

#[tokio::test]
async fn batch_with_line_numbers_stays_within_serialized_budget() {
    use webcodex_workspace::file_read_normalize::MAX_SERIALIZED_OUTPUT_PAYLOAD_BYTES;
    let tmp = tempfile::tempdir().unwrap();
    // A moderately large file whose numbered text would overflow if the batch
    // envelope + per-item metadata were ignored.
    let body = (1..=2000)
        .map(|i| format!("line-{i}-{}", "x".repeat(40)))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(tmp.path().join("big.rs"), &body).unwrap();
    let runtime = runtime_with_agent_project("batch-budget");
    let project =
        register_agent_project_at_path(&runtime, "batch-budget", "proj", tmp.path()).await;

    // A single item with a large limit: numbered text may exceed a single
    // serialized cap. The batch must not crash and must serialize valid JSON.
    let result = run_batch_and_complete_all(
        &runtime,
        "batch-budget",
        &project,
        vec![test_read_item("big.rs", Some(1), Some(1000))],
        true,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    let serialized = serde_json::to_vec(&result.output).unwrap();
    assert!(
        serialized.len() <= MAX_SERIALIZED_OUTPUT_PAYLOAD_BYTES,
        "batch output exceeded payload budget: {} > {}",
        serialized.len(),
        MAX_SERIALIZED_OUTPUT_PAYLOAD_BYTES
    );
}

#[tokio::test]
async fn batch_long_paths_and_max_items_serialize_within_budget() {
    use webcodex_workspace::file_read_normalize::MAX_SERIALIZED_OUTPUT_PAYLOAD_BYTES;
    let tmp = tempfile::tempdir().unwrap();
    let long_name = format!("dir-{}/file-{}.rs", "a".repeat(100), "b".repeat(100));
    let full_path = tmp.path().join(&long_name);
    std::fs::create_dir_all(full_path.parent().unwrap()).unwrap();
    std::fs::write(&full_path, "content\n").unwrap();
    let runtime = runtime_with_agent_project("batch-long");
    let project = register_agent_project_at_path(&runtime, "batch-long", "proj", tmp.path()).await;

    let mut items = Vec::new();
    for i in 0..16 {
        items.push(test_read_item(&format!("f{i:02}.rs"), None, None));
    }
    std::fs::write(tmp.path().join("f00.rs"), "x\n").unwrap();
    std::fs::write(tmp.path().join("f01.rs"), "y\n").unwrap();
    // The long path item is at index 16 — skip it; the count bound is 16.
    items[0] = test_read_item(&long_name, None, None);
    for i in 2..16 {
        std::fs::write(tmp.path().join(format!("f{i:02}.rs")), "z\n").unwrap();
    }
    let result = run_batch_and_complete_all(&runtime, "batch-long", &project, items, false).await;
    assert!(result.success, "{:?}", result.error);
    let serialized = serde_json::to_vec(&result.output).unwrap();
    assert!(
        serialized.len() <= MAX_SERIALIZED_OUTPUT_PAYLOAD_BYTES,
        "long-path batch exceeded payload budget: {}",
        serialized.len()
    );
}

#[tokio::test]
async fn batch_json_escaping_stays_within_serialized_budget() {
    use webcodex_workspace::file_read_normalize::MAX_SERIALIZED_OUTPUT_PAYLOAD_BYTES;
    let tmp = tempfile::tempdir().unwrap();
    // Heavy escaping (quotes, backslashes, control chars) expands JSON size.
    let escaped = "\"\\\n\t\u{1}\u{2}";
    std::fs::write(tmp.path().join("escaped.rs"), escaped.repeat(1000)).unwrap();
    let runtime = runtime_with_agent_project("batch-escape");
    let project =
        register_agent_project_at_path(&runtime, "batch-escape", "proj", tmp.path()).await;

    let result = run_batch_and_complete_all(
        &runtime,
        "batch-escape",
        &project,
        vec![test_read_item("escaped.rs", None, None)],
        false,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    let serialized = serde_json::to_vec(&result.output).unwrap();
    assert!(
        serialized.len() <= MAX_SERIALIZED_OUTPUT_PAYLOAD_BYTES,
        "escaped batch exceeded payload budget: {}",
        serialized.len()
    );
}

#[tokio::test]
async fn batch_next_items_preserves_original_requests_whole() {
    use webcodex_workspace::file_read_normalize::MAX_SERIALIZED_OUTPUT_PAYLOAD_BYTES;
    let tmp = tempfile::tempdir().unwrap();
    // A file whose plain text is near the raw cap: a single normalized result is
    // too large to fit alongside the batch envelope, so it must move to
    // `next_items` whole (never partially serialized).
    let body = (1..=2000)
        .map(|i| format!("l{i}-{}", "y".repeat(90)))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(tmp.path().join("huge.rs"), &body).unwrap();
    let runtime = runtime_with_agent_project("batch-next");
    let project = register_agent_project_at_path(&runtime, "batch-next", "proj", tmp.path()).await;

    let result = run_batch_and_complete_all(
        &runtime,
        "batch-next",
        &project,
        vec![
            test_read_item("huge.rs", Some(1), Some(2000)),
            test_read_item("huge.rs", Some(1), Some(2000)),
        ],
        false,
    )
    .await;
    assert!(result.success, "{:?}", result.error);
    let serialized = serde_json::to_vec(&result.output).unwrap();
    assert!(
        serialized.len() <= MAX_SERIALIZED_OUTPUT_PAYLOAD_BYTES,
        "next_items batch exceeded payload budget: {}",
        serialized.len()
    );
    // Either nothing fits (all in next_items) or at least the first fits and
    // the rest go to next_items. Every next_items entry keeps the original
    // path/start_line/limit.
    let next_items = result.output["next_items"].as_array().unwrap();
    let returned = result.output["items"].as_array().unwrap();
    assert!(!returned.is_empty() || !next_items.is_empty());
    for entry in next_items {
        assert!(entry.get("path").is_some());
        assert!(entry.get("start_line").is_some());
        assert!(entry.get("limit").is_some());
    }
}

#[tokio::test]
async fn batch_generic_tool_call_path_dispatches_batch() {
    // Exercise the generic HTTP/MCP parsing path (`from_tool_name`) end to end:
    // a batch `read_file` call must deserialize and dispatch, and the kernel
    // path must accept `items` as a top-level argument.
    use crate::tool_runtime::kernel::{ToolCallContext, ToolCallRequest, ToolTransport};
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "alpha\n").unwrap();
    std::fs::write(tmp.path().join("b.rs"), "beta\n").unwrap();
    let runtime = runtime_with_agent_project("batch-kernel");
    let bootstrap = bootstrap_auth_context();
    let project = register_agent_project_at_path_with_auth(
        &runtime,
        "batch-kernel",
        "proj",
        tmp.path(),
        &bootstrap,
    )
    .await;

    let runtime_for_task = runtime.clone();
    let project_for_task = project.clone();
    let auth_for_task = bootstrap.clone();
    let task = tokio::spawn(async move {
        runtime_for_task
            .call_tool_with_context(
                ToolCallRequest {
                    tool_name: "read_file".to_string(),
                    arguments: json!({
                        "project": project_for_task,
                        "items": [
                            {"path": "a.rs", "limit": 1},
                            {"path": "b.rs", "limit": 1}
                        ]
                    }),
                },
                ToolCallContext {
                    transport: ToolTransport::Api,
                    session_id: None,
                    auth: Some(&auth_for_task),
                    window: None,
                    record_oauth_scope_denials: false,
                },
            )
            .await
    });
    for _ in 0..200 {
        let req = runtime
            .shell_clients
            .poll(ShellAgentPollRequest {
                client_id: "batch-kernel".to_string(),
                agent_instance_id: "inst".to_string(),
                projects: None,
            })
            .await
            .unwrap();
        let Some(req) = req else {
            break;
        };
        complete_agent_request_by_running_locally(&runtime, "batch-kernel", req).await;
    }
    let outcome = task.await.unwrap();
    assert!(
        outcome.result.is_some(),
        "batch dispatch should produce a result"
    );
    let result = outcome.result.unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["mode"], "batch");
    assert_eq!(result.output["requested_count"], 2);
}
