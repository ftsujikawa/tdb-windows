// Hardware (debug-register) watchpoints for x64. The CPU has exactly four
// address-match slots (DR0-DR3, enabled/configured via DR7), so at most four
// watchpoints can be active at once; this manager tracks which of the four
// slots are in use and builds the DR0-DR3/DR7 values a thread's debug
// registers should hold to reflect them.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchKind {
    // Triggers on data writes only (DR7 condition bits 01).
    Write,
    // Triggers on data reads or writes (DR7 condition bits 11). x86 debug
    // registers have no "read-only" condition, so a read-watchpoint is
    // implemented as this same "access" kind.
    Access,
}

impl std::fmt::Display for WatchKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WatchKind::Write => write!(f, "write"),
            WatchKind::Access => write!(f, "read/write"),
        }
    }
}

impl WatchKind {
    fn condition_bits(self) -> u64 {
        match self {
            WatchKind::Write => 0b01,
            WatchKind::Access => 0b11,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Watchpoint {
    pub id: usize,
    pub expr: String,
    pub address: usize,
    pub size: u32,
    pub kind: WatchKind,
    // Last observed scalar value, cached so a hit can report "old -> new";
    // None if the expression isn't scalar-readable.
    pub last_value: Option<i64>,
}

// Maps a requested byte size to the nearest size a debug register can
// actually match (1, 2, 4, or 8 bytes); the caller rounds addresses/reads
// accordingly. Returns None if the expression is too wide for a single
// hardware slot to cover.
pub fn hw_size(requested: u32) -> Option<u32> {
    match requested {
        0 | 1 => Some(1),
        2 => Some(2),
        3 | 4 => Some(4),
        5..=8 => Some(8),
        _ => None,
    }
}

fn len_bits(size: u32) -> u64 {
    match size {
        1 => 0b00,
        2 => 0b01,
        8 => 0b10,
        _ => 0b11, // 4 bytes
    }
}

#[derive(Debug, Default)]
pub struct WatchpointManager {
    slots: [Option<Watchpoint>; 4],
    next_id: usize,
}

impl WatchpointManager {
    pub fn new() -> Self {
        Self::default()
    }

    // Installs `watchpoint` in the first free hardware slot, or None if all
    // four are already in use.
    pub fn add(
        &mut self,
        expr: String,
        address: usize,
        size: u32,
        kind: WatchKind,
        last_value: Option<i64>,
    ) -> Option<usize> {
        let slot = self.slots.iter().position(|s| s.is_none())?;
        self.next_id += 1;
        let id = self.next_id;
        self.slots[slot] = Some(Watchpoint {
            id,
            expr,
            address,
            size,
            kind,
            last_value,
        });
        Some(id)
    }

    pub fn remove_by_id(&mut self, id: usize) -> Option<Watchpoint> {
        let slot = self
            .slots
            .iter()
            .position(|s| s.as_ref().is_some_and(|w| w.id == id))?;
        self.slots[slot].take()
    }

    pub fn list(&self) -> impl Iterator<Item = &Watchpoint> {
        self.slots.iter().filter_map(|s| s.as_ref())
    }

    pub fn get_by_slot(&self, slot: usize) -> Option<&Watchpoint> {
        self.slots.get(slot)?.as_ref()
    }

    pub fn get_by_slot_mut(&mut self, slot: usize) -> Option<&mut Watchpoint> {
        self.slots.get_mut(slot)?.as_mut()
    }

    // Slot indices whose DR6 "hit" bit (bit i, i in 0..4) is set in `mask`.
    pub fn slots_hit(&self, mask: u8) -> Vec<usize> {
        (0..4)
            .filter(|&i| mask & (1 << i) != 0 && self.slots[i].is_some())
            .collect()
    }

    // The DR0-DR3/DR7 values a thread's debug registers should hold to
    // reflect the currently active watchpoints. Empty slots leave their DRi
    // at 0 and their DR7 enable bit clear.
    pub fn dr7_and_addrs(&self) -> (u64, u64, u64, u64, u64) {
        let mut dr = [0u64; 4];
        let mut dr7 = 0u64;
        for (i, slot) in self.slots.iter().enumerate() {
            if let Some(wp) = slot {
                dr[i] = wp.address as u64;
                dr7 |= 1 << (i * 2); // local enable (Li)
                dr7 |= wp.kind.condition_bits() << (16 + i * 4);
                dr7 |= len_bits(wp.size) << (18 + i * 4);
            }
        }
        (dr[0], dr[1], dr[2], dr[3], dr7)
    }
}
