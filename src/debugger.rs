use crate::breakpoint::BreakpointManager;
use crate::commands::{self, Command};
use crate::error::{DebuggerError, Result};
use crate::eval;
use crate::eval::EvalContext as _;
use crate::process;
use crate::process::{AlignedContext, DebuggeeProcess};
use crate::symbols;
use crate::symbols::SymbolResolver;
use crate::watchpoint::{self, WatchKind, WatchpointManager};
use std::collections::HashSet;
use std::io::{self, Write};
use windows::Win32::System::Diagnostics::Debug::{
    CONTEXT, CREATE_PROCESS_DEBUG_EVENT, CREATE_THREAD_DEBUG_EVENT, DEBUG_EVENT,
    EXIT_PROCESS_DEBUG_EVENT, EXIT_THREAD_DEBUG_EVENT, LOAD_DLL_DEBUG_EVENT,
    OUTPUT_DEBUG_STRING_EVENT, RIP_EVENT, UNLOAD_DLL_DEBUG_EVENT,
};

use windows::Win32::System::Diagnostics::Debug::DEBUG_EVENT_CODE;

const EXCEPTION_DEBUG_EVENT: DEBUG_EVENT_CODE = DEBUG_EVENT_CODE(1);
const EXCEPTION_BREAKPOINT: u32 = 0x80000003;
const EXCEPTION_SINGLE_STEP: u32 = 0x80000004;
const DBG_CONTINUE: u32 = 0x00010002;
const DBG_EXCEPTION_NOT_HANDLED: u32 = 0x80010001;

const STATUS_BREAKPOINT: u32 = EXCEPTION_BREAKPOINT;
const STATUS_SINGLE_STEP: u32 = EXCEPTION_SINGLE_STEP;
// Raised on a thread Windows injects into the debuggee to run its Ctrl+C
// handler, specifically so a debugger sees it before the handler runs. This
// is what process::interrupt()'s forwarded Ctrl+C shows up as here.
const DBG_CONTROL_C: u32 = 0x40010005;

struct StepTarget {
    over: bool,
    start: Option<(std::path::PathBuf, u32)>,
}

// `leak start` hooks these three allocator entry points transparently (not
// through the user-visible breakpoint list): malloc/calloc need their
// return value, so the entry hook finds the caller via stack unwinding
// (same technique as `finish`/`up`) and sets a temp breakpoint there to
// capture RAX; free only needs its argument, captured at entry.
#[derive(Debug, Clone, Copy, PartialEq)]
enum LeakHookKind {
    Malloc,
    Calloc,
    Free,
}

#[derive(Debug, Clone, Copy)]
struct PendingLeak {
    kind: LeakHookKind,
    arg: usize,
    hook_addr: usize,
    return_addr: usize,
    return_original_byte: u8,
}

// `set print` settings (gdb-style). `usize::MAX` represents "unlimited" for
// elements/repeats, matching how the rest of this file treats "no cap".
#[derive(Debug, Clone, Copy)]
struct PrintSettings {
    pretty: bool,
    address: bool,
    elements: usize,
    repeats: usize,
}

impl Default for PrintSettings {
    fn default() -> Self {
        Self {
            pretty: false,
            address: true,
            elements: 200,
            repeats: 10,
        }
    }
}

pub struct Debugger {
    target_path: String,
    process: Option<DebuggeeProcess>,
    breakpoints: BreakpointManager,
    watchpoints: WatchpointManager,
    // Thread ids seen via CREATE_PROCESS/CREATE_THREAD debug events, tracked
    // so a new watchpoint can be pushed to every live thread (debug
    // registers are per-thread) and a newly created thread can pick up
    // whatever watchpoints are already active.
    threads: HashSet<u32>,
    // Set by `lock`: the one thread allowed to run.
    locked_thread: Option<u32>,
    // The threads actually holding an extra SuspendThread from `lock`, on
    // top of the process-wide debug-event freeze. Tracked explicitly
    // (rather than inferred as "everyone but locked_thread") because a
    // thread created after `lock` is deliberately left out of this set: new
    // threads are often OS/CRT-internal helpers (e.g. for buffered I/O)
    // that the locked thread's own forward progress may depend on, and
    // freezing them can deadlock the very thread `lock` was letting run.
    frozen_threads: HashSet<u32>,
    symbols: Option<SymbolResolver>,
    last_event: Option<DEBUG_EVENT>,
    last_thread_id: u32,
    running: bool,
    pending_breakpoint: Option<usize>,
    pending_continue: Option<(u32, u32, u32)>,
    step_target: Option<StepTarget>,
    step_over_bp: Option<(usize, u8)>,
    finish_bp: Option<(usize, u8)>,
    leak_hooks: std::collections::HashMap<usize, (LeakHookKind, u8)>,
    allocations: std::collections::HashMap<u64, usize>,
    pending_leak: Option<PendingLeak>,
    pending_leak_rearm: Option<(usize, LeakHookKind)>,
    print_settings: PrintSettings,
}

impl Debugger {
    pub fn new(target_path: String) -> Self {
        Self {
            target_path,
            process: None,
            breakpoints: BreakpointManager::new(),
            watchpoints: WatchpointManager::new(),
            threads: HashSet::new(),
            locked_thread: None,
            frozen_threads: HashSet::new(),
            symbols: None,
            last_event: None,
            last_thread_id: 0,
            running: false,
            pending_breakpoint: None,
            pending_continue: None,
            step_target: None,
            step_over_bp: None,
            finish_bp: None,
            leak_hooks: std::collections::HashMap::new(),
            allocations: std::collections::HashMap::new(),
            pending_leak: None,
            pending_leak_rearm: None,
            print_settings: PrintSettings::default(),
        }
    }

    pub fn run_repl(&mut self) -> Result<()> {
        println!("tdb-windows - tiny debugger for Windows");
        println!("Type 'help' for available commands.");

        let stdin = io::stdin();
        let mut stdout = io::stdout();
        let mut line = String::new();

        loop {
            print!("(tdb) ");
            stdout.flush()?;
            line.clear();
            stdin.read_line(&mut line)?;

            match commands::parse(&line) {
                Ok(cmd) => match self.handle_command(cmd) {
                    Ok(should_quit) => {
                        if should_quit {
                            break;
                        }
                    }
                    Err(e) => eprintln!("Error: {}", e),
                },
                Err(e) => eprintln!("Error: {}", e),
            }
        }

        Ok(())
    }

    fn handle_command(&mut self, cmd: Command) -> Result<bool> {
        match cmd {
            Command::Run => {
                self.restart()?;
                self.continue_execution()?;
            }
            Command::Continue => {
                if self.process.is_some() {
                    self.continue_execution()?;
                } else {
                    eprintln!("No process is running. Use 'run' first.");
                }
            }
            Command::StepInto => self.start_step(false)?,
            Command::StepOver => self.start_step(true)?,
            Command::Up => self.finish()?,
            Command::Break(target) => {
                self.set_breakpoint(&target)?;
            }
            Command::Delete(id) => {
                if let Some(bp) = self.breakpoints.remove_by_id(id) {
                    if let Some(proc) = self.process.as_ref() {
                        proc.remove_breakpoint(bp.address, bp.original_byte)?;
                    }
                    println!(
                        "Deleted breakpoint #{} at {:#x}{}",
                        bp.id,
                        bp.address,
                        describe_address(self.symbols.as_ref(), bp.address)
                    );
                } else {
                    return Err(DebuggerError::NoBreakpoint);
                }
            }
            Command::Watch(expr, kind) => self.set_watchpoint(&expr, kind)?,
            Command::DeleteWatch(id) => self.delete_watchpoint(id)?,
            Command::ListWatchpoints => self.list_watchpoints(),
            Command::Threads => self.list_threads(),
            Command::Thread(id) => self.switch_thread(id)?,
            Command::Lock(id) => self.lock_thread(id)?,
            Command::Unlock => self.unlock_thread()?,
            Command::ListBreakpoints => {
                let mut list: Vec<_> = self.breakpoints.list().collect();
                list.sort_by_key(|bp| bp.id);
                for bp in list {
                    println!(
                        "#{} {:#x}: {}{}",
                        bp.id,
                        bp.address,
                        if bp.enabled { "enabled" } else { "disabled" },
                        describe_address(self.symbols.as_ref(), bp.address)
                    );
                }
            }
            Command::Registers => {
                if let Some(proc) = self.process.as_ref() {
                    // Needs CONTEXT_FLOATING_POINT (only get_full_thread_context_x64
                    // requests it), or the FPU/MMX/XMM area below reads back zeroed.
                    let ctx = proc.get_full_thread_context_x64(self.last_thread_id)?;
                    print_context(&ctx);
                } else {
                    eprintln!("No process is running.");
                }
            }
            Command::Memory { address, count } => {
                self.dump_memory(&address, count)?;
            }
            Command::Backtrace => self.backtrace()?,
            Command::List(target) => self.list_source(target.as_deref())?,
            Command::Print(expr, fmt) => self.print_expr(&expr, fmt)?,
            Command::Set(assignment) => self.set_expr(&assignment)?,
            Command::Show(kind) => self.show_vars(kind)?,
            Command::Leak(action) => self.leak_command(action)?,
            Command::SetPrint(option) => self.set_print_option(option),
            Command::ShowPrintSettings => self.show_print_settings(),
            Command::Help => {
                println!("{}", commands::help_text());
            }
            Command::Quit => {
                self.shutdown()?;
                return Ok(true);
            }
            Command::Empty => {}
        }
        Ok(false)
    }

    fn restart(&mut self) -> Result<()> {
        self.shutdown()?;

        println!("Starting: {}", self.target_path);
        let proc = DebuggeeProcess::launch(&self.target_path)?;
        let handle = proc.handle;
        self.symbols = Some(SymbolResolver::new(handle, None)?);
        self.process = Some(proc);
        self.running = false;
        Ok(())
    }

