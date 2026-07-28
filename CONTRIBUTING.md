# Contributing to Vortex Contracts

Thank you for contributing! This document covers the day-to-day workflow for
**contributors** and includes a dedicated [Maintainer Guide](#maintainer-guide)
section covering CI, branch protection, and required-check management.

---

## Contributor Quick-start

### Prerequisites

- Rust 1.78+ with the `wasm32-unknown-unknown` target
- [Stellar CLI](https://developers.stellar.org/docs/tools/developer-tools/cli/stellar-cli)
- [GNU Make](https://www.gnu.org/software/make/) (optional but recommended)
- [`just`](https://just.systems/) (optional alternative to Make)

### Local development commands

A `Makefile` (and equivalent `justfile`) is provided at the repo root so you
never need to copy-paste raw multi-flag commands from the README.

```bash
# inside the repo root
make fmt        # cargo fmt --all
make lint       # cargo clippy --all-targets -- -D warnings   ← same as CI
make test       # cargo test
make build      # cargo build --target wasm32-unknown-unknown --release
make all        # fmt + lint + test + build (full pre-push check)
make deploy-testnet   # stellar contract deploy … --network testnet
```

See [`Makefile`](./Makefile) and [`justfile`](./justfile) for the full list of
targets, or run `make help` / `just --list`.

### Pre-push checklist

Before opening a PR, run `make all` (or its `just` equivalent) and confirm:

1. `cargo fmt --all -- --check` exits 0
2. `cargo clippy --all-targets -- -D warnings` exits 0 (no warnings allowed)
3. `cargo test` passes
4. The wasm binary builds cleanly

---

## Maintainer Guide

This section is for maintainers with write access to the repository. It
documents which CI jobs are **required** for merging and how to keep that
configuration correct as the workflow grows.

### Current CI jobs and required-check status

| Job name (as reported by GitHub) | Workflow file | Required to merge? |
|---|---|---|
| `Contract (stable)` | `ci.yml` / `contract` matrix leg | ✅ Yes |
| `Contract (1.78)` | `ci.yml` / `contract` matrix leg | ✅ Yes |
| `Dependency audit` | `ci.yml` / `audit` | ✅ Yes |

> **Note:** Matrix jobs are reported to GitHub as `<job.name> (<matrix value>)`.
> The exact strings you must enter in the branch-protection UI are
> `Contract (stable)` and `Contract (1.78)`.

Each `contract` matrix leg runs:

1. `cargo fmt --all -- --check` (stable leg only)
2. `cargo clippy --all-targets -- -D warnings` — **identical** to the command
   documented in README.md; `-D warnings` is enforced on every leg
3. `cargo test`
4. `cargo build --target wasm32-unknown-unknown --release`

### Verifying clippy `-D warnings` is enforced

1. Open `.github/workflows/ci.yml`.
2. Find the `Clippy` step inside the `contract` job.
3. Confirm the `run:` value is exactly:
   ```
   cargo clippy --all-targets -- -D warnings
   ```
   Any relaxation (e.g. dropping `-D warnings`, adding `--allow …`) must be
   reviewed and approved by a second maintainer before merging.

### How to confirm a required-check is actually enforced (GitHub UI)

1. Go to **Settings → Branches** on the GitHub repository page.
2. Click **Edit** next to the `main` branch rule (or create one if absent).
3. Enable **"Require status checks to pass before merging"**.
4. Enable **"Require branches to be up to date before merging"**.
5. In the search box, type the exact job names from the table above and select
   each one. If a job name doesn't appear in autocomplete, trigger a CI run on
   any open PR first — GitHub only indexes checks it has seen recently.
6. Save the rule.
7. To verify enforcement: open a test PR that intentionally fails one of the
   required checks and confirm the **Merge** button is blocked.

> **Tip — GitHub CLI alternative:**
> ```bash
> gh api repos/{owner}/{repo}/branches/main/protection \
>   --jq '.required_status_checks.contexts'
> ```
> This prints the list of currently required check names without needing the UI.

### How to update required checks when new CI jobs are added

When a new job is added to a workflow file (e.g. `wasm-size`, `coverage`):

1. Add a row to the table above with the correct job name, workflow, and
   proposed required status.
2. Add the new check name to branch protection (see steps above) **in the same
   PR** that introduces the workflow job — never after.
3. If the job is advisory-only (informational, not blocking), mark it as
   `❌ No (advisory)` in the table and add a comment in the workflow step
   explaining why it is advisory.

#### Proposed future required checks

The following jobs are under discussion or in the roadmap. Update this table
once they are merged:

| Job name | Workflow | Notes |
|---|---|---|
| `WASM size gate` | `ci.yml` (planned) | Blocks merges that grow the wasm by > N KB |
| `Coverage` | `coverage.yml` (planned) | Advisory until a baseline is established |

### MSRV policy

The declared MSRV is **Rust 1.78** (see README.md). CI enforces this via a
matrix leg that runs on toolchain `1.78` alongside `stable`.

- If a dependency update or language feature requires bumping the MSRV, update
  both `ci.yml` (the matrix value) and README.md in the same PR, and note it
  in `CHANGELOG.md`.
- The MSRV leg intentionally skips `rustfmt` to avoid false failures from
  format-output changes across Rust versions; linting and testing still run on
  both legs.

---

## Code style

- Format: `cargo fmt --all`
- Lints: `cargo clippy --all-targets -- -D warnings` (zero warnings policy)
- Commit messages: conventional-commits style
  (`feat:`, `fix:`, `chore:`, `docs:`, `test:`, `refactor:`)

## Pull request checklist

- [ ] Branch is up to date with `main`
- [ ] All required CI checks pass
- [ ] PR description includes `Closes #<issue-number>`
- [ ] New public items have doc-comments
- [ ] `CHANGELOG.md` updated under `[Unreleased]`

## License

By contributing you agree your work will be licensed under the
[MIT License](./LICENSE).
