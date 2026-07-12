use crate::error::{DebuggerError, Result};
use crate::process::AlignedContext;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{LocalFree, BOOL, HANDLE, HLOCAL};
use windows::Win32::System::Diagnostics::Debug::{
    ReadProcessMemory, StackWalk64, SymCleanup, SymEnumLinesW, SymEnumSymbolsW, SymFromAddrW,
    SymFunctionTableAccess64, SymFromNameW, SymGetLineFromAddrW64, SymGetModuleBase64,
    SymGetTypeInfo, SymInitializeW, SymLoadModuleExW, SymSetContext, ADDRESS_MODE,
    IMAGEHLP_LINEW64, IMAGEHLP_STACK_FRAME, SLMFLAG_NONE, SRCCODEINFOW, STACKFRAME64,
    SYMBOL_INFOW, SYMFLAG_REGREL, TI_FINDCHILDREN, TI_FINDCHILDREN_PARAMS, TI_GET_BASETYPE,
    TI_GET_CHILDRENCOUNT, TI_GET_COUNT, TI_GET_DATAKIND, TI_GET_LENGTH, TI_GET_LEXICALPARENT,
    TI_GET_OFFSET, TI_GET_SYMNAME, TI_GET_SYMTAG, TI_GET_TYPE, TI_GET_TYPEID, TI_GET_UDTKIND,
    IMAGEHLP_SYMBOL_TYPE_INFO,
};

const ADDR_MODE_FLAT: ADDRESS_MODE = ADDRESS_MODE(3);
const IMAGE_FILE_MACHINE_AMD64: u32 = 0x8664;

// SymTagEnum values (DIA SDK cvconst.h) relevant to type navigation.
pub const SYM_TAG_DATA: u32 = 7;
pub const SYM_TAG_UDT: u32 = 11;
pub const SYM_TAG_POINTER_TYPE: u32 = 14;
pub const SYM_TAG_ARRAY_TYPE: u32 = 15;
pub const SYM_TAG_BASE_TYPE: u32 = 16;
pub const SYM_TAG_TYPEDEF: u32 = 17;
// BasicType (DIA SDK cvconst.h) value for floating-point base types.
pub const BASE_TYPE_FLOAT: u32 = 8;
// DataKind (DIA SDK cvconst.h) values distinguishing locals/params/globals.
const DATA_IS_LOCAL: u32 = 1;
const DATA_IS_STATIC_LOCAL: u32 = 2;
const DATA_IS_PARAM: u32 = 3;
const DATA_IS_FILE_STATIC: u32 = 5;
const DATA_IS_GLOBAL: u32 = 6;
// UdtKind (DIA SDK cvconst.h) values, used to prefix type_name()'s SymTagUDT
// output with "struct"/"union"/"class" the way C source would spell it.
const UDT_KIND_CLASS: u32 = 1;
const UDT_KIND_UNION: u32 = 2;

// CV_AMD64_* register codes (DIA SDK cvconst.h) that SYMBOL_INFOW::Register
// can report for a SYMFLAG_REGREL local's base register.
fn register_by_cv_code(ctx: &AlignedContext, code: u32) -> Option<u64> {
    Some(match code {
        328 => ctx.Rax,
        329 => ctx.Rbx,
        330 => ctx.Rcx,
        331 => ctx.Rdx,
        332 => ctx.Rsi,
        333 => ctx.Rdi,
        334 => ctx.Rbp,
        335 => ctx.Rsp,
        336 => ctx.R8,
        337 => ctx.R9,
        338 => ctx.R10,
        339 => ctx.R11,
        340 => ctx.R12,
        341 => ctx.R13,
        342 => ctx.R14,
        343 => ctx.R15,
        33 => ctx.Rip,
        _ => return None,
    })
}