    fn shutdown(&mut self) -> Result<()> {
        self.symbols = None;
        self.process = None;
        self.pending_continue = None;
        self.pending_breakpoint = None;
        self.step_target = None;
        self.step_over_bp = None;
        self.finish_bp = None;
        // Watchpoint addresses are only meaningful for this process instance
        // (ASLR may relocate everything on the next run), and debug
        // registers belong to threads that no longer exist, so tracking
        // must restart from scratch rather than carry over stale state.
        self.watchpoints = WatchpointManager::new();
        self.threads.clear();
        self.locked_thread = None;
        self.frozen_threads.clear();
        // Hook addresses/original bytes are tied to this specific process
        // instance; a new run needs fresh hooks (and ASLR may relocate the
        // allocator entry points anyway), so tracking must restart from
        // scratch rather than carry over stale state.
        self.leak_hooks.clear();
        self.allocations.clear();
        self.pending_leak = None;
        self.pending_leak_rearm = None;
        Ok(())
    }

    fn leak_command(&mut self, action: commands::LeakAction) -> Result<()> {
        match action {
            commands::LeakAction::Start => self.start_leak_tracking(),
            commands::LeakAction::Stop => self.stop_leak_tracking(),
            commands::LeakAction::Report => {
                self.report_leaks();
                Ok(())
            }
        }
    }

    fn start_leak_tracking(&mut self) -> Result<()> {
        let Some(proc) = self.process.as_ref() else {
            eprintln!("No process is running.");
            return Ok(());
        };
        let Some(symbols) = self.symbols.as_ref() else {
            eprintln!("No symbols loaded.");
            return Ok(());
        };
        if !self.leak_hooks.is_empty() {
            eprintln!("Leak tracking is already running.");
            return Ok(());
        }

        let targets = [
            ("malloc", LeakHookKind::Malloc),
            ("calloc", LeakHookKind::Calloc),
            ("free", LeakHookKind::Free),
        ];

        let mut installed = 0;
        for (name, kind) in targets {
            match symbols.resolve_function_entry(name) {
                Ok(addr) => {
                    let addr = addr as usize;
                    match proc.set_breakpoint(addr) {
                        Ok(original) => {
                            self.leak_hooks.insert(addr, (kind, original));
                            installed += 1;
                        }
                        Err(e) => eprintln!("Failed to hook {}: {}", name, e),
                    }
                }
                Err(_) => eprintln!(
                    "Could not resolve '{}'; is the CRT statically linked with debug info?",
                    name
                ),
            }
        }

        if installed == 0 {
            eprintln!("Leak tracking not started: no allocator functions could be hooked.");
            return Ok(());
        }

        self.allocations.clear();
        println!(
            "Leak tracking started ({} of {} functions hooked).",
            installed,
            targets.len()
        );
        Ok(())
    }

    fn stop_leak_tracking(&mut self) -> Result<()> {
        if let Some(proc) = self.process.as_ref() {
            for (addr, (_, original_byte)) in self.leak_hooks.drain() {
                let _ = proc.remove_breakpoint(addr, original_byte);
            }
        } else {
            self.leak_hooks.clear();
        }
        self.pending_leak = None;
        self.pending_leak_rearm = None;
        println!("Leak tracking stopped.");
        Ok(())
    }

    fn report_leaks(&self) {
        if self.leak_hooks.is_empty() {
            println!("Leak tracking is not running. Use 'leak start' first.");
            return;
        }
        if self.allocations.is_empty() {
            println!("No outstanding allocations.");
            return;
        }
        let total: usize = self.allocations.values().sum();
        let mut entries: Vec<_> = self.allocations.iter().collect();
        entries.sort_by_key(|&(addr, _)| *addr);
        println!(
            "{} outstanding allocation(s), {} bytes total:",
            entries.len(),
            total
        );
        for (addr, size) in entries {
            println!("  {:#x}: {} bytes", addr, size);
        }
    }

    // "up": run until the current function returns, stopping right after the
    // call site in the caller (gdb calls this "finish").
    fn finish(&mut self) -> Result<()> {
        let Some(proc) = self.process.as_ref() else {
            eprintln!("No process is running.");
            return Ok(());
        };
        let Some(symbols) = self.symbols.as_ref() else {
            eprintln!("No symbols loaded.");
            return Ok(());
        };

        let thread_id = self.last_thread_id;
        let mut ctx = proc.get_full_thread_context_x64(thread_id)?;
        let thread_handle = proc.thread_handle(thread_id)?;
        let return_addr = symbols.unwind_return_address(thread_handle, &mut ctx)?;
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(thread_handle);
        }

        let Some(return_addr) = return_addr else {
            eprintln!("Could not determine the return address.");
            return Ok(());
        };
        let return_addr = return_addr as usize;

        if !self.breakpoints.contains(return_addr) {
            let original = proc.set_breakpoint(return_addr)?;
            self.finish_bp = Some((return_addr, original));
        }

