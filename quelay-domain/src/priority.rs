/// Operator-visible priority levels for streams and file transfers.
///
/// `Priority` is the external concept. The DRR scheduler maps each level
/// to an internal quantum (weight) and may adjust dynamically. More levels
/// can be added here without changing scheduler internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Priority(i8);

const STRICT_MIN: i8 = 64;

// ---

impl Priority {
    // ---
    /// Initial DRR quantum (bytes) assigned when a stream is registered.
    /// C2I gets a large quantum so it drains immediately when it has data.
    /// BulkTransfer starts equal; the scheduler may adjust dynamically.
    pub fn initial_quantum(&self) -> u32 {
        // ---
        if self.is_strict() {
            65_536_u32
        } else {
            8_192_u32
        }
    }

    /// Map a raw Thrift priority byte (0..=127) to a `Priority` level.
    pub fn from_i8(value: i8) -> Self {
        // ---
        Self(value)
    }

    /// Return the raw priority value.
    pub fn as_i8(self) -> i8 {
        // ---
        self.0
    }

    /// Default DRR quantum in bytes for bulk transfer streams (priority < STRICT_MIN).
    /// All bulk streams start equal; the scheduler may adjust dynamically.
    pub fn bulk_quantum() -> u32 {
        8_192
    }

    /// Minimum priority value for strict-priority (c2i or non-DRR) scheduling.
    /// Streams at or above this priority bypass DRR and are always drained
    /// before any bulk stream.
    pub fn c2i_priority_min() -> Self {
        // ---
        Self(STRICT_MIN)
    }

    // ---

    /// Returns `true` if this level bypasses DRR and is sent as soon as
    /// bandwidth allows (strict priority).
    pub fn is_strict(&self) -> bool {
        // ---
        self.0 >= 64
    }
}
