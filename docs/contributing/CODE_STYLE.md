# Code Style

## Format and Lint

`cargo xfmt` is authoritative for Rust layout. It applies this repository's
`rustfmt.toml`, including nightly formatter options. Run it after changing Rust
code:

```bash
cargo xfmt
```

Check formatting without modifying files:

```bash
cargo xfmt --check
```

CI also requires:

```bash
cargo clippy --all-targets -- -D warnings
```

Do not substitute `cargo fmt`: it does not apply the full repository formatting
contract. Do not manually enforce brace position, line width, or import layout;
the formatter owns those decisions.

## Layout Conventions

Use `// ---` only to separate meaningful sections: top-level items, import
groups, or substantial groups inside a type or block. Do not use a bare
`// ---` as the first item after an opening brace; the Allman brace style
already provides that separation.

```rust
use std::time::Duration;

// ---

use tokio::sync::mpsc;

// ---

use crate::scheduler::DrrScheduler;
```

For a large import from one crate, `// ---` immediately inside the import
brace group keeps one symbol per line:

```rust
use quelay_thrift::{
    // ---
    LinkState,
    StreamInfo,
    TServer,
};
```

Use banner comments to introduce top-level types, `impl` blocks, and
free-function groups. Use descriptive inline comments to label distinct enum
or field groups and processing phases; comments should explain the grouping or
reason, not merely create vertical space.

```rust
// ---------------------------------------------------------------------------
// SessionManager
// ---------------------------------------------------------------------------
```

## Project Conventions

Quelay follows the [Explicit Module Boundary Pattern (EMBP)](https://github.com/JohnBasrai/architecture-patterns/blob/main/rust/embp.md).
The [architecture guide](ARCHITECTURE.md) is the source of truth for crate
layering and module boundaries. In particular, use private `mod` declarations
and curated `pub use` re-exports from each crate gateway.

- Use `thiserror` for library errors and `QueLayError` from `quelay-domain` as
  the workspace error type.
- Do not use `unwrap()` in library code; use `?` or handle the failure
  explicitly. `unwrap()` is acceptable in tests.
- Public types and functions need `///` documentation. Include `# Examples`
  for non-trivial public APIs.
- Use Tokio for asynchronous work. Prefer native async trait methods where
  dynamic dispatch is not needed; retain `#[async_trait]` for dyn-compatible
  boundaries such as `Box<dyn QueLayStream>`.
