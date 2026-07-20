use crate::error::{DebuggerError, Result};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use windows::Win32::Foundation::{CloseHandle, BOOL, ERROR_SEM_TIMEOUT, HANDLE, NTSTATUS};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_DEBUG_NAME,
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Console::{
    GenerateConsoleCtrlEvent, SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT,
};
use windows::Win32::System::Diagnostics::Debug::{
    ContinueDebugEvent, GetThreadContext, ReadProcessMemory, SetThreadContext, WaitForDebugEventEx,
    WriteProcessMemory, CONTEXT, CONTEXT_FLAGS, DEBUG_EVENT, M128A,
};
use windows::Win32::System::Memory::{
    VirtualProtectEx, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS,
};
use windows::Win32::System::Threading::{
    CreateProcessW, GetCurrentProcess, IsWow64Process, OpenProcessToken, OpenThread, ResumeThread,
    SuspendThread, TerminateProcess, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
    THREAD_ACCESS_RIGHTS, THREAD_GET_CONTEXT, THREAD_SET_CONTEXT, THREAD_SUSPEND_RESUME,
};

const INT3: u8 = 0xCC;
const CONTEXT_CONTROL_X64: CONTEXT_FLAGS = CONTEXT_FLAGS(0x00100001);
// CONTEXT_CONTROL | CONTEXT_INTEGER | CONTEXT_FLOATING_POINT: stack unwinding
// needs the full register set (not just Rip/Rsp/EFlags), since unwind codes
// can reference any non-volatile GPR (Rbp, Rbx, R12-R15, ...).
const CONTEXT_FULL_X64: CONTEXT_FLAGS = CONTEXT_FLAGS(0x0010000B);
const CONTEXT_DEBUG_REGISTERS_X64: CONTEXT_FLAGS = CONTEXT_FLAGS(0x00100010);
// DEBUG_PROCESS (rather than DEBUG_ONLY_THIS_PROCESS) so that any child
// process the debuggee later creates is debugged too, instead of running
// completely outside tdb's control: Windows delivers CREATE_PROCESS/
// CREATE_THREAD/EXIT_* debug events for the whole process tree.
const DEBUG_PROCESS: PROCESS_CREATION_FLAGS = PROCESS_CREATION_FLAGS(0x00000001);
// Puts the debuggee in its own process group (while still sharing our
// console) so GenerateConsoleCtrlEvent can target it specifically without
// also hitting the debugger itself.
const CREATE_NEW_PROCESS_GROUP: PROCESS_CREATION_FLAGS = PROCESS_CREATION_FLAGS(0x00000200);

// Ctrl+C forwarding state. A console control handler runs on a separate
// OS-spawned thread with no access to `self`, so the debuggee's pid and the
// "forward it" request are threaded through statics instead.
static DEBUGGEE_PID: AtomicU32 = AtomicU32::new(0);
static INTERRUPT_REQUESTED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> BOOL {
    if ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT {
        INTERRUPT_REQUESTED.store(true, Ordering::SeqCst);
        // Tell Windows we handled it, so it doesn't also apply the default
        // action (terminating this process) on top of our own handling.
        BOOL(1)
    } else {
        BOOL(0)
    }
}

// Registers the Ctrl+C/Ctrl+Break handler above. Call once at startup;
// without this, the debugger has no default handler and Ctrl+C just kills
// it immediately like any other console app.
pub fn install_interrupt_handler() -> Result<()> {
    unsafe {
        SetConsoleCtrlHandler(Some(console_ctrl_handler), true)?;
    }
    Ok(())
}

