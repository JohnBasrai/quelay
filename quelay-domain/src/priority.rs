/// Operator-visible priority levels for streams and file transfers.
///
/// `Priority` is the external concept. The DRR scheduler maps each level
/// to an internal quantum (weight) and may adjust dynamically. More levels
/// can be added here without changing scheduler internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    // ---
    /// Command and control traffic. Very small payloads, very rare.
    /// Bypasses DRR entirely via strict-priority queue; always drained
    /// before any BulkTransfer stream is scheduled.
    C2I,

    /// All file and stream transfers. Scheduled fairly via DRR among
    /// all active streams at this level.
    BulkTransfer,
}

// ---

impl Priority {
    // ---
    /// Initial DRR quantum (bytes) assigned when a stream is registered.
    /// C2I gets a large quantum so it drains immediately when it has data.
    /// BulkTransfer starts equal; the scheduler may adjust dynamically.
    pub fn initial_quantum(&self) -> u32 {
        // ---
        match self {
            Priority::C2I => 65_536,
            Priority::BulkTransfer => 8_192,
        }
    }

    /// Map a raw Thrift priority byte (0..=127) to a `Priority` level.
    ///
    /// Convention (mirrors the IDL constants):
    ///   0..=63   → BulkTransfer
    ///   64..=127 → C2I
    pub fn from_i8(v: i8) -> Self {
        // ---
        if v >= 64 {
            Priority::C2I
        } else {
            Priority::BulkTransfer
        }
    }

    // ---

    /// Returns `true` if this level bypasses DRR and is sent as soon as
    /// bandwidth allows (strict priority).
    pub fn is_strict(&self) -> bool {
        // ---
        matches!(self, Priority::C2I)
    }
}
