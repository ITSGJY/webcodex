use serde_json::{json, Value};

use super::common::{
    array_schema, continuation_semantics_schema, nullable_schema, open_object_schema, schema_type,
    suggested_tool_call_schema, wrapped_output_schema,
};
use webcodex_core::runtime_contract::{ContinuationCarrier, ContinuationKind};

fn git_diff_hunks_recovery_arguments_schema() -> Value {
    json!({
        "type": "object",
        "description": "Parser-ready git_diff_hunks arguments for one bounded recovery step.",
        "additionalProperties": false,
        "properties": {
            "project": {"type": "string"},
            "paths": {"type": "array", "items": {"type": "string"}},
            "max_hunks": {"type": "integer"},
            "max_hunk_lines": {"type": "integer"},
            "max_page_bytes": {
                "type": "integer",
                "minimum": webcodex_core::runtime_contract::MIN_GIT_DIFF_HUNKS_PAGE_BYTES,
                "maximum": webcodex_core::runtime_contract::MAX_GIT_DIFF_HUNKS_PAGE_BYTES
            },
            "cached": {"type": "boolean"},
            "base_commit": {"type": "string"},
            "head_commit": {"type": "string"},
            "continuation": {"type": "string"}
        },
        "required": ["project", "paths", "max_hunks", "max_hunk_lines", "max_page_bytes"]
    })
}

fn git_diff_hunks_recovery_call_schema() -> Value {
    suggested_tool_call_schema(
        "git_diff_hunks",
        git_diff_hunks_recovery_arguments_schema(),
        "Parser-ready advisory git_diff_hunks call for later-record continuation, proven bounded parameter refinement, or exact current-hunk fragment continuation. It grants no authority and is not the continuation identity itself.",
    )
}

fn nullable_git_diff_hunks_recovery_call_schema() -> Value {
    json!({
        "anyOf": [git_diff_hunks_recovery_call_schema(), {"type": "null"}]
    })
}

fn nullable_git_diff_hunks_recovery_arguments_schema() -> Value {
    json!({
        "anyOf": [git_diff_hunks_recovery_arguments_schema(), {"type": "null"}]
    })
}

fn nullable_continuation_semantics_schema(
    kind: ContinuationKind,
    carrier: ContinuationCarrier,
    description: &str,
) -> Value {
    json!({
        "anyOf": [
            continuation_semantics_schema(kind, carrier, description),
            {"type": "null"}
        ]
    })
}

fn git_diff_hunks_omitted_lines_continuation_semantics_schema() -> Value {
    json!({
        "anyOf": [
            continuation_semantics_schema(
                ContinuationKind::Refine,
                ContinuationCarrier::None,
                "Bounded parameter refinement such as increasing max_hunk_lines and/or narrowing paths. It is not a cursor and safe_continuation_for_omitted_lines remains false.",
            ),
            continuation_semantics_schema(
                ContinuationKind::Page,
                ContinuationCarrier::OpaqueToken,
                "Exact scope/fence-bound current-hunk fragment cursor. It advances to later complete lines of the same logical hunk and is distinct from next_continuation, which remains later-record only.",
            ),
            {"type": "null"}
        ]
    })
}