        self.continue_execution()
    }

    fn backtrace(&mut self) -> Result<()> {
        let Some(proc) = self.process.as_ref() else {
            eprintln!("No process is running.");
            return Ok(());
        };
        let Some(symbols) = self.symbols.as_ref() else {
            eprintln!("No symbols loaded.");
            return Ok(());
        };

        let thread_id = self.last_thread_id;
        let mut ctx = proc.get_full_thread_context_x64(thread_id)?;
        let thread_handle = proc.thread_handle(thread_id)?;
        let frames = symbols.stack_trace(thread_handle, &mut ctx, 32)?;
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(thread_handle);
        }

        for (i, pc) in frames.iter().enumerate() {
            println!(
                "#{} {:#x}{}",
                i,
                pc,
                describe_address(self.symbols.as_ref(), *pc as usize)
            );
        }
        Ok(())
    }

    fn start_step(&mut self, over: bool) -> Result<()> {
        let Some(proc) = self.process.as_ref() else {
            eprintln!("No process is running.");
            return Ok(());
        };

        let rip = proc.get_thread_context_x64(self.last_thread_id)?.Rip;
        let start = self
            .symbols
            .as_ref()
            .and_then(|s| s.resolve(rip).ok())
            .and_then(|info| Some((info.file?, info.line?)));

        self.step_target = Some(StepTarget { over, start });
        self.arm_step()?;
        self.continue_execution()
    }

    // Prepares exactly one unit of stepping on the currently-halted thread:
    // for step-over, a CALL is skipped by planting a temporary breakpoint at
    // the return address instead of single-stepping into it; everything else
    // (including step-into) just arms the trap flag for one instruction.
    fn arm_step(&mut self) -> Result<()> {
        let Some(target) = self.step_target.as_ref() else {
            return Ok(());
        };
        let over = target.over;
        let thread_id = self.last_thread_id;

        let proc = self.process.as_ref().unwrap();
        let rip = proc.get_thread_context_x64(thread_id)?.Rip;

        let decoded = {
            let mut buf = [0u8; 16];
            let read = proc.read_memory(rip as usize, &mut buf).unwrap_or(0);
            crate::disasm::decode_one(&buf[..read], rip)
        };

        if over && decoded.as_ref().is_some_and(|d| d.is_call) {
            let return_addr = (rip + decoded.unwrap().length as u64) as usize;
            if !self.breakpoints.contains(return_addr) {
                let original = proc.set_breakpoint(return_addr)?;
                self.step_over_bp = Some((return_addr, original));
            }
        } else {
            proc.single_step(thread_id)?;
        }

        Ok(())
    }

    // Whether a step-driven landing is still on the line it started from, and
    // stepping should keep going rather than stop and report it.
    fn should_keep_stepping(&self, rip: u64) -> bool {
        let Some(target) = self.step_target.as_ref() else {
            return false;
        };
        let Some(start) = target.start.as_ref() else {
            return false;
        };
        let loc = self
            .symbols
            .as_ref()
            .and_then(|s| s.resolve(rip).ok())
            .and_then(|info| Some((info.file?, info.line?)));
        loc.as_ref() == Some(start)
    }

    // Resolves a breakpoint/list target ("0x1234", "1234", "add", "test.c:4")
    // to an address. Returns Ok(None) when it already reported why it
    // couldn't (no symbols/process yet), matching this file's habit of
    // reporting via eprintln! and returning Ok(()) rather than an Err.
    fn resolve_target_address(&self, target: &str) -> Result<Option<usize>> {
        let is_address = target.starts_with("0x")
            || target.starts_with("0X")
            || target.chars().all(|c| c.is_ascii_digit());

        if let Some((file, line)) = parse_file_line(target) {
            let Some(symbols) = self.symbols.as_ref() else {
                eprintln!("No symbols loaded. Use 'run' first.");
                return Ok(None);
            };
            Ok(Some(symbols.resolve_by_file_line(file, line)? as usize))
        } else if is_address {
            Ok(Some(commands::parse_address(target)?))
        } else if let Some(symbols) = self.symbols.as_ref() {
            Ok(Some(symbols.resolve_function_entry(target)? as usize))
        } else {
            eprintln!("No symbols loaded. Use 'run' first.");
            Ok(None)
        }
    }

    fn set_breakpoint(&mut self, target: &str) -> Result<()> {
        let Some(address) = self.resolve_target_address(target)? else {
            return Ok(());
        };

        if let Some(proc) = self.process.as_ref() {
            let original = proc.set_breakpoint(address)?;
            let id = self.breakpoints.add(address, original);
            println!(
                "Breakpoint #{} set at {:#x}{}",
                id,
                address,
                describe_address(self.symbols.as_ref(), address)
            );
        } else {
            eprintln!("No process is running. Use 'run' first.");
        }
        Ok(())
    }

    // Resolves `raw` to a memory location and arms a hardware watchpoint
    // (DR0-DR3/DR7) on it. `kind` selects write-only vs. read-or-write.
    fn set_watchpoint(&mut self, raw: &str, kind: WatchKind) -> Result<()> {
        if self.process.is_none() {
            eprintln!("No process is running. Use 'run' first.");
            return Ok(());
        }

        let expr = match eval::parse(raw) {
            Ok(expr) => expr,
            Err(e) => {
                eprintln!("{}", e);
                return Ok(());
            }
        };

        // Scoped so the borrow of `self` inside `ctx` ends before this
        // function needs to mutate `self.watchpoints` below.
        let (address, size, initial_value) = {
            let Some(ctx) = self.make_eval_context()? else {
                return Ok(());
            };
            let loc = match eval::eval_location(&expr, &ctx) {
                Ok(loc) => loc,
                Err(e) => {
                    eprintln!("{}", e);
                    return Ok(());
                }
            };
            let eval::Location::Memory(tl) = loc else {
                eprintln!("Cannot watch a register; expected an addressable expression.");
                return Ok(());
            };
            let Some(size) = watchpoint::hw_size(tl.size) else {
                eprintln!(
                    "'{}' is {} bytes, too large for a hardware watchpoint (max 8 bytes).",
                    raw.trim(),
                    tl.size
                );
                return Ok(());
            };
            let initial_value = eval::eval(&expr, &ctx).ok().map(|v| v.as_i64());
            (tl.address as usize, size, initial_value)
        };

        match self
            .watchpoints
            .add(raw.trim().to_string(), address, size, kind, initial_value)
        {
            Some(id) => {
                self.sync_watchpoints_to_threads()?;
                println!(
                    "Watchpoint #{} set on '{}' at {:#x} ({} byte{}, {})",
                    id,
                    raw.trim(),
                    address,
                    size,
                    if size == 1 { "" } else { "s" },
                    kind
                );
            }
            None => eprintln!(
                "Cannot set watchpoint: all 4 hardware watchpoint slots are already in use."
            ),
        }
        Ok(())
    }

    fn delete_watchpoint(&mut self, id: usize) -> Result<()> {
        let Some(wp) = self.watchpoints.remove_by_id(id) else {
            return Err(DebuggerError::NoWatchpoint);
        };
        self.sync_watchpoints_to_threads()?;
        println!("Deleted watchpoint #{} ('{}')", wp.id, wp.expr);
        Ok(())
    }

    fn list_watchpoints(&self) {
        let mut list: Vec<_> = self.watchpoints.list().collect();
        list.sort_by_key(|w| w.id);
        if list.is_empty() {
            println!("No watchpoints.");
            return;
        }
        for w in list {
            println!(
                "#{} {:#x} ({} byte{}, {}): '{}'",
                w.id,
                w.address,
                w.size,
                if w.size == 1 { "" } else { "s" },
                w.kind,
                w.expr
            );
        }
    }

    // Pushes the currently active watchpoints' DR0-DR3/DR7 values to every
    // known thread; debug registers are per-thread, so a watchpoint isn't
    // effective process-wide until this runs.
    fn sync_watchpoints_to_threads(&self) -> Result<()> {
        let Some(proc) = self.process.as_ref() else {
            return Ok(());
        };
        let (dr0, dr1, dr2, dr3, dr7) = self.watchpoints.dr7_and_addrs();
        for &tid in &self.threads {
            proc.set_debug_registers(tid, dr0, dr1, dr2, dr3, dr7)?;
        }
        Ok(())
    }

    fn apply_watchpoints_to_thread(&self, thread_id: u32) -> Result<()> {
        let Some(proc) = self.process.as_ref() else {
            return Ok(());
        };
        let (dr0, dr1, dr2, dr3, dr7) = self.watchpoints.dr7_and_addrs();
        proc.set_debug_registers(thread_id, dr0, dr1, dr2, dr3, dr7)
    }

    // Reads DR6 on the thread that just single-stepped/trapped and reports
    // which watchpoint slots (if any) it shows as hit, clearing DR6
    // afterward so the same hit doesn't appear to still be pending next
    // time.
    // Never propagates a hard error: this runs on every single-step event
    // regardless of which thread raised it, and a thread that denies
    // GetContext/SetContext (the same rare case `lock` has to tolerate)
    // must not abort this match arm before the mandatory ContinueDebugEvent
    // call at the bottom of the loop runs, or the debug loop desyncs
    // forever (see the CREATE_THREAD_DEBUG_EVENT handler for the same
    // caveat in more detail).
    fn take_watchpoint_hits(&mut self, thread_id: u32) -> Vec<usize> {
        if self.watchpoints.list().next().is_none() {
            return Vec::new();
        }
        let Some(proc) = self.process.as_ref() else {
            return Vec::new();
        };
        let dr6 = match proc.get_debug_registers(thread_id) {
            Ok(ctx) => ctx.Dr6,
            Err(e) => {
                eprintln!("Warning: could not read debug registers on thread {}: {}", thread_id, e);
                return Vec::new();
            }
        };
        let mask = (dr6 & 0xF) as u8;
        if mask == 0 {
            return Vec::new();
        }
        let hits = self.watchpoints.slots_hit(mask);
        let (dr0, dr1, dr2, dr3, dr7) = self.watchpoints.dr7_and_addrs();
        if let Err(e) = proc.set_debug_registers(thread_id, dr0, dr1, dr2, dr3, dr7) {
            eprintln!("Warning: could not clear debug registers on thread {}: {}", thread_id, e);
        }
        hits
    }

    // Prints old/new (or current) values for each hit watchpoint slot and
    // the stopped location, then updates each watchpoint's cached value.
    fn report_watchpoints(&mut self, slots: &[usize], stop_addr: usize) {
        let Some(proc) = self.process.as_ref() else {
            return;
        };
        let Ok(ctx) = proc.get_full_thread_context_x64(self.last_thread_id) else {
            return;
        };
        let eval_ctx = DebugEvalContext {
            proc,
            symbols: self.symbols.as_ref(),
            thread_id: self.last_thread_id,
            ctx,
        };

        for &slot in slots {
            let Some(wp) = self.watchpoints.get_by_slot(slot) else {
                continue;
            };
            let new_value = eval::parse(&wp.expr)
                .ok()
                .and_then(|expr| eval::eval(&expr, &eval_ctx).ok())
                .map(|v| v.as_i64());

            println!();
            match wp.kind {
                WatchKind::Write => {
                    println!("Watchpoint #{} hit: {}", wp.id, wp.expr);
                    if let Some(old) = wp.last_value {
                        println!("Old value = {}", old);
                    }
                    match new_value {
                        Some(new) => println!("New value = {}", new),
                        // The watched address's original expression no
                        // longer resolves from here, e.g. a stack local
                        // whose slot was reused after its frame returned.
                        None => println!(
                            "(current value unavailable: '{}' is not in scope here)",
                            wp.expr
                        ),
                    }
                }
                WatchKind::Access => {
                    println!("Watchpoint #{} hit (read/write): {}", wp.id, wp.expr);
                    match new_value {
                        Some(v) => println!("Value = {}", v),
                        None => println!(
                            "(current value unavailable: '{}' is not in scope here)",
                            wp.expr
                        ),
                    }
                }
            }

            if let Some(wp) = self.watchpoints.get_by_slot_mut(slot) {
                wp.last_value = new_value;
            }
        }

        println!(
            "Stopped at {:#x}{}",
            stop_addr,
            describe_address(self.symbols.as_ref(), stop_addr)
        );
        if let Some(proc) = self.process.as_ref() {
            show_context(self.symbols.as_ref(), proc, stop_addr);
        }
    }

    // All Win32 debug events freeze the whole process until ContinueDebugEvent
    // is called, so every known thread is genuinely halted here and safe to
    // inspect regardless of which one raised the last event.
    fn list_threads(&self) {
        let Some(proc) = self.process.as_ref() else {
            eprintln!("No process is running.");
            return;
        };
        if self.threads.is_empty() {
            println!("No threads.");
            return;
        }
        let mut ids: Vec<u32> = self.threads.iter().copied().collect();
        ids.sort();
        for tid in ids {
            let marker = if tid == self.last_thread_id { "*" } else { " " };
            let main_tag = if tid == proc.main_thread_id { " (main)" } else { "" };
            let lock_tag = if self.locked_thread == Some(tid) {
                " (locked)"
            } else if self.frozen_threads.contains(&tid) {
                " (frozen)"
            } else {
                ""
            };
            match proc.get_thread_context_x64(tid) {
                Ok(ctx) => {
                    let rip = ctx.Rip as usize;
                    println!(
                        "{} {}{}{}  {:#x}{}",
                        marker,
                        tid,
                        main_tag,
                        lock_tag,
                        rip,
                        describe_address(self.symbols.as_ref(), rip)
                    );
                }
                Err(_) => println!("{} {}{}{}  <context unavailable>", marker, tid, main_tag, lock_tag),
            }
        }
    }

    // Changes which thread subsequent regs/bt/list/print/show/step commands
    // operate on. Doesn't touch execution state: the next continue/step still
    // resumes the whole process, and the next debug event will move
    // last_thread_id again regardless of this selection.
    fn switch_thread(&mut self, id: u32) -> Result<()> {
        let Some(proc) = self.process.as_ref() else {
            eprintln!("No process is running.");
            return Ok(());
        };
        if !self.threads.contains(&id) {
            eprintln!("No such thread: {}", id);
            return Ok(());
        }
        self.last_thread_id = id;
        match proc.get_thread_context_x64(id) {
            Ok(ctx) => {
                let addr = ctx.Rip as usize;
                println!(
                    "Switched to thread {}: {:#x}{}",
                    id,
                    addr,
                    describe_address(self.symbols.as_ref(), addr)
                );
            }
            // Some threads (e.g. certain OS-internal worker threads) refuse
            // GetThreadContext/SuspendThread even to the active debugger;
            // the switch itself (which commands operate on) still succeeds.
            Err(e) => println!("Switched to thread {} (context unavailable: {})", id, e),
        }
        Ok(())
    }

    // Freezes every known thread except `id` (the current thread if not
    // given), so that only it actually runs on the next continue/step; the
    // rest stay suspended even though ContinueDebugEvent lets the process
    // as a whole proceed. Switching the lock to a different thread thaws
    // the previous one and freezes the new one instead.
    //
    // Suspending/resuming is best-effort per thread: some threads (certain
    // OS-internal worker threads) deny SUSPEND_RESUME access even to the
    // active debugger, and one such thread must not stop `lock` from
    // freezing every other thread it *can* reach.
    fn lock_thread(&mut self, id: Option<u32>) -> Result<()> {
        let Some(proc) = self.process.as_ref() else {
            eprintln!("No process is running.");
            return Ok(());
        };
        let target = id.unwrap_or(self.last_thread_id);
        if !self.threads.contains(&target) {
            eprintln!("No such thread: {}", target);
            return Ok(());
        }

        match self.locked_thread {
            Some(current) if current == target => {
                eprintln!("Already locked to thread {}.", target);
                return Ok(());
            }
            Some(current) => {
                // Retarget: `target` was frozen and must be thawed;
                // `current` now joins the freeze in its place. Threads that
                // arrived after the original lock (never frozen, see
                // frozen_threads's doc comment) are left exactly as they are.
                if self.frozen_threads.remove(&target) {
                    if let Err(e) = proc.resume_thread(target) {
                        eprintln!("Warning: could not thaw thread {}: {}", target, e);
                    }
                }
                match proc.suspend_thread(current) {
                    Ok(()) => {
                        self.frozen_threads.insert(current);
                    }
                    Err(e) => eprintln!("Warning: could not freeze thread {}: {}", current, e),
                }
            }
            None => {
                for &tid in &self.threads {
                    if tid == target {
                        continue;
                    }
                    match proc.suspend_thread(tid) {
                        Ok(()) => {
                            self.frozen_threads.insert(tid);
                        }
                        Err(e) => eprintln!("Warning: could not freeze thread {}: {}", tid, e),
                    }
                }
            }
        }

        self.locked_thread = Some(target);
        self.last_thread_id = target;
        println!(
            "Locked to thread {}; all other threads frozen until 'unlock'.",
            target
        );
        Ok(())
    }

    fn unlock_thread(&mut self) -> Result<()> {
        let Some(locked) = self.locked_thread.take() else {
            eprintln!("Not locked to a thread.");
            return Ok(());
        };
        if let Some(proc) = self.process.as_ref() {
            for tid in self.frozen_threads.drain() {
                // Best-effort: a frozen thread may have already exited or
                // otherwise be unreachable; either way that's not a reason
                // to skip thawing the rest.
                let _ = proc.resume_thread(tid);
            }
        } else {
            self.frozen_threads.clear();
        }
        println!("Unlocked thread {}; all threads resumed.", locked);
        Ok(())
    }

    fn list_source(&mut self, target: Option<&str>) -> Result<()> {
        if self.process.is_none() {
            eprintln!("No process is running.");
            return Ok(());
        }

        let address = match target {
            Some(target) => match self.resolve_target_address(target)? {
                Some(address) => address,
                None => return Ok(()),
            },
            None => {
                let proc = self.process.as_ref().unwrap();
                proc.get_thread_context_x64(self.last_thread_id)?.Rip as usize
            }
        };

        let proc = self.process.as_ref().unwrap();
        show_context_with_window(self.symbols.as_ref(), proc, address, 5, 5);
        Ok(())
    }

    fn make_eval_context(&self) -> Result<Option<DebugEvalContext<'_>>> {
        let Some(proc) = self.process.as_ref() else {
            eprintln!("No process is running.");
            return Ok(None);
        };
        let ctx = proc.get_full_thread_context_x64(self.last_thread_id)?;
        Ok(Some(DebugEvalContext {
            proc,
            symbols: self.symbols.as_ref(),
            thread_id: self.last_thread_id,
            ctx,
        }))
    }

    fn print_expr(&mut self, raw: &str, fmt: Option<char>) -> Result<()> {
        let expr = match eval::parse(raw) {
            Ok(expr) => expr,
            Err(e) => {
                eprintln!("{}", e);
                return Ok(());
            }
        };
        let Some(ctx) = self.make_eval_context()? else {
            return Ok(());
        };

        // /s reads memory as a C string, an entirely different operation
        // from formatting an already-evaluated scalar/aggregate value.
        if fmt == Some('s') {
            match self.print_string(&expr, &ctx) {
                Ok(text) => println!("{} = {}", raw.trim(), text),
                Err(e) => eprintln!("{}", e),
            }
            return Ok(());
        }

        match self.format_print_expr(&expr, &ctx, fmt) {
            Ok(text) => println!("{} = {}", raw.trim(), text),
            Err(e) => eprintln!("{}", e),
        }
        Ok(())
    }

    // Struct/array-typed lvalues get pretty-printed per `set print`
    // settings; everything else (literals, arithmetic results, scalars)
    // falls back to plain scalar formatting. `fmt` (a GDB print format
    // letter) overrides the default rendering of every scalar encountered,
    // including inside a struct/array.
    fn format_print_expr(
        &self,
        expr: &eval::Expr,
        ctx: &DebugEvalContext,
        fmt: Option<char>,
    ) -> std::result::Result<String, String> {
        let loc = eval::eval_location(expr, ctx).ok();
        let ty = match &loc {
            Some(eval::Location::Memory(tl)) => tl.ty,
            _ => None,
        };

        if let Some(eval::Location::Memory(tl)) = &loc {
            if let Some(ty) = ty {
                if let Some(text) = format_aggregate(
                    &self.print_settings,
                    self.symbols.as_ref(),
                    ctx,
                    ty,
                    tl.address,
                    0,
                    fmt,
                ) {
                    return Ok(with_type_prefix(&self.print_settings, self.symbols.as_ref(), ty, text));
                }
            }
        }

        let value = eval::eval(expr, ctx)?;
        let text = format_scalar(
            &self.print_settings,
            self.symbols.as_ref(),
            ctx,
            value,
            loc.as_ref(),
            fmt,
        );
        Ok(match ty {
            Some(ty) => with_type_prefix(&self.print_settings, self.symbols.as_ref(), ty, text),
            None => text,
        })
    }

    // Resolves `expr` to an address to read a C string from: a pointer's
    // value, a char array's own address, or (as a last resort) the
    // expression's raw numeric value treated as an address.
    fn print_string(
        &self,
        expr: &eval::Expr,
        ctx: &DebugEvalContext,
    ) -> std::result::Result<String, String> {
        let loc = eval::eval_location(expr, ctx).ok();

        let address = match &loc {
            Some(eval::Location::Memory(tl)) => match tl.ty.and_then(|ty| {
                self.symbols
                    .as_ref()
                    .and_then(|s| s.type_tag(ty.0, ty.1))
            }) {
                Some(symbols::SYM_TAG_ARRAY_TYPE) => tl.address,
                _ => ctx
                    .read_typed(*tl)
                    .ok_or_else(|| format!("cannot read memory at {:#x}", tl.address))?
                    .as_i64() as u64,
            },
            _ => eval::eval(expr, ctx)?.as_i64() as u64,
        };

        let Some(proc) = self.process.as_ref() else {
            return Err("No process is running.".to_string());
        };
        let max_len = self.print_settings.elements.min(4096).max(1);
        match read_c_string(proc, address, max_len) {
            Some(s) => Ok(format!("{:#x} \"{}\"", address, escape_c_string(&s))),
            None => Ok(format!("{:#x} <unreadable string>", address)),
        }
    }

    fn set_print_option(&mut self, option: commands::PrintOption) {
        match option {
            commands::PrintOption::Pretty(v) => self.print_settings.pretty = v,
            commands::PrintOption::Address(v) => self.print_settings.address = v,
            commands::PrintOption::Elements(v) => self.print_settings.elements = v.unwrap_or(usize::MAX),
            commands::PrintOption::Repeats(v) => self.print_settings.repeats = v.unwrap_or(usize::MAX),
        }
    }

    fn show_print_settings(&self) {
        let onoff = |b: bool| if b { "on" } else { "off" };
        let limit = |n: usize| {
            if n == usize::MAX {
                "unlimited".to_string()
            } else {
                n.to_string()
            }
        };
        println!("print pretty:   {}", onoff(self.print_settings.pretty));
        println!("print address:  {}", onoff(self.print_settings.address));
        println!("print elements: {}", limit(self.print_settings.elements));
        println!("print repeats:  {}", limit(self.print_settings.repeats));
    }

    fn set_expr(&mut self, raw: &str) -> Result<()> {
        let Some((lhs, rhs)) = eval::split_assignment(raw) else {
            eprintln!("Invalid assignment (expected <lvalue> = <expr>): {}", raw);
            return Ok(());
        };
        let (lhs, rhs) = (lhs.trim(), rhs.trim());

        let lhs_expr = match eval::parse(lhs) {
            Ok(expr) => expr,
            Err(e) => {
                eprintln!("{}", e);
                return Ok(());
            }
        };
        let rhs_expr = match eval::parse(rhs) {
            Ok(expr) => expr,
            Err(e) => {
                eprintln!("{}", e);
                return Ok(());
            }
        };

        let Some(ctx) = self.make_eval_context()? else {
            return Ok(());
        };

        let value = match eval::eval(&rhs_expr, &ctx) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{}", e);
                return Ok(());
            }
        };
        let loc = match eval::eval_location(&lhs_expr, &ctx) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{}", e);
                return Ok(());
            }
        };

        let ok = match &loc {
            eval::Location::Memory(tl) => ctx.write_typed(*tl, value),
            eval::Location::Register(name) => ctx.write_register(name, value),
        };

        if ok {
            println!("{} = {}", lhs, value);
        } else {
            eprintln!("Failed to write to {}", lhs);
        }
        Ok(())
    }

    fn show_vars(&mut self, kind: commands::ShowKind) -> Result<()> {
        if self.process.is_none() {
            eprintln!("No process is running.");
            return Ok(());
        }
        let Some(sym_resolver) = self.symbols.as_ref() else {
            eprintln!("No symbols loaded.");
            return Ok(());
        };

        let vars = match kind {
            commands::ShowKind::Globals => {
                let source_file = sym_resolver.main_source_file();
                sym_resolver.enum_globals(source_file.as_deref())?
            }
            commands::ShowKind::Locals | commands::ShowKind::Args => {
                let proc = self.process.as_ref().unwrap();
                let ctx = proc.get_full_thread_context_x64(self.last_thread_id)?;
                let want_param = kind == commands::ShowKind::Args;
                sym_resolver
                    .enum_locals(&ctx)?
                    .into_iter()
                    .filter(|v| v.is_param == want_param)
                    .collect()
            }
        };

        if vars.is_empty() {
            println!("(none)");
            return Ok(());
        }

        let Some(eval_ctx) = self.make_eval_context()? else {
            return Ok(());
        };

        for var in vars {
            let meta = type_meta(sym_resolver, var.mod_base, var.type_id);
            let loc = eval::TypedLocation {
                address: var.address,
                size: meta.size,
                kind: meta.kind,
                ty: meta.ty,
            };

            let text = match meta
                .ty
                .and_then(|ty| format_aggregate(&self.print_settings, Some(sym_resolver), &eval_ctx, ty, var.address, 0, None))
            {
                Some(agg_text) => with_type_prefix(&self.print_settings, Some(sym_resolver), meta.ty.unwrap(), agg_text),
                None => match eval_ctx.read_typed(loc) {
                    Some(value) => {
                        let text = format_scalar(
                            &self.print_settings,
                            Some(sym_resolver),
                            &eval_ctx,
                            value,
                            Some(&eval::Location::Memory(loc)),
                            None,
                        );
                        match meta.ty {
                            Some(ty) => with_type_prefix(&self.print_settings, Some(sym_resolver), ty, text),
                            None => text,
                        }
                    }
                    None => format!("<unreadable> ({:#x})", var.address),
                },
            };
            println!("{} = {}", var.name, text);
        }
        Ok(())
    }

    fn dump_memory(&self, address_str: &str, count: usize) -> Result<()> {
        let address = commands::parse_address(address_str)?;
        if let Some(proc) = self.process.as_ref() {
            let mut buf = vec![0u8; count];
            let read = proc.read_memory(address, &mut buf)?;
            print_memory(address, &buf[..read]);
        } else {
            eprintln!("No process is running.");
        }
        Ok(())
    }

    fn continue_execution(&mut self) -> Result<()> {
        if self.process.is_none() {
            return Err(DebuggerError::NotAttached);
        }

        if let Some((pid, tid, status)) = self.pending_continue.take() {
            self.process.as_ref().unwrap().continue_event(pid, tid, status)?;
        }

        self.running = true;
        loop {
            if process::take_interrupt_requested() {
                if let Some(proc) = self.process.as_ref() {
                    let _ = proc.interrupt();
                }
            }

            let Some(event) = self.process.as_ref().unwrap().wait_for_event(100)? else {
                // Timed out with no event: the debuggee is just running.
                // Loop back around so the interrupt-flag check above keeps
                // getting a chance to run instead of returning and going
                // silent until the next command.
                continue;
            };
            self.last_event = Some(event);
            self.last_thread_id = event.dwThreadId;

            match event.dwDebugEventCode {
                EXCEPTION_DEBUG_EVENT => {
                    let info = unsafe { event.u.Exception };
                    let code = info.ExceptionRecord.ExceptionCode;
                    let address = info.ExceptionRecord.ExceptionAddress as usize;

                    if code.0 as u32 == STATUS_BREAKPOINT {
                        if let Some(pending) = self.pending_leak {
                            if address == pending.return_addr {
                                let proc = self.process.as_ref().unwrap();
                                proc.remove_breakpoint(pending.return_addr, pending.return_original_byte)?;
                                proc.set_rip(event.dwThreadId, pending.return_addr as u64)?;

                                if matches!(pending.kind, LeakHookKind::Malloc | LeakHookKind::Calloc) {
                                    // RAX is part of CONTEXT_INTEGER, not the
                                    // CONTEXT_CONTROL that get_thread_context_x64
                                    // requests, so it must be read via the full
                                    // context or it comes back as always-zero.
                                    let ctx = proc.get_full_thread_context_x64(event.dwThreadId)?;
                                    if ctx.Rax != 0 {
                                        self.allocations.insert(ctx.Rax, pending.arg);
                                    }
                                }

                                let new_byte = proc.set_breakpoint(pending.hook_addr)?;
                                self.leak_hooks.insert(pending.hook_addr, (pending.kind, new_byte));
                                self.pending_leak = None;
                                proc.continue_event(event.dwProcessId, event.dwThreadId, DBG_CONTINUE)?;
                                continue;
                            }
                        }

                        if let Some(&(kind, original_byte)) = self.leak_hooks.get(&address) {
                            let proc = self.process.as_ref().unwrap();
                            proc.remove_breakpoint(address, original_byte)?;
                            proc.set_rip(event.dwThreadId, address as u64)?;

                            let full_ctx = proc.get_full_thread_context_x64(event.dwThreadId)?;
                            let arg = match kind {
                                LeakHookKind::Malloc => full_ctx.Rcx as usize,
                                LeakHookKind::Calloc => {
                                    (full_ctx.Rcx as usize).saturating_mul(full_ctx.Rdx as usize)
                                }
                                LeakHookKind::Free => full_ctx.Rcx as usize,
                            };
                            if kind == LeakHookKind::Free {
                                self.allocations.remove(&(arg as u64));
                            }

                            // malloc/calloc need their return value (the new
                            // pointer), so find where this call returns to
                            // and capture RAX there; free's argument is all
                            // we need.
                            let return_addr = if kind != LeakHookKind::Free {
                                self.symbols.as_ref().and_then(|symbols| {
                                    let mut unwind_ctx = full_ctx;
                                    let thread_handle = proc.thread_handle(event.dwThreadId).ok()?;
                                    let addr = symbols
                                        .unwind_return_address(thread_handle, &mut unwind_ctx)
                                        .ok()
                                        .flatten();
                                    unsafe {
                                        let _ = windows::Win32::Foundation::CloseHandle(thread_handle);
                                    }
                                    addr
                                })
                            } else {
                                None
                            };

                            match return_addr {
                                Some(return_addr) => {
                                    let return_addr = return_addr as usize;
                                    let return_original_byte = proc.set_breakpoint(return_addr)?;
                                    self.pending_leak = Some(PendingLeak {
                                        kind,
                                        arg,
                                        hook_addr: address,
                                        return_addr,
                                        return_original_byte,
                                    });
                                    proc.continue_event(event.dwProcessId, event.dwThreadId, DBG_CONTINUE)?;
                                }
                                None => {
                                    // No return value to wait for (free), or
                                    // unwinding failed: the original
                                    // instruction at `address` was already
                                    // restored above, so step over it once
                                    // before re-arming the hook, or the CPU
                                    // would trap on the same INT3 forever
                                    // without ever making progress.
                                    self.pending_leak_rearm = Some((address, kind));
                                    proc.single_step(event.dwThreadId)?;
                                    proc.continue_event(event.dwProcessId, event.dwThreadId, DBG_CONTINUE)?;
                                }
                            }
                            continue;
                        }

                        if let Some((bp_addr, original_byte)) = self.finish_bp {
                            if address == bp_addr {
                                let proc = self.process.as_ref().unwrap();
                                proc.remove_breakpoint(bp_addr, original_byte)?;
                                proc.set_rip(event.dwThreadId, bp_addr as u64)?;
                                self.finish_bp = None;

                                println!(
                                    "\nReturned to {:#x}{}",
                                    bp_addr,
                                    describe_address(self.symbols.as_ref(), bp_addr)
                                );
                                if let Some(proc) = self.process.as_ref() {
                                    show_context(self.symbols.as_ref(), proc, bp_addr);
                                }
                                self.running = false;
                                self.pending_continue =
                                    Some((event.dwProcessId, event.dwThreadId, DBG_CONTINUE));
                                return Ok(());
                            }
                        }

                        if let Some((bp_addr, original_byte)) = self.step_over_bp {
                            if address == bp_addr {
                                let proc = self.process.as_ref().unwrap();
                                proc.remove_breakpoint(bp_addr, original_byte)?;
                                proc.set_rip(event.dwThreadId, bp_addr as u64)?;
                                self.step_over_bp = None;

                                if self.should_keep_stepping(bp_addr as u64) {
                                    self.arm_step()?;
                                    self.process.as_ref().unwrap().continue_event(
                                        event.dwProcessId,
                                        event.dwThreadId,
                                        DBG_CONTINUE,
                                    )?;
                                    continue;
                                }

                                self.step_target = None;
                                println!(
                                    "\nStep at {:#x}{}",
                                    bp_addr,
                                    describe_address(self.symbols.as_ref(), bp_addr)
                                );
                                if let Some(proc) = self.process.as_ref() {
                                    show_context(self.symbols.as_ref(), proc, bp_addr);
                                }
                                self.running = false;
                                self.pending_continue =
                                    Some((event.dwProcessId, event.dwThreadId, DBG_CONTINUE));
                                return Ok(());
                            }
                        }

                        if let Some(bp) = self.breakpoints.get_mut(address) {
                            self.step_target = None;
                            self.step_over_bp = None;
                            self.finish_bp = None;
                            self.pending_leak = None;
                            self.pending_leak_rearm = None;
                            println!(
                                "\nBreakpoint hit at {:#x}{}",
                                address,
                                describe_address(self.symbols.as_ref(), address)
                            );
                            let proc = self.process.as_ref().unwrap();
                            proc.remove_breakpoint(address, bp.original_byte)?;
                            bp.enabled = false;
                            show_context(self.symbols.as_ref(), proc, address);
                            proc.set_rip(event.dwThreadId, address as u64)?;
                            self.pending_breakpoint = Some(address);
                            proc.single_step(event.dwThreadId)?;
                            self.running = false;
                            proc.continue_event(event.dwProcessId, event.dwThreadId, DBG_CONTINUE)?;
                            // Don't return here: this continue only steps over the
                            // restored instruction and re-arms the INT3. The
                            // resulting SINGLE_STEP event must be reaped by this
                            // same call, or it's left unclaimed and the next
                            // step/continue command wrongly picks it up instead of
                            // its own event, making step/next appear to lag by one.
                            continue;
                        }
                    } else if code.0 as u32 == STATUS_SINGLE_STEP {
                        // A hardware watchpoint trap also reports as
                        // STATUS_SINGLE_STEP (both are INT1/#DB), so DR6 must
                        // be inspected to tell a watchpoint hit apart from
                        // one of this file's own single-instruction steps.
                        let watch_hits = self.take_watchpoint_hits(event.dwThreadId);

                        if let Some((hook_addr, kind)) = self.pending_leak_rearm.take() {
                            let proc = self.process.as_ref().unwrap();
                            let new_byte = proc.set_breakpoint(hook_addr)?;
                            self.leak_hooks.insert(hook_addr, (kind, new_byte));
                            proc.continue_event(event.dwProcessId, event.dwThreadId, DBG_CONTINUE)?;
                            continue;
                        }

                        let was_breakpoint_rearm = self.pending_breakpoint.is_some();
                        if let Some(addr) = self.pending_breakpoint.take() {
                            let proc = self.process.as_ref().unwrap();
                            if let Some(bp) = self.breakpoints.get_mut(addr) {
                                let original = proc.set_breakpoint(addr)?;
                                bp.original_byte = original;
                                bp.enabled = true;
                            }
                        }
                        if was_breakpoint_rearm {
                            if !watch_hits.is_empty() {
                                self.step_target = None;
                                self.report_watchpoints(&watch_hits, address);
                            }
                            self.running = false;
                            self.pending_continue =
                                Some((event.dwProcessId, event.dwThreadId, DBG_CONTINUE));
                            return Ok(());
                        }

                        if !watch_hits.is_empty() {
                            self.step_target = None;
                            self.report_watchpoints(&watch_hits, address);
                            self.running = false;
                            self.pending_continue =
                                Some((event.dwProcessId, event.dwThreadId, DBG_CONTINUE));
                            return Ok(());
                        }

                        if self.should_keep_stepping(address as u64) {
                            self.arm_step()?;
                            self.process.as_ref().unwrap().continue_event(
                                event.dwProcessId,
                                event.dwThreadId,
                                DBG_CONTINUE,
                            )?;
                            continue;
                        }

                        self.step_target = None;
                        println!(
                            "\nStep at {:#x}{}",
                            address,
                            describe_address(self.symbols.as_ref(), address)
                        );
                        if let Some(proc) = self.process.as_ref() {
                            show_context(self.symbols.as_ref(), proc, address);
                        }
                        self.running = false;
                        self.pending_continue =
                            Some((event.dwProcessId, event.dwThreadId, DBG_CONTINUE));
                        return Ok(());
                    } else if code.0 as u32 == DBG_CONTROL_C {
                        // This fires on a thread Windows injected into the
                        // debuggee just to run its Ctrl+C handler, not on
                        // whatever thread the user's own code was running
                        // on, so `address` here is meaningless. The whole
                        // process is halted regardless of which thread
                        // raised the event, so show the main thread's RIP
                        // instead, and point later commands (step/regs/...)
                        // at it rather than the throwaway handler thread.
                        self.step_target = None;
                        self.step_over_bp = None;
                        self.finish_bp = None;
                        self.pending_leak = None;
                        self.pending_leak_rearm = None;
                        println!("\nInterrupted (Ctrl+C)");
                        if let Some(proc) = self.process.as_ref() {
                            if let Ok(ctx) = proc.get_thread_context_x64(proc.main_thread_id) {
                                let addr = ctx.Rip as usize;
                                self.last_thread_id = proc.main_thread_id;
                                println!(
                                    "Stopped at {:#x}{}",
                                    addr,
                                    describe_address(self.symbols.as_ref(), addr)
                                );
                                show_context(self.symbols.as_ref(), proc, addr);
                            }
                        }
                        self.running = false;
                        self.pending_continue = Some((
                            event.dwProcessId,
                            event.dwThreadId,
                            DBG_EXCEPTION_NOT_HANDLED,
                        ));
                        return Ok(());
                    }

                    self.step_target = None;
                    self.step_over_bp = None;
                    self.finish_bp = None;
                    self.pending_leak = None;
                    self.pending_leak_rearm = None;
                    println!(
                        "\nException {:#x} at {:#x}{}",
                        code.0,
                        address,
                        describe_address(self.symbols.as_ref(), address)
                    );
                    if let Some(proc) = self.process.as_ref() {
                        show_context(self.symbols.as_ref(), proc, address);
                    }
                    self.running = false;
                    self.pending_continue =
                        Some((event.dwProcessId, event.dwThreadId, DBG_EXCEPTION_NOT_HANDLED));
                    return Ok(());
                }
                CREATE_PROCESS_DEBUG_EVENT => {
                    println!("Process created: {}", event.dwProcessId);
                    self.threads.insert(event.dwThreadId);
                    unsafe {
                        let info = event.u.CreateProcessInfo;
                        if !info.hFile.is_invalid() {
                            if let Some(symbols) = self.symbols.as_ref() {
                                match symbols.load_module(info.hFile, info.lpBaseOfImage as u64) {
                                    Ok(base) => println!("Symbols loaded at {:#x}", base),
                                    Err(e) => eprintln!("Failed to load symbols: {}", e),
                                }
                            }
                            let _ = windows::Win32::Foundation::CloseHandle(info.hFile);
                        }
                    }
                }
                EXIT_PROCESS_DEBUG_EVENT => {
                    println!("\nProcess exited with code {}", unsafe {
                        event.u.ExitProcess.dwExitCode
                    });
                    self.running = false;
                    // Without this, self.process stays Some after the
                    // debuggee is fully gone, so a later `continue` would
                    // pass every "is a process running?" guard and then
                    // spin in wait_for_event forever, since a fully exited
                    // process can never produce another debug event.
                    self.shutdown()?;
                    return Ok(());
                }
                CREATE_THREAD_DEBUG_EVENT => {
                    println!("Thread created: {}", event.dwThreadId);
                    self.threads.insert(event.dwThreadId);
                    // Debug registers are per-thread and start zeroed, so a
                    // new thread must be given the currently active
                    // watchpoints explicitly or it simply won't trap on them.
                    //
                    // Must not use `?` here: some threads (certain
                    // OS-internal worker threads) deny GetContext/SetContext
                    // even to the active debugger, and propagating that
                    // error would abort this match arm before the
                    // mandatory ContinueDebugEvent call at the bottom of
                    // the loop runs, permanently desyncing the debug loop
                    // (the debuggee stays halted forever, since Windows
                    // requires that call to acknowledge every debug event
                    // before the process can run again).
                    if let Err(e) = self.apply_watchpoints_to_thread(event.dwThreadId) {
                        eprintln!(
                            "Warning: could not sync watchpoints to thread {}: {}",
                            event.dwThreadId, e
                        );
                    }
                    // Deliberately NOT auto-frozen even while locked: see
                    // frozen_threads's doc comment on the Debugger struct.
                }
                EXIT_THREAD_DEBUG_EVENT => {
                    println!("Thread exited: {}", event.dwThreadId);
                    self.threads.remove(&event.dwThreadId);
                    self.frozen_threads.remove(&event.dwThreadId);
                    if self.locked_thread == Some(event.dwThreadId) {
                        // The thread `lock` was keeping everyone else frozen
                        // for is gone: every other thread must be thawed
                        // here, or they stay suspended forever with no
                        // `unlock` able to reach them (locked_thread already
                        // being cleared makes unlock_thread() a no-op).
                        if let Some(proc) = self.process.as_ref() {
                            for tid in self.frozen_threads.drain() {
                                let _ = proc.resume_thread(tid);
                            }
                        }
                        self.locked_thread = None;
                        eprintln!(
                            "Locked thread {} exited; lock released and other threads unfrozen.",
                            event.dwThreadId
                        );
                    }
                }
                LOAD_DLL_DEBUG_EVENT => unsafe {
                    let info = event.u.LoadDll;
                    if !info.hFile.is_invalid() {
                        if let Some(symbols) = self.symbols.as_ref() {
                            let _ = symbols.load_module(info.hFile, info.lpBaseOfDll as u64);
                        }
                        let _ = windows::Win32::Foundation::CloseHandle(info.hFile);
                    }
                },
                UNLOAD_DLL_DEBUG_EVENT => {}
                OUTPUT_DEBUG_STRING_EVENT => {}
                RIP_EVENT => {}
                _ => {}
            }

            self.process
                .as_ref()
                .unwrap()
                .continue_event(event.dwProcessId, event.dwThreadId, DBG_CONTINUE)?;
        }
    }
}