// Enables SeDebugPrivilege in this process's own token, best-effort. Without
// it, OpenThread (thread_handle() below) is denied ERROR_ACCESS_DENIED for
// some threads even though this process is the active debugger of theirs:
// being the debugger only grants the implicit right to receive/continue
// debug events, not to freely OpenProcess/OpenThread them, which still goes
// through the normal DACL check unless the caller holds SeDebugPrivilege
// (as real debuggers like WinDbg enable at startup). Silently does nothing
// if the privilege isn't available to this token (e.g. not running
// elevated) or any step fails; callers just keep tolerating the occasional
// access-denied warning as before.
pub fn enable_debug_privilege() {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .is_err()
        {
            return;
        }

        let mut priv_value = Default::default();
        if LookupPrivilegeValueW(None, SE_DEBUG_NAME, &mut priv_value).is_err() {
            let _ = CloseHandle(token);
            return;
        }

        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: priv_value,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let _ = AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None);
        let _ = CloseHandle(token);
    }
}

// True at most once per Ctrl+C/Ctrl+Break (the flag is cleared on read), so
// callers naturally get edge-triggered semantics from a loop that polls it.
pub fn take_interrupt_requested() -> bool {
    INTERRUPT_REQUESTED.swap(false, Ordering::SeqCst)
}

// The real x64 CONTEXT struct is DECLSPEC_ALIGN(16); windows-rs's binding only
// has #[repr(C)], so GetThreadContext/SetThreadContext fail with ERROR_NOACCESS
// if the buffer isn't explicitly over-aligned like this.
#[repr(align(16))]
#[derive(Clone, Copy)]
pub struct AlignedContext(pub CONTEXT);

impl std::ops::Deref for AlignedContext {
    type Target = CONTEXT;
    fn deref(&self) -> &CONTEXT {
        &self.0
    }
}

impl std::ops::DerefMut for AlignedContext {
    fn deref_mut(&mut self) -> &mut CONTEXT {
        &mut self.0
    }
}

// Parses "<prefix><n>" (e.g. "mm3", "xmm12") into n, bounded to < max.
fn indexed_register(name: &str, prefix: &str, max: usize) -> Option<usize> {
    let i: usize = name.strip_prefix(prefix)?.parse().ok()?;
    (i < max).then_some(i)
}

fn mm_index(name: &str) -> Option<usize> {
    indexed_register(name, "mm", 8)
}

fn st_index(name: &str) -> Option<usize> {
    indexed_register(name, "st", 8)
}

// "xmmN" is the low 64 bits, "xmmNh" the high 64 bits: a full 128-bit value
// doesn't fit this file's i64-based register plumbing, so the two halves
// are addressed as separate pseudo-registers instead.
fn xmm_index(name: &str) -> Option<(usize, bool)> {
    match name.strip_suffix('h') {
        Some(n) => indexed_register(n, "xmm", 16).map(|i| (i, true)),
        None => indexed_register(name, "xmm", 16).map(|i| (i, false)),
    }
}

impl AlignedContext {
    pub fn register(&self, name: &str) -> Option<i64> {
        let name = name.to_ascii_lowercase();
        match name.as_str() {
            "rax" => return Some(self.Rax as i64),
            "rbx" => return Some(self.Rbx as i64),
            "rcx" => return Some(self.Rcx as i64),
            "rdx" => return Some(self.Rdx as i64),
            "rsi" => return Some(self.Rsi as i64),
            "rdi" => return Some(self.Rdi as i64),
            "rbp" => return Some(self.Rbp as i64),
            "rsp" => return Some(self.Rsp as i64),
            "rip" => return Some(self.Rip as i64),
            "r8" => return Some(self.R8 as i64),
            "r9" => return Some(self.R9 as i64),
            "r10" => return Some(self.R10 as i64),
            "r11" => return Some(self.R11 as i64),
            "r12" => return Some(self.R12 as i64),
            "r13" => return Some(self.R13 as i64),
            "r14" => return Some(self.R14 as i64),
            "r15" => return Some(self.R15 as i64),
            "eflags" => return Some(self.EFlags as i64),
            "mxcsr" => return Some(unsafe { self.Anonymous.FltSave.MxCsr } as i64),
            _ => {}
        }
        // Only meaningful when the context was fetched with
        // CONTEXT_FLOATING_POINT (get_full_thread_context_x64); otherwise
        // this reads back zeroed, unpopulated memory.
        if let Some(i) = mm_index(&name) {
            return Some(unsafe { self.Anonymous.FltSave.FloatRegisters[i].Low } as i64);
        }
        if let Some((i, high)) = xmm_index(&name) {
            let reg = unsafe { self.Anonymous.FltSave.XmmRegisters[i] };
            return Some(if high { reg.High } else { reg.Low as i64 });
        }
        None
    }