fn git_diff_hunks_recovery_schema() -> Value {
    let refine_semantics = continuation_semantics_schema(
        ContinuationKind::Refine,
        ContinuationCarrier::None,
        "Bounded parameter refinement for omitted hunk lines.",
    );
    let fragment_semantics = continuation_semantics_schema(
        ContinuationKind::Page,
        ContinuationCarrier::OpaqueToken,
        "Exact current-hunk fragment continuation.",
    );
    json!({
        "type": "object",
        "description": "Actionable bounded recovery. recovery.continuation obtains later logical diff records only. recovery.omitted_lines independently reports bounded refinement or an exact scope/fence-bound current-hunk fragment continuation; fixed byte or line ceilings never receive a fake recovery call.",
        "additionalProperties": false,
        "properties": {
            "kind": {"type": "string", "enum": ["page", "hunk_lines", "mixed"]},
            "tool": {"type": "string", "const": "git_diff_hunks"},
            "arguments": nullable_git_diff_hunks_recovery_arguments_schema(),
            "safe_continuation_for_omitted_lines": nullable_schema("boolean", "True only when recovery.omitted_lines carries a safe exact hunk-fragment opaque token; false when only parameter refinement exists or omitted lines are unrecoverable; null when no current-hunk lines were omitted."),
            "continuation": {
                "type": "object",
                "description": "Later-record lane only. This continuation never stands in for current-hunk omitted lines.",
                "additionalProperties": false,
                "properties": {
                    "available": {"type": "boolean"},
                    "recovers_later_hunks": {"type": "boolean"},
                    "recovers_omitted_lines": {"type": "boolean", "const": false},
                    "continuation_semantics": nullable_continuation_semantics_schema(
                        ContinuationKind::Page,
                        ContinuationCarrier::OpaqueToken,
                        "When available, next_continuation is a scope/fence-bound opaque page cursor that recovers later diff records only; it never recovers omitted lines from the current hunk.",
                    ),
                    "next_call": nullable_git_diff_hunks_recovery_call_schema()
                },
                "required": [
                    "available", "recovers_later_hunks", "recovers_omitted_lines",
                    "continuation_semantics", "next_call"
                ]
            },
            "omitted_lines": {
                "type": "object",
                "description": "Current-hunk omitted-line lane. Recoverable content uses either bounded refinement or a distinct exact hunk-fragment opaque token.",
                "additionalProperties": false,
                "properties": {
                    "present": {"type": "boolean"},
                    "recoverable": {"type": "boolean"},
                    "reason_code": {
                        "anyOf": [
                            {
                                "type": "string",
                                "enum": [
                                    "larger_max_hunk_lines_available",
                                    "hunk_fragment_continuation_available",
                                    "page_byte_budget_prevents_proven_recovery",
                                    "max_hunk_lines_ceiling_reached",
                                    "max_hunk_lines_ceiling_insufficient",
                                    "bounded_recovery_unavailable"
                                ]
                            },
                            {"type": "null"}
                        ],
                        "description": "Stable reason why omitted hunk content is recoverable or unrecoverable within the current bounded git_diff_hunks contract."
                    },
                    "path_provenance": {"type": "string", "enum": ["none", "scope", "exact"]},
                    "paths": {"type": "array", "items": {"type": "string"}},
                    "continuation_semantics": git_diff_hunks_omitted_lines_continuation_semantics_schema(),
                    "next_call": nullable_git_diff_hunks_recovery_call_schema()
                },
                "required": [
                    "present", "recoverable", "reason_code", "path_provenance", "paths",
                    "continuation_semantics", "next_call"
                ],
                "allOf": [
                    {
                        "if": {"properties": {"present": {"const": false}}, "required": ["present"]},
                        "then": {"properties": {
                            "recoverable": {"const": false},
                            "reason_code": {"type": "null"},
                            "path_provenance": {"const": "none"},
                            "continuation_semantics": {"type": "null"},
                            "next_call": {"type": "null"}
                        }}
                    },
                    {
                        "if": {"properties": {"present": {"const": true}}, "required": ["present"]},
                        "then": {"properties": {"reason_code": {"type": "string"}}}
                    },
                    {
                        "if": {"properties": {"reason_code": {"const": "larger_max_hunk_lines_available"}}, "required": ["reason_code"]},
                        "then": {"properties": {
                            "recoverable": {"const": true},
                            "continuation_semantics": refine_semantics,
                            "next_call": git_diff_hunks_recovery_call_schema()
                        }}
                    },
                    {
                        "if": {"properties": {"reason_code": {"const": "hunk_fragment_continuation_available"}}, "required": ["reason_code"]},
                        "then": {"properties": {
                            "recoverable": {"const": true},
                            "continuation_semantics": fragment_semantics,
                            "next_call": git_diff_hunks_recovery_call_schema()
                        }}
                    },
                    {
                        "if": {"properties": {"reason_code": {"enum": [
                            "page_byte_budget_prevents_proven_recovery",
                            "max_hunk_lines_ceiling_reached",
                            "max_hunk_lines_ceiling_insufficient",
                            "bounded_recovery_unavailable"
                        ]}}, "required": ["reason_code"]},
                        "then": {"properties": {
                            "recoverable": {"const": false},
                            "continuation_semantics": {"type": "null"},
                            "next_call": {"type": "null"}
                        }}
                    }
                ]
            }
        },
        "required": [
            "kind", "tool", "arguments", "safe_continuation_for_omitted_lines",
            "continuation", "omitted_lines"
        ],
        "allOf": [
            {
                "if": {"properties": {"omitted_lines": {"properties": {"present": {"const": false}}, "required": ["present"]}}, "required": ["omitted_lines"]},
                "then": {"properties": {"safe_continuation_for_omitted_lines": {"type": "null"}}}
            },
            {
                "if": {"properties": {"omitted_lines": {"properties": {"reason_code": {"const": "hunk_fragment_continuation_available"}}, "required": ["reason_code"]}}, "required": ["omitted_lines"]},
                "then": {"properties": {"safe_continuation_for_omitted_lines": {"const": true}}}
            },
            {
                "if": {"properties": {"omitted_lines": {"properties": {"reason_code": {"enum": [
                    "larger_max_hunk_lines_available",
                    "page_byte_budget_prevents_proven_recovery",
                    "max_hunk_lines_ceiling_reached",
                    "max_hunk_lines_ceiling_insufficient",
                    "bounded_recovery_unavailable"
                ]}}, "required": ["reason_code"]}}, "required": ["omitted_lines"]},
                "then": {"properties": {"safe_continuation_for_omitted_lines": {"const": false}}}
            }
        ]
    })
}