// Bridges the pure eval::EvalContext trait to the debuggee: variable() reads
// dbghelp for the current stack frame, memory reads/writes go through
// ReadProcessMemory/WriteProcessMemory, and register writes are committed
// immediately via SetThreadContext (the thread is halted, so this is safe).
struct DebugEvalContext<'a> {
    proc: &'a DebuggeeProcess,
    symbols: Option<&'a SymbolResolver>,
    thread_id: u32,
    ctx: AlignedContext,
}

fn sign_extend(raw: u64, size: u32) -> i64 {
    match size {
        1 => (raw as u8) as i8 as i64,
        2 => (raw as u16) as i16 as i64,
        4 => (raw as u32) as i32 as i64,
        _ => raw as i64,
    }
}

// Resolves through SymTagTypedef chains and classifies the underlying type
// as int- or float-shaped, so memory reads/writes know how to interpret the
// bytes (pointers read/write as plain addresses, i.e. Int).
fn classify_kind(symbols: &SymbolResolver, mod_base: u64, type_id: u32) -> eval::ValueKind {
    let mut tid = type_id;
    for _ in 0..8 {
        match symbols.type_tag(mod_base, tid) {
            Some(symbols::SYM_TAG_TYPEDEF) => match symbols.type_pointee(mod_base, tid) {
                Some(inner) => {
                    tid = inner;
                    continue;
                }
                None => return eval::ValueKind::Int,
            },
            Some(symbols::SYM_TAG_BASE_TYPE) => {
                return match symbols.type_base_type(mod_base, tid) {
                    Some(symbols::BASE_TYPE_FLOAT) => eval::ValueKind::Float,
                    _ => eval::ValueKind::Int,
                };
            }
            _ => return eval::ValueKind::Int,
        }
    }
    eval::ValueKind::Int
}