    pub fn set_register(&mut self, name: &str, value: i64) -> bool {
        let name = name.to_ascii_lowercase();
        match name.as_str() {
            "rax" => self.Rax = value as u64,
            "rbx" => self.Rbx = value as u64,
            "rcx" => self.Rcx = value as u64,
            "rdx" => self.Rdx = value as u64,
            "rsi" => self.Rsi = value as u64,
            "rdi" => self.Rdi = value as u64,
            "rbp" => self.Rbp = value as u64,
            "rsp" => self.Rsp = value as u64,
            "rip" => self.Rip = value as u64,
            "r8" => self.R8 = value as u64,
            "r9" => self.R9 = value as u64,
            "r10" => self.R10 = value as u64,
            "r11" => self.R11 = value as u64,
            "r12" => self.R12 = value as u64,
            "r13" => self.R13 = value as u64,
            "r14" => self.R14 = value as u64,
            "r15" => self.R15 = value as u64,
            "eflags" => self.EFlags = value as u32,
            // Assignment through a union field is safe in Rust (only
            // *reading* one can be UB), so this needs no unsafe block.
            "mxcsr" => self.Anonymous.FltSave.MxCsr = value as u32,
            _ => {
                if let Some(i) = mm_index(&name) {
                    unsafe { self.Anonymous.FltSave.FloatRegisters[i].Low = value as u64 };
                } else if let Some((i, high)) = xmm_index(&name) {
                    unsafe {
                        if high {
                            self.Anonymous.FltSave.XmmRegisters[i].High = value;
                        } else {
                            self.Anonymous.FltSave.XmmRegisters[i].Low = value as u64;
                        }
                    }
                } else {
                    return false;
                }
            }
        }
        true
    }

    // ST0-ST7 hold 80-bit x87 extended-precision values, which don't fit
    // this file's i64 register plumbing; these two methods are the
    // float-valued counterpart of register()/set_register() for them.
    pub fn register_f64(&self, name: &str) -> Option<f64> {
        let i = st_index(&name.to_ascii_lowercase())?;
        let reg = unsafe { self.Anonymous.FltSave.FloatRegisters[i] };
        Some(decode_x87_extended(reg.Low, reg.High as u64))
    }

    pub fn set_register_f64(&mut self, name: &str, value: f64) -> bool {
        let Some(i) = st_index(&name.to_ascii_lowercase()) else {
            return false;
        };
        let (low, high) = encode_x87_extended(value);
        unsafe {
            self.Anonymous.FltSave.FloatRegisters[i] = M128A { Low: low, High: high };
        }
        true
    }
}

// Decodes an 80-bit x87 extended-precision value (FXSAVE layout: a 64-bit
// mantissa in `mantissa`, then a 1-bit sign + 15-bit biased exponent in the
// low 16 bits of `high`) into the nearest f64. Precision beyond f64's
// 52-bit mantissa is lost, an accepted trade-off for a register
// display/edit convenience rather than exact arithmetic.
pub fn decode_x87_extended(mantissa: u64, high: u64) -> f64 {
    let sign_exp = (high & 0xffff) as u16;
    let sign = ((sign_exp >> 15) & 1) as u64;
    let exponent = (sign_exp & 0x7fff) as i64;

    if exponent == 0 && mantissa == 0 {
        return f64::from_bits(sign << 63);
    }
    if exponent == 0x7fff {
        return if mantissa == (1u64 << 63) {
            if sign == 1 { f64::NEG_INFINITY } else { f64::INFINITY }
        } else {
            f64::NAN
        };
    }

    let exp_f64 = exponent - 16383 + 1023;
    if !(1..0x7ff).contains(&exp_f64) {
        // Outside f64's normal exponent range: fall back to a coarser but
        // overflow/underflow-safe approximation rather than constructing an
        // f64 bit pattern with an invalid exponent field.
        let significand = mantissa as f64 / (1u64 << 63) as f64;
        let value = significand * 2f64.powi((exponent - 16383) as i32);
        return if sign == 1 { -value } else { value };
    }
    // Drop the explicit integer bit (extended precision has one, f64's is
    // implicit) and the low 11 bits of the remaining 63-bit fraction, to
    // match f64's 52-bit mantissa field.
    let mantissa_f64 = (mantissa << 1) >> 12;
    f64::from_bits((sign << 63) | ((exp_f64 as u64) << 52) | mantissa_f64)
}