fn show_changes_handoff_arguments_schema() -> Value {
    json!({
        "type": "object",
        "description": "Ready-to-call worktree git_diff_hunks arguments. paths stays empty when show_changes cannot prove a narrower omitted-line path.",
        "additionalProperties": false,
        "properties": {
            "project": {"type": "string"},
            "cached": {"type": "boolean", "const": false},
            "paths": {"type": "array", "items": {"type": "string"}},
            "max_hunks": {"type": "integer"},
            "max_hunk_lines": {"type": "integer"},
            "max_page_bytes": {
                "type": "integer",
                "const": webcodex_core::runtime_contract::DEFAULT_GIT_DIFF_HUNKS_PAGE_BYTES
            }
        },
        "required": ["project", "cached", "paths", "max_hunks", "max_hunk_lines", "max_page_bytes"]
    })
}

fn show_changes_diff_hunk_schema() -> Value {
    json!({
        "type": "object",
        "description": "One bounded diff hunk. source_completeness is the single authoritative statement about whether the returned hunk is proven source-complete.",
        "additionalProperties": true,
        "properties": {
            "source_completeness": {
                "type": "string",
                "enum": ["complete", "unknown"],
                "description": "complete means producer metadata and parsing prove this returned hunk source-complete; unknown means the system cannot prove completeness."
            }
        },
        "required": ["source_completeness"]
    })
}

fn show_changes_diff_file_schema() -> Value {
    json!({
        "type": "object",
        "description": "One changed file with bounded diff hunks.",
        "additionalProperties": true,
        "properties": {
            "hunks": {
                "type": "array",
                "items": show_changes_diff_hunk_schema()
            }
        }
    })
}

