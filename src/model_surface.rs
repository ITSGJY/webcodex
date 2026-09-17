//! Startup selection for model-facing runtime exposure.
//!
//! WebCodex exposes exactly one top-level `RuntimeExposure` per process:
//!
//! - An unset `WEBCODEX_MCP_MODEL_SURFACE` selects `Runtime(AdaptiveRuntime)`.
//!   Explicit `local-coding-v1`,
//!   `adaptive-runtime-v1`, and `full-operator-v1` values select the corresponding
//!   runtime `ModelSurface`.
//! - Unsupported values are startup configuration errors and never silently
//!   fall through to another exposure.

use crate::tool_runtime::tool_definition::{
    adaptive_runtime_direct_tool_definitions, is_adaptive_runtime_direct_tool,
    is_model_visible_tool_name, LOCAL_CODING_TOOL_NAMES,
};
use crate::tool_runtime::{registered_tool_specs, ToolSpec};

pub(crate) const MODEL_SURFACE_LOCAL_CODING: &str = "local_coding";
pub(crate) const MODEL_SURFACE_ADAPTIVE_RUNTIME: &str = "adaptive_runtime";
pub(crate) const MODEL_SURFACE_FULL_OPERATOR_RUNTIME: &str = "full_operator_runtime";

pub(crate) const MCP_MODEL_SURFACE_ENV: &str = "WEBCODEX_MCP_MODEL_SURFACE";
pub(crate) const MCP_MODEL_SURFACE_LOCAL_CODING_V1: &str = "local-coding-v1";
pub(crate) const MCP_MODEL_SURFACE_ADAPTIVE_RUNTIME_V1: &str = "adaptive-runtime-v1";
pub(crate) const MCP_MODEL_SURFACE_FULL_OPERATOR_V1: &str = "full-operator-v1";

pub(crate) const ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME: &str = "call_runtime_tool";
pub(crate) const TOOL_SURFACE_AVAILABILITY_DIRECT: &str = "direct";
pub(crate) const TOOL_SURFACE_AVAILABILITY_GATEWAY: &str = "gateway";
pub(crate) const TOOL_SURFACE_AVAILABILITY_UNAVAILABLE: &str = "unavailable";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdaptiveRuntimeGatewayTargetRoute {
    Gateway,
    Direct,
    Recursive,
    Unknown,
}

/// Protocol-neutral routing for one ordinary canonical Adaptive Runtime tool.
/// This classifies only model-surface availability: adapters may further reject
/// transport-incompatible tools, while kernel scope/authority/permission checks
/// remain final and unchanged.
pub(crate) fn adaptive_runtime_gateway_target_route(
    target: &str,
) -> AdaptiveRuntimeGatewayTargetRoute {
    if target == ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME {
        return AdaptiveRuntimeGatewayTargetRoute::Recursive;
    }
    match ModelSurface::AdaptiveRuntime.runtime_tool_invocation_route(target) {
        (TOOL_SURFACE_AVAILABILITY_DIRECT, None) => AdaptiveRuntimeGatewayTargetRoute::Direct,
        (TOOL_SURFACE_AVAILABILITY_GATEWAY, Some(ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME)) => {
            AdaptiveRuntimeGatewayTargetRoute::Gateway
        }
        _ => AdaptiveRuntimeGatewayTargetRoute::Unknown,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeExposure {
    Runtime(ModelSurface),
}

impl RuntimeExposure {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Runtime(surface) => surface.name(),
        }
    }

    pub(crate) fn model_surface(self) -> Option<ModelSurface> {
        match self {
            Self::Runtime(surface) => Some(surface),
        }
    }
}

