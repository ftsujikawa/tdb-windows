//! Shared test harness for tdb-windows's integration tests.
//!
//! tdb-windows is an interactive REPL that drives a debuggee through the
//! Win32 debug API, so these tests work by spawning the real
//! `tdb-windows.exe` against a small C fixture, feeding it a REPL command
//! script, and asserting on the text it prints. See docs/04_テスト仕様書.md
//! for the test cases this suite automates (each test below references the
//! TC-ID(s) it covers).
#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Path to the tdb-windows.exe built for this test run. Cargo sets
/// `CARGO_BIN_EXE_<name>` for every binary target of the package under
/// test, so this always points at the freshly built binary without
/// hardcoding target/debug vs. target/release.
pub fn tdb_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tdb-windows"))
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

// Guards fixture builds: several #[test] functions in the same test binary
// can ask to build the same fixture concurrently (libtest runs tests within
// one binary on multiple threads by default), and two `cl.exe` invocations
// racing on the same output file would be unsafe. A single process-wide
// mutex serializes them; the mtime check below then makes every build but
// the first a cheap no-op.
static BUILD_LOCK: Mutex<()> = Mutex::new(());

/// Compiles `tests/fixtures/<name>.c` into `tests/fixtures/<name>.exe` with
/// MSVC (`cl.exe`), rebuilding only if the executable is missing or older
/// than its source. Panics with a clear message if no MSVC toolchain can be
/// found, since that's a genuine environment problem the caller needs to
/// know about, not something a test can work around.
pub fn build_fixture(name: &str) -> PathBuf {
    let _guard = BUILD_LOCK.lock().unwrap();

    let dir = fixtures_dir();
    let src = dir.join(format!("{name}.c"));
    let exe = dir.join(format!("{name}.exe"));

    let needs_build = match (std::fs::metadata(&exe), std::fs::metadata(&src)) {
        (Ok(exe_meta), Ok(src_meta)) => {
            src_meta.modified().unwrap() > exe_meta.modified().unwrap()
        }
        _ => true,
    };
    if needs_build {
        compile_with_cl(&dir, &src, &exe, name);
    }

    assert!(
        exe.exists(),
        "fixture {name} did not produce {}",
        exe.display()
    );
    exe
}

fn compile_with_cl(dir: &Path, src: &Path, exe: &Path, name: &str) {
    // Most shells don't have cl.exe on PATH unless launched from a Visual
    // Studio "Developer Command Prompt", so try it directly first (cheap,
    // and works when the test happens to run inside one) before falling
    // back to locating Visual Studio and running through vcvars64.bat.
    if run_cl_direct(dir, src, exe) {
        return;
    }
    run_cl_via_vcvars(dir, src, exe, name);
}

fn cl_args(src: &Path, exe: &Path, name: &str) -> Vec<String> {
    vec![
        "/nologo".to_string(),
        "/Zi".to_string(),
        "/EHsc".to_string(),
        format!("/Fd:{name}.pdb"),
        format!("/Fe:{}", exe.display()),
        src.display().to_string(),
    ]
}