// Whether a type is char/signed char/unsigned char (any of which a `char *`
// GDB-style debugger treats as "probably a string" for default printing),
// resolved through typedefs the same way classify_kind is.
fn is_char_type(symbols: &SymbolResolver, mod_base: u64, type_id: u32) -> bool {
    const BASE_TYPE_CHAR: u32 = 2;
    const BASE_TYPE_INT: u32 = 6;
    const BASE_TYPE_UINT: u32 = 7;

    let mut tid = type_id;
    for _ in 0..8 {
        match symbols.type_tag(mod_base, tid) {
            Some(symbols::SYM_TAG_TYPEDEF) => match symbols.type_pointee(mod_base, tid) {
                Some(inner) => {
                    tid = inner;
                    continue;
                }
                None => return false,
            },
            Some(symbols::SYM_TAG_BASE_TYPE) => {
                let bt = symbols.type_base_type(mod_base, tid).unwrap_or(0);
                let size = symbols.type_size(mod_base, tid);
                return bt == BASE_TYPE_CHAR || (matches!(bt, BASE_TYPE_INT | BASE_TYPE_UINT) && size == 1);
            }
            _ => return false,
        }
    }
    false
}

fn type_meta(symbols: &SymbolResolver, mod_base: u64, type_id: u32) -> eval::TypeMeta {
    let size = match symbols.type_size(mod_base, type_id) {
        0 => 4,
        n => n,
    };
    eval::TypeMeta {
        size,
        kind: classify_kind(symbols, mod_base, type_id),
        ty: if type_id == 0 {
            None
        } else {
            Some(eval::TypeHandle(mod_base, type_id))
        },
    }
}

