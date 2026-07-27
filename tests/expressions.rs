//! Covers docs/04_テスト仕様書.md §4.7 式評価・変数表示 (TC-EXPR-*).
//!
//! All tests that need initialized locals break at bp_target.c:27 (the
//! `printf("hello\n")` call), by which point `x`, `s`, `arr`, `ps` and
//! `str` have all been assigned (see tests/fixtures/bp_target.c).

mod common;

use common::{build_fixture, TdbSession};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

fn session_at_locals_ready(exe: &std::path::Path) -> TdbSession {
    let mut session = TdbSession::spawn(exe);
    session.send("run");
    session.wait_for("Exception 0x80000003", TIMEOUT);
    session.send("break bp_target.c:27");
    session.wait_for("Breakpoint #1 set", TIMEOUT);
    session.send("continue");
    session.wait_for("Breakpoint hit at", TIMEOUT);
    session
}

/// TC-EXPR-01: arithmetic with operator precedence.
#[test]
fn arithmetic_expression() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["run", "print 1 + 2 * 3", "quit"]);
    let out = session.finish(TIMEOUT);
    assert!(out.contains("1 + 2 * 3 = 7"), "unexpected arithmetic result:\n{out}");
}

/// TC-EXPR-02, TC-EXPR-03: reading a local variable and a register.
#[test]
fn variable_and_register_read() {
    let exe = build_fixture("bp_target");
    let mut session = session_at_locals_ready(&exe);
    session.send("print x");
    let out = session.wait_for("x = 10", TIMEOUT);
    let _ = out;

    // Registers have no static type, so a plain `print` renders them in
    // decimal by default; `/x` is what makes this a meaningful, readable
    // check (and matches how registers are conventionally inspected).
    session.send("print/x $rip");
    let out = session.wait_for("$rip = 0x", TIMEOUT);
    let _ = out;
    session.finish(TIMEOUT);
}

/// TC-EXPR-04: struct member access.
#[test]
fn struct_member_access() {
    let exe = build_fixture("bp_target");
    let mut session = session_at_locals_ready(&exe);
    session.send("print s.a");
    session.wait_for("s.a = 1", TIMEOUT);
    session.send("print s.b");
    session.wait_for("s.b = 2", TIMEOUT);
    session.finish(TIMEOUT);
}

/// TC-EXPR-05: array indexing.
#[test]
fn array_indexing() {
    let exe = build_fixture("bp_target");
    let mut session = session_at_locals_ready(&exe);
    session.send("print arr[2]");
    session.wait_for("arr[2] = 3", TIMEOUT);
    session.finish(TIMEOUT);
}

/// TC-EXPR-06: hex print format.
#[test]
fn print_format_hex() {
    let exe = build_fixture("bp_target");
    let mut session = session_at_locals_ready(&exe);
    session.send("print/x x");
    session.wait_for("x = 0xa", TIMEOUT);
    session.finish(TIMEOUT);
}

/// TC-EXPR-07: `/s` reads a `char *` as a C string.
#[test]
fn print_format_string() {
    let exe = build_fixture("bp_target");
    let mut session = session_at_locals_ready(&exe);
    // Wait for the next prompt rather than for the printed text directly:
    // the breakpoint's source listing above already contains the exact
    // substring `"Hello, World!"` (from the `char *str = "Hello, World!";`
    // declaration line), so matching on that text alone could be satisfied
    // before `print/s` even runs.
    let prompts_before = session.count("(tdb) ");
    session.send("print/s str");
    let out = session.wait_for_count("(tdb) ", prompts_before + 1, TIMEOUT);
    assert!(out.contains("str = 0x"), "expected a print/s result line:\n{out}");
    assert!(out.contains("\"Hello, World!\""), "expected the C string to be printed:\n{out}");
    session.finish(TIMEOUT);
}

/// TC-EXPR-08: assigning to a variable.
#[test]
fn assign_to_variable() {
    let exe = build_fixture("bp_target");
    let mut session = session_at_locals_ready(&exe);
    session.send("set x = 42");
    session.wait_for("x = 42", TIMEOUT);
    session.send("print x");
    session.wait_for("x = 42", TIMEOUT);
    session.finish(TIMEOUT);
}

/// TC-EXPR-09: assigning to a register.
#[test]
fn assign_to_register() {
    let exe = build_fixture("bp_target");
    let mut session = session_at_locals_ready(&exe);
    session.send("set $rax = 0x10");
    session.wait_for("$rax = 16", TIMEOUT);
    session.send("registers");
    // Wait for the line after RAX's so its own line is guaranteed complete.
    let out = session.wait_for("RBX:", TIMEOUT);
    assert!(
        out.contains("RAX: 0x0000000000000010"),
        "RAX should reflect the assignment:\n{out}"
    );
    session.finish(TIMEOUT);
}

/// TC-EXPR-11: `set print elements` truncates array printing.
#[test]
fn print_elements_limit_truncates_arrays() {
    let exe = build_fixture("bp_target");
    let mut session = session_at_locals_ready(&exe);
    session.send("set print elements 2");
    session.send("print arr");
    // Each element renders as "N (0xN)" (eval::Value's Display), same as any
    // other plain-int scalar print with no format spec.
    let out = session.wait_for("...", TIMEOUT);
    assert!(
        out.contains("{1 (0x1), 2 (0x2)...}"),
        "expected the array to be truncated to 2 elements:\n{out}"
    );
    session.finish(TIMEOUT);
}

/// TC-EXPR-12: `show print` reports the current settings.
#[test]
fn show_print_settings() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["run", "show print", "quit"]);
    let out = session.finish(TIMEOUT);
    for expected in ["print pretty:", "print address:", "print elements:", "print repeats:"] {
        assert!(out.contains(expected), "missing {expected}:\n{out}");
    }
}

/// TC-EXPR-13: `show locals` lists local variables and their values.
#[test]
fn show_locals_lists_variables() {
    let exe = build_fixture("bp_target");
    let mut session = session_at_locals_ready(&exe);
    // `arr` is declared after `x` among bp_target.c's locals, so waiting for
    // its line guarantees `x`'s own line already arrived too. (Waiting for
    // "str =" would be ambiguous: the source listing from the breakpoint
    // hit above already shows the `char *str = "Hello, World!";`
    // declaration line, which itself contains that substring.)
    session.send("show locals");
    let out = session.wait_for("arr =", TIMEOUT);
    assert!(out.contains("x = 10"), "expected x among the locals:\n{out}");
    session.finish(TIMEOUT);
}

/// TC-EXPR-16: a malformed expression reports an error without killing the
/// REPL.
#[test]
fn invalid_expression_reports_error() {
    let exe = build_fixture("bp_target");
    let mut session = TdbSession::spawn(&exe);
    session.send_all(&["run", "print +", "print 1 + 1", "quit"]);
    let out = session.finish(TIMEOUT);
    assert!(out.contains("1 + 1 = 2"), "REPL should keep working after a bad expression:\n{out}");
}