// Inverse of decode_x87_extended: widens an f64 into the 80-bit extended
// layout (explicit integer bit, exponent rebiased from 1023 to 16383).
// Exact for zero/normal/infinite/NaN values; f64 subnormals (a vanishingly
// narrow case for a register-set command) are approximated via the same
// log2-based fallback decode uses for out-of-range values.
pub fn encode_x87_extended(value: f64) -> (u64, i64) {
    let bits = value.to_bits();
    let sign = (bits >> 63) & 1;
    let exp_f64 = ((bits >> 52) & 0x7ff) as i64;
    let mantissa_f64 = bits & 0xf_ffff_ffff_ffff;

    if exp_f64 == 0 && mantissa_f64 == 0 {
        return (0, (sign << 15) as i64);
    }
    if exp_f64 == 0x7ff {
        return if mantissa_f64 == 0 {
            (1u64 << 63, ((sign << 15) | 0x7fff) as i64)
        } else {
            ((1u64 << 63) | (mantissa_f64 << 11), ((sign << 15) | 0x7fff) as i64)
        };
    }
    if exp_f64 == 0 {
        let normalized = value.abs();
        let exponent = normalized.log2().floor() as i64;
        let mantissa_ext = ((normalized / 2f64.powi(exponent as i32)) * (1u64 << 63) as f64) as u64;
        let biased_exponent = ((exponent + 16383) & 0x7fff) as u64;
        return (mantissa_ext, ((sign << 15) | biased_exponent) as i64);
    }

    let exponent_ext = (exp_f64 - 1023 + 16383) as u64;
    let mantissa_ext = (1u64 << 63) | (mantissa_f64 << 11);
    (mantissa_ext, ((sign << 15) | (exponent_ext & 0x7fff)) as i64)
}

// One tracked process: the root (originally launched) process, or a child
// it later spawned via CreateProcess and that Windows is now also reporting
// debug events for (see DEBUG_PROCESS above).
struct ProcessEntry {
    handle: HANDLE,
    main_thread: HANDLE,
    main_thread_id: u32,
    #[allow(dead_code)]
    is64bit: bool,
}

pub struct DebuggeeProcess {
    pub root_pid: u32,
    processes: HashMap<u32, ProcessEntry>,
}

impl DebuggeeProcess {
    pub fn launch(cmdline: &str) -> Result<Self> {
        let mut cmdline_w: Vec<u16> = OsStr::new(cmdline)
            .encode_wide()
            .chain(Some(0))
            .collect();

        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;

        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

        unsafe {
            CreateProcessW(
                None,
                windows::core::PWSTR(cmdline_w.as_mut_ptr()),
                None,
                None,
                false,
                PROCESS_CREATION_FLAGS(DEBUG_PROCESS.0 | CREATE_NEW_PROCESS_GROUP.0),
                None,
                None,
                &si,
                &mut pi,
            )?;
        }

        let is64bit = unsafe { is_wow64_process(pi.hProcess)? };
        DEBUGGEE_PID.store(pi.dwProcessId, Ordering::SeqCst);

        let mut processes = HashMap::new();
        processes.insert(
            pi.dwProcessId,
            ProcessEntry {
                handle: pi.hProcess,
                main_thread: pi.hThread,
                main_thread_id: pi.dwThreadId,
                is64bit,
            },
        );

        Ok(Self {
            root_pid: pi.dwProcessId,
            processes,
        })
    }