fn run_cl_direct(dir: &Path, src: &Path, exe: &Path) -> bool {
    let name = exe.file_stem().unwrap().to_string_lossy().to_string();
    Command::new("cl")
        .args(cl_args(src, exe, &name))
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_cl_via_vcvars(dir: &Path, src: &Path, exe: &Path, name: &str) {
    let vswhere = PathBuf::from(
        r"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe",
    );
    let output = Command::new(&vswhere)
        .args(["-latest", "-products", "*", "-property", "installationPath"])
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "could not build fixture {name}: cl.exe is not on PATH and vswhere.exe failed ({e}). \
                 Build it manually from a Visual Studio Developer Command Prompt: cl {} {}",
                cl_args(src, exe, name).join(" "),
                src.display()
            )
        });
    let install_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        !install_path.is_empty(),
        "vswhere.exe did not report a Visual Studio installation; cannot build fixture {name}"
    );

    let vcvars = format!(r"{install_path}\VC\Auxiliary\Build\vcvars64.bat");

    // A temporary .bat file, rather than `cmd /C "<one big quoted string>"`:
    // nesting cl.exe's own `"..."`-quoted arguments inside the single string
    // `cmd /C` takes gets mangled by cmd's separate (and separately quirky)
    // re-parsing of that argument. Quoting inside a .bat file's own lines
    // doesn't have that problem, since only cmd's normal script interpreter
    // ever parses it.
    let bat_path = dir.join(format!("_build_{name}.bat"));
    let bat_contents = format!(
        "@echo off\r\ncall \"{vcvars}\" >nul\r\ncl {}\r\n",
        cl_args(src, exe, name)
            .iter()
            .map(|a| format!("\"{a}\""))
            .collect::<Vec<_>>()
            .join(" ")
    );
    std::fs::write(&bat_path, bat_contents)
        .unwrap_or_else(|e| panic!("failed to write build script for fixture {name}: {e}"));

    let status = Command::new("cmd")
        .arg("/C")
        .arg(&bat_path)
        .current_dir(dir)
        .status();
    let _ = std::fs::remove_file(&bat_path);
    let status =
        status.unwrap_or_else(|e| panic!("failed to invoke cmd.exe/cl.exe for fixture {name}: {e}"));
    assert!(status.success(), "cl.exe failed to compile fixture {name}");
}

/// An interactive tdb-windows session: commands can be sent one at a time,
/// with `wait_for` used in between to block until output that depends on
/// runtime state (e.g. a dynamically assigned thread id) has actually
/// appeared, before deciding what to send next.
pub struct TdbSession {
    child: Child,
    stdin: std::process::ChildStdin,
    buffer: Arc<Mutex<String>>,
}

impl TdbSession {
    /// Spawns `tdb-windows.exe <target_exe>`.
    ///
    /// stdout and stderr are both wired to the *same* OS pipe (rather than
    /// two independently piped fds read by two separate pump threads): the
    /// debugger is single-threaded, so its writes to stdout and stderr are
    /// strictly ordered relative to each other, but two independent reader
    /// threads racing to append to a shared buffer can't preserve that
    /// order - a stdout chunk sitting unread in its pipe can lose the race
    /// to a stderr chunk that arrived later, scrambling the captured text
    /// (e.g. splitting an `eprintln!` line apart with unrelated stdout
    /// output). Merging into one pipe up front makes the kernel the single
    /// arbiter of ordering, which matches how the process actually wrote
    /// the bytes.
    pub fn spawn(target_exe: &Path) -> Self {
        let (reader, writer) = os_pipe::pipe().expect("failed to create merged stdout/stderr pipe");
        let writer_clone = writer
            .try_clone()
            .expect("failed to clone pipe writer for stderr");

        let mut child = Command::new(tdb_exe())
            .arg(target_exe)
            .stdin(Stdio::piped())
            .stdout(writer)
            .stderr(writer_clone)
            .spawn()
            .expect("failed to spawn tdb-windows.exe");

        let stdin = child.stdin.take().unwrap();
        let buffer = Arc::new(Mutex::new(String::new()));

        spawn_pump(reader, buffer.clone());

        Self {
            child,
            stdin,
            buffer,
        }
    }

    /// Sends one REPL command line.
    pub fn send(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("failed to write to tdb-windows stdin");
    }

    /// Sends every line of `script` (convenience for tests that don't need
    /// to react to intermediate output).
    pub fn send_all(&mut self, script: &[&str]) {
        for line in script {
            self.send(line);
        }
    }

    /// The full output captured so far.
    pub fn snapshot(&self) -> String {
        self.buffer.lock().unwrap().clone()
    }

