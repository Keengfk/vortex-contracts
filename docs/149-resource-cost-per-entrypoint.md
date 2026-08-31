# Resource Cost per Entrypoint (Solver Gas Estimation)

**Issue:** [#149](https://github.com/stellar-vortex-protocol/vortex-contracts/issues/149)
**Status:** Partially complete — `MAX_BATCH_SIZE` justified and documented; per-entrypoint
CPU-instruction numbers still blocked on the resource-benchmarking harness.

---

## 1. Purpose

Solver bots decide whether filling an intent is profitable before submitting
a transaction. That decision needs an estimate of the transaction's resource
cost — CPU instructions, ledger read/write counts, and read/write bytes —
so the bot can convert resource usage into an expected fee without running
the Soroban resource simulator itself for every candidate intent.

This document is the publication target for those numbers. The per-entrypoint
CPU-instruction table (§4) still requires a benchmarking harness (see §5) and
remains `TBD`. The batch-size boundary analysis (§3) has been completed and is
the deliverable for issue #149's "verify `MAX_BATCH_SIZE` is safe" requirement.

---

## 2. Why the per-entrypoint CPU numbers aren't here yet

This issue is explicit that the source data for CPU instructions has to come
from a resource-benchmarking harness landing first. As of this branch, no such
harness exists in this repository — there is no `benches/` directory, no use
of Soroban's `Budget`/instruction-count APIs in `intent_settlement`, and no
recorded CPU-instruction figures anywhere in the codebase.

Publishing fabricated numbers here would be actively harmful: solver authors
would size fee bids off of them, and a wrong estimate causes either
underpriced transactions that fail to land or overpriced ones that eat into
solver margin. So §4 defines the shape the published data will take, and where
it will live, without inventing the figures.

---

## 3. `MAX_BATCH_SIZE` — boundary analysis and justification

### 3.1 Chosen value

```rust
pub const MAX_BATCH_SIZE: u32 = 10;
```

Declared in `intent_settlement/src/lib.rs` as part of this issue's
deliverable.

### 3.2 Binding constraint: per-transaction write-entry limit

Soroban's resource model (CAP-0046-07) enforces hard per-transaction limits.
The relevant limit for batch operations is the **write-entry cap**:

| Resource             | Per-transaction limit (Protocol 27, 2026-07-20) |
|----------------------|-------------------------------------------------|
| CPU instructions     | 400,000,000                                     |
| Footprint entries    | 400 (read + write combined)                     |
| Disk-read entries    | 200                                             |
| **Written entries**  | **200**                                         |
| Written bytes        | 132,096 (~129 KiB)                              |

Source: Stellar Docs appendix, verified 2026-07-20 against Protocol 27 mainnet
settings. Always re-verify with `stellar network settings --network mainnet`
before relying on these numbers — validators can change them.

### 3.3 Per-item write footprint: `fill_intent` (worst case)

`fill_intent` is the most expensive batched operation. Its write footprint per
single call:

| Ledger entry                          | Storage tier | Written? |
|---------------------------------------|--------------|----------|
| `DataKey::Intent(intent_id)`          | persistent   | ✓        |
| `DataKey::Solver(solver)`             | persistent   | ✓        |
| contract instance (shared, all calls) | instance     | ✓ (1 total per tx, not per item) |
| SAC balance: solver (src of transfer) | SAC internal | ✓        |
| SAC balance: user (dst of transfer)   | SAC internal | ✓        |
| SAC balance: fee_recipient (fee xfer) | SAC internal | ✓ (only when fee > 0) |

**Dominant per-item cost: 2 persistent writes + up to 3 SAC balance writes = 5
distinct entries written per fill.**

The instance entry is written once per transaction regardless of batch size
(it is a single shared entry), so it does not scale linearly with batch size.

### 3.4 Footprint at `MAX_BATCH_SIZE = 10`

| Resource          | Calculation                        | Value at 10 items |
|-------------------|------------------------------------|-------------------|
| Persistent writes | 2 per item × 10                    | 20                |
| SAC writes        | ≤ 3 per item × 10                  | ≤ 30              |
| Instance writes   | 1 shared                           | 1                 |
| **Total writes**  |                                    | **≤ 51**          |
| Write limit       |                                    | 200               |
| **Headroom**      |                                    | **≥ 149 (75%+)**  |

At 10 items, the worst-case write count is **≤ 51**, leaving more than 70%
of the write budget free. Even doubling `MAX_BATCH_SIZE` to 20 would yield
≤ 101 writes — still within the 200-entry cap. The conservative choice of 10
provides a large safety margin for:

- Future additions to `fill_intent`'s storage footprint (e.g. per-fill event
  metadata, additional solver tracking fields).
- Soroban host overhead (the host may write internal entries not visible at
  the contract level).
- Transaction envelope size: each `Intent` + `Solver` entry is at most ~1 KiB
  serialized; 20 footprint entries ≈ 20 KiB, well under the 132 KiB
  transaction-size cap.

### 3.5 Per-item write footprint: other batch operations

`batch_accept_intent`, `batch_submit_intent`, and `batch_cancel_intent` are
all cheaper than `batch_fill_intent`:

