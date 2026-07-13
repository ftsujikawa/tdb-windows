use crate::error::Result;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use windows::Win32::Foundation::{CloseHandle, BOOL, ERROR_SEM_TIMEOUT, HANDLE, NTSTATUS};
use windows::Win32::System::Console::{
    GenerateConsoleCtrlEvent, SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT,
};
use windows::Win32::System::Diagnostics::Debug::{
    ContinueDebugEvent, GetThreadContext, ReadProcessMemory, SetThreadContext, WaitForDebugEventEx,
    WriteProcessMemory, CONTEXT, CONTEXT_FLAGS, DEBUG_EVENT,
};
use windows::Win32::System::Memory::{
    VirtualProtectEx, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS,
};
use windows::Win32::System::Threading::{
    CreateProcessW, IsWow64Process, OpenThread, ResumeThread, SuspendThread, TerminateProcess,
    PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW, THREAD_ACCESS_RIGHTS,
    THREAD_GET_CONTEXT, THREAD_SET_CONTEXT, THREAD_SUSPEND_RESUME,
};

const INT3: u8 = 0xCC;
const CONTEXT_CONTROL_X64: CONTEXT_FLAGS = CONTEXT_FLAGS(0x00100001);
// CONTEXT_CONTROL | CONTEXT_INTEGER | CONTEXT_FLOATING_POINT: stack unwinding
// needs the full register set (not just Rip/Rsp/EFlags), since unwind codes
// can reference any non-volatile GPR (Rbp, Rbx, R12-R15, ...).
const CONTEXT_FULL_X64: CONTEXT_FLAGS = CONTEXT_FLAGS(0x0010000B);
const CONTEXT_DEBUG_REGISTERS_X64: CONTEXT_FLAGS = CONTEXT_FLAGS(0x00100010);
const DEBUG_ONLY_THIS_PROCESS: PROCESS_CREATION_FLAGS = PROCESS_CREATION_FLAGS(0x00000002);
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

impl AlignedContext {
    pub fn register(&self, name: &str) -> Option<i64> {
        Some(match name.to_ascii_lowercase().as_str() {
            "rax" => self.Rax as i64,
            "rbx" => self.Rbx as i64,
            "rcx" => self.Rcx as i64,
            "rdx" => self.Rdx as i64,
            "rsi" => self.Rsi as i64,
            "rdi" => self.Rdi as i64,
            "rbp" => self.Rbp as i64,
            "rsp" => self.Rsp as i64,
            "rip" => self.Rip as i64,
            "r8" => self.R8 as i64,
            "r9" => self.R9 as i64,
            "r10" => self.R10 as i64,
            "r11" => self.R11 as i64,
            "r12" => self.R12 as i64,
            "r13" => self.R13 as i64,
            "r14" => self.R14 as i64,
            "r15" => self.R15 as i64,
            "eflags" => self.EFlags as i64,
            _ => return None,
        })
    }

    pub fn set_register(&mut self, name: &str, value: i64) -> bool {
        match name.to_ascii_lowercase().as_str() {
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
            _ => return false,
        }
        true
    }
}

pub struct DebuggeeProcess {
    pub pid: u32,
    pub handle: HANDLE,
    pub main_thread_id: u32,
    pub main_thread: HANDLE,
    #[allow(dead_code)]
    pub is64bit: bool,
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
                PROCESS_CREATION_FLAGS(DEBUG_ONLY_THIS_PROCESS.0 | CREATE_NEW_PROCESS_GROUP.0),
                None,
                None,
                &si,
                &mut pi,
            )?;
        }

        let is64bit = unsafe { is_wow64_process(pi.hProcess)? };
        DEBUGGEE_PID.store(pi.dwProcessId, Ordering::SeqCst);

        Ok(Self {
            pid: pi.dwProcessId,
            handle: pi.hProcess,
            main_thread_id: pi.dwThreadId,
            main_thread: pi.hThread,
            is64bit,
        })
    }

    // Forwards a Ctrl+C to the debuggee, the closest Windows equivalent of
    // sending it SIGINT: CREATE_NEW_PROCESS_GROUP at launch made its pid
    // double as its own console process group id, so this reaches it
    // without also hitting the debugger.
    pub fn interrupt(&self) -> Result<()> {
        unsafe {
            GenerateConsoleCtrlEvent(CTRL_C_EVENT, self.pid)?;
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

    pub fn read_memory(&self, address: usize, buf: &mut [u8]) -> Result<usize> {
        let mut read = 0usize;
        unsafe {
            ReadProcessMemory(
                self.handle,
                address as *const _,
                buf.as_mut_ptr() as *mut _,
                buf.len(),
                Some(&mut read),
            )?;
        }
        Ok(read)
    }

    pub fn write_memory(&self, address: usize, data: &[u8]) -> Result<usize> {
        let mut written = 0usize;
        unsafe {
            WriteProcessMemory(
                self.handle,
                address as *const _,
                data.as_ptr() as *const _,
                data.len(),
                Some(&mut written),
            )?;
        }
        Ok(written)
    }

    pub fn set_breakpoint(&self, address: usize) -> Result<u8> {
        let mut old_byte = 0u8;
        self.read_memory(address, unsafe {
            std::slice::from_raw_parts_mut(&mut old_byte, 1)
        })?;

        let mut old_protect = PAGE_PROTECTION_FLAGS(0);
        unsafe {
            VirtualProtectEx(
                self.handle,
                address as *const _,
                1,
                PAGE_EXECUTE_READWRITE,
                &mut old_protect,
            )?;
        }

        let mut written = 0usize;
        unsafe {
            WriteProcessMemory(
                self.handle,
                address as *const _,
                &INT3 as *const _ as *const _,
                1,
                Some(&mut written),
            )?;
        }

        unsafe {
            VirtualProtectEx(
                self.handle,
                address as *const _,
                1,
                old_protect,
                &mut old_protect,
            )?;
        }

        Ok(old_byte)
    }

    pub fn remove_breakpoint(&self, address: usize, original_byte: u8) -> Result<()> {
        let mut old_protect = PAGE_PROTECTION_FLAGS(0);
        unsafe {
            VirtualProtectEx(
                self.handle,
                address as *const _,
                1,
                PAGE_EXECUTE_READWRITE,
                &mut old_protect,
            )?;
        }

        let mut written = 0usize;
        unsafe {
            WriteProcessMemory(
                self.handle,
                address as *const _,
                &original_byte as *const _ as *const _,
                1,
                Some(&mut written),
            )?;
        }

        unsafe {
            VirtualProtectEx(
                self.handle,
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
        let _ = DEBUGGEE_PID.compare_exchange(self.pid, 0, Ordering::SeqCst, Ordering::SeqCst);
        unsafe {
            let _ = TerminateProcess(self.handle, 0);
            let _ = CloseHandle(self.handle);
            let _ = CloseHandle(self.main_thread);
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
