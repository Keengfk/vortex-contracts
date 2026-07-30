# Resource Cost per Entrypoint (Solver Gas Estimation)

**Issue:** [#149](https://github.com/stellar-vortex-protocol/vortex-contracts/issues/149)
**Status:** Blocked — waiting on the resource-benchmarking harness
**Blocked by:** resource-benchmarking harness issue (not yet landed in this repo)

---

## 1. Purpose

Solver bots decide whether filling an intent is profitable before submitting
a transaction. That decision needs an estimate of the transaction's resource
cost — CPU instructions, ledger read/write counts, and read/write bytes —
so the bot can convert resource usage into an expected fee without running
the Soroban resource simulator itself for every candidate intent.

This document is the publication target for those numbers. It intentionally
ships **without** the numbers themselves.

## 2. Why the numbers aren't here yet

This issue is explicit that the source data has to come from a
resource-benchmarking harness landing first. As of this branch, no such
harness exists in this repository — there is no `benches/` directory, no use
of Soroban's `Budget`/instruction-count APIs in `intent_settlement`, and no
recorded CPU-instruction or read/write figures anywhere in the codebase.

Publishing fabricated numbers here would be actively harmful: solver authors
would size fee bids off of them, and a wrong estimate causes either
underpriced transactions that fail to land or overpriced ones that eat into
solver margin. So this doc defines the shape the published data will take,
and where it will live, without inventing the figures.

## 3. Entrypoints this doc will cover

Once the benchmarking harness exists, each of the following state-changing
entrypoints in `intent_settlement` should get a row. These are the calls a
solver bot actually submits as transactions (view/read-only calls like
`get_intent`, `is_solver_eligible`, `get_stats`, etc. are simulated locally
and aren't the fee-estimation bottleneck this issue is about):

| Entrypoint | Why a solver calls it |
|---|---|
| `register_solver` | One-time (or top-up) bond lock before participating |
| `deregister_solver` | Exit and reclaim bond |
| `withdraw_bond` | Partial bond withdrawal |
| `accept_intent` | Claim exclusive fill rights on an intent |
| `fill_intent` | Deliver output, settle, collect fee |
| `batch_accept_intent` | Claim rights on multiple intents in one call |
| `slash_solver` | Permissionless cleanup of a missed fill |
| `expire_intent` | Permissionless cleanup of an unfilled `Open` intent |
| `request_extension` | Ask for more time on a fill in progress |
| `submit_intent` / `batch_submit_intent` | User-facing, not solver-facing, but listed for completeness since the same fee model applies |
| `cancel_intent` | User-facing, listed for completeness |

## 4. Planned table format (to fill in once the harness lands)

```markdown
| Entrypoint | CPU instructions | Ledger reads | Ledger writes | Read bytes | Write bytes |
|---|---|---|---|---|---|
| register_solver | TBD | TBD | TBD | TBD | TBD |
| accept_intent | TBD | TBD | TBD | TBD | TBD |
| fill_intent | TBD | TBD | TBD | TBD | TBD |
| ... | | | | | |
```

Each row should be generated from the harness's output, not hand-estimated,
and should note the ledger state assumed (e.g. cold vs. warm TTL, allowlists
enabled/disabled) since those change the read/write footprint of
`submit_intent` and `fill_intent` in particular.

## 5. Follow-up

Once the resource-benchmarking harness lands:

1. Run it against each entrypoint in section 3.
2. Replace the table in section 4 with the real output.
3. Link the harness itself from this doc so the numbers can be regenerated
   after future contract changes (resource costs drift as the code does).

---

*Tracks #149. Not closing it — the numbers still need the benchmarking
harness to land first.*