| Operation              | Persistent writes/item | SAC writes/item | Total/item |
|------------------------|------------------------|-----------------|------------|
| `batch_fill_intent`    | 2                      | ≤ 3             | ≤ 5        |
| `batch_accept_intent`  | 2 (Intent + Solver)    | 0               | 2          |
| `batch_submit_intent`  | 1 (Intent)             | 0               | 1          |
| `batch_cancel_intent`  | 2 (Intent + CancelCooldown) | 0          | 2          |

`fill_intent` dominates. The `MAX_BATCH_SIZE = 10` limit is safe for all four
operations.

### 3.6 Simulation vs. mainnet caveat

The analysis above is a **static footprint count** derived from reading the
contract source. Actual CPU-instruction consumption is determined at runtime
by the Soroban metering host and is not captured here. Two important caveats:

1. **Simulation numbers are indicative, not binding.** `simulateTransaction`
   returns a resource estimate for the specific ledger state at simulation
   time. Hot vs. cold cache state, TTL-extension rent costs, and bucket-list
   size all affect the final fee. Always simulate immediately before
   submission.

2. **Network limits can change.** Validators vote on Soroban resource settings.
   A limit reduction would make the current `MAX_BATCH_SIZE = 10` less safe;
   a limit increase would allow a higher value. Monitor
   `stellar network settings --network mainnet` after protocol upgrades.

### 3.7 Regression test

`intent_settlement/src/test.rs` contains boundary stress tests under the
`// ─── #149: batch boundary stress tests` section:

- `batch_fill_intent_at_max_batch_size_completes` — submits, accepts, and
  fills `MAX_BATCH_SIZE` intents in one `batch_fill_intent` call and asserts
  all end up `Filled` with the solver's `fills_completed` counter equal to
  `MAX_BATCH_SIZE`.
- `batch_cancel_intent_at_max_batch_size_completes` — cancels `MAX_BATCH_SIZE`
  intents (one per distinct user, to avoid `CANCEL_COOLDOWN` collisions within
  the batch).
- `batch_cancel_single_user_trips_cancel_cooldown_after_first_item` — documents
  that `CANCEL_COOLDOWN` fires on the second item when a single user tries to
  cancel multiple intents in one batch call, causing the whole batch to revert.
- `batch_fill_intent_over_limit_rejected` — asserts `MAX_BATCH_SIZE + 1` items
  is rejected.
- `batch_cancel_intent_over_limit_rejected` — same for `batch_cancel_intent`.

---

## 4. Entrypoints this doc will cover (CPU numbers TBD)

Once the benchmarking harness exists, each of the following state-changing
entrypoints in `intent_settlement` should get a row:

| Entrypoint | Why a solver calls it |
|---|---|
| `register_solver` | One-time (or top-up) bond lock before participating |
| `deregister_solver` | Exit and reclaim bond |
| `withdraw_bond` | Partial bond withdrawal |
| `accept_intent` | Claim exclusive fill rights on an intent |
| `fill_intent` | Deliver output, settle, collect fee |
| `batch_accept_intent` | Claim rights on multiple intents in one call |
| `batch_fill_intent` | Fill multiple intents in one call |
| `batch_cancel_intent` | Cancel multiple intents in one call |
| `slash_solver` | Permissionless cleanup of a missed fill |
| `expire_intent` | Permissionless cleanup of an unfilled `Open` intent |
| `request_extension` | Ask for more time on a fill in progress |
| `submit_intent` / `batch_submit_intent` | User-facing, listed for completeness |
| `cancel_intent` | User-facing, listed for completeness |

## 5. Planned table format (to fill in once the harness lands)

```markdown
| Entrypoint | CPU instructions | Ledger reads | Ledger writes | Read bytes | Write bytes |
|---|---|---|---|---|---|
| register_solver | TBD | TBD | TBD | TBD | TBD |
| accept_intent | TBD | TBD | TBD | TBD | TBD |
| fill_intent | TBD | TBD | TBD | TBD | TBD |
| batch_fill_intent (×10) | TBD | TBD | TBD | TBD | TBD |
| ... | | | | | |
```

Each row should be generated from the harness's output, not hand-estimated,
and should note the ledger state assumed (e.g. cold vs. warm TTL, allowlists
enabled/disabled) since those change the read/write footprint of
`submit_intent` and `fill_intent` in particular.

## 6. Follow-up

Once the resource-benchmarking harness lands:

1. Run it against each entrypoint in section 4.
2. Replace the table in section 5 with the real output.
3. Link the harness itself from this doc so the numbers can be regenerated
   after future contract changes (resource costs drift as the code does).
4. Re-evaluate `MAX_BATCH_SIZE` against actual CPU-instruction measurements.
   The current value of 10 is justified by the write-entry analysis in §3 and
   is conservative enough that it is unlikely to need reduction, but the
   CPU-instruction budget should be verified once simulation data is available.

---

*Tracks #149. The write-entry boundary analysis and MAX_BATCH_SIZE
justification are complete. CPU-instruction numbers still require the
benchmarking harness.*
