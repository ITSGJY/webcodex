// Standalone process-tree fixture for validation execution lifecycle tests.
// Tests compile this file directly with rustc; it is not a production binary
// target (same pattern as fake_claude_mcp.rs / fake_server.rs / the
// job-manager use of webcodex-process's process_tree_helper.rs).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIG_IGN: usize = 1;
#[cfg(unix)]
unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("sleep");
    match mode {
        "sleep" => {
            println!("VALIDATION_HELPER_STDOUT");
            eprintln!("VALIDATION_HELPER_STDERR");
            let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
            let code: i32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
            std::thread::sleep(std::time::Duration::from_secs(secs));
            std::process::exit(code);
        }
        "spawn-descendant" => {
            spawn_descendant(&args, false);
            // Wait until the descendant has written its own marker, so the
            // direct child never exits before the descendant is provably
            // alive (a race-free "parent exits first" fixture).
            let alive_marker = args.get(3).expect("alive marker path");
            wait_until_file(Path::new(alive_marker));
            std::process::exit(0);
        }
        "spawn-descendant-keepalive" => {
            spawn_descendant(&args, true);
        }
        "ignore-term-keepalive" => {
            #[cfg(unix)]
            // SAFETY: setting SIGTERM to SIG_IGN is async-signal-safe; the
            // helper is single-threaded at this point.
            unsafe {
                signal(SIGTERM, SIG_IGN);
            }
            #[cfg(not(unix))]
            {
                eprintln!("validation_tree_helper: ignore-term-keepalive is unix-only");
                std::process::exit(2);
            }
            spawn_descendant(&args, true);
        }
        "descendant" => {
            let alive_marker = args.get(2).expect("alive marker path");
            let total: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(60);
            std::fs::write(
                alive_marker,
                format!("DESCENDANT_PID={}\n", std::process::id()),
            )
            .expect("write alive marker");
            std::thread::sleep(std::time::Duration::from_secs(total));
            std::process::exit(0);
        }
        other => {
            eprintln!("validation_tree_helper: unknown mode: {other}");
            std::process::exit(2);
        }
    }
}

/// Write the parent/descendant pid markers, spawn ourself as a descendant that
/// inherits stdout/stderr (so it keeps the validation capture pipe open), print
/// the descendant pid on stdout, then either exit (`keepalive = false`) or
/// sleep `total` seconds (`keepalive = true`).
/// Poll until `path` exists (bounded, so a missing descendant cannot hang the
/// fixture's parent forever).
fn wait_until_file(path: &Path) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_descendant(args: &[String], keepalive: bool) {
    let parent_marker = args.get(2).expect("parent marker path");
    let alive_marker = args.get(3).expect("alive marker path");
    let total: u64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(60);
    let self_exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(self_exe);
    cmd.args(["descendant", alive_marker, &total.to_string()]);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let descendant = cmd.spawn().expect("spawn descendant");
    let pid = descendant.id();
    std::fs::write(
        parent_marker,
        format!("PARENT_PID={}\nDESCENDANT_PID={pid}\n", std::process::id()),
    )
    .expect("write parent marker");
    println!("DESCENDANT_PID={pid}");
    std::io::stdout().flush().expect("flush stdout");
    if keepalive {
        std::thread::sleep(std::time::Duration::from_secs(total));
        std::process::exit(0);
    }
}