// BasicType (DIA SDK cvconst.h) values, resolved against the type's own
// byte length since MSVC reuses the same basic-type category across widths
// (e.g. btInt covers short/int/long long depending on size).
fn base_type_name(basic_type: u32, size: u32) -> String {
    match basic_type {
        1 => "void".to_string(),
        2 => "char".to_string(),
        3 => "wchar_t".to_string(),
        6 => match size {
            1 => "signed char".to_string(),
            2 => "short".to_string(),
            8 => "long long".to_string(),
            _ => "int".to_string(),
        },
        7 => match size {
            1 => "unsigned char".to_string(),
            2 => "unsigned short".to_string(),
            8 => "unsigned long long".to_string(),
            _ => "unsigned int".to_string(),
        },
        8 => match size {
            4 => "float".to_string(),
            _ => "double".to_string(),
        },
        10 => "bool".to_string(),
        13 => match size {
            8 => "long long".to_string(),
            _ => "long".to_string(),
        },
        14 => match size {
            8 => "unsigned long long".to_string(),
            _ => "unsigned long".to_string(),
        },
        32 => "char16_t".to_string(),
        33 => "char32_t".to_string(),
        34 => "char8_t".to_string(),
        _ => "int".to_string(),
    }
}

// SYMFLAG_REGREL symbols (locals) store `Address` as an offset from whichever
// register `Register` names, not an absolute address; `ctx` is required in
// that case (and unavailable for e.g. globals, which are always absolute).
fn resolve_symbol_address(ctx: Option<&AlignedContext>, sym: &SYMBOL_INFOW) -> Option<u64> {
    if sym.Flags.0 & SYMFLAG_REGREL.0 != 0 {
        let reg_value = register_by_cv_code(ctx?, sym.Register)?;
        Some(reg_value.wrapping_add(sym.Address))
    } else {
        Some(sym.Address)
    }
}

// SYMBOL_INFOW::NameLen (as reported by SymEnumSymbolsW callbacks) includes
// the trailing NUL, unlike the MaxNameLen-scanning used elsewhere for
// SymFromNameW/SymFromAddrW results; find the real terminator instead of
// trusting NameLen literally, or names end up with a stray embedded '\0'.
fn symbol_name(sym: &SYMBOL_INFOW) -> String {
    let name_ptr = sym.Name.as_ptr();
    let len = (0..sym.NameLen)
        .position(|i| unsafe { *name_ptr.add(i as usize) } == 0)
        .unwrap_or(sym.NameLen as usize);
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(name_ptr, len) })
}

pub struct SymbolResolver {
    process: HANDLE,
    initialized: bool,
    main_module_base: std::cell::Cell<u64>,
}

const MAX_SYM_NAME: usize = 2000;

// SYMBOL_INFOW::Name is a C flexible-array member exposed as `[u16; 1]`; dbghelp
// writes up to MaxNameLen chars starting at that address, so the struct must be
// backed by a buffer sized for the full name, not a stack-allocated SYMBOL_INFOW.
// SymEnumLinesW's own file filter requires an exact match, not a substring/tail
// match, so a bare filename like "test.c" never matches the absolute path dbghelp
// reports; do the tail comparison ourselves instead (case-insensitive, either
// separator style).
fn file_matches(candidate: &str, query: &str) -> bool {
    let normalize = |s: &str| s.to_lowercase().replace('/', "\\");
    let candidate = normalize(candidate);
    let query = normalize(query);
    candidate == query
        || candidate.ends_with(&format!("\\{}", query))
}

