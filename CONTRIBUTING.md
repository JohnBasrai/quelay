# Contributing to Quelay

Thanks for considering contributing!

## Getting Started

New to the project? Start here:

- **[Quick Start Guide](docs/contributing/QUICK_START.md)** — Get up and running in 5 minutes
- **[Local Testing](docs/contributing/LOCAL_TESTING.md)** — Test before committing

## Before Submitting a PR

Run these commands:

```bash
./scripts/ci-lint.sh && ./scripts/ci-test.sh
```

Or for comprehensive testing:

```bash
./scripts/local-test.sh
```

**PR Checklist:**
- Keep commits focused and descriptive
- Add tests for new features
- Update `CHANGELOG.md` under `[Unreleased]` if behaviour changes
- Verify all CI checks pass locally

We follow [Keep a Changelog](https://keepachangelog.com/en/1.0.0/) and
[Semantic Versioning](https://semver.org/).

## Documentation

- **[Quick Start](docs/contributing/QUICK_START.md)** — Examples and basic usage
- **[Local Testing](docs/contributing/LOCAL_TESTING.md)** — Running CI locally
- **[Code Style](docs/contributing/CODE_STYLE.md)** — Formatting and style conventions
- **[Architecture](docs/contributing/ARCHITECTURE.md)** — EMBP, module structure, layering
- **[Testing Strategy](docs/contributing/TESTING.md)** — When and how to add tests

## Quick Reference

| Task | Command |
|---|---|
| Format + lint | `./scripts/ci-lint.sh` |
| Run tests | `./scripts/ci-test.sh` |
| Full CI check | `./scripts/local-test.sh` |
| Build docs | `cargo doc --open` |

## For Maintainers

**Publishing a release:**
1. Update version in workspace `Cargo.toml` and `CHANGELOG.md`
2. Run `./scripts/pre-publish.sh` to verify packaging
3. `cargo publish -p quelay-domain` (publish domain crate first)
4. `cargo publish -p quelay-mock`
5. Tag and push: `git tag vX.Y.Z && git push origin vX.Y.Z`

## Questions?

Open an issue or discussion on GitHub.