impl eval::EvalContext for DebugEvalContext<'_> {
    fn read_typed(&self, loc: eval::TypedLocation) -> Option<eval::Value> {
        let n = (loc.size.min(8)) as usize;
        let mut buf = [0u8; 8];
        let read = self.proc.read_memory(loc.address as usize, &mut buf[..n]).ok()?;
        if read < n {
            return None;
        }
        Some(match loc.kind {
            eval::ValueKind::Float if loc.size == 4 => {
                eval::Value::Float(f32::from_le_bytes(buf[..4].try_into().unwrap()) as f64)
            }
            eval::ValueKind::Float => eval::Value::Float(f64::from_le_bytes(buf)),
            eval::ValueKind::Int => eval::Value::Int(sign_extend(u64::from_le_bytes(buf), loc.size)),
        })
    }

    fn write_typed(&self, loc: eval::TypedLocation, value: eval::Value) -> bool {
        let n = (loc.size.min(8)) as usize;
        let bytes: [u8; 8] = match loc.kind {
            eval::ValueKind::Float if loc.size == 4 => {
                let mut b8 = [0u8; 8];
                b8[..4].copy_from_slice(&(value.as_f64() as f32).to_le_bytes());
                b8
            }
            eval::ValueKind::Float => value.as_f64().to_le_bytes(),
            eval::ValueKind::Int => value.as_i64().to_le_bytes(),
        };
        self.proc
            .write_memory(loc.address as usize, &bytes[..n])
            .is_ok_and(|written| written == n)
    }

    fn variable(&self, name: &str) -> Option<eval::Location> {
        let symbols = self.symbols?;
        let var = symbols.resolve_variable(&self.ctx, name).ok()?;
        let kind = classify_kind(symbols, var.mod_base, var.type_id);
        let ty = if var.type_id == 0 {
            None
        } else {
            Some(eval::TypeHandle(var.mod_base, var.type_id))
        };
        Some(eval::Location::Memory(eval::TypedLocation {
            address: var.address,
            size: var.size,
            kind,
            ty,
        }))
    }

    fn register(&self, name: &str) -> Option<eval::Value> {
        if let Some(f) = self.ctx.register_f64(name) {
            return Some(eval::Value::Float(f));
        }
        self.ctx.register(name).map(eval::Value::Int)
    }

    fn write_register(&self, name: &str, value: eval::Value) -> bool {
        let mut ctx = self.ctx;
        // Tries the float-valued registers (ST0-ST7) first, since those
        // need `value`'s fractional part rather than truncated-to-i64.
        let ok = ctx.set_register_f64(name, value.as_f64()) || ctx.set_register(name, value.as_i64());
        if !ok {
            return false;
        }
        self.proc.set_thread_context_x64(self.thread_id, &ctx).is_ok()
    }

    fn shape(&self, ty: eval::TypeHandle) -> eval::TypeShape {
        let Some(symbols) = self.symbols else {
            return eval::TypeShape::Other;
        };
        match symbols.type_tag(ty.0, ty.1) {
            Some(symbols::SYM_TAG_POINTER_TYPE) => eval::TypeShape::Pointer,
            Some(symbols::SYM_TAG_ARRAY_TYPE) => eval::TypeShape::Array,
            _ => eval::TypeShape::Other,
        }
    }

    fn element(&self, ty: eval::TypeHandle) -> Option<eval::TypeMeta> {
        let symbols = self.symbols?;
        let pointee_id = symbols.type_pointee(ty.0, ty.1)?;
        Some(type_meta(symbols, ty.0, pointee_id))
    }

    fn member(&self, ty: eval::TypeHandle, field: &str) -> Option<(u32, eval::TypeMeta)> {
        let symbols = self.symbols?;
        let (offset, member_type) = symbols.type_member(ty.0, ty.1, field)?;
        Some((offset, type_meta(symbols, ty.0, member_type)))
    }
}

