//! Covers docs/04_テスト仕様書.md §4.2 ブレークポイント (TC-BP-*).

mod common;

use common::{build_fixture, strip_prompt, TdbSession};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

/// TC-BP-01, TC-BP-04: setting a breakpoint by symbol name and hitting it.
#[test]
fn break_by_symbol_and_hit() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["run", "break main", "continue", "quit"]);
    let out = session.finish(TIMEOUT);

    assert!(
        out.contains("Breakpoint #1 set at") && out.contains("[main"),
        "breakpoint was not reported as set on main:\n{out}"
    );
    assert!(
        out.contains("Breakpoint hit at") && out.contains("[main"),
        "breakpoint on main was never hit:\n{out}"
    );
}

/// TC-BP-02: setting a breakpoint by `file:line`.
#[test]
fn break_by_file_line() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["run", "break bp_target.c:27", "continue", "quit"]);
    let out = session.finish(TIMEOUT);

    assert!(
        out.contains("Breakpoint #1 set at") && out.contains("bp_target.c:27"),
        "breakpoint was not resolved to bp_target.c:27:\n{out}"
    );
    assert!(out.contains("Breakpoint hit at"), "breakpoint was never hit:\n{out}");
}

/// TC-BP-05, TC-BP-06: listing and deleting a breakpoint prevents it from
/// being hit.
#[test]
fn list_and_delete_breakpoint() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&[
        "run",
        "break main",
        "breakpoints",
        "delete 1",
        "continue",
        "quit",
    ]);
    let out = session.finish(TIMEOUT);

    assert!(out.contains("#1") && out.contains("enabled"), "breakpoint listing missing:\n{out}");
    assert!(out.contains("Deleted breakpoint #1"), "delete confirmation missing:\n{out}");
    assert!(
        !out.contains("Breakpoint hit at"),
        "deleted breakpoint should never be hit:\n{out}"
    );
    let pid = common::root_pid(&out);
    assert!(
        out.contains(&format!("Process {pid} exited with code 0")),
        "target should have run to completion after the breakpoint was deleted:\n{out}"
    );
}

/// TC-BP-07: deleting a breakpoint number that doesn't exist is an error,
/// not a crash.
#[test]
fn delete_nonexistent_breakpoint_is_an_error() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["run", "delete 99", "quit"]);
    let out = session.finish(TIMEOUT);
    assert!(out.contains("Error"), "expected an error for deleting an unknown breakpoint:\n{out}");
}

/// TC-BP-08: `break` before a process exists reports the missing process,
/// not a panic.
#[test]
fn break_before_run_reports_error() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["break main", "quit"]);
    let out = session.finish(TIMEOUT);
    assert!(
        out.contains("No process is running. Use 'run' first."),
        "expected the not-attached message:\n{out}"
    );
}

/// TC-BP-03, TC-BP-09: a raw-address breakpoint is reported against the
/// currently selected process (the root process here, since bp_target.exe
/// has no children) rather than being silently ignored.
#[test]
fn break_by_raw_address_reports_target_pid() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    let out = session.wait_for("Exception 0x80000003", TIMEOUT);
    // Known from the "run" output already, so we can wait for the *exact*
    // "(pid <n>)" string below instead of re-parsing a line that might only
    // be partially arrived.
    let pid = common::root_pid(&out);

    session.send("registers");
    // Wait for the *next* register line, not "RIP:" itself: otherwise the
    // poll can catch the buffer mid-write, after "RIP:" but before its
    // value/newline have arrived, and then fail to find a full line below.
    let out = session.wait_for("RSP:", TIMEOUT);
    let rip = out
        .lines()
        .find_map(|l| strip_prompt(l).strip_prefix("RIP: "))
        .unwrap_or_else(|| panic!("RIP line missing from `registers` output:\n{out:?}"))
        .trim()
        .to_string();

    session.send(&format!("break {rip}"));
    let out = session.wait_for(&format!("(pid {pid})"), TIMEOUT);
    assert!(
        out.contains("Breakpoint #1 set at"),
        "raw-address breakpoint should have been set:\n{out}"
    );
    session.finish(TIMEOUT);
}
