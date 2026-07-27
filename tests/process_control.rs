//! Covers docs/04_テスト仕様書.md §4.1 プロセス制御 (TC-PROC-*).

mod common;

use common::{build_fixture, root_pid, TdbSession};
use std::time::Duration;

/// TC-PROC-01, TC-PROC-02: launching a plain program runs to completion and
/// reports a clean exit once continued past the initial breakpoint.
#[test]
fn run_and_exit_cleanly() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", Duration::from_secs(10));
    session.send("continue");
    let out = session.finish(Duration::from_secs(10));

    assert!(out.contains("Starting: "), "missing startup message:\n{out}");
    assert!(out.contains("Process created: "), "missing process-created message:\n{out}");
    assert!(out.contains("Symbols loaded at"), "missing symbol-load message:\n{out}");
    assert!(out.contains("hello"), "missing target stdout:\n{out}");
    let pid = root_pid(&out);
    assert!(
        out.contains(&format!("Process {pid} exited with code 0")),
        "missing clean-exit message for root pid {pid}:\n{out}"
    );
}

/// TC-PROC-04: `continue` before `run` is a no-op error, not a crash/hang.
#[test]
fn continue_without_run_reports_error() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("continue");
    let out = session.finish(Duration::from_secs(10));
    assert!(
        out.contains("No process is running. Use 'run' first."),
        "expected the not-attached message:\n{out}"
    );
}

/// TC-PROC-03: a second `run` restarts the target rather than reusing the
/// old process.
#[test]
fn run_again_restarts_the_target() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", Duration::from_secs(10));
    session.send("run");
    let out = session.finish(Duration::from_secs(10));
    let starts = out.matches("Starting: ").count();
    let creates = out.matches("Process created: ").count();
    assert_eq!(starts, 2, "expected two restarts:\n{out}");
    assert_eq!(creates, 2, "expected two distinct process-created messages:\n{out}");
}

/// TC-PROC-05, TC-PROC-06, TC-PROC-09, TC-PROC-10: child and grandchild
/// processes are tracked and shown (with the root/child distinction) in
/// `processes`, without requiring the parent to be debugged alone.
#[test]
fn child_and_grandchild_processes_are_tracked() {
    let exe = build_fixture("child_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", Duration::from_secs(10));

    // First continue: the child (cmd.exe) is created and stops at its own
    // initial breakpoint.
    session.send("continue");
    session.wait_for("Child process created: ", Duration::from_secs(15));

    // Second continue: cmd.exe spawns ping.exe (grandchild), which stops at
    // its own initial breakpoint in turn.
    session.send("continue");
    let out = session.wait_for_count(
        "Child process created: ",
        2,
        Duration::from_secs(15),
    );
    let _ = out;

    // Wait for the prompt to reappear after `processes`, rather than for
    // "(root)"/"(child)" directly: root and child entries are separate
    // lines in pid order, so waiting on just one of the two tags doesn't
    // guarantee the other one's line has fully arrived yet.
    let prompts_before = session.count("(tdb) ");
    session.send("processes");
    let out = session.wait_for_count("(tdb) ", prompts_before + 1, Duration::from_secs(10));
    assert!(out.contains("(root)"), "expected a (root) entry:\n{out}");
    assert!(out.contains("(child)"), "expected at least one (child) entry:\n{out}");

    // Drain the rest of the process tree (ping.exe finishing, cmd.exe
    // exiting, then the root's own remaining work) up to a clean exit.
    for _ in 0..6 {
        session.send("continue");
    }
    let out = session.finish(Duration::from_secs(30));

    let pid = root_pid(&out);
    let root_exit = format!("Process {pid} exited with code 0");
    assert!(out.contains(&root_exit), "root process never exited cleanly:\n{out}");
}

/// TC-PROC-07, TC-PROC-08: a child process exiting only ends that child's
/// tracking - the session (and the root process) keeps running until the
/// root itself exits.
#[test]
fn child_exit_does_not_end_session() {
    let exe = build_fixture("child_target");
    let mut session = TdbSession::spawn(&exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", Duration::from_secs(10));
    for _ in 0..8 {
        session.send("continue");
    }
    let out = session.finish(Duration::from_secs(30));

    let pid = root_pid(&out);
    let root_exit_msg = format!("Process {pid} exited with code 0");
    let root_exit_at = out
        .find(&root_exit_msg)
        .unwrap_or_else(|| panic!("root process never exited cleanly:\n{out}"));

    // Everything before the root's own exit must not have reported the
    // session as unattached: a child process exiting along the way should
    // not have torn down debugging state early.
    let before_root_exit = &out[..root_exit_at];
    assert!(
        !before_root_exit.contains("No process is running"),
        "session appears to have ended before the root process exited:\n{out}"
    );
    assert!(
        before_root_exit.contains("exited with code"),
        "expected at least the child process to have exited before the root:\n{out}"
    );
}
