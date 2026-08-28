# Resource Cost per Entrypoint (Solver Gas Estimation)

**Issue:** [#149](https://github.com/stellar-vortex-protocol/vortex-contracts/issues/149)
**Status:** Populated — first snapshot from the `bench` harness (issue #195)
**Harness:** `intent_settlement/src/bench.rs`

---

## 1. Purpose

Solver bots decide whether filling an intent is profitable before submitting
a transaction. That decision needs an estimate of the transaction's resource
cost so the bot can convert resource usage into an expected fee without
running the Soroban resource simulator for every candidate intent.

This document publishes the first real measurements and the methodology used
to produce them, so the numbers can be regenerated after future contract
changes (resource costs drift as the code does).

## 2. Methodology

The numbers come from `intent_settlement/src/bench.rs`, a `#[cfg(test)]`
harness that:

1. builds an isolated fixture per entrypoint (`Env::default()` +
   `mock_all_auths()`, a fresh contract instance, freshly registered
   solver/intent as needed),
2. calls `env.budget().reset_default()`,
3. invokes the single entrypoint under measurement,
4. reads `Budget::cpu_instruction_cost()` and
   `Budget::memory_bytes_cost()`.

Regenerate with:

```text
cd intent_settlement
cargo test --features testutils bench::resource_cost_report -- --nocapture
```

`bench::resource_cost_is_reproducible` is a smoke test that runs the same
measurement twice and asserts the two results are byte-for-byte identical, so
the published figures are stable run to run for a given toolchain + SDK
version.

### Caveats (read before using these for fee bids)

- **Native, not Wasm.** The SDK executes the contract as native Rust in
  tests. Per the SDK's own documentation, CPU-instruction and memory figures
  are **approximate and generally an underestimate** of on-chain cost. Use
  them as a consistent *relative ranking* between entrypoints and a lower
  bound, not as a fee quote. For an authoritative per-transaction cost, run
  `stellar contract invoke --cost …` against the built Wasm on a network.
- **Ledger entry read/write counts** are not exposed by the `soroban-sdk`
  21 testutils `Budget`. Getting them requires the on-chain simulator or
  `soroban-sdk >= 22`'s `Env::cost_estimate`. The record-size table in
  section 4 covers the write-bytes dimension that matters most for cost
  (and for issue #196).
- Token transfers inside `fill_intent`, `register_solver`, `withdraw_bond`,
  `deregister_solver`, and `slash_solver` invoke the Stellar Asset Contract;
  that cost is included in the row.
- **Toolchain / SDK pinning.** Numbers below were taken with
  `soroban-sdk 21.7.7` on stable Rust. A different SDK patch or `rustc`
  version will shift them; rerun the harness after bumping either.

## 3. Per-entrypoint cost

| Entrypoint | CPU instructions | Memory bytes |
|---|--:|--:|
| `submit_intent` | 277,759 | 38,508 |
| `accept_intent` | 289,315 | 45,974 |
| `fill_intent` (full fill — closes the intent) | 609,248 | 95,091 |
| `fill_intent` (partial fill — re-opens the intent) | 628,355 | 95,807 |
| `cancel_intent` | 231,527 | 38,032 |
| `expire_intent` | 196,158 | 30,634 |
| `slash_solver` | 434,756 | 63,741 |
| `request_extension` | 173,160 | 32,070 |
| `register_solver` (first registration) | 330,084 | 50,389 |
| `register_solver` (top-up of an existing bond) | 304,036 | 43,334 |
| `withdraw_bond` | 306,530 | 43,951 |
| `deregister_solver` | 320,996 | 46,832 |

Notes:

- **`fill_intent` is the most expensive solver call by ~2x** — it does two
  token transfers (output to user, fee to recipient), rewrites the full
  `IntentRecord` and `SolverRecord`, and updates instance stats.
- The **partial-fill path costs slightly more than the full-fill path**: it
  re-opens the intent (resets solver/deadline, bumps `OpenIntents` back up)
  instead of closing it out.
- `slash_solver` is the next most expensive — one token transfer plus a full
  rewrite of both records.

### Batch operations (per-item)

`batch_submit_intent` / `batch_accept_intent` are thin loops over the
single-item entrypoint plus a one-off `MAX_BATCH_SIZE` check, so per-item
cost is measured by running N sequential single calls:

| Sequence | CPU total | CPU / item | Mem total | Mem / item |
|---|--:|--:|--:|--:|
| `submit_intent` ×1 | 277,759 | 277,759 | 38,508 | 38,508 |
| `submit_intent` ×5 | 1,526,222 | 305,244 | 212,024 | 42,404 |
| `submit_intent` ×10 | 3,224,140 | 322,414 | 465,819 | 46,581 |
| `accept_intent` ×1 | 289,315 | 289,315 | 45,974 | 45,974 |
| `accept_intent` ×5 | 1,486,982 | 297,396 | 247,334 | 49,466 |
| `accept_intent` ×10 | 3,152,960 | 315,296 | 550,559 | 55,055 |

Per-item cost is roughly flat (a mild upward drift from the growing
`UserIntents` vector on `submit_intent`); batching amortises only the
transaction envelope, not the per-item work.

## 4. Persistent record sizes

Serialised XDR size of the two records rewritten on the hot paths, read back
from storage after `accept_intent`:

| Record | Serialised size |
|---|--:|
| `IntentRecord` | 624 bytes |
| `SolverRecord` | 340 bytes |

`accept_intent` and both `fill_intent` paths rewrite the **entire**
`IntentRecord` (624 bytes) even though only a few fields change, plus the
full `SolverRecord` (340 bytes). This is the write-bytes cost issue #196
targets: splitting the write-once fields (`src_chain`, `src_token`,
`src_amount`, `user`, `created_at`) out of the frequently-mutated state would
cut the bytes rewritten on every transition.

## 5. Follow-up

- Issue #196 uses the `IntentRecord` size above as its before/after baseline.
- A CI job that regenerates this table on every change is intentionally out
  of scope here (separate DevOps issue); for now, rerun the harness manually
  after any change to `lib.rs` storage shape or the SDK version and update
  sections 3–4.

---

*Snapshot generated from `src/bench.rs` at `soroban-sdk 21.7.7`.*
