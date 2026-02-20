# Local Testing

Run the same checks that CI runs, before pushing.

## Individual Steps

```bash
# Format check
cargo fmt --all -- --check

# Lint
cargo clippy --all-targets -- -D warnings

# Tests
cargo test --workspace
```

## All-in-One

```bash
./scripts/local-test.sh
```

This runs format, lint, and tests in order and stops on first failure.

## Checking a Single Crate

```bash
cargo test -p quelay-domain
cargo test -p quelay-link-sim
```

## Docs

```bash
cargo doc --workspace --no-deps --open
```
