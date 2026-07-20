use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Breakpoint {
    pub id: usize,
    pub pid: u32,
    pub address: usize,
    pub original_byte: u8,
    pub enabled: bool,
}

// Keyed by (pid, address) rather than just address: with child-process
// debugging, two different processes can legitimately have the same address
// mapped (e.g. two instances of the same non-ASLR binary), and a plain
// address key would let one clobber the other.
#[derive(Debug, Default)]
pub struct BreakpointManager {
    breakpoints: HashMap<(u32, usize), Breakpoint>,
    next_id: usize,
}

impl BreakpointManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, pid: u32, address: usize, original_byte: u8) -> usize {
        self.next_id += 1;
        let id = self.next_id;
        self.breakpoints.insert(
            (pid, address),
            Breakpoint {
                id,
                pid,
                address,
                original_byte,
                enabled: true,
            },
        );
        id
    }

    pub fn remove_by_id(&mut self, id: usize) -> Option<Breakpoint> {
        let key = self
            .breakpoints
            .values()
            .find(|bp| bp.id == id)
            .map(|bp| (bp.pid, bp.address))?;
        self.breakpoints.remove(&key)
    }

    #[allow(dead_code)]
    pub fn get(&self, pid: u32, address: usize) -> Option<&Breakpoint> {
        self.breakpoints.get(&(pid, address))
    }

    pub fn get_mut(&mut self, pid: u32, address: usize) -> Option<&mut Breakpoint> {
        self.breakpoints.get_mut(&(pid, address))
    }

    pub fn list(&self) -> impl Iterator<Item = &Breakpoint> {
        self.breakpoints.values()
    }

    pub fn contains(&self, pid: u32, address: usize) -> bool {
        self.breakpoints.contains_key(&(pid, address))
    }
}
