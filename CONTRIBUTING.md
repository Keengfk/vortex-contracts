# Contributing to vortex-contracts

This document covers everything specific to contributing to *this* repository:
toolchain setup, project structure, code conventions, and how to run the test
suite. For general process (issue triage, PR etiquette, code of conduct), see
the org-wide
[CONTRIBUTING.md](https://github.com/vortex-protocol/.github/blob/main/CONTRIBUTING.md).

---

## Table of Contents

1. [Toolchain Setup](#toolchain-setup)
2. [Project Structure](#project-structure)
3. [Build](#build)
4. [Testing](#testing)
5. [Linting and Formatting](#linting-and-formatting)
6. [Dependency Auditing](#dependency-auditing)
7. [Code Conventions](#code-conventions)
8. [Submitting a PR](#submitting-a-pr)

---

## Toolchain Setup

### Rust

Install Rust via [rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

This project requires **Rust 1.78 or later**. Check your version:

```bash
rustc --version
```

### wasm32 target

Soroban contracts compile to WebAssembly. Add the target:

```bash
rustup target add wasm32-unknown-unknown
```

### Stellar CLI

Install the [Stellar CLI](https://developers.stellar.org/docs/tools/developer-tools/cli/stellar-cli),
which is required for `stellar contract build` and deployment:

```bash
cargo install --locked stellar-cli --features opt
```

Verify the install:

```bash
stellar --version
```

### cargo-audit (optional but recommended)

The CI dependency-audit job runs `cargo audit`. Install it locally to catch
advisories before pushing:

```bash
cargo install --locked cargo-audit
```

---

## Project Structure

```
vortex-contracts/
├── intent_settlement/       # The deployed Soroban contract
│   ├── Cargo.toml
│   ├── Cargo.lock
│   └── src/
│       ├── lib.rs           # All contract logic, types, and errors
│       └── test.rs          # soroban_sdk testutils test suite
├── docs/                    # Extended documentation
│   ├── solver-integration-guide.md
│   ├── mainnet-deployment-runbook.md
│   ├── ttl-constants-rationale.md
│   └── CONTRIBUTING.md      # This file
├── README.md
├── CHANGELOG.md
└── .github/
    └── workflows/
        └── ci.yml
```

The entire on-chain logic lives in `intent_settlement/src/lib.rs`. There are no
additional modules — keep it that way unless a refactor is explicitly discussed
and agreed on in an issue first.

---

## Build

```bash
cd intent_settlement

stellar contract build
```

The optimized wasm artifact is written to:

```
intent_settlement/target/wasm32-unknown-unknown/release/vortex_intent_settlement.wasm
```

`stellar contract build` runs `cargo build --target wasm32-unknown-unknown
--release` under the hood and applies wasm-opt automatically.

---

## Testing

Tests live in `intent_settlement/src/test.rs` and use `soroban_sdk`'s
`testutils` feature to run a simulated Soroban environment in-process — no
network or deployed contract required.

```bash
cd intent_settlement
cargo test
```

Run a single test by name:

```bash
cargo test test_fill_intent
```

Run with output visible (useful when debugging):

```bash
cargo test -- --nocapture
```

### What the test suite covers

- Full intent lifecycle: `submit → accept → fill`
- `cancel_intent` (open intents only)
- `expire_intent` (permissionless expiry)
- `slash_solver` (missed fill window, bond deduction, re-open)
- Bond deactivation when post-slash bond drops below `MIN_BOND`
- `register_solver` top-up and `withdraw_bond`
- `deregister_solver` with and without active intents
- Admin controls: `set_fee_recipient`, `transfer_admin`
- Pause/unpause and gated functions
- Destination token allowlist enforcement
- Storage TTL management (instance and persistent)
- All relevant error paths

When adding a new entrypoint or changing existing behavior, add or update a test
that exercises the new code path. PRs that change logic without a corresponding
test change will be asked to add coverage.

---

## Linting and Formatting

All of these must pass cleanly before a PR is merged. Run them locally before
pushing:

```bash
cd intent_settlement

# Format (edits in place)
cargo fmt --all

# Lint (must produce zero warnings)
cargo clippy --all-targets -- -D warnings
```

The CI workflow runs both with the same flags; a `clippy` warning that is
suppressed locally with `#[allow(...)]` must include a comment explaining why
the suppression is intentional and safe.

---

## Dependency Auditing

```bash
cd intent_settlement
cargo audit
```

This checks `Cargo.lock` against the [RustSec advisory database](https://rustsec.org/).
Any unresolved `error`-level advisory will fail CI. If you add a dependency,
run `cargo audit` before pushing.

When upgrading a dependency to resolve an advisory, note the advisory ID in
the CHANGELOG entry.

---

## Code Conventions

### `#![no_std]`

The contract uses `#![no_std]` (required for wasm32 targets). Do not add any
crate that pulls in `std`; use `soroban_sdk` types (`String`, `Vec`, `Map`,
`Bytes`, etc.) instead of their `std` equivalents.

### Error variants

All errors are defined in the `Error` enum with explicit `#[repr(u32)]`
discriminants. When adding a new error:

1. Append it at the end of the enum — do not renumber existing variants.
2. Add a comment explaining the condition that triggers it.
3. Update the relevant section of this document or the integration guide if the
   error is user-facing.

### Storage keys

All storage keys are variants of the `DataKey` enum. Do not store anything
directly under a raw string or bytes key.

### TTL bumping

Every function that writes to persistent storage must call the appropriate
`bump_*_ttl` helper (`bump_intent_ttl`, `bump_solver_ttl`). Every public
function must call `bump_instance_ttl`. See
[`docs/ttl-constants-rationale.md`](./ttl-constants-rationale.md) for why.

### Events

Every state transition emits an event. New entrypoints should follow the
existing pattern:

```rust
env.events().publish(
    (Symbol::new(&env, "event_name"), actor_address),
    payload_value,
);
```

Event topic and payload shapes are documented in the
[Solver Integration Guide](./solver-integration-guide.md#event-topics).

### Rustdoc

Public functions must have rustdoc comments explaining their behavior,
preconditions, and authorization requirements. Internal helpers (prefixed with
`fn`, not `pub fn`) should have inline comments for anything non-obvious.

---

## Submitting a PR

1. Fork the repo and create a branch from `main`:
   ```bash
   git checkout -b <type>/<short-description>
   ```
   Use `fix/`, `feat/`, or `docs/` prefixes to match CI branch naming.

2. Make your changes, then run the full check suite locally:
   ```bash
   cd intent_settlement
   cargo fmt --all
   cargo clippy --all-targets -- -D warnings
   cargo test
   stellar contract build
   cargo audit
   ```

3. Commit with a conventional commit message:
   ```
   <type>: <short summary in imperative mood>
   ```
   Examples: `fix: prevent bond withdrawal while intents are active`,
   `feat: add solver reputation score view`, `docs: expand TTL rationale`.

4. Open a PR against `main`. The description must include:
   - A summary of what changed and why.
   - `Closes #<issue-number>` for every issue the PR resolves.
   - Notes on anything that could not be tested (e.g., mainnet-only behavior).

5. All CI jobs must be green before merge:
   - `fmt` — `cargo fmt --check`
   - `clippy` — zero warnings
   - `test` — all tests pass
   - `build` — wasm artifact produced
   - `audit` — no unresolved advisories
