//! Covers docs/04_テスト仕様書.md §4.9 REPL・ヘルプ・エラー処理 (TC-REPL-*).

mod common;

use common::{build_fixture, TdbSession};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

/// TC-REPL-01: `help` prints the command reference.
#[test]
fn help_lists_commands() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["help", "quit"]);
    let out = session.finish(TIMEOUT);
    assert!(out.contains("Commands:"), "missing help header:\n{out}");
    assert!(out.contains("r, run"), "missing run entry in help text:\n{out}");
}

/// TC-REPL-02: an unrecognized command reports an error and the REPL keeps
/// accepting input afterward.
#[test]
fn unknown_command_reports_error_and_continues() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["foobar", "help", "quit"]);
    let out = session.finish(TIMEOUT);
    assert!(
        out.contains("Error: Invalid command: unknown command: foobar"),
        "unexpected error text:\n{out}"
    );
    assert!(out.contains("Commands:"), "REPL should still process later commands:\n{out}");
}

/// TC-REPL-03: a command missing a required argument reports an error
/// rather than panicking.
#[test]
fn missing_argument_reports_error() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["break", "quit"]);
    let out = session.finish(TIMEOUT);
    assert!(
        out.contains("Error: Invalid command: break requires an address or symbol"),
        "unexpected error text:\n{out}"
    );
}

/// TC-REPL-04: an empty line is a no-op, not an error.
#[test]
fn empty_line_is_a_no_op() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["run", "", "print 1 + 1", "quit"]);
    let out = session.finish(TIMEOUT);
    assert!(!out.contains("Error"), "an empty line should not produce an error:\n{out}");
    assert!(out.contains("1 + 1 = 2"), "REPL should still process the next command:\n{out}");
}
