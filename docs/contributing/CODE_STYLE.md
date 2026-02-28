# Code Style

## Formatter

All Rust code is formatted with `rustfmt`. Run before every commit:

```bash
cargo fmt --all
```

The CI lint script enforces this.

## Linter

```bash
cargo clippy --all-targets -- -D warnings
```

## Section Separators

Use `// ---` to separate logical sections within a file — between top-level
items, between `use` groups, and inside blocks.  This creates visual rhythm
without relying on blank lines alone.  See the sep being used between
the stuct and the unrelated `impl` block.

```rust
pub struct Foo {

    x: u32,
}

// ---

impl Foo {

    pub fn new(x: u32) -> Self { Self { x } }
}
```

Trivial structs and their `impl` blocks do not require an opening `// ---`;
the rule is described under **Opening separator** below.

## Import Grouping

Separate `use` blocks into groups with `// ---` between them:

1. `std::` imports
2. Third-party crates (`tokio`, `anyhow`, etc.)
3. Workspace crates (`quelay_thrift`, `quelay_domain`, etc.)
4. Local (`super::`, `crate::`)

```rust
use std::collections::HashMap;
use std::time::Duration;

// ---

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

// ---

use quelay_thrift::LinkState;

// ---

use super::Role;
use super::TunerCmd;
```

When importing several symbols from one crate, use a brace group with
`// ---` after the opening brace.  This forces `rustfmt` to keep one symbol
per line and preserves the visual grouping:

```rust
use quelay_thrift::{
    // ---
    QueLayAgentSyncClient,
    QueLayCallbackSyncProcessor,
    StreamInfo,
    TIoChannel,
    TQueLayAgentSyncClient,
    TServer,
    TTcpChannel,
};
```

For small imports (two or three symbols) a single-line brace group is also
acceptable when the line stays well within the right margin.

## Type and `impl` Block Layout

### Section banners

Precede every top-level type, `impl` block, and free-function group with a
banner comment.  The long-dash banner is the standard form:

```rust
// ---------------------------------------------------------------------------
// MyType
// ---------------------------------------------------------------------------
```

Shorter inline banners are used inside `enum` and `struct` bodies to label
sub-groups (see **Enum variants** below).

### Opening separator

Place `// ---` as the first line inside `struct`, `enum`, and `impl` opening
braces.  This visually separates the opening brace from the first item and
makes the block boundary clear at a glance:

```rust
pub struct Cic {
    // ---
    cfg: CicConfig,
    cic_rx: mpsc::Receiver<CicMsg>,
}

impl Cic {
    // ---

    pub fn new(cfg: CicConfig) -> Self { /* ... */ }
}
```

**Exceptions — omit `// ---` when:**

- The struct is trivial (one or two fields, no grouping needed):
  ```rust
  pub struct TunerPair {
      pub sender_uuid: String,
      pub receiver_uuid: String,
  }
  ```
- The first item inside an `impl` block already carries a doc comment; the
  doc comment itself creates sufficient visual separation:
  ```rust
  impl Cic {
      /// Run the dispatch loop until all tuners finish.
      pub async fn run(mut self) -> anyhow::Result<Vec<TunerResult>> { /* ... */ }
  }
  ```

### Enum variants

Separate enum variants with a blank line.  Use short inline comments to
label logical sub-groups:

```rust
pub enum CicMsg {
    // --- from callbacks (UUID-bearing → routed to the matching tuner) ---
    /// Agent opened an ephemeral port for this stream.
    StreamStarted { role: Role, uuid: String, port: u16 },

    /// Agent finished transferring this stream normally.
    StreamDone { role: Role, uuid: String, bytes: u64 },

    // --- from tuner tasks ---
    /// A tuner task has completed and is about to exit.
    TunerFinished { uuid: String, role: Role, result: TunerResult },
}
```

### Struct fields

Separate logically distinct field groups with a blank line.  When every
field carries a doc comment, precede each doc comment with a blank line:

```rust
pub struct Cic {
    // ---
    cfg: CicConfig,
    cic_rx: mpsc::Receiver<CicMsg>,
    cic_tx: mpsc::Sender<CicMsg>,

    /// Master dispatch map: `(uuid, role)` → tuner command channel.
    dispatch: HashMap<TunerJobKey, mpsc::Sender<TunerCmd>>,

    /// Paired UUIDs — used for coordinated shutdown.
    pairs: Vec<TunerPair>,

    /// Collected results, filled as `TunerFinished` messages arrive.
    results: Vec<TunerResult>,
}
```

### Function body opening

Place `// ---` as the first statement inside non-trivial function bodies.
This visually separates the function signature from the start of the body,
which is especially helpful when the signature spans multiple lines:

```rust
pub fn register(
    &mut self,
    uuid: String,
    role: Role,
    cmd_tx: mpsc::Sender<TunerCmd>,
) {
    // ---
    self.dispatch.insert((uuid.clone(), role), cmd_tx);
}
```

Short single-expression functions and closures may omit it.

### Inline step comments

Use plain `//` comments to label distinct processing phases inside a
function body:

```rust
// Join all handles for cleanup; results already collected via channel.
for ((uuid, role), handle) in self.handles {
    // ...
}
```

## Module Files (`mod.rs` / `lib.rs`)

Place a brief block comment between the `mod` declarations and the
`pub use` re-exports to signal the boundary:

```rust
mod cic;
mod tuner;

// --- public API ---

pub use cic::{Cic, CicHandle, CicMsg};
pub use tuner::TunerResult;
```

## Module Structure — EMBP

This project follows the
[Explicit Module Boundary Pattern (EMBP)](https://github.com/JohnBasrai/architecture-patterns/blob/main/rust/embp.md).

Key rules:
- Module files use `mod submodule;` (never `pub mod`)
- `lib.rs` / `mod.rs` gateways use `pub use` to control the public API
- Siblings import from each other via `super::Symbol`
- External modules import via `crate::module::Symbol`
- Trait pointer types are co-located with their trait definition

See [ARCHITECTURE.md](ARCHITECTURE.md) for crate-level structure.

## Naming

- Types and traits: `UpperCamelCase`
- Functions and methods: `snake_case`
- Constants: `UPPER_SNAKE_CASE`
- Crates and modules: `snake_case`

## Error Handling

- Use `thiserror` for library error types.
- Use `QueLayError` from `quelay-domain` as the workspace error type.
- Never use `unwrap()` in library code; use `?` or explicit error handling.
- `unwrap()` is acceptable in tests.

## Doc Comments

- All public types and functions must have doc comments (`///`).
- Private helpers may use `//` or `///` at author discretion.
- Use `# Examples` sections in doc comments for non-trivial public APIs.

## Async

- All async code uses `tokio`.
- Trait methods that must be async use `#[async_trait]` from the
  `async-trait` crate until `async fn in trait` stabilises.
