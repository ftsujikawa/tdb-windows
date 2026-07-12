use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Breakpoint {
    pub id: usize,
    pub address: usize,
    pub original_byte: u8,
    pub enabled: bool,
}

#[derive(Debug, Default)]
pub struct BreakpointManager {
    breakpoints: HashMap<usize, Breakpoint>,
    next_id: usize,
}

impl BreakpointManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, address: usize, original_byte: u8) -> usize {
        self.next_id += 1;
        let id = self.next_id;
        self.breakpoints.insert(
            address,
            Breakpoint {
                id,
                address,
                original_byte,
                enabled: true,
            },
        );
        id
    }

    pub fn remove_by_id(&mut self, id: usize) -> Option<Breakpoint> {
        let address = self.breakpoints.values().find(|bp| bp.id == id)?.address;
        self.breakpoints.remove(&address)
    }

    #[allow(dead_code)]
    pub fn get(&self, address: usize) -> Option<&Breakpoint> {
        self.breakpoints.get(&address)
    }

    pub fn get_mut(&mut self, address: usize) -> Option<&mut Breakpoint> {
        self.breakpoints.get_mut(&address)
    }

    pub fn list(&self) -> impl Iterator<Item = &Breakpoint> {
        self.breakpoints.values()
    }

    pub fn contains(&self, address: usize) -> bool {
        self.breakpoints.contains_key(&address)
    }
}