pub(super) fn output_schema_for_tool(name: &str) -> Option<Value> {
    match name {
        "git_commit_paths" => Some(wrapped_output_schema(vec![
            ("committed", nullable_schema("boolean", "True only when the exact-path commit is known to have completed; null when dispatch outcome is unknown.")),
            ("expected_head", schema_type("string", "Exact caller-supplied HEAD fence.")),
            ("previous_head", nullable_schema("string", "Known parent HEAD used by the successful commit, or null before it is proven.")),
            ("actual_head", nullable_schema("string", "Observed HEAD when a deterministic precondition failure can report it.")),
            ("new_head", nullable_schema("string", "New exact commit SHA when known.")),
            (
                "committed_paths",
                array_schema(
                    schema_type("string", "Exact project-relative path committed."),
                    "Exact requested paths committed on known success; empty otherwise.",
                ),
            ),
            ("state_changed", nullable_schema("boolean", "Whether repository HEAD is known to have changed.")),
            ("outcome_unknown", schema_type("boolean", "True only when Runner dispatch may have executed but no trustworthy terminal result was received.")),
            ("failure_kind", nullable_schema("string", "Stable bounded commit rejection/failure kind.")),
            ("hook_policy", schema_type("string", "Always bypassed_exact_tree: commit-tree is used so hooks cannot add unrelated paths.")),
        ])),
        "git_status" => Some(wrapped_output_schema(vec![
            (
                "exit_code",
                nullable_schema("integer", "Git command exit code."),
            ),
            ("stdout", schema_type("string", "Git command stdout.")),
            ("stderr", schema_type("string", "Git command stderr.")),
        ])),
        "git_review_summary" => Some(wrapped_output_schema(vec![
            ("project", schema_type("string", "Runtime project input.")),
            (
                "scope",
                open_object_schema("Exact requested commits, merge-base, ancestry, commit count, and effective diff range."),
            ),
            (
                "stats",
                open_object_schema("Exact aggregate committed-range file and line statistics."),
            ),
            (
                "file_classes",
                open_object_schema("Deterministic observed file-class counts plus partial metadata."),
            ),
            (
                "subsystems",
                array_schema(open_object_schema("Bounded deterministic subsystem bucket."), "Touched subsystem buckets."),
            ),
            (
                "signals",
                array_schema(open_object_schema("Bounded reviewer-attention signal; never a correctness claim."), "Deterministic review signals."),
            ),
            (
                "files",
                array_schema(open_object_schema("Bounded changed-file review metadata and symbol hints."), "Changed files returned within fixed bounds."),
            ),
            (
                "coverage",
                open_object_schema("Production/test/docs change observation; false becomes null when classification is partial."),
            ),
            ("bounds", open_object_schema("Fixed producer and model-result bounds used by this invocation.")),
            (
                "truncation",
                open_object_schema("Explicit file, symbol, subsystem, and signal partiality metadata."),
            ),
            ("deterministic", schema_type("boolean", "Always true for this built-in deterministic classifier.")),
            ("llm_summary", schema_type("boolean", "Always false; no LLM is used for this review map.")),
            ("truncated", schema_type("boolean", "Whether any review-map observation is partial or bounded.")),
            (
                "warnings",
                array_schema(schema_type("string", "Stable bounded review warning."), "Bounded non-fatal warnings."),
            ),
            ("reason_code", nullable_schema("string", "Stable structured failure reason when review observation cannot proceed.")),
        ])),
        "git_diff_hunks" => Some(wrapped_output_schema(vec![
            ("project", schema_type("string", "Runtime project input.")),
            (
                "paths",
                array_schema(
                    schema_type("string", "Normalized project-relative diff path."),
                    "Normalized diff scope paths.",
                ),
            ),
            ("cached", schema_type("boolean", "Whether the staged diff is inspected.")),
            (
                "max_page_bytes",
                schema_type("integer", "Effective raw producer-page byte budget. This is not the final serialized model-facing result budget."),
            ),
            (
                "scope",
                json!({
                    "type": "object",
                    "description": "Committed-range identity when base_commit/head_commit mode is used; omitted for worktree/cached mode.",
                    "additionalProperties": false,
                    "properties": {
                        "mode": {
                            "type": "string",
                            "enum": ["committed"]
                        },
                        "requested_base": nullable_schema("string", "Normalized exact requested base commit id, or null when input is invalid."),
                        "requested_head": nullable_schema("string", "Normalized exact requested head commit id, or null when input is invalid."),
                        "merge_base": nullable_schema("string", "Single exact best merge-base used for the review diff, or null before range resolution succeeds."),
                        "base_is_ancestor": nullable_schema("boolean", "Whether requested_base is an ancestor of requested_head, or null before range resolution succeeds."),
                        "diff_range": nullable_schema("string", "Effective merge_base..requested_head committed diff range, or null before range resolution succeeds.")
                    },
                    "required": [
                        "mode",
                        "requested_base",
                        "requested_head",
                        "merge_base",
                        "base_is_ancestor",
                        "diff_range"
                    ]
                }),
            ),
            (
                "files",
                array_schema(open_object_schema("File diff hunks."), "Changed files."),
            ),
            ("hunk_count", schema_type("integer", "Returned hunk count.")),
            (
                "truncated",
                schema_type("boolean", "Whether any page or per-hunk preview bound fired."),
            ),
            (
                "truncation_reasons",
                array_schema(
                    schema_type("string", "Stable git diff hunk truncation reason."),
                    "Stable reasons for page/hunk truncation.",
                ),
            ),
            (
                "has_more",
                schema_type("boolean", "Whether another logical diff page exists."),
            ),
            (
                "next_continuation",
                nullable_schema("string", "Opaque scope/fence-bound continuation for later logical diff records only. It is classified as page + opaque_token by recovery.continuation.continuation_semantics and never stands in for recovery.omitted_lines current-hunk fragment continuation."),
            ),
            ("recovery", git_diff_hunks_recovery_schema()),
            (
                "exit_code",
                nullable_schema("integer", "Git diff exit code."),
            ),
            ("stderr", schema_type("string", "Bounded Git diff stderr.")),
        ])),
        "git_log" => Some(wrapped_output_schema(vec![
            ("project", schema_type("string", "Runtime project id.")),
            ("limit", schema_type("integer", "Effective commit limit.")),
            ("skip", schema_type("integer", "Effective commit offset.")),
            ("count", schema_type("integer", "Returned commit count.")),
            (
                "truncated",
                schema_type("boolean", "Whether more commits were available."),
            ),
            (
                "next_skip",
                nullable_schema(
                    "integer",
                    "Exact skip value for the next page when another parser-ready page exists inside the bounded skip domain; null on the final page or when the 10000 skip ceiling prevents a safe forward page.",
                ),
            ),
            (
                "commits",
                array_schema(open_object_schema("Git commit summary."), "Recent commits."),
            ),
        ])),
        "show_changes" => Some(wrapped_output_schema(vec![
            ("project", schema_type("string", "Runtime project id.")),
            (
                "git_available",
                schema_type(
                    "boolean",
                    "Whether git-backed inspection was available. False for non-git projects.",
                ),
            ),
            (
                "non_git_project",
                schema_type(
                    "boolean",
                    "True when the project directory is not inside a git repository.",
                ),
            ),
            (
                "git_error",
                nullable_schema(
                    "string",
                    "Short summary when git-backed inspection is unavailable; null otherwise.",
                ),
            ),
            (
                "branch",
                nullable_schema(
                    "string",
                    "Current git branch from porcelain status; null for detached or unavailable Git state.",
                ),
            ),
            (
                "upstream_status",
                json!({
                    "type": "string",
                    "enum": ["available", "absent", "gone", "unobserved"],
                    "description": "Whether a tracking branch is available, absent, gone, or unobserved."
                }),
            ),
            (
                "upstream_reason_code",
                nullable_schema(
                    "string",
                    "Stable reason code for gone or unobserved upstream state.",
                ),
            ),
            (
                "upstream",
                nullable_schema("string", "Configured upstream tracking branch when observed."),
            ),
            (
                "ahead",
                nullable_schema("integer", "Commits ahead of an available upstream."),
            ),
            (
                "behind",
                nullable_schema("integer", "Commits behind an available upstream."),
            ),
            (
                "head",
                json!({
                    "type": "object",
                    "description": "Current HEAD commit metadata.",
                    "properties": {
                        "commit": nullable_schema("string", "Full HEAD commit hash when observed."),
                        "short": nullable_schema("string", "Short HEAD commit hash when observed."),
                        "summary": nullable_schema("string", "HEAD commit subject when observed.")
                    },
                    "required": ["commit", "short", "summary"],
                    "additionalProperties": false
                }),
            ),
            (
                "status_observation",
                json!({
                    "type": "object",
                    "description": "Independent git status execution and repository-probe result.",
                    "properties": {
                        "status": {
                            "type": "string",
                            "enum": ["observed", "non_git", "command_failed", "output_unavailable"]
                        },
                        "reason_code": nullable_schema("string", "Stable reason code when status was not observed."),
                        "exit_code": nullable_schema("integer", "Exit code from git status itself."),
                        "repository_probe": {
                            "type": "string",
                            "enum": ["inside_worktree", "outside_worktree", "unavailable"]
                        },
                        "repository_probe_exit_code": nullable_schema("integer", "Exit code from the explicit repository probe.")
                    },
                    "required": [
                        "status", "reason_code", "exit_code", "repository_probe",
                        "repository_probe_exit_code"
                    ],
                    "additionalProperties": false
                }),
            ),
            (
                "clean",
                nullable_schema(
                    "boolean",
                    "Whether the worktree is clean when Git was observed; null otherwise.",
                ),
            ),
            (
                "counts",
                json!({
                    "type": "object",
                    "description": "Parsed status counts. conflicted is null when Git status was not observed.",
                    "properties": {
                        "modified": schema_type("integer", "Modified file count."),
                        "added": schema_type("integer", "Added file count."),
                        "deleted": schema_type("integer", "Deleted file count."),
                        "renamed": schema_type("integer", "Renamed file count."),
                        "copied": schema_type("integer", "Copied file count."),
                        "untracked": schema_type("integer", "Untracked file count."),
                        "conflicted": nullable_schema("integer", "Conflict count when observed; null otherwise."),
                        "staged": schema_type("integer", "Staged file count."),
                        "unstaged": schema_type("integer", "Unstaged file count.")
                    },
                    "required": [
                        "modified", "added", "deleted", "renamed", "copied",
                        "untracked", "conflicted", "staged", "unstaged"
                    ],
                    "additionalProperties": false
                }),
            ),
            (
                "files",
                array_schema(open_object_schema("Changed file status."), "Changed files."),
            ),
            (
                "files_total",
                nullable_schema(
                    "integer",
                    "Exact count of all status entries, even when the returned file records were bounded by the production-side limit. Null when status was not observed.",
                ),
            ),
            (
                "files_returned",
                schema_type(
                    "integer",
                    "Number of changed-file records actually returned (files.len()).",
                ),
            ),
            (
                "files_truncated",
                schema_type(
                    "boolean",
                    "Whether the returned file records were bounded by the production-side limit.",
                ),
            ),
            (
                "files_limit",
                schema_type(
                    "integer",
                    "Production-side cap on returned changed-file records.",
                ),
            ),
            (
                "transport_safe",
                schema_type(
                    "boolean",
                    "Whether the production-side output stayed within the transport budget so no tail-retention truncation occurred.",
                ),
            ),
            (
                "output_budget_bytes",
                schema_type(
                    "integer",
                    "Production-side stdout budget in bytes the command is constructed to stay under.",
                ),
            ),
            (
                "output_truncated",
                schema_type(
                    "boolean",
                    "Whether the final output was truncated by the output budget (reported via this structured field, never inferred from a truncation marker string).",
                ),
            ),
            (
                "truncation_reasons",
                array_schema(
                    schema_type("string", "Stable reason code for an output truncation."),
                    "Reasons the production-side output was bounded/truncated.",
                ),
            ),
            (
                "diff_stat",
                schema_type("string", "Git diff --stat output."),
            ),
            (
                "diff_exit",
                nullable_schema(
                    "integer",
                    "Real full `git diff` exit code captured by the production-side loop; null when unavailable.",
                ),
            ),
            (
                "diff_status",
                json!({
                    "type": "object",
                    "description": "Structured full `git diff` observation: observed (exit captured), command_failed (non-zero exit), or output_unavailable.",
                    "properties": {
                        "status": {
                            "type": "string",
                            "enum": ["observed", "command_failed", "output_unavailable"]
                        },
                        "exit_code": nullable_schema("integer", "Full git diff exit code when observed.")
                    },
                    "required": ["status", "exit_code"],
                    "additionalProperties": false
                }),
            ),
            (
                "diff_stat_exit",
                nullable_schema(
                    "integer",
                    "Real `git diff --stat` exit code; null when unavailable.",
                ),
            ),
            (
                "diff_stat_status",
                json!({
                    "type": "object",
                    "description": "Structured `git diff --stat` observation. This inspection status is independent from transport_safe and participates in tool success for confirmed Git worktrees.",
                    "properties": {
                        "status": {
                            "type": "string",
                            "enum": ["observed", "command_failed", "output_unavailable"]
                        },
                        "exit_code": nullable_schema(
                            "integer",
                            "Real git diff --stat exit code when observed."
                        ),
                        "reason_code": nullable_schema(
                            "string",
                            "Stable reason code when diff-stat did not succeed."
                        )
                    },
                    "required": ["status", "exit_code", "reason_code"],
                    "additionalProperties": false
                }),
            ),
            (
                "head_exit",
                nullable_schema(
                    "integer",
                    "Real `git log -1` HEAD metadata exit code; null when unavailable.",
                ),
            ),
            (
                "hunks",
                array_schema(
                    show_changes_diff_file_schema(),
                    "Diff hunks. source_completeness is authoritative for each returned hunk; top-level truncation metadata describes omitted records/content outside that per-hunk proof.",
                ),
            ),
            (
                "hunk_count",
                schema_type("integer", "Returned bounded diff hunk count."),
            ),
            (
                "hunks_truncated",
                schema_type("boolean", "Whether diff hunks were truncated by limits."),
            ),
            (
                "diff_review_handoff",
                json!({
                    "type": "object",
                    "description": "Present only when bounded show_changes diff hunks are incomplete; identifies the canonical focused/paged review tool without starting it or inventing a continuation.",
                    "additionalProperties": false,
                    "properties": {
                        "scope": {
                            "type": "string",
                            "const": "worktree"
                        },
                        "reason": {
                            "type": "string",
                            "const": "show_changes_diff_truncated"
                        },
                        "truncation_reasons": {
                            "type": "array",
                            "description": "Actual show_changes diff bounds that caused this handoff.",
                            "items": {
                                "type": "string",
                                "enum": [
                                    "diff_hunk_count_limit",
                                    "diff_hunk_line_limit",
                                    "diff_byte_budget",
                                    "diff_hunk_byte_budget"
                                ]
                            }
                        },
                        "recovery": {
                            "type": "object",
                            "description": "Canonical parser-ready first recovery call is the exact tool + arguments pair here; kind and safety fields classify the recovery. For line or mixed truncation, continuation is explicitly unsafe for omitted current-hunk lines.",
                            "additionalProperties": false,
                            "properties": {
                                "kind": {"type": "string", "enum": ["page", "hunk_lines", "mixed"]},
                                "tool": {"type": "string", "const": "git_diff_hunks"},
                                "arguments": show_changes_handoff_arguments_schema(),
                                "safe_continuation_for_omitted_lines": nullable_schema("boolean", "False when show_changes omitted current-hunk content because this handoff starts a fresh git_diff_hunks observation and carries no exact hunk-fragment identity; null for page-only truncation. The resulting git_diff_hunks observation may itself return a safe fragment continuation.")
                            },
                            "required": ["kind", "tool", "arguments", "safe_continuation_for_omitted_lines"]
                        }
                    },
                    "required": ["scope", "reason", "truncation_reasons", "recovery"]
                }),
            ),
            (
                "untracked_previews",
                array_schema(
                    open_object_schema("Bounded untracked file preview or skip reason."),
                    "Untracked file previews.",
                ),
            ),
            (
                "untracked_previews_truncated",
                schema_type(
                    "boolean",
                    "Whether the untracked preview file list was bounded/truncated.",
                ),
            ),
            (
                "warnings",
                array_schema(open_object_schema("Review warning."), "Warnings."),
            ),
            (
                "suggested_next_actions",
                array_schema(
                    schema_type("string", "Suggested action."),
                    "Suggested actions.",
                ),
            ),
            (
                "verdict",
                open_object_schema("Operator-friendly review verdict: status pass/warn/fail, blocking, blocking_reasons, warning_reasons, and suggested_next_actions. Additive UX summary only; does not change safety semantics."),
            ),
            (
                "session",
                nullable_schema("object", "Optional session activity summary."),
            ),
            (
                "exit_code",
                nullable_schema("integer", "Git inspection command exit code."),
            ),
            ("stderr", schema_type("string", "Bounded Git inspection stderr.")),
        ])),
        _ => None,
    }
}