fn parse_file_line(target: &str) -> Option<(&str, u32)> {
    let (file, line_str) = target.rsplit_once(':')?;
    let line: u32 = line_str.parse().ok()?;
    if file.is_empty() || line == 0 {
        return None;
    }
    Some((file, line))
}

// Struct/array pretty-printing for `print`/`show`, honoring `set print`
// settings. Returns None for scalar types, signaling the caller to fall
// back to format_scalar. `fmt` (a GDB print format letter) applies to every
// scalar leaf encountered while recursing.
fn format_aggregate(
    settings: &PrintSettings,
    symbols: Option<&SymbolResolver>,
    ctx: &DebugEvalContext,
    ty: eval::TypeHandle,
    address: u64,
    depth: usize,
    fmt: Option<char>,
) -> Option<String> {
    let symbols = symbols?;
    match symbols.type_tag(ty.0, ty.1)? {
        symbols::SYM_TAG_UDT => Some(format_struct(settings, symbols, ctx, ty, address, depth, fmt)),
        symbols::SYM_TAG_ARRAY_TYPE => {
            Some(format_array(settings, symbols, ctx, ty, address, depth, fmt))
        }
        _ => None,
    }
}

// Prepends "(type) " to a printed value when `set print pretty` is on.
fn with_type_prefix(
    settings: &PrintSettings,
    symbols: Option<&SymbolResolver>,
    ty: eval::TypeHandle,
    text: String,
) -> String {
    if !settings.pretty {
        return text;
    }
    let Some(symbols) = symbols else {
        return text;
    };
    format!("({}) {}", symbols.type_name(ty.0, ty.1), text)
}

fn format_struct(
    settings: &PrintSettings,
    symbols: &SymbolResolver,
    ctx: &DebugEvalContext,
    ty: eval::TypeHandle,
    address: u64,
    depth: usize,
    fmt: Option<char>,
) -> String {
    let parts: Vec<String> = symbols
        .type_members(ty.0, ty.1)
        .into_iter()
        .map(|(name, offset, member_type_id)| {
            let member_addr = address.wrapping_add(offset as u64);
            let member_ty = eval::TypeHandle(ty.0, member_type_id);
            let value_text =
                format_field(settings, symbols, ctx, member_ty, member_addr, depth + 1, fmt);
            // Struct/union members (but not array elements, which reuse
            // format_field too) get their type spelled out before the
            // value when pretty-printing, e.g. "x = (int) 1".
            let value_text = with_type_prefix(settings, Some(symbols), member_ty, value_text);
            format!("{} = {}", name, value_text)
        })
        .collect();

    if !settings.pretty || parts.is_empty() {
        return format!("{{{}}}", parts.join(", "));
    }

    let inner_indent = "  ".repeat(depth + 1);
    let outer_indent = "  ".repeat(depth);
    format!(
        "{{\n{}{}\n{}}}",
        inner_indent,
        parts.join(&format!(",\n{}", inner_indent)),
        outer_indent
    )
}

fn format_array(
    settings: &PrintSettings,
    symbols: &SymbolResolver,
    ctx: &DebugEvalContext,
    ty: eval::TypeHandle,
    address: u64,
    depth: usize,
    fmt: Option<char>,
) -> String {
    let elem_type_id = symbols.type_pointee(ty.0, ty.1).unwrap_or(0);
    let count = symbols.type_count(ty.0, ty.1).unwrap_or(0) as usize;
    let elem_ty = eval::TypeHandle(ty.0, elem_type_id);
    let elem_size = symbols.type_size(ty.0, elem_type_id).max(1) as u64;

    let mut parts: Vec<String> = Vec::new();
    let mut truncated = false;
    let mut i = 0usize;
    while i < count {
        if parts.len() >= settings.elements {
            truncated = true;
            break;
        }
        let elem_addr = address.wrapping_add(i as u64 * elem_size);
        let text = format_field(settings, symbols, ctx, elem_ty, elem_addr, depth + 1, fmt);

        let mut run = 1usize;
        if settings.repeats != usize::MAX {
            while i + run < count {
                let next_addr = address.wrapping_add((i + run) as u64 * elem_size);
                if format_field(settings, symbols, ctx, elem_ty, next_addr, depth + 1, fmt) != text
                {
                    break;
                }
                run += 1;
            }
        }

        if run >= settings.repeats {
            parts.push(format!("{} <repeats {} times>", text, run));
        } else {
            for _ in 0..run {
                parts.push(text.clone());
            }
        }
        i += run;
    }

    if truncated {
        format!("{{{}...}}", parts.join(", "))
    } else {
        format!("{{{}}}", parts.join(", "))
    }
}

