# Governance Design: Stake-Weighted Governor

This document describes the stake-weighted `Governor` contract that lets bonded
solvers govern protocol parameters (fees, fill windows, tier thresholds) without
a single-admin trust assumption. Voting power is derived from solver bond plus
delegated stake recorded in `solver_registry`, snapshotted at proposal creation.

## Goals

- Bonded participants propose and vote on protocol parameter changes.
- Voting power is checkpointed stake, read at the proposal snapshot ledger.
- Passed proposals are queued into the `TimelockController`, then executed.
- No governance token: power comes only from existing bond + delegated stake.

## Components

- `governor/` crate: `Governor` contract (propose, vote, quorum, queue, execute).
- `solver_registry`: historical balance checkpoints with binary search over a
  bounded checkpoint array.
- `timelock`: `TimelockController` that holds the admin role and executes queued
  operations after the delay.

## Lifecycle

1. **Propose** — a caller whose snapshot voting power meets the proposal
   threshold submits a proposal. The current ledger is recorded as the snapshot.
2. **Vote** — during the voting period, eligible accounts cast `For`, `Against`,
   or `Abstain`. Weight is the checkpointed balance at the snapshot ledger.
3. **Quorum** — a proposal succeeds only if `For + Abstain` reaches the quorum
   fraction of total stake at the snapshot.
4. **Queue** — a succeeded proposal is scheduled in the `TimelockController`.
5. **Execute** — after the timelock delay, the queued operations are executed.

## Voting Power & Checkpoints

The registry stores a bounded, sorted array of `(ledger, balance)` checkpoints
per account. `balance_at(account, ledger)` binary-searches the array and returns
the most recent checkpoint at or before `ledger`. Delegation transfers voting
power to a delegate; the delegate's checkpoint reflects the delegated amount.

## Anti-Flash-Stake Protection

Voting power is always read at the proposal's snapshot ledger, never at the
current ledger. Stake acquired after the snapshot has no effect on that
proposal, so flash-staking cannot influence an in-flight vote. A slash between
snapshot and vote also does not change the recorded snapshot weight.

## Edge Cases

- **Slash after snapshot** — the snapshot weight is used; the vote is unaffected.
- **Proposal targeting the governor** — allowed, but must route through the
  timelock so the change is delayed and observable.
- **Concentrated stake** — quorum is measured against total snapshot stake, so a
  single large holder still needs to reach the quorum threshold to pass.

## Testing

- Full proposal lifecycle: propose, vote, queue, execute.
- Attack test: acquiring stake after the snapshot does not change voting power.

## Out of Scope

- A governance token.