// A data symbol's compiland (TI_GET_LEXICALPARENT) is named after the
// *object* file it was built into (e.g. "test.obj"), not the source file
// ("test.c") `show globals` filters against, so compare by file stem
// (extension-independent) rather than the full name.
fn same_compilation_unit(compiland: &str, source_file: &str) -> bool {
    let stem = |s: &str| {
        std::path::Path::new(s)
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase())
    };
    match (stem(compiland), stem(source_file)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn new_symbol_info_buffer() -> Vec<u8> {
    let size = std::mem::size_of::<SYMBOL_INFOW>() + MAX_SYM_NAME * std::mem::size_of::<u16>();
    let mut buffer = vec![0u8; size];
    let symbol = buffer.as_mut_ptr() as *mut SYMBOL_INFOW;
    unsafe {
        (*symbol).SizeOfStruct = std::mem::size_of::<SYMBOL_INFOW>() as u32;
        (*symbol).MaxNameLen = MAX_SYM_NAME as u32;
    }
    buffer
}

#[derive(Debug, Clone)]
pub struct SymbolInfo {
    pub name: String,
    pub displacement: u64,
    pub line: Option<u32>,
    pub file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
pub struct VarLocation {
    pub address: u64,
    pub size: u32,
    /// 0 when dbghelp reported no type for the symbol.
    pub type_id: u32,
    pub mod_base: u64,
}

#[derive(Debug, Clone)]
pub struct VarSummary {
    pub name: String,
    pub address: u64,
    /// 0 when dbghelp reported no type for the symbol.
    pub type_id: u32,
    pub mod_base: u64,
    pub is_param: bool,
    /// dbghelp's own symbol index, needed to look up its lexical parent
    /// (compiland/source file) after enumeration.
    index: u32,
}

impl SymbolResolver {
    pub fn new(process: HANDLE, search_path: Option<&str>) -> Result<Self> {
        let search_path_w: Vec<u16> = search_path
            .map(|s| OsStr::new(s).encode_wide().chain(Some(0)).collect())
            .unwrap_or_else(|| vec![0]);

        unsafe {
            SymInitializeW(
                process,
                PCWSTR(search_path_w.as_ptr()),
                false,
            )?;
        }

        Ok(Self {
            process,
            initialized: true,
            main_module_base: std::cell::Cell::new(0),
        })
    }

    pub fn load_module(&self, file: HANDLE, base: u64) -> Result<u64> {
        let base_addr = unsafe {
            SymLoadModuleExW(
                self.process,
                file,
                PCWSTR::null(),
                PCWSTR::null(),
                base,
                0,
                None,
                SLMFLAG_NONE,
            )
        };

        if base_addr == 0 {
            return Err(DebuggerError::Windows(windows::core::Error::from_win32()));
        }

        if self.main_module_base.get() == 0 {
            self.main_module_base.set(base_addr);
        }

        Ok(base_addr)
    }

    // Walks up to `max_frames` stack frames starting at `ctx` (which must be a
    // full register set, not just CONTEXT_CONTROL), returning each frame's PC
    // (frame 0 is the current position). x64 has no reliable "return address
    // is at [rsp]" shortcut mid-function, so this uses dbghelp's cross-process
    // unwinder (StackWalk64) rather than hand-rolling frame-pointer chasing.
    pub fn stack_trace(
        &self,
        thread: HANDLE,
        ctx: &mut AlignedContext,
        max_frames: usize,
    ) -> Result<Vec<u64>> {
        let mut frame = STACKFRAME64::default();
        frame.AddrPC.Offset = ctx.Rip;
        frame.AddrPC.Mode = ADDR_MODE_FLAT;
        frame.AddrFrame.Offset = ctx.Rbp;
        frame.AddrFrame.Mode = ADDR_MODE_FLAT;
        frame.AddrStack.Offset = ctx.Rsp;
        frame.AddrStack.Mode = ADDR_MODE_FLAT;

        unsafe extern "system" fn read_memory(
            hprocess: HANDLE,
            base: u64,
            buffer: *mut std::ffi::c_void,
            size: u32,
            bytes_read: *mut u32,
        ) -> BOOL {
            let mut read = 0usize;
            let ok = unsafe {
                ReadProcessMemory(hprocess, base as *const _, buffer, size as usize, Some(&mut read))
            };
            unsafe {
                *bytes_read = read as u32;
            }
            BOOL(if ok.is_ok() { 1 } else { 0 })
        }

        unsafe extern "system" fn function_table_access(
            hprocess: HANDLE,
            addr_base: u64,
        ) -> *mut std::ffi::c_void {
            unsafe { SymFunctionTableAccess64(hprocess, addr_base) }
        }

        unsafe extern "system" fn get_module_base(hprocess: HANDLE, address: u64) -> u64 {
            unsafe { SymGetModuleBase64(hprocess, address) }
        }

        let mut frames = Vec::new();
        while frames.len() < max_frames {
            let ok = unsafe {
                StackWalk64(
                    IMAGE_FILE_MACHINE_AMD64,
                    self.process,
                    thread,
                    &mut frame,
                    &mut ctx.0 as *mut _ as *mut std::ffi::c_void,
                    Some(read_memory),
                    Some(function_table_access),
                    Some(get_module_base),
                    None,
                )
            };
            if ok.0 == 0 || frame.AddrPC.Offset == 0 {
                break;
            }
            frames.push(frame.AddrPC.Offset);
        }

        Ok(frames)
    }

    // Unwinds exactly one stack frame from `ctx` (which must be a full
    // register set, not just CONTEXT_CONTROL) to find the address the current
    // function will return to.
    pub fn unwind_return_address(&self, thread: HANDLE, ctx: &mut AlignedContext) -> Result<Option<u64>> {
        // Frame 0 re-resolves the current PC; frame 1 is the caller, whose
        // AddrPC is the return address.
        Ok(self.stack_trace(thread, ctx, 2)?.into_iter().nth(1))
    }

    // Breakpoints set on a bare function name should land past the prologue
    // (frame setup / parameter homing), like GDB's "skip prologue" behavior,
    // not on the function's raw entry instruction. The compiler attributes the
    // prologue to the function's opening line in the line table, and the first
    // real statement gets its own line entry starting right after it, so the
    // smallest line-table address inside the function that's greater than the
    // entry address is a reliable post-prologue breakpoint location.
    pub fn resolve_function_entry(&self, name: &str) -> Result<u64> {
        let name_w: Vec<u16> = OsStr::new(name).encode_wide().chain(Some(0)).collect();

        let mut buffer = new_symbol_info_buffer();
        let symbol = buffer.as_mut_ptr() as *mut SYMBOL_INFOW;

        unsafe {
            SymFromNameW(self.process, PCWSTR(name_w.as_ptr()), symbol)
                .map_err(|_| DebuggerError::SymbolNotFound(name.to_string()))?;
        }

        let (entry, size) = unsafe { ((*symbol).Address, (*symbol).Size as u64) };
        Ok(self.skip_prologue(entry, size).unwrap_or(entry))
    }

    fn skip_prologue(&self, entry: u64, size: u64) -> Option<u64> {
        let info = self.resolve(entry).ok()?;
        let file = info.file?.to_str()?.to_string();
        let limit = if size > 0 { entry + size } else { entry + 0x1000 };

        struct EnumState<'a> {
            query: &'a str,
            entry: u64,
            limit: u64,
            best: Option<u64>,
        }

        unsafe extern "system" fn callback(
            line_info: *const SRCCODEINFOW,
            user_context: *const std::ffi::c_void,
        ) -> BOOL {
            let state = unsafe { &mut *(user_context as *mut EnumState) };
            let info = unsafe { &*line_info };
            if info.Address > state.entry && info.Address < state.limit {
                let len = info.FileName.iter().position(|&c| c == 0).unwrap_or(0);
                let name = String::from_utf16_lossy(&info.FileName[..len]);
                if file_matches(&name, state.query) {
                    state.best = Some(state.best.map_or(info.Address, |b| b.min(info.Address)));
                }
            }
            BOOL(1)
        }

        let mut state = EnumState {
            query: &file,
            entry,
            limit,
            best: None,
        };

        unsafe {
            SymEnumLinesW(
                self.process,
                self.main_module_base.get(),
                PCWSTR::null(),
                PCWSTR::null(),
                Some(callback),
                Some(&mut state as *mut _ as *const std::ffi::c_void),
            )
            .ok()?;
        }

        state.best
    }

    pub fn resolve_by_file_line(&self, file: &str, line: u32) -> Result<u64> {
        struct EnumState<'a> {
            query: &'a str,
            results: Vec<(u32, u64)>,
        }

        unsafe extern "system" fn callback(
            line_info: *const SRCCODEINFOW,
            user_context: *const std::ffi::c_void,
        ) -> BOOL {
            let state = unsafe { &mut *(user_context as *mut EnumState) };
            let info = unsafe { &*line_info };
            let len = info.FileName.iter().position(|&c| c == 0).unwrap_or(0);
            let name = String::from_utf16_lossy(&info.FileName[..len]);
            if file_matches(&name, state.query) {
                state.results.push((info.LineNumber, info.Address));
            }
            BOOL(1)
        }

        let mut state = EnumState {
            query: file,
            results: Vec::new(),
        };

        unsafe {
            SymEnumLinesW(
                self.process,
                self.main_module_base.get(),
                PCWSTR::null(),
                PCWSTR::null(),
                Some(callback),
                Some(&mut state as *mut _ as *const std::ffi::c_void),
            )
            .map_err(|_| DebuggerError::SymbolNotFound(format!("{}:{}", file, line)))?;
        }

        let mut results = state.results;
        results.sort_by_key(|&(l, _)| l);
        results
            .iter()
            .find(|&&(l, _)| l == line)
            .or_else(|| results.iter().find(|&&(l, _)| l >= line))
            .map(|&(_, addr)| addr)
            .ok_or_else(|| DebuggerError::SymbolNotFound(format!("{}:{}", file, line)))
    }

    // Resolves a name typed into `print`/`set` to an address, size, and type.
    // Locals are stack-relative (SYMFLAG_REGREL: `Address` is an offset from
    // whichever register `Register` names, not an absolute address), so this
    // establishes the current stack frame via SymSetContext first and then
    // computes the effective address using the caller's live register values.
    pub fn resolve_variable(&self, ctx: &AlignedContext, name: &str) -> Result<VarLocation> {
        let mut frame = IMAGEHLP_STACK_FRAME::default();
        frame.InstructionOffset = ctx.Rip;
        frame.FrameOffset = ctx.Rbp;
        frame.StackOffset = ctx.Rsp;

        unsafe {
            // dbghelp documents that the very first call in a session returns
            // an error (nothing to compare the new context against yet); the
            // lookup below still works, so the result is intentionally ignored.
            let _ = SymSetContext(self.process, &frame, None);
        }

        let name_w: Vec<u16> = OsStr::new(name).encode_wide().chain(Some(0)).collect();
        let mut buffer = new_symbol_info_buffer();
        let symbol = buffer.as_mut_ptr() as *mut SYMBOL_INFOW;

        unsafe {
            SymFromNameW(self.process, PCWSTR(name_w.as_ptr()), symbol)
                .map_err(|_| DebuggerError::SymbolNotFound(name.to_string()))?;
        }

        let (type_id, mod_base) = unsafe { ((*symbol).TypeIndex, (*symbol).ModBase) };
        let base_addr = resolve_symbol_address(Some(ctx), unsafe { &*symbol }).ok_or_else(|| {
            DebuggerError::Other(format!("unsupported register-relative base for '{}'", name))
        })?;

        let size = match self.type_size(mod_base, type_id) {
            0 => 4,
            n => n,
        };

        Ok(VarLocation {
            address: base_addr,
            size,
            type_id,
            mod_base,
        })
    }

    // Enumerates the parameters and local variables visible at `ctx`'s
    // current instruction (both come back together; `show locals`/`show
    // args` split them via `is_param`). Requires the same SymSetContext dance
    // as resolve_variable, since dbghelp only reports locals for an
    // established stack frame.
    pub fn enum_locals(&self, ctx: &AlignedContext) -> Result<Vec<VarSummary>> {
        let mut frame = IMAGEHLP_STACK_FRAME::default();
        frame.InstructionOffset = ctx.Rip;
        frame.FrameOffset = ctx.Rbp;
        frame.StackOffset = ctx.Rsp;
        unsafe {
            let _ = SymSetContext(self.process, &frame, None);
        }

        struct EnumState<'a> {
            process: HANDLE,
            ctx: &'a AlignedContext,
            results: Vec<VarSummary>,
        }

        unsafe extern "system" fn callback(
            psyminfo: *const SYMBOL_INFOW,
            _size: u32,
            user_context: *const std::ffi::c_void,
        ) -> BOOL {
            let state = unsafe { &mut *(user_context as *mut EnumState) };
            let sym = unsafe { &*psyminfo };

            let mut data_kind: u32 = 0;
            let ok = unsafe {
                SymGetTypeInfo(
                    state.process,
                    sym.ModBase,
                    sym.Index,
                    TI_GET_DATAKIND,
                    &mut data_kind as *mut _ as *mut std::ffi::c_void,
                )
            };
            if ok.is_err() {
                return BOOL(1);
            }
            let is_param = data_kind == DATA_IS_PARAM;
            if !is_param && data_kind != DATA_IS_LOCAL && data_kind != DATA_IS_STATIC_LOCAL {
                return BOOL(1);
            }

            let Some(address) = resolve_symbol_address(Some(state.ctx), sym) else {
                return BOOL(1);
            };
            let name = symbol_name(sym);

            state.results.push(VarSummary {
                name,
                address,
                type_id: sym.TypeIndex,
                mod_base: sym.ModBase,
                is_param,
                index: sym.Index,
            });
            BOOL(1)
        }

        let mut state = EnumState {
            process: self.process,
            ctx,
            results: Vec::new(),
        };
        unsafe {
            let _ = SymEnumSymbolsW(
                self.process,
                0,
                PCWSTR::null(),
                Some(callback),
                Some(&mut state as *mut _ as *const std::ffi::c_void),
            );
        }
        Ok(state.results)
    }

    // Determines the primary source file the debuggee was compiled from, by
    // resolving where its `main` function is defined. Used to filter `show
    // globals` down to the user's own globals instead of the
    // statically-linked CRT's.
    pub fn main_source_file(&self) -> Option<String> {
        let entry = self.resolve_function_entry("main").ok()?;
        let info = self.resolve(entry).ok()?;
        info.file?.to_str().map(|s| s.to_string())
    }

    // The source file (compiland) a symbol was defined in, resolved via its
    // lexical parent. Returns None for symbols dbghelp can't attribute to a
    // compiland (e.g. synthetic/imported symbols).
    fn symbol_source_file(&self, mod_base: u64, index: u32) -> Option<String> {
        let parent_id = self.get_type_info_u32(mod_base, index, TI_GET_LEXICALPARENT)?;
        let mut name_ptr = PWSTR::null();
        let ok = unsafe {
            SymGetTypeInfo(
                self.process,
                mod_base,
                parent_id,
                TI_GET_SYMNAME,
                &mut name_ptr as *mut _ as *mut std::ffi::c_void,
            )
        };
        if ok.is_err() || name_ptr.is_null() {
            return None;
        }
        let name = unsafe { name_ptr.to_string() }.ok();
        unsafe {
            let _ = LocalFree(HLOCAL(name_ptr.as_ptr() as *mut std::ffi::c_void));
        }
        name
    }

    // Enumerates module-level globals (including file-static globals).
    // `source_file` restricts results to symbols defined in a matching
    // compiland (see `symbol_source_file`); pass None for every global in
    // the module, including the statically-linked CRT's.
    pub fn enum_globals(&self, source_file: Option<&str>) -> Result<Vec<VarSummary>> {
        struct EnumState {
            process: HANDLE,
            results: Vec<VarSummary>,
        }

        unsafe extern "system" fn callback(
            psyminfo: *const SYMBOL_INFOW,
            _size: u32,
            user_context: *const std::ffi::c_void,
        ) -> BOOL {
            let state = unsafe { &mut *(user_context as *mut EnumState) };
            let sym = unsafe { &*psyminfo };

            if sym.Tag != SYM_TAG_DATA {
                return BOOL(1);
            }

            let mut data_kind: u32 = 0;
            let ok = unsafe {
                SymGetTypeInfo(
                    state.process,
                    sym.ModBase,
                    sym.Index,
                    TI_GET_DATAKIND,
                    &mut data_kind as *mut _ as *mut std::ffi::c_void,
                )
            };
            if ok.is_err() || (data_kind != DATA_IS_GLOBAL && data_kind != DATA_IS_FILE_STATIC) {
                return BOOL(1);
            }

            let Some(address) = resolve_symbol_address(None, sym) else {
                return BOOL(1);
            };
            let name = symbol_name(sym);

            state.results.push(VarSummary {
                name,
                address,
                type_id: sym.TypeIndex,
                mod_base: sym.ModBase,
                is_param: false,
                index: sym.Index,
            });
            BOOL(1)
        }

        let mut state = EnumState {
            process: self.process,
            results: Vec::new(),
        };
        unsafe {
            let _ = SymEnumSymbolsW(
                self.process,
                self.main_module_base.get(),
                PCWSTR::null(),
                Some(callback),
                Some(&mut state as *mut _ as *const std::ffi::c_void),
            );
        }

        let mut results = state.results;
        if let Some(source_file) = source_file {
            results.retain(|v| {
                self.symbol_source_file(v.mod_base, v.index)
                    .is_some_and(|f| same_compilation_unit(&f, source_file))
            });
        }
        Ok(results)
    }

    fn get_type_info_u32(&self, mod_base: u64, type_id: u32, kind: IMAGEHLP_SYMBOL_TYPE_INFO) -> Option<u32> {
        let mut value: u32 = 0;
        unsafe {
            SymGetTypeInfo(self.process, mod_base, type_id, kind, &mut value as *mut _ as *mut _).ok()?;
        }
        Some(value)
    }

    pub fn type_tag(&self, mod_base: u64, type_id: u32) -> Option<u32> {
        self.get_type_info_u32(mod_base, type_id, TI_GET_SYMTAG)
    }

    pub fn type_base_type(&self, mod_base: u64, type_id: u32) -> Option<u32> {
        self.get_type_info_u32(mod_base, type_id, TI_GET_BASETYPE)
    }

    // Pointee type for a pointer, or element type for an array.
    pub fn type_pointee(&self, mod_base: u64, type_id: u32) -> Option<u32> {
        self.get_type_info_u32(mod_base, type_id, TI_GET_TYPE)
    }

    pub fn type_size(&self, mod_base: u64, type_id: u32) -> u32 {
        if type_id == 0 {
            return 0;
        }
        let mut size: u64 = 0;
        unsafe {
            let _ = SymGetTypeInfo(
                self.process,
                mod_base,
                type_id,
                TI_GET_LENGTH,
                &mut size as *mut u64 as *mut std::ffi::c_void,
            );
        }
        size as u32
    }

    // The child type indices of a struct/union/array (member variables,
    // element type, etc). TI_FINDCHILDREN_PARAMS::ChildId is (like
    // SYMBOL_INFOW::Name) a C flexible-array member declared as `[u32; 1]`,
    // so the backing buffer must be sized for `Count` entries up front.
    fn type_children(&self, mod_base: u64, type_id: u32) -> Vec<u32> {
        let Some(count) = self.get_type_info_u32(mod_base, type_id, TI_GET_CHILDRENCOUNT) else {
            return Vec::new();
        };
        if count == 0 {
            return Vec::new();
        }

        let header_size = std::mem::size_of::<TI_FINDCHILDREN_PARAMS>();
        let buf_size = header_size + (count as usize - 1) * std::mem::size_of::<u32>();
        let mut buffer = vec![0u8; buf_size];
        let params = buffer.as_mut_ptr() as *mut TI_FINDCHILDREN_PARAMS;
        let ok = unsafe {
            (*params).Count = count;
            (*params).Start = 0;
            SymGetTypeInfo(
                self.process,
                mod_base,
                type_id,
                TI_FINDCHILDREN,
                params as *mut _ as *mut std::ffi::c_void,
            )
        };
        if ok.is_err() {
            return Vec::new();
        }

        unsafe { std::slice::from_raw_parts((*params).ChildId.as_ptr(), count as usize) }.to_vec()
    }

    fn type_child_name(&self, mod_base: u64, child_id: u32) -> Option<String> {
        let mut name_ptr = PWSTR::null();
        let ok = unsafe {
            SymGetTypeInfo(
                self.process,
                mod_base,
                child_id,
                TI_GET_SYMNAME,
                &mut name_ptr as *mut _ as *mut std::ffi::c_void,
            )
        };
        if ok.is_err() || name_ptr.is_null() {
            return None;
        }
        let name = unsafe { name_ptr.to_string() }.ok();
        unsafe {
            let _ = LocalFree(HLOCAL(name_ptr.as_ptr() as *mut std::ffi::c_void));
        }
        name
    }

    // Looks up a named field of a struct/union type, returning its byte
    // offset and type.
    pub fn type_member(&self, mod_base: u64, type_id: u32, field: &str) -> Option<(u32, u32)> {
        for child_id in self.type_children(mod_base, type_id) {
            if self.type_child_name(mod_base, child_id).as_deref() != Some(field) {
                continue;
            }
            let offset = self.get_type_info_u32(mod_base, child_id, TI_GET_OFFSET)?;
            let member_type = self.get_type_info_u32(mod_base, child_id, TI_GET_TYPEID)?;
            return Some((offset, member_type));
        }
        None
    }

    // Every field of a struct/union type, in declaration order, as
    // (name, offset, member type id). Used to pretty-print a whole struct
    // for `print`/`show`.
    pub fn type_members(&self, mod_base: u64, type_id: u32) -> Vec<(String, u32, u32)> {
        let mut members = Vec::new();
        for child_id in self.type_children(mod_base, type_id) {
            let Some(name) = self.type_child_name(mod_base, child_id) else {
                continue;
            };
            let Some(offset) = self.get_type_info_u32(mod_base, child_id, TI_GET_OFFSET) else {
                continue;
            };
            let Some(member_type) = self.get_type_info_u32(mod_base, child_id, TI_GET_TYPEID) else {
                continue;
            };
            members.push((name, offset, member_type));
        }
        members
    }

    // Element count of an array type.
    pub fn type_count(&self, mod_base: u64, type_id: u32) -> Option<u32> {
        self.get_type_info_u32(mod_base, type_id, TI_GET_COUNT)
    }

    // A human-readable C type name ("int", "struct Point *", "char [16]"),
    // for `set print pretty`'s "(type) value" prefix. type_id == 0 means "no
    // type info" (e.g. an untyped literal), reported as "void" since that's
    // dbghelp's own convention for it elsewhere.
    pub fn type_name(&self, mod_base: u64, type_id: u32) -> String {
        if type_id == 0 {
            return "void".to_string();
        }
        let Some(tag) = self.type_tag(mod_base, type_id) else {
            return "?".to_string();
        };
        match tag {
            SYM_TAG_POINTER_TYPE => {
                let pointee = self.type_pointee(mod_base, type_id).unwrap_or(0);
                format!("{} *", self.type_name(mod_base, pointee))
            }
            SYM_TAG_ARRAY_TYPE => {
                let elem = self.type_pointee(mod_base, type_id).unwrap_or(0);
                let count = self.type_count(mod_base, type_id).unwrap_or(0);
                format!("{} [{}]", self.type_name(mod_base, elem), count)
            }
            SYM_TAG_BASE_TYPE => {
                let bt = self.type_base_type(mod_base, type_id).unwrap_or(0);
                let size = self.type_size(mod_base, type_id);
                base_type_name(bt, size)
            }
            SYM_TAG_UDT => {
                let name = self
                    .type_child_name(mod_base, type_id)
                    .unwrap_or_else(|| "?".to_string());
                match self.get_type_info_u32(mod_base, type_id, TI_GET_UDTKIND) {
                    Some(UDT_KIND_UNION) => format!("union {}", name),
                    Some(UDT_KIND_CLASS) => format!("class {}", name),
                    // UdtStruct, UdtInterface, UdtTaggedUnion, and anything
                    // dbghelp doesn't report a kind for all read naturally as
                    // "struct" in C.
                    _ => format!("struct {}", name),
                }
            }
            _ => self
                .type_child_name(mod_base, type_id)
                .unwrap_or_else(|| "?".to_string()),
        }
    }

    pub fn resolve(&self, address: u64) -> Result<SymbolInfo> {
        let mut buffer = new_symbol_info_buffer();
        let symbol = buffer.as_mut_ptr() as *mut SYMBOL_INFOW;

        let mut displacement = 0u64;
        unsafe {
            SymFromAddrW(self.process, address, Some(&mut displacement), symbol)?;
        }

        let (name_ptr, max_name_len) = unsafe { ((*symbol).Name.as_ptr(), (*symbol).MaxNameLen) };
        let name_len = (0..max_name_len)
            .position(|i| unsafe { *name_ptr.add(i as usize) } == 0)
            .unwrap_or(0);
        let name =
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(name_ptr, name_len) });

        let mut line_info: IMAGEHLP_LINEW64 = unsafe { std::mem::zeroed() };
        line_info.SizeOfStruct = std::mem::size_of::<IMAGEHLP_LINEW64>() as u32;
        let mut line_displacement = 0u32;
        let line_result = unsafe {
            SymGetLineFromAddrW64(
                self.process,
                address,
                &mut line_displacement,
                &mut line_info,
            )
        };

        let (line, file) = match line_result {
            Ok(_) => {
                let file = if line_info.FileName.0.is_null() {
                    None
                } else {
                    let len = (0..)
                        .position(|i| unsafe { *line_info.FileName.0.add(i) } == 0)
                        .unwrap_or(0);
                    let s = String::from_utf16_lossy(unsafe {
                        std::slice::from_raw_parts(line_info.FileName.0, len)
                    });
                    Some(PathBuf::from(s))
                };
                (Some(line_info.LineNumber), file)
            }
            Err(_) => (None, None),
        };

        Ok(SymbolInfo {
            name,
            displacement,
            line,
            file,
        })
    }
}

impl Drop for SymbolResolver {
    fn drop(&mut self) {
        if self.initialized {
            unsafe {
                let _ = SymCleanup(self.process);
            }
        }
    }
}