fn format_field(
    settings: &PrintSettings,
    symbols: &SymbolResolver,
    ctx: &DebugEvalContext,
    ty: eval::TypeHandle,
    address: u64,
    depth: usize,
    fmt: Option<char>,
) -> String {
    if let Some(text) = format_aggregate(settings, Some(symbols), ctx, ty, address, depth, fmt) {
        return text;
    }
    let meta = type_meta(symbols, ty.0, ty.1);
    let loc = eval::TypedLocation {
        address,
        size: meta.size,
        kind: meta.kind,
        ty: meta.ty,
    };
    match ctx.read_typed(loc) {
        Some(value) => format_scalar(
            settings,
            Some(symbols),
            ctx,
            value,
            Some(&eval::Location::Memory(loc)),
            fmt,
        ),
        None => "<unreadable>".to_string(),
    }
}

fn format_scalar(
    settings: &PrintSettings,
    symbols: Option<&SymbolResolver>,
    ctx: &DebugEvalContext,
    value: eval::Value,
    loc: Option<&eval::Location>,
    fmt: Option<char>,
) -> String {
    if let Some(f) = fmt {
        let size = match loc {
            Some(eval::Location::Memory(tl)) => tl.size,
            _ => 4,
        };
        return format_value_with_spec(value, f, size, symbols);
    }

    let base = value.to_string();
    let eval::Value::Int(n) = value else {
        return base;
    };
    let Some(eval::Location::Memory(eval::TypedLocation { ty: Some(ty), .. })) = loc else {
        return base;
    };
    let Some(symbols) = symbols else {
        return base;
    };
    if symbols.type_tag(ty.0, ty.1) != Some(symbols::SYM_TAG_POINTER_TYPE) {
        return base;
    }

    // char* (or a typedef of it) gets its pointee read and shown as a
    // string, like GDB's default print, taking priority over the plain
    // symbol-annotation path below.
    if n != 0 {
        if let Some(pointee) = symbols.type_pointee(ty.0, ty.1) {
            if is_char_type(symbols, ty.0, pointee) {
                let max_len = settings.elements.min(4096).max(1);
                if let Some(s) = read_c_string(ctx.proc, n as u64, max_len) {
                    return format!("{} \"{}\"", base, escape_c_string(&s));
                }
            }
        }
    }

    if !settings.address {
        return base;
    }
    let annotation = describe_address(Some(symbols), n as usize);
    if annotation.is_empty() {
        base
    } else {
        format!("{}{}", base, annotation)
    }
}

// Renders a scalar Value per a GDB print format letter (x/d/u/o/t/c/f/a/z).
fn format_value_with_spec(
    value: eval::Value,
    fmt: char,
    size: u32,
    symbols: Option<&SymbolResolver>,
) -> String {
    let n = value.as_i64();
    let u = n as u64;
    match fmt {
        'x' => format!("{:#x}", u),
        'd' => format!("{}", n),
        'u' => format!("{}", u),
        'o' => {
            if u == 0 {
                "0".to_string()
            } else {
                format!("0{:o}", u)
            }
        }
        't' => format!("{:b}", u),
        'c' => {
            let b = (n & 0xff) as u8;
            let ch = if b.is_ascii_graphic() || b == b' ' {
                (b as char).to_string()
            } else {
                format!("\\{:03o}", b)
            };
            format!("{} '{}'", n, ch)
        }
        'f' => format!("{}", value.as_f64()),
        'a' => {
            let addr = u as usize;
            format!("{:#x}{}", addr, describe_address(symbols, addr))
        }
        'z' => {
            let hex_digits = (size.clamp(1, 8) as usize) * 2;
            format!("{:#0width$x}", u, width = hex_digits + 2)
        }
        _ => value.to_string(),
    }
}

// Reads a NUL-terminated string from the debuggee, byte by byte (arrays are
// small and this is a REPL command, not a hot path). Returns None only if
// the very first byte couldn't be read; a failure partway through just
// truncates the string, matching how a debugger would show a string that
// runs off the end of a mapped region.
fn read_c_string(proc: &DebuggeeProcess, address: u64, max_len: usize) -> Option<String> {
    let mut bytes = Vec::new();
    for i in 0..max_len {
        let mut byte = [0u8; 1];
        match proc.read_memory((address as usize).wrapping_add(i), &mut byte) {
            Ok(1) if byte[0] != 0 => bytes.push(byte[0]),
            Ok(1) => break,
            _ => {
                if bytes.is_empty() {
                    return None;
                }
                break;
            }
        }
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn escape_c_string(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => {
                out.push_str(&format!("\\{:03o}", c as u32))
            }
            c => out.push(c),
        }
    }
    out
}

fn describe_address(symbols: Option<&SymbolResolver>, address: usize) -> String {
    let Some(symbols) = symbols else {
        return String::new();
    };
    let Ok(info) = symbols.resolve(address as u64) else {
        return String::new();
    };

    let location = match (&info.file, info.line) {
        (Some(file), Some(line)) => format!(" ({}:{})", file.display(), line),
        _ => String::new(),
    };
    format!(" [{}+{:#x}{}]", info.name, info.displacement, location)
}

fn show_context(symbols: Option<&SymbolResolver>, proc: &DebuggeeProcess, address: usize) {
    show_context_with_window(symbols, proc, address, 3, 2);
}

fn show_context_with_window(
    symbols: Option<&SymbolResolver>,
    proc: &DebuggeeProcess,
    address: usize,
    before: usize,
    after: usize,
) {
    if let Some(symbols) = symbols {
        if let Ok(info) = symbols.resolve(address as u64) {
            if let (Some(file), Some(line)) = (&info.file, info.line) {
                if print_source(file, line, before, after) {
                    return;
                }
            }
        }
    }
    print_disassembly(proc, address);
}

fn print_source(file: &std::path::Path, line: u32, before: usize, after: usize) -> bool {
    let Ok(content) = std::fs::read_to_string(file) else {
        return false;
    };
    let lines: Vec<&str> = content.lines().collect();
    if line == 0 || line as usize > lines.len() {
        return false;
    }

    let target = line as usize;
    let start = target.saturating_sub(before).max(1);
    let end = (target + after).min(lines.len());
    for n in start..=end {
        let marker = if n == target { "=>" } else { "  " };
        println!("{} {:4}  {}", marker, n, lines[n - 1]);
    }
    true
}

fn print_disassembly(proc: &DebuggeeProcess, address: usize) {
    let mut buf = vec![0u8; 128];
    match proc.read_memory(address, &mut buf) {
        Ok(read) if read > 0 => {
            for line in crate::disasm::disassemble(&buf[..read], address as u64, 8) {
                println!("{}", line);
            }
        }
        _ => println!("(disassembly unavailable)"),
    }
}

fn print_context(ctx: &CONTEXT) {
    println!("RIP: {:#018x}", ctx.Rip);
    println!("RSP: {:#018x}", ctx.Rsp);
    println!("RBP: {:#018x}", ctx.Rbp);
    println!("RAX: {:#018x}", ctx.Rax);
    println!("RBX: {:#018x}", ctx.Rbx);
    println!("RCX: {:#018x}", ctx.Rcx);
    println!("RDX: {:#018x}", ctx.Rdx);
    println!("RSI: {:#018x}", ctx.Rsi);
    println!("RDI: {:#018x}", ctx.Rdi);
    println!("R8:  {:#018x}", ctx.R8);
    println!("R9:  {:#018x}", ctx.R9);
    println!("R10: {:#018x}", ctx.R10);
    println!("R11: {:#018x}", ctx.R11);
    println!("R12: {:#018x}", ctx.R12);
    println!("R13: {:#018x}", ctx.R13);
    println!("R14: {:#018x}", ctx.R14);
    println!("R15: {:#018x}", ctx.R15);
    println!("EFLAGS: {:#010x}", ctx.EFlags);

    // Only populated when CONTEXT_FLOATING_POINT was requested
    // (get_full_thread_context_x64); a plain get_thread_context_x64 context
    // would print all-zero garbage here.
    let flt = unsafe { ctx.Anonymous.FltSave };
    println!();
    println!(
        "MXCSR: {:#010x}  Control: {:#06x}  Status: {:#06x}  Tag: {:#04x}",
        flt.MxCsr, flt.ControlWord, flt.StatusWord, flt.TagWord
    );
    // MM0-MM7 alias the low 64 bits (the mantissa field) of ST0-ST7, per the
    // x87/MMX register aliasing defined by the Intel SDM.
    for (i, reg) in flt.FloatRegisters.iter().enumerate() {
        println!(
            "ST{}: {:<width$}  MM{}: {:#018x}",
            i,
            format_x87_extended(reg.Low, reg.High as u64),
            i,
            reg.Low,
            width = 24,
        );
    }
    for (i, reg) in flt.XmmRegisters.iter().enumerate() {
        println!("XMM{:<2}: {:016x}{:016x}", i, reg.High as u64, reg.Low);
    }
}

// Decodes an 80-bit x87 extended-precision value (as stored in FXSAVE's
// 16-byte-aligned FloatRegisters slots: a 64-bit mantissa in `low`, then a
// 1-bit sign + 15-bit biased exponent in the low 16 bits of `high`) into an
// f64 for display. This loses precision beyond f64's 52-bit mantissa, which
// is an accepted trade-off for a register dump rather than exact arithmetic.
fn format_x87_extended(mantissa: u64, high: u64) -> String {
    format!("{}", process::decode_x87_extended(mantissa, high))
}

fn print_memory(mut base: usize, bytes: &[u8]) {
    for chunk in bytes.chunks(16) {
        print!("{:#018x}: ", base);
        for b in chunk {
            print!("{:02x} ", b);
        }
        for _ in chunk.len()..16 {
            print!("   ");
        }
        print!(" |");
        for b in chunk {
            let c = *b as char;
            if c.is_ascii_graphic() {
                print!("{}", c);
            } else {
                print!(".");
            }
        }
        println!("|");
        base += chunk.len();
    }
}
