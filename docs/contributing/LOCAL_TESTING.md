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

This runs format, lint, unit tests, smoke test, and integration tests in order
and stops on first failure.

## Integration Tests Only

```bash
./scripts/ci-integration-test.sh
```

Starts two agents, runs all `e2e_test` subcommands across two BW configurations,
and stops the agents. Takes ~2 minutes.

## Checking a Single Crate

```bash
cargo test -p quelay-domain
cargo test -p quelay-link-sim
```

## Docs

```bash
cargo doc --workspace --no-deps --open
```