    pub fn root_handle(&self) -> Result<HANDLE> {
        self.handle_for(self.root_pid)
    }

    pub fn root_main_thread_id(&self) -> u32 {
        self.processes
            .get(&self.root_pid)
            .map(|p| p.main_thread_id)
            .unwrap_or(0)
    }

    // Called from CREATE_PROCESS_DEBUG_EVENT for a genuine child process
    // (the root's own such event is already covered by launch() above).
    // Bitness detection is best-effort: a process this debugger isn't
    // allowed to query for (rare) still gets tracked, just without that bit
    // recorded, since nothing here treats is64bit as anything but informational.
    pub fn register_process(&mut self, pid: u32, handle: HANDLE, main_thread: HANDLE, main_thread_id: u32) {
        let is64bit = unsafe { is_wow64_process(handle) }.unwrap_or(false);
        self.processes.insert(
            pid,
            ProcessEntry {
                handle,
                main_thread,
                main_thread_id,
                is64bit,
            },
        );
    }

    // Called from EXIT_PROCESS_DEBUG_EVENT once a (root or child) process is
    // gone, so its handle isn't leaked and later lookups correctly report it
    // as no longer tracked.
    pub fn remove_process(&mut self, pid: u32) {
        if let Some(entry) = self.processes.remove(&pid) {
            unsafe {
                let _ = CloseHandle(entry.handle);
                let _ = CloseHandle(entry.main_thread);
            }
        }
    }

    // (pid, main_thread_id, is_root) for every currently tracked process,
    // backing the `processes` REPL command.
    pub fn list_processes(&self) -> Vec<(u32, u32, bool)> {
        self.processes
            .iter()
            .map(|(&pid, entry)| (pid, entry.main_thread_id, pid == self.root_pid))
            .collect()
    }

    fn handle_for(&self, pid: u32) -> Result<HANDLE> {
        self.processes
            .get(&pid)
            .map(|entry| entry.handle)
            .ok_or(DebuggerError::NoSuchProcess(pid))
    }

    // Forwards a Ctrl+C to the debuggee, the closest Windows equivalent of
    // sending it SIGINT: CREATE_NEW_PROCESS_GROUP at launch made its pid
    // double as its own console process group id, so this reaches it
    // without also hitting the debugger.
    pub fn interrupt(&self) -> Result<()> {
        unsafe {
            GenerateConsoleCtrlEvent(CTRL_C_EVENT, self.root_pid)?;
        }
        Ok(())
    }

