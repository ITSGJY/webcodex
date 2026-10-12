//! Console projection parity checks and opt-in event-clone cost benchmark.
use super::*;

fn console_hook_event_unrelated(_: &SessionEvent) -> bool {
    false
}

fn console_hook_event_validation(event: &SessionEvent) -> bool {
    event.tool_name == "cargo_test"
}

fn console_hook_event_all(_: &SessionEvent) -> bool {
    true
}

fn console_hook_summary_identity(events: &[SessionEvent], _: usize) -> Value {
    json!({"available": !events.is_empty(), "latest_status": if events.is_empty() { "not_run" } else { "passed" }})
}

fn console_hook_summary_no_validation(_: &[SessionEvent], _: usize) -> Value {
    json!({"available": false, "latest_status": "unavailable"})
}

#[test]
fn console_nonvalidation_sessions_skip_deep_event_clones_but_preserve_validation_path() {
    let record = fixture(2000);
    let without_validation = ConsoleValidationHooks {
        event_observes_validation_activity: console_hook_event_validation,
        validation_summary_from_events: console_hook_summary_identity,
    };
    let no_validation = crate::console::build_list_item(&record, "project", without_validation);
    assert_eq!(no_validation.overview.validation.state, "not_run");

    let mut with_validation = record;
    Arc::make_mut(&mut with_validation.events[0]).tool_name = "cargo_test".to_string();
    let observed = crate::console::build_list_item(&with_validation, "project", without_validation);
    assert_eq!(observed.overview.validation.state, "passed");
    assert_eq!(observed.session_id, no_validation.session_id);
}

#[test]
#[ignore = "manual console list projection benchmark using 2,000 synthetic retained events"]
fn console_nonvalidation_event_clone_cost_comparison() {
    let record = fixture(2000);
    let fast = ConsoleValidationHooks {
        event_observes_validation_activity: console_hook_event_unrelated,
        validation_summary_from_events: console_hook_summary_no_validation,
    };
    let legacy_clone = ConsoleValidationHooks {
        event_observes_validation_activity: console_hook_event_all,
        validation_summary_from_events: console_hook_summary_no_validation,
    };
    let reference = crate::console::build_list_item(&record, "project", legacy_clone);
    let optimized = crate::console::build_list_item(&record, "project", fast);
    assert_eq!(
        serde_json::to_value(reference).unwrap(),
        serde_json::to_value(optimized).unwrap()
    );
    let mut slow_ns = 0u128;
    let mut fast_ns = 0u128;
    for index in 0..40 {
        if index % 2 == 0 {
            let start = Instant::now();
            std::hint::black_box(crate::console::build_list_item(
                &record,
                "project",
                legacy_clone,
            ));
            slow_ns += start.elapsed().as_nanos();
        }
        let start = Instant::now();
        std::hint::black_box(crate::console::build_list_item(&record, "project", fast));
        fast_ns += start.elapsed().as_nanos();
        if index % 2 != 0 {
            let start = Instant::now();
            std::hint::black_box(crate::console::build_list_item(
                &record,
                "project",
                legacy_clone,
            ));
            slow_ns += start.elapsed().as_nanos();
        }
    }
    println!(
        "console_2000_events legacy_clone_avg_ms={:.3} fast_avg_ms={:.3} ratio={:.2}x",
        slow_ns as f64 / 40.0 / 1e6,
        fast_ns as f64 / 40.0 / 1e6,
        slow_ns as f64 / fast_ns as f64
    );
}
