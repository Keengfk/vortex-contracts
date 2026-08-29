# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).
This project has not yet made a versioned release; entries below are grouped
under "Unreleased" and will be cut into a version once `intent_settlement`
first deploys to mainnet.

## [Unreleased]

### Fixed

- **Restored a compiling baseline for `intent_settlement/src/lib.rs`**
  (closes #202, #203, #204). The file referenced twelve constants, nine
  `DataKey` variants, and eight `Error` variants that were never declared,
  and three `Error` variants collided on discriminant 22 with two more on
  23 — so the crate did not build. All are now declared:
  - Constants: `DEFAULT_MIN_BOND`, `DEFAULT_FILL_WINDOW`,
    `DEFAULT_INTENT_EXPIRY`, `DEFAULT_PROTOCOL_FEE_BPS` (all equal to the
    historical hard-coded values so `initialize` / `load_config` behaviour
    is unchanged), plus the `set_config` bounds `MAX_PROTOCOL_FEE_BPS`
    (100 bps), `MIN_FILL_WINDOW_SECS` (60), `MIN_INTENT_EXPIRY_SECS`
    (300), `MIN_BOND_FLOOR` (1 USDC), and `SLASH_COOLDOWN` (1 h),
    `CANCEL_COOLDOWN` (5 min), `MAX_BATCH_SIZE` (20),
    `MAX_EXTENSION_DURATION` (5 min).
  - `DataKey`: `Config`, `PendingAdmin`, `AllowedDstTokenList`,
    `PendingDstTokenAdd(Address)`, `PendingDstTokenRemove(Address)`,
    `UserIntents(Address)`, `MinBondMultiplier(Address)`,
    `CancelCooldown(Address)`, `ExtensionGranted(BytesN<32>)`, each with a
    rustdoc block matching the existing variants. No change to what any
    existing key stores.
  - `Error`: added `TimelockNotElapsed`, `NoPendingAdminTransfer`,
    `NoPendingDstTokenChange`, `InvalidConfig`, `AmountTooLarge`,
    `CancelCooldownNotExpired`.

### Changed

- **`Error` discriminant migration** (#203). Every `#[contracterror]`
  variant now has a unique `#[repr(u32)]` code. Following
  `CONTRIBUTING.md` ("do not renumber existing variants"), codes 1–24 and
  28 are untouched; the collided/mis-documented variants moved to freshly
  appended numbers:
  - `NoPendingFeeRecipient`: 22 → **29**
  - `SrcChainNotAllowed`: 22 → **30**
  - `RescueProtectedToken`: 23 → **31**
  - `TimelockNotElapsed`: documented as 25, never assigned → **32**
  - `NoPendingAdminTransfer`: documented as 26 → **33**
  - `NoPendingDstTokenChange`: documented as 27 → **34**
  - New: `InvalidConfig` = 35, `AmountTooLarge` = 36,
    `CancelCooldownNotExpired` = 37, `ExtensionCapExceeded` = 38.

  Integrators, solvers, and indexers that hard-coded error code 22 ("src
  chain not allowed" / "no pending fee recipient") or 23 ("rescue
  protected token") **must remap** to the numbers above. The README Error
  Reference table has been regenerated to match the enum exactly.
- **`request_extension` is now reputation-gated** (#200). It previously
  granted exactly one fixed-size extension per intent to any accepted
  solver. It now grants repeated `MAX_EXTENSION_DURATION` extensions as
  long as the intent's cumulative extension time stays within a
  per-solver budget derived from the solver's local tier
  (`fills_completed` + `compute_reputation_score`): +10% / +20% / +30% /
  +50% for Bronze / Silver / Gold / Platinum, mirroring
  `docs/solver-registry-design.md`. **Unranked solvers keep the exact
  old one-shot behaviour.** A tier-independent ceiling
  (`MAX_TOTAL_EXTENSION` = 2 × `MAX_EXTENSION_DURATION`) caps every
  intent regardless of tier, and an extension requested after the
  deadline has already passed is now rejected with `FillWindowExpired`.
  The `DataKey::ExtensionGranted(intent_id)` value changed from a `bool`
  presence flag to a `u64` running total of seconds consumed.

### Fixed

- `deregister_solver` now refuses to return a solver's bond while they hold
  an `Accepted` intent, closing a path to dodge `slash_solver` by
  withdrawing before the fill window expired.
- `register_solver` checks the *cumulative* bond total against `MIN_BOND`
  instead of each individual deposit, so a solver already above the
  minimum can top up by a smaller amount without being wrongly rejected.
- A solver whose bond falls below `MIN_BOND` after a slash is now
  automatically deactivated, rather than staying eligible to accept
  further intents while under-collateralized.

### Added

- **Storage TTL management**: persistent `Intent`/`Solver` entries and the
  contract instance now have their TTL extended on every write, closing a
  gap where none of Soroban's state-archival requirements were handled.
- **Admin key management**: `set_fee_recipient`, `transfer_admin`
  (requires auth from both the outgoing and incoming admin), and
  `get_admin`/`get_fee_recipient` views -- previously no rotation path
  existed for either role.
- **Emergency pause**: `pause()`/`unpause()`/`is_paused()`, gating
  `submit_intent`/`accept_intent`/`fill_intent` for incident response.
  `slash_solver` and `cancel_intent` stay available throughout.
- **Partial bond withdrawal**: `withdraw_bond(amount)` lets a solver
  reclaim excess collateral above `MIN_BOND` without fully deregistering.
- **Permissionless intent expiry**: `expire_intent()` materializes an
  `Open` intent's `Expired` state once its deadline passes, instead of
  relying on a lazy check inside `accept_intent`.
- **Views**: `get_bond_token`, `get_solver_count` (backed by a new
  `TotalSolvers` stat), `is_solver_eligible`.
- **Aggregate health view**: `get_protocol_health` bundles `is_paused`,
  `get_stats`, and `get_solver_count` into a single `ProtocolHealth`
  struct so dashboard/monitoring integrations need one call instead of
  three (#112).
- **Destination token allowlist**: `add_allowed_dst_token` /
  `remove_allowed_dst_token` / `is_dst_token_allowed`, enforced in
  `submit_intent` only once an admin opts in via
  `set_dst_allowlist_enabled` (off by default).
- **Timelocked admin actions** (#115, #116): sensitive admin changes now go
  through a propose-then-execute flow with a 48-hour delay, so users and
  solvers have a window to notice and react before a change takes effect.
  A distinct `*_proposed` event fires immediately at proposal time, ahead of
  the delay, giving off-chain monitors advance notice either way.
  - `set_fee_recipient` is superseded by `propose_fee_recipient` /
    `accept_fee_recipient`, now timelocked (`get_pending_fee_recipient`
    returns `(Address, u64 eta)`).
  - `transfer_admin` is superseded by `propose_admin_transfer` /
    `accept_admin_transfer`, now timelocked (`get_pending_admin`).
  - `add_allowed_dst_token` / `remove_allowed_dst_token` are superseded by
    `propose_add_dst_token` / `execute_add_dst_token` and
    `propose_remove_dst_token` / `execute_remove_dst_token` (#118).
    `execute_*` is permissionless once the delay has elapsed, since the
    change was already authorized by the admin at proposal time.
- **Enumerable dst_token allowlist** (#117): `list_allowed_dst_tokens()`
  returns every token currently on the allowlist, so integrators and
  auditors no longer have to replay `dst_token_allowed` /
  `dst_token_disallowed` events to reconstruct the full list.

### Changed

- CI now also runs a dependency-audit job (`cargo audit` against the
  RustSec advisory database) alongside the existing fmt/clippy/test/build
  checks.

### Documentation

- README: added `stellar contract invoke` usage examples for the core
  intent lifecycle and an up-to-date entrypoint list.
- Filled in missing rustdoc on `unpause`, `is_paused`, and the view
  functions.
- Added `docs/110-monitoring-alerting-spec.md`: signals and thresholds an
  ops team should watch, including slash rate, bond utilization, and
  pause/unpause activity (#110).
- Added `docs/111-expire-intent-event-coverage.md`: confirms and documents
  the gap between the `intent_expired` event and an intent that is merely
  past its deadline but not yet materialized as `Expired` (#111).
- Added `docs/113-event-topic-naming-conventions.md`: documents the
  current event topic conventions in `intent_settlement` and sets the
  naming convention future contracts (e.g. `solver_registry`) should
  follow (#113).