/// Resolve the MCP `tools/list` schema projection after startup exposure is known.
///
/// An explicit operator override always wins. Without one, Adaptive Runtime uses
/// compact discovery to reduce model schema/context cost; compatibility surfaces
/// and compatibility surfaces preserve their configured projection.
pub(crate) fn effective_mcp_compact_schemas(
    exposure: RuntimeExposure,
    configured_override: Option<bool>,
) -> bool {
    configured_override.unwrap_or(matches!(
        exposure,
        RuntimeExposure::Runtime(ModelSurface::AdaptiveRuntime)
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelSurface {
    LocalCoding,
    AdaptiveRuntime,
    FullOperatorRuntime,
}

impl ModelSurface {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::LocalCoding => MODEL_SURFACE_LOCAL_CODING,
            Self::AdaptiveRuntime => MODEL_SURFACE_ADAPTIVE_RUNTIME,
            Self::FullOperatorRuntime => MODEL_SURFACE_FULL_OPERATOR_RUNTIME,
        }
    }

    /// Operator-style stateless MCP extensions stay available on the adaptive
    /// surface even though their individual schemas are hidden behind one gateway.
    pub(crate) fn supports_operator_extensions(self) -> bool {
        matches!(self, Self::AdaptiveRuntime | Self::FullOperatorRuntime)
    }

    /// Model-surface routing for one registered model-visible runtime tool.
    /// This does not grant OAuth scope, project authority, feature availability,
    /// or permission; those remain enforced by the selected tool at invocation.
    pub(crate) fn runtime_tool_invocation_route(
        self,
        tool_name: &str,
    ) -> (&'static str, Option<&'static str>) {
        self.runtime_tool_invocation_route_with_operator_extension(tool_name, false)
    }

    /// Route one runtime tool when the protocol adapter has already admitted a
    /// ModelHidden Stateless operator extension. `operator_extension_admitted`
    /// is server-owned request context; callers cannot use this to make an
    /// arbitrary hidden tool model-visible.
    pub(crate) fn runtime_tool_invocation_route_with_operator_extension(
        self,
        tool_name: &str,
        operator_extension_admitted: bool,
    ) -> (&'static str, Option<&'static str>) {
        if operator_extension_admitted {
            return match self {
                Self::LocalCoding => (TOOL_SURFACE_AVAILABILITY_UNAVAILABLE, None),
                Self::AdaptiveRuntime => (
                    TOOL_SURFACE_AVAILABILITY_GATEWAY,
                    Some(ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME),
                ),
                Self::FullOperatorRuntime => (TOOL_SURFACE_AVAILABILITY_DIRECT, None),
            };
        }
        if !is_model_visible_tool_name(tool_name) {
            return (TOOL_SURFACE_AVAILABILITY_UNAVAILABLE, None);
        }
        match self {
            Self::LocalCoding => {
                if LOCAL_CODING_TOOL_NAMES.contains(&tool_name) {
                    (TOOL_SURFACE_AVAILABILITY_DIRECT, None)
                } else {
                    (TOOL_SURFACE_AVAILABILITY_UNAVAILABLE, None)
                }
            }
            Self::AdaptiveRuntime => {
                if is_adaptive_runtime_direct_tool(tool_name) {
                    (TOOL_SURFACE_AVAILABILITY_DIRECT, None)
                } else {
                    (
                        TOOL_SURFACE_AVAILABILITY_GATEWAY,
                        Some(ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME),
                    )
                }
            }
            Self::FullOperatorRuntime => (TOOL_SURFACE_AVAILABILITY_DIRECT, None),
        }
    }
}

/// Resolve the top-level runtime exposure from the process environment.
///
/// Unset selects ordinary Adaptive Runtime. Explicit compatibility values remain
/// available for operators, but no project-scoped deployment selects a second
/// coding runtime.
pub(crate) fn resolve_runtime_exposure() -> Result<RuntimeExposure, String> {
    let configured = std::env::var(MCP_MODEL_SURFACE_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    match configured.as_deref() {
        None => Ok(RuntimeExposure::Runtime(ModelSurface::AdaptiveRuntime)),
        Some(MCP_MODEL_SURFACE_LOCAL_CODING_V1) => {
            Ok(RuntimeExposure::Runtime(ModelSurface::LocalCoding))
        }
        Some(MCP_MODEL_SURFACE_ADAPTIVE_RUNTIME_V1) => {
            Ok(RuntimeExposure::Runtime(ModelSurface::AdaptiveRuntime))
        }
        Some(MCP_MODEL_SURFACE_FULL_OPERATOR_V1) => {
            Ok(RuntimeExposure::Runtime(ModelSurface::FullOperatorRuntime))
        }
        Some(value) => Err(format!(
            "unsupported {MCP_MODEL_SURFACE_ENV} '{value}'; expected {MCP_MODEL_SURFACE_LOCAL_CODING_V1}, {MCP_MODEL_SURFACE_ADAPTIVE_RUNTIME_V1}, or {MCP_MODEL_SURFACE_FULL_OPERATOR_V1}"
        )),
    }
}

/// Registered ToolSpecs for the local_coding surface, in
/// `LOCAL_CODING_TOOL_NAMES` order. Every name must resolve to a registered
/// model-visible ToolSpec; the MCP `tools/list` surface is built from this.
pub(crate) fn local_coding_tool_specs() -> Vec<ToolSpec> {
    let mut by_name: std::collections::HashMap<String, ToolSpec> = registered_tool_specs()
        .into_iter()
        .map(|spec| (spec.name.clone(), spec))
        .collect();
    LOCAL_CODING_TOOL_NAMES
        .iter()
        .map(|name| {
            by_name.remove(*name).unwrap_or_else(|| {
                panic!("{name} local_coding tool is missing a registered ToolSpec")
            })
        })
        .collect()
}

/// Registered direct ToolSpecs for the adaptive runtime surface, ordered by
/// the rank statically declared on canonical ToolDefinitions. Ordinary
/// model-visible runtime tools default to the long-tail gateway unless their
/// ToolDefinition explicitly promotes them to direct.
pub(crate) fn adaptive_runtime_direct_tool_specs() -> Vec<ToolSpec> {
    let mut by_name: std::collections::HashMap<String, ToolSpec> = registered_tool_specs()
        .into_iter()
        .map(|spec| (spec.name.clone(), spec))
        .collect();
    adaptive_runtime_direct_tool_definitions()
        .into_iter()
        .map(|definition| {
            by_name.remove(definition.name).unwrap_or_else(|| {
                panic!(
                    "{} adaptive_runtime direct tool is missing a registered ToolSpec",
                    definition.name
                )
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_coding_tool_names_are_ordered_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for name in LOCAL_CODING_TOOL_NAMES {
            assert!(
                seen.insert(*name),
                "{name} is duplicated in LOCAL_CODING_TOOL_NAMES"
            );
        }
        assert_eq!(
            LOCAL_CODING_TOOL_NAMES.len(),
            seen.len(),
            "local_coding tool set must be unique"
        );
    }

    #[test]
    fn local_coding_tools_are_fully_registered_in_order() {
        let specs = local_coding_tool_specs();
        let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(names, LOCAL_CODING_TOOL_NAMES);
        for spec in &specs {
            assert!(
                crate::tool_runtime::tool_definition::is_model_visible_tool_name(&spec.name),
                "{} must be model-visible",
                spec.name
            );
        }
    }

    #[test]
    fn adaptive_runtime_routes_every_local_coding_compatibility_tool() {
        for tool_name in LOCAL_CODING_TOOL_NAMES {
            let (availability, via) =
                ModelSurface::AdaptiveRuntime.runtime_tool_invocation_route(tool_name);
            assert_ne!(
                availability, TOOL_SURFACE_AVAILABILITY_UNAVAILABLE,
                "AdaptiveRuntime must preserve Local Coding capability {tool_name}"
            );
            if availability == TOOL_SURFACE_AVAILABILITY_DIRECT {
                assert_eq!(via, None, "direct tool {tool_name} must not name a gateway");
            } else {
                assert_eq!(availability, TOOL_SURFACE_AVAILABILITY_GATEWAY);
                assert_eq!(via, Some(ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME));
            }
        }
    }

    #[test]
    fn coding_intent_tools_are_all_adaptive_reachable_with_expected_routes() {
        let expected_gateway = [
            "project_overview",
            "document_symbols",
            "document_diagnostics",
            "hover",
            "workspace_symbols",
            "goto_definition",
            "find_references",
            "call_hierarchy",
            "apply_patch",
            "run_script",
            "cargo_fmt",
            "go_test",
        ];
        for tool_name in crate::tool_runtime::tool_definition::CODING_INTENT_TOOL_NAMES {
            let (availability, via) =
                ModelSurface::AdaptiveRuntime.runtime_tool_invocation_route(tool_name);
            assert_ne!(
                availability, TOOL_SURFACE_AVAILABILITY_UNAVAILABLE,
                "coding intent tool {tool_name} must remain Adaptive reachable"
            );
            if expected_gateway.contains(tool_name) {
                assert_eq!(
                    (availability, via),
                    (
                        TOOL_SURFACE_AVAILABILITY_GATEWAY,
                        Some(ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME)
                    ),
                    "coding specialist {tool_name} must remain gateway-routed"
                );
            } else {
                assert_eq!(
                    (availability, via),
                    (TOOL_SURFACE_AVAILABILITY_DIRECT, None),
                    "ordinary coding tool {tool_name} should use the Adaptive direct path"
                );
            }
        }
    }

    const EXPECTED_ADAPTIVE_RUNTIME_DIRECT_TOOL_NAMES: &[&str] = &[
        "work_on_project",
        "session_discussion_summary",
        "session_handoff_summary",
        "present_goal_plan",
        "present_agent_continuation",
        "rotate_agent_continuation_endpoint",
        "runtime_status",
        "wait_for_agent_events",
        "plugin_tool",
        "skill_load",
        "tool_manifest",
        "search_project_texts",
        "read_files",
        "import_conversation_files_to_project",
        "project_artifact",
        "apply_text_edits",
        "run_process",
        "run_detached_process",
        "run_shell",
        "observe_jobs",
        "list_jobs",
        "cargo_check",
        "cargo_test",
        "git_review_summary",
        "git_diff_hunks",
        "show_changes",
        "workspace_hygiene_check",
        "finish_coding_task",
        "present_work_result",
        "present_changes",
    ];

    #[test]
    fn adaptive_runtime_direct_set_is_definition_derived_and_preserves_current_order() {
        let specs = adaptive_runtime_direct_tool_specs();
        let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(names, EXPECTED_ADAPTIVE_RUNTIME_DIRECT_TOOL_NAMES);
        for spec in &specs {
            assert!(
                crate::tool_runtime::tool_definition::is_model_visible_tool_name(&spec.name),
                "{} must be model-visible",
                spec.name
            );
        }
    }

    #[test]
    fn adaptive_runtime_structured_action_targets_are_directly_actionable() {
        for (source_tool, edge, target_tool) in [
            (
                "read_files",
                "session_hint.suggested_next_tool",
                "session_discussion_summary",
            ),
            ("observe_jobs", "items[].suggested_call.tool", "list_jobs"),
            (
                "run_process",
                "session_continuity.suggested_call.tool",
                "session_handoff_summary",
            ),
            (
                "show_changes",
                "diff_review_handoff.recovery.tool",
                "git_diff_hunks",
            ),
            (
                "finish_coding_task",
                "changes.show_changes.diff_review_handoff.recovery.tool",
                "git_diff_hunks",
            ),
        ] {
            assert_eq!(
                ModelSurface::AdaptiveRuntime.runtime_tool_invocation_route(source_tool),
                (TOOL_SURFACE_AVAILABILITY_DIRECT, None),
                "structured action source {source_tool}.{edge} must itself be Adaptive direct"
            );
            assert_eq!(
                ModelSurface::AdaptiveRuntime.runtime_tool_invocation_route(target_tool),
                (TOOL_SURFACE_AVAILABILITY_DIRECT, None),
                "{source_tool}.{edge} points to non-direct Adaptive target {target_tool}"
            );
        }

        assert_eq!(
            ModelSurface::AdaptiveRuntime.runtime_tool_invocation_route("apply_patch"),
            (
                TOOL_SURFACE_AVAILABILITY_GATEWAY,
                Some(ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME)
            ),
            "specialized patching should be discovered through the Adaptive gateway"
        );
        assert_eq!(
            ModelSurface::AdaptiveRuntime.runtime_tool_invocation_route("read_files"),
            (TOOL_SURFACE_AVAILABILITY_DIRECT, None),
            "apply_patch recovery must still point to a directly actionable read_files target"
        );
    }

    #[test]
    fn adaptive_handoff_promotions_preserve_local_and_full_operator_routes() {
        for tool_name in ["list_jobs", "git_diff_hunks"] {
            assert_eq!(
                ModelSurface::LocalCoding.runtime_tool_invocation_route(tool_name),
                (TOOL_SURFACE_AVAILABILITY_DIRECT, None),
                "{tool_name} was already Local Coding direct"
            );
        }
        for tool_name in ["list_jobs", "git_diff_hunks"] {
            assert_eq!(
                ModelSurface::FullOperatorRuntime.runtime_tool_invocation_route(tool_name),
                (TOOL_SURFACE_AVAILABILITY_DIRECT, None),
                "Full Operator must remain direct for {tool_name}"
            );
        }
    }

    #[test]
    fn ordinary_model_visible_tool_defaults_to_adaptive_gateway() {
        for tool_name in ["run_script", "apply_patch"] {
            assert!(is_model_visible_tool_name(tool_name));
            assert!(!is_adaptive_runtime_direct_tool(tool_name));
            assert_eq!(
                ModelSurface::AdaptiveRuntime.runtime_tool_invocation_route(tool_name),
                (
                    TOOL_SURFACE_AVAILABILITY_GATEWAY,
                    Some(ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME)
                )
            );
        }
    }

    #[test]
    fn specialized_patch_remains_direct_on_compatibility_surfaces() {
        assert!(LOCAL_CODING_TOOL_NAMES.contains(&"apply_patch"));
        for surface in [ModelSurface::LocalCoding, ModelSurface::FullOperatorRuntime] {
            assert_eq!(
                surface.runtime_tool_invocation_route("apply_patch"),
                (TOOL_SURFACE_AVAILABILITY_DIRECT, None),
                "apply_patch must remain directly callable on {surface:?}"
            );
        }
    }

    #[test]
    fn ergonomics_promotions_do_not_duplicate_artifact_reads() {
        assert!(is_adaptive_runtime_direct_tool(
            "import_conversation_files_to_project"
        ));
        assert!(!LOCAL_CODING_TOOL_NAMES.contains(&"import_conversation_files_to_project"));

        assert!(is_adaptive_runtime_direct_tool("project_artifact"));
        assert!(LOCAL_CODING_TOOL_NAMES.contains(&"project_artifact"));
        for legacy in [
            "read_project_artifact_metadata",
            "read_project_artifact",
            "export_project_artifact",
        ] {
            assert!(!LOCAL_CODING_TOOL_NAMES.contains(&legacy));
            assert!(!is_adaptive_runtime_direct_tool(legacy));
            assert_eq!(
                ModelSurface::AdaptiveRuntime.runtime_tool_invocation_route(legacy),
                (TOOL_SURFACE_AVAILABILITY_GATEWAY, Some(ADAPTIVE_RUNTIME_GATEWAY_TOOL_NAME)),
                "legacy artifact specialist {legacy} should remain reachable through the Adaptive gateway"
            );
        }
        for tool_name in ["import_conversation_files_to_project", "project_artifact"] {
            assert_eq!(
                ModelSurface::FullOperatorRuntime.runtime_tool_invocation_route(tool_name),
                (TOOL_SURFACE_AVAILABILITY_DIRECT, None),
                "Full Operator must remain direct for {tool_name}"
            );
        }
        assert!(is_adaptive_runtime_direct_tool("run_shell"));
        assert!(!is_adaptive_runtime_direct_tool("run_script"));
    }

    #[test]
    fn computer_tools_are_full_operator_only() {
        let full = registered_tool_specs();
        let full_names = full
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>();
        for name in [
            "computer_observe",
            "computer_control",
            "computer_save_snapshot",
        ] {
            assert!(
                full_names.contains(&name),
                "{name} must be in full_operator_runtime"
            );
            assert!(
                !LOCAL_CODING_TOOL_NAMES.contains(&name),
                "{name} must not expand local_coding"
            );
            assert!(
                !is_adaptive_runtime_direct_tool(name),
                "{name} must stay behind adaptive_runtime discovery"
            );
        }
    }

    #[test]
    fn legacy_computer_tool_names_are_not_model_visible() {
        let names = registered_tool_specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<std::collections::HashSet<_>>();
        for legacy in [
            "computer_list_targets",
            "computer_list_windows",
            "computer_list_displays",
            "computer_list_applications",
            "computer_launch_application",
            "computer_accessibility_status",
            "computer_accessibility_tree",
            "computer_find_elements",
            "computer_element_state",
            "computer_activate_window",
            "computer_scroll_to_element",
            "computer_key_input",
            "computer_input_text",
            "computer_pointer_move",
            "computer_pointer_click",
            "computer_read_clipboard",
            "computer_write_clipboard",
            "computer_snapshot",
            "computer_snapshot_display",
        ] {
            assert!(
                !names.contains(legacy),
                "legacy Computer tool leaked: {legacy}"
            );
        }
    }

    #[test]
    fn compact_schema_policy_defaults_only_adaptive_runtime_to_compact() {
        for exposure in [
            RuntimeExposure::Runtime(ModelSurface::LocalCoding),
            RuntimeExposure::Runtime(ModelSurface::AdaptiveRuntime),
            RuntimeExposure::Runtime(ModelSurface::FullOperatorRuntime),
        ] {
            let expected_default = matches!(
                exposure,
                RuntimeExposure::Runtime(ModelSurface::AdaptiveRuntime)
            );
            assert_eq!(
                effective_mcp_compact_schemas(exposure, None),
                expected_default,
                "unset compact policy drifted for {exposure:?}"
            );
            assert!(
                effective_mcp_compact_schemas(exposure, Some(true)),
                "explicit true must win for {exposure:?}"
            );
            assert!(
                !effective_mcp_compact_schemas(exposure, Some(false)),
                "explicit false must win for {exposure:?}"
            );
        }
    }

    #[test]
    fn default_surface_is_adaptive_runtime() {
        let mut env = crate::test_support::TestEnvGuard::new();
        env.remove(MCP_MODEL_SURFACE_ENV);
        assert_eq!(
            resolve_runtime_exposure(),
            Ok(RuntimeExposure::Runtime(ModelSurface::AdaptiveRuntime))
        );
    }

    #[test]
    fn explicit_local_coding_adaptive_and_full_operator_values() {
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set(MCP_MODEL_SURFACE_ENV, MCP_MODEL_SURFACE_LOCAL_CODING_V1);
        assert_eq!(
            resolve_runtime_exposure(),
            Ok(RuntimeExposure::Runtime(ModelSurface::LocalCoding))
        );
        env.set(MCP_MODEL_SURFACE_ENV, MCP_MODEL_SURFACE_ADAPTIVE_RUNTIME_V1);
        assert_eq!(
            resolve_runtime_exposure(),
            Ok(RuntimeExposure::Runtime(ModelSurface::AdaptiveRuntime))
        );
        env.set(MCP_MODEL_SURFACE_ENV, MCP_MODEL_SURFACE_FULL_OPERATOR_V1);
        assert_eq!(
            resolve_runtime_exposure(),
            Ok(RuntimeExposure::Runtime(ModelSurface::FullOperatorRuntime))
        );
        env.remove(MCP_MODEL_SURFACE_ENV);
    }

    #[test]
    fn invalid_surface_value_fails_resolution() {
        let mut env = crate::test_support::TestEnvGuard::new();
        env.set(MCP_MODEL_SURFACE_ENV, "bogus-surface");
        let error = resolve_runtime_exposure().expect_err("invalid value must fail");
        assert!(error.contains("unsupported"), "error: {error}");
        assert!(error.contains("bogus-surface"), "error: {error}");
        env.remove(MCP_MODEL_SURFACE_ENV);
    }
}