    /// Polls until the accumulated output contains `needle`, returning the
    /// snapshot at that point. Panics if `timeout` elapses first, since that
    /// means the debugger never reached the expected state.
    ///
    /// Note: this only guarantees `needle` itself has fully arrived, not
    /// anything printed after it (a poll can observe the buffer mid-write).
    /// To then read content from a *later* line/position, wait on that
    /// later content directly (or use `wait_for_count` on the prompt) rather
    /// than re-parsing this call's snapshot.
    pub fn wait_for(&self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let snap = self.snapshot();
            if snap.contains(needle) {
                return snap;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for {needle:?} in tdb-windows output; got:\n{snap}");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// How many times `needle` currently appears in the captured output.
    pub fn count(&self, needle: &str) -> usize {
        self.snapshot().matches(needle).count()
    }

    /// Polls until `needle` appears at least `min_count` times, returning
    /// the snapshot at that point. Used to wait for a whole (possibly
    /// multi-line) command's output to have fully arrived by waiting for
    /// the REPL prompt to reappear one more time, rather than for a
    /// substring that might be in the middle of that output (see
    /// `wait_for`'s caveat).
    pub fn wait_for_count(&self, needle: &str, min_count: usize, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let snap = self.snapshot();
            if snap.matches(needle).count() >= min_count {
                return snap;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {min_count} occurrences of {needle:?}; got:\n{snap}"
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Sends `quit` and waits for the process to exit (killing it if it
    /// doesn't within `timeout`, so a hung debug loop fails the test instead
    /// of hanging the test suite). Returns the full captured output.
    pub fn finish(mut self, timeout: Duration) -> String {
        self.send("quit");
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(50));
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        // Give the pump threads a brief moment to drain whatever the
        // process wrote right before exiting.
        thread::sleep(Duration::from_millis(100));
        self.snapshot()
    }
}

impl Drop for TdbSession {
    fn drop(&mut self) {
        // Best-effort: if a test panics (e.g. via wait_for's timeout) before
        // calling finish(), don't leave tdb-windows.exe (and, transitively,
        // its whole debuggee process tree - Windows tears that down when
        // the debugger process dies) running in the background.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_pump(mut reader: impl Read + Send + 'static, buffer: Arc<Mutex<String>>) {
    thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&chunk[..n]);
                    buffer.lock().unwrap().push_str(&text);
                }
            }
        }
    });
}

/// Strips a leading REPL prompt ("(tdb) ") from a captured line, if present.
/// The prompt is printed *without* a trailing newline right before each
/// command is read, so a command's very first output line is otherwise
/// glued onto it as one line (e.g. "(tdb) RIP: 0x...") when captured
/// through a plain byte stream - callers that split on lines and match a
/// prefix should run this first or the match silently fails.
pub fn strip_prompt(line: &str) -> &str {
    line.strip_prefix("(tdb) ").unwrap_or(line)
}

/// Extracts the pid reported by tdb-windows's root-process creation message
/// ("Process created: {pid}", as opposed to "Child process created: ...").
/// Panics if the message isn't present, since every session that got far
/// enough to be interesting must have printed it.
pub fn root_pid(output: &str) -> u32 {
    for line in output.lines() {
        if let Some(rest) = strip_prompt(line).strip_prefix("Process created: ") {
            return rest.trim().parse().unwrap_or_else(|_| {
                panic!("could not parse pid out of {line:?}");
            });
        }
    }
    panic!("no \"Process created: <pid>\" line found in output:\n{output}");
}

/// Finds a thread id from `threads` command output that is neither the
/// current thread (marked `*`) nor a process's main thread (marked
/// `(main)`), for tests that need to switch to "some other" real thread.
pub fn pick_non_main_thread_id(threads_output: &str) -> u32 {
    for raw_line in threads_output.lines() {
        let line = strip_prompt(raw_line.trim_start()).trim_start();
        // Require "(pid " too, not just "not '*' and not '(main)'": the
        // passed-in text is often the *whole* session buffer, which can
        // also contain source listings whose line-number gutter (e.g.
        // "     17      {") would otherwise look just like a valid,
        // non-main thread line to the checks below.
        if line.starts_with('*') || line.contains("(main)") || !line.contains("(pid ") {
            continue;
        }
        if let Some(tid_str) = line.split_whitespace().next() {
            if let Ok(tid) = tid_str.parse::<u32>() {
                return tid;
            }
        }
    }
    panic!("no non-main worker thread id found in threads output:\n{threads_output}");
}
