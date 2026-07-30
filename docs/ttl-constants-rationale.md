# Storage TTL Constants — Rationale

This document explains the four TTL constants defined at the top of
`intent_settlement/src/lib.rs` (lines 27–35), why their specific values were
chosen, and the trade-offs involved for anyone tuning them.

---

## Background: Soroban State Archival

Soroban (Stellar's smart-contract platform) distinguishes between two storage
tiers that matter here:

- **Persistent storage** — used for `Intent` and `Solver` records. Entries
  survive indefinitely if their TTL is extended before it expires; otherwise
  they are archived (removed from the active ledger state) and can only be
  accessed again after an explicit restore operation.
- **Instance storage** — a single ledger entry that holds the contract's
  global state (`Admin`, `FeeRecipient`, `BondToken`, protocol stats) *and*
  the contract's own executable code. If this entry archives, the entire
  contract becomes unreachable until restored.

Both storage tiers measure TTL in **ledgers**, not seconds.

---

## The `DAY_IN_LEDGERS` Assumption

```rust
const DAY_IN_LEDGERS: u32 = 17280; // ~5s per ledger
```

Stellar mainnet targets a ledger close time of approximately 5 seconds.
One day therefore corresponds to:

```
86400 s/day ÷ 5 s/ledger = 17280 ledgers/day
```

This is the baseline from which all other TTL constants are derived. It is
a *target*, not a guarantee — actual close times vary with network load —
but it is the standard assumption used across the Soroban ecosystem.

---

## Persistent Storage Constants (Intent and Solver Records)

```rust
const PERSISTENT_TTL_THRESHOLD: u32 = DAY_IN_LEDGERS * 14; // ~14 days
const PERSISTENT_TTL_EXTEND_TO: u32 = DAY_IN_LEDGERS * 30; // ~30 days
```

### How they work together

On every write to a `Intent` or `Solver` entry, the contract calls
`extend_ttl(PERSISTENT_TTL_THRESHOLD, PERSISTENT_TTL_EXTEND_TO)`.

Soroban's `extend_ttl` only extends if the current remaining TTL is *below*
the threshold. This avoids redundant ledger writes on entries that were
recently extended. Concretely:

- If an entry has fewer than 14 days of TTL remaining, extend it to 30 days.
- If it already has more than 14 days remaining, do nothing.

### Why 14 days as the threshold?

An intent's maximum active lifespan is capped by `INTENT_EXPIRY` (30 minutes)
plus one `FILL_WINDOW` (5 minutes). Even accounting for re-opens after a slash
(each granting another 30-minute window), an intent cannot remain in an active
state for more than a few hours under normal circumstances.

14 days is therefore a very conservative floor: any entry that is still being
accessed (written to) within a 14-day window is by definition still relevant to
active protocol activity, and the cost of extending it is justified. An entry
that has not been written to for 14 days is either:

1. In a terminal state (`Filled`, `Cancelled`, `Expired`, `Slashed`) and
   unlikely to need further on-chain reads; or
2. Genuinely abandoned and can safely archive.

Solver records have a longer natural activity cycle (a solver might be dormant
between market opportunities for days), so 14 days also comfortably covers
typical inactivity windows without requiring constant top-up transactions.

### Why 30 days as the extend-to target?

Extending to 30 days on each write means that even if a record is never touched
again after the last write, it remains accessible for up to 30 days. This
provides:

- **Archive-risk buffer**: front-ends and indexers querying historical intent
  data have a full month to read records before they archive.
- **Cost proportionality**: extending to 30 days from a 14-day threshold means
  at most ~16 days of "paid-for but potentially unneeded" TTL per write — a
  small overhead relative to the per-byte ledger rent costs.
- **Operational headroom**: in incident scenarios (contract paused, indexer
  outage), 30 days provides enough time for operators to react without records
  disappearing.

---

## Instance Storage Constants (Contract Instance Entry)

```rust
const INSTANCE_TTL_THRESHOLD: u32 = DAY_IN_LEDGERS * 30; // ~30 days
const INSTANCE_TTL_EXTEND_TO: u32 = DAY_IN_LEDGERS * 60; // ~60 days
```

### Why are these higher than the persistent constants?

The contract instance entry is special: if it archives, the *entire contract*
becomes unreachable. Every public function calls `bump_instance_ttl`, so the
instance TTL is refreshed on every state-changing transaction. However, the
consequences of it ever archiving are far more severe than a single Intent or
Solver record archiving, so the safety margins are larger.

### Why 30 days as the threshold?

If the contract goes completely dormant (no transactions for any reason), the
instance entry must still survive long enough for operators to notice and either
submit a transaction or restore it. 30 days gives a full calendar month of
dormancy tolerance before the extension is triggered.

On an active deployment the threshold will never actually be reached (transactions
happen many times per day), so this is purely a safety floor for the worst case.

### Why 60 days as the extend-to target?

Extending to 60 days means even zero activity for an entire month will not
threaten the instance entry for another full month after that. This 2× ratio
(threshold = ½ × extend-to) mirrors the pattern of the persistent constants
and gives the same cost-proportionality property: on every write, the overhead
is at most ~30 days of "extra" TTL.

---

## Cost vs. Archival-Risk Trade-off Summary

| Constant                    | Value      | Rationale                                                        |
|-----------------------------|------------|------------------------------------------------------------------|
| `PERSISTENT_TTL_THRESHOLD`  | 14 days    | Conservative inactivity floor; covers dormant solvers            |
| `PERSISTENT_TTL_EXTEND_TO`  | 30 days    | ~1 month buffer; reasonable archive-risk/rent-cost balance       |
| `INSTANCE_TTL_THRESHOLD`    | 30 days    | Full calendar month of dormancy tolerance for the contract itself |
| `INSTANCE_TTL_EXTEND_TO`    | 60 days    | 2-month buffer; high safety margin justified by catastrophic consequence of archiving |

If you are deploying in an environment with significantly different ledger close
times or different rent cost structures, recalculate `DAY_IN_LEDGERS` first and
then re-evaluate the day multipliers using the same logic above.

Raising `PERSISTENT_TTL_EXTEND_TO` increases ledger rent costs linearly.
Lowering `PERSISTENT_TTL_THRESHOLD` increases the frequency of TTL-extension
writes (each write bumps TTL, so more writes happen when the threshold is lower
relative to the extend-to target). The values chosen aim for a sensible middle
ground on Stellar mainnet pricing as of the contract's initial deployment.