    // `None` means the timeout elapsed with no debug event (normal while the
    // debuggee is just running); WaitForDebugEventEx reports that the same
    // way as a real failure (return FALSE + GetLastError), so it must be
    // special-cased here rather than let a plain `?` turn every idle poll
    // into an Err and kill the caller's polling loop.
    pub fn wait_for_event(&self, timeout_ms: u32) -> Result<Option<DEBUG_EVENT>> {
        let mut event: DEBUG_EVENT = unsafe { std::mem::zeroed() };
        match unsafe { WaitForDebugEventEx(&mut event, timeout_ms) } {
            Ok(()) => Ok(Some(event)),
            Err(e) if e.code() == ERROR_SEM_TIMEOUT.to_hresult() => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn continue_event(&self, process_id: u32, thread_id: u32, status: u32) -> Result<()> {
        unsafe {
            ContinueDebugEvent(process_id, thread_id, NTSTATUS(status as i32))?;
        }
        Ok(())
    }

    pub fn read_memory(&self, pid: u32, address: usize, buf: &mut [u8]) -> Result<usize> {
        let handle = self.handle_for(pid)?;
        let mut read = 0usize;
        unsafe {
            ReadProcessMemory(
                handle,
                address as *const _,
                buf.as_mut_ptr() as *mut _,
                buf.len(),
                Some(&mut read),
            )?;
        }
        Ok(read)
    }

    pub fn write_memory(&self, pid: u32, address: usize, data: &[u8]) -> Result<usize> {
        let handle = self.handle_for(pid)?;
        let mut written = 0usize;
        unsafe {
            WriteProcessMemory(
                handle,
                address as *const _,
                data.as_ptr() as *const _,
                data.len(),
                Some(&mut written),
            )?;
        }
        Ok(written)
    }

    pub fn set_breakpoint(&self, pid: u32, address: usize) -> Result<u8> {
        let handle = self.handle_for(pid)?;
        let mut old_byte = 0u8;
        self.read_memory(pid, address, unsafe {
            std::slice::from_raw_parts_mut(&mut old_byte, 1)
        })?;

        let mut old_protect = PAGE_PROTECTION_FLAGS(0);
        unsafe {
            VirtualProtectEx(
                handle,
                address as *const _,
                1,
                PAGE_EXECUTE_READWRITE,
                &mut old_protect,
            )?;
        }

        let mut written = 0usize;
        unsafe {
            WriteProcessMemory(
                handle,
                address as *const _,
                &INT3 as *const _ as *const _,
                1,
                Some(&mut written),
            )?;
        }

        unsafe {
            VirtualProtectEx(
                handle,
                address as *const _,
                1,
                old_protect,
                &mut old_protect,
            )?;
        }

        Ok(old_byte)
    }

    pub fn remove_breakpoint(&self, pid: u32, address: usize, original_byte: u8) -> Result<()> {
        let handle = self.handle_for(pid)?;
        let mut old_protect = PAGE_PROTECTION_FLAGS(0);
        unsafe {
            VirtualProtectEx(
                handle,
                address as *const _,
                1,
                PAGE_EXECUTE_READWRITE,
                &mut old_protect,
            )?;
        }

        let mut written = 0usize;
        unsafe {
            WriteProcessMemory(
                handle,
                address as *const _,
                &original_byte as *const _ as *const _,
                1,
                Some(&mut written),
            )?;
        }

        unsafe {
            VirtualProtectEx(
                handle,
                address as *const _,
                1,
                old_protect,
                &mut old_protect,
            )?;
        }

        Ok(())
    }

    pub fn thread_handle(&self, thread_id: u32) -> Result<HANDLE> {
        let handle = unsafe { OpenThread(THREAD_ACCESS_RIGHTS(THREAD_GET_CONTEXT.0 | THREAD_SET_CONTEXT.0 | THREAD_SUSPEND_RESUME.0), false, thread_id)? };
        Ok(handle)
    }

    fn get_thread_context(&self, thread_id: u32, flags: CONTEXT_FLAGS) -> Result<AlignedContext> {
        let handle = self.thread_handle(thread_id)?;
        let mut ctx = AlignedContext(unsafe { std::mem::zeroed() });
        ctx.ContextFlags = flags;
        unsafe {
            SuspendThread(handle);
            GetThreadContext(handle, &mut ctx.0)?;
            ResumeThread(handle);
            CloseHandle(handle)?;
        }
        Ok(ctx)
    }

    pub fn get_thread_context_x64(&self, thread_id: u32) -> Result<AlignedContext> {
        self.get_thread_context(thread_id, CONTEXT_CONTROL_X64)
    }

    pub fn get_full_thread_context_x64(&self, thread_id: u32) -> Result<AlignedContext> {
        self.get_thread_context(thread_id, CONTEXT_FULL_X64)
    }

    pub fn set_thread_context_x64(&self, thread_id: u32, ctx: &AlignedContext) -> Result<()> {
        let handle = self.thread_handle(thread_id)?;
        unsafe {
            SuspendThread(handle);
            SetThreadContext(handle, &ctx.0)?;
            ResumeThread(handle);
            CloseHandle(handle)?;
        }
        Ok(())
    }

    pub fn single_step(&self, thread_id: u32) -> Result<()> {
        let mut ctx = self.get_thread_context_x64(thread_id)?;
        ctx.EFlags |= 0x100; // TF
        self.set_thread_context_x64(thread_id, &ctx)
    }

    #[allow(dead_code)]
    pub fn get_rip(&self, thread_id: u32) -> Result<u64> {
        let ctx = self.get_thread_context_x64(thread_id)?;
        Ok(ctx.Rip)
    }

    pub fn set_rip(&self, thread_id: u32, rip: u64) -> Result<()> {
        let mut ctx = self.get_thread_context_x64(thread_id)?;
        ctx.Rip = rip;
        self.set_thread_context_x64(thread_id, &ctx)
    }

    // Adds one to the thread's suspend count, independent of (and stacking
    // on top of) the whole-process freeze the OS already applies while a
    // debug event is pending: this is what lets a thread stay stopped even
    // after ContinueDebugEvent lets the rest of the process run again
    // (`thread lock`'s mechanism, same technique WinDbg's freeze uses).
    pub fn suspend_thread(&self, thread_id: u32) -> Result<()> {
        let handle = self.thread_handle(thread_id)?;
        unsafe {
            SuspendThread(handle);
            CloseHandle(handle)?;
        }
        Ok(())
    }

    pub fn resume_thread(&self, thread_id: u32) -> Result<()> {
        let handle = self.thread_handle(thread_id)?;
        unsafe {
            ResumeThread(handle);
            CloseHandle(handle)?;
        }
        Ok(())
    }

    pub fn get_debug_registers(&self, thread_id: u32) -> Result<AlignedContext> {
        self.get_thread_context(thread_id, CONTEXT_DEBUG_REGISTERS_X64)
    }

    // Writes DR0-DR3/DR7 (hardware watchpoint address/control) on one
    // thread, always clearing DR6 (the status register) at the same time:
    // debug registers are per-thread, so this must be called on every live
    // thread to make a watchpoint effective process-wide, and DR6's hit
    // bits must be cleared after each reported hit or they read as still
    // pending.
    pub fn set_debug_registers(
        &self,
        thread_id: u32,
        dr0: u64,
        dr1: u64,
        dr2: u64,
        dr3: u64,
        dr7: u64,
    ) -> Result<()> {
        let mut ctx = AlignedContext(unsafe { std::mem::zeroed() });
        ctx.ContextFlags = CONTEXT_DEBUG_REGISTERS_X64;
        ctx.Dr0 = dr0;
        ctx.Dr1 = dr1;
        ctx.Dr2 = dr2;
        ctx.Dr3 = dr3;
        ctx.Dr6 = 0;
        ctx.Dr7 = dr7;
        self.set_thread_context_x64(thread_id, &ctx)
    }
}

impl Drop for DebuggeeProcess {
    fn drop(&mut self) {
        // Only clear if we're still the current debuggee: a stale drop
        // racing a newer launch() must not clobber the new pid.
        let _ = DEBUGGEE_PID.compare_exchange(self.root_pid, 0, Ordering::SeqCst, Ordering::SeqCst);
        for (_, entry) in self.processes.drain() {
            unsafe {
                // Only the root process needs an explicit terminate: killing
                // it takes any surviving child/grandchild debuggees down
                // with it (they were all launched under it), and terminating
                // a process that's already gone (a child that exited on its
                // own, already removed via remove_process but possibly still
                // present here on abnormal shutdown) is harmless either way.
                let _ = TerminateProcess(entry.handle, 0);
                let _ = CloseHandle(entry.handle);
                let _ = CloseHandle(entry.main_thread);
            }
        }
    }
}

unsafe fn is_wow64_process(handle: HANDLE) -> Result<bool> {
    use windows::Win32::Foundation::BOOL;
    let mut wow64 = BOOL(0);
    unsafe {
        IsWow64Process(handle, &mut wow64)?;
    }
    Ok(wow64.0 == 0)
}
