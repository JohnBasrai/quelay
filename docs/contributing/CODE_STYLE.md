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

Use `// ---` to separate logical sections within a file. This aids
readability by creating visual whitespace without blank lines alone.

```rust
// Good
pub struct Foo {
    x: u32,
}

// ---

impl Foo {
    pub fn new(x: u32) -> Self { Self { x } }
}
```

Do not use `// ===`, `// ***`, or comment banners.

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
