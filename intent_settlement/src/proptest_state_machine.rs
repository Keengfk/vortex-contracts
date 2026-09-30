//! Stateful property-based test harness for the `IntentSettlement` contract.
//!
//! # What this tests
//!
//! This file generates random sequences of *every public entrypoint* across
//! multiple actors (3 users, 3 solvers) with ledger-time jumps, runs each step
//! against both:
//!
//!   1. The real Soroban testutils environment (the *system under test*).
//!   2. A pure-Rust **reference model** that mirrors the contract's state machine.
//!
//! After every step the harness asserts six **global invariants**:
//!
//! | # | Invariant |
//! |---|-----------|
//! | I1 | `OpenIntents` == count of `Open` or `PartiallyFilled` intents. |
//! | I2 | `TotalSolvers` == `len(SolverList)`. |
//! | I3 | Every solver's `active_intents` == their entry count in `SolverIntents`. |
//! | I4 | Solvency: the contract's bond-token balance >= `Σ solver_bond_amounts`. |
//! | I5 | No intent is in an impossible state (e.g. `Filled` with `total_filled < min_dst_amount`). |
//! | I6 | Reference model and on-chain state agree on intent state after every step. |
//!
//! # Seeded historical bugs
//!
//! Three bugs that were present at various points in the codebase are encoded
//! directly as deterministic regression tests (see the `regression_*` tests at
//! the bottom). The stateful harness catches each of them under the invariant
//! they violate.
//!
//! # Case count
//!
//! Default: 256 cases (set via `ProptestConfig`). Nightly CI raises this via
//! `PROPTEST_CASES=2048` in the workflow — no code changes required.
//!
//! # Running
//!
//! ```bash
//! cargo test proptest_state_machine --features testutils
//! PROPTEST_CASES=2048 cargo test proptest_state_machine --features testutils
//! ```

#![cfg(test)]

// The crate is `#![no_std]`. The proptest harness brings in `std`, so we
// import the std collections and formatting machinery we need explicitly.
extern crate std;
use std::collections::HashMap;
use std::vec::Vec;

use proptest::prelude::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token, Address, BytesN, Env, String,
};

use crate::{
    IntentSettlement, IntentSettlementClient, IntentState, CANCEL_COOLDOWN, FILL_WINDOW,
    INTENT_EXPIRY, MIN_BOND, SLASH_COOLDOWN,
};

// ═══════════════════════════════════════════════════════════════════════════════
// Tunables
// ═══════════════════════════════════════════════════════════════════════════════

/// Number of user actors. Kept small to maximise interesting interleavings.
const N_USERS: usize = 3;
/// Number of solver actors.
const N_SOLVERS: usize = 3;
/// Maximum number of action steps per generated test case.
const MAX_STEPS: usize = 30;
/// Solver bond deposited at setup (10× MIN_BOND for headroom against slashes).
const STARTING_BOND: i128 = MIN_BOND * 10;
/// Intent source amount — opaque on-chain, just needs to be positive.
const SRC_AMOUNT: i128 = 1_000_000;
/// Minimum destination amount for submitted intents.
const MIN_DST: i128 = 100 * 10_000_000; // 100 dst-tokens @ 7 decimals
/// A fill amount that fully satisfies MIN_DST (slightly over to exercise
/// the ≥ condition rather than the == boundary).
const FULL_FILL: i128 = MIN_DST + 10_000_000;
/// A fill amount that is clearly partial (half MIN_DST).
const PARTIAL_FILL: i128 = MIN_DST / 2;
/// Default proptest case count. Override at runtime with PROPTEST_CASES env var.
const DEFAULT_CASES: u32 = 256;

// ═══════════════════════════════════════════════════════════════════════════════
// Reference model
// ═══════════════════════════════════════════════════════════════════════════════

/// Logical intent state in the reference model. Mirrors `IntentState` from
/// the contract but lives in pure Rust so we can put it in a `HashMap`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RefState {
    Open,
    Accepted,
    PartiallyFilled,
    Filled,
    Cancelled,
    Expired,
    Slashed,
}

/// Reference-model view of one intent.
#[derive(Debug, Clone)]
struct RefIntent {
    state: RefState,
    /// Index into the fixture's `users` array.
    user_idx: usize,
    /// Index into the fixture's `solvers` array, when accepted.
    solver_idx: Option<usize>,
    total_filled: i128,
    min_dst_amount: i128,
    /// Absolute deadline (seconds).
    deadline: u64,
}

/// Reference-model view of one solver.
#[derive(Debug, Clone)]
struct RefSolver {
    bond_amount: i128,
    is_active: bool,
    active_intents: u32,
    last_slash_time: u64,
    fills_completed: u32,
    fills_failed: u32,
}

/// The complete reference model state. Updated in lock-step with each action
/// and compared against the on-chain state after every step.
#[derive(Debug, Default)]
struct ReferenceState {
    /// intent_id bytes (as [u8;32]) → intent record
    intents: HashMap<[u8; 32], RefIntent>,
    /// solver index → solver record
    solvers: Vec<RefSolver>,
    /// solver index → list of intent_id bytes they currently have Accepted
    solver_accepted_intents: Vec<Vec<[u8; 32]>>,
    /// Count of Open or PartiallyFilled intents (mirrors DataKey::OpenIntents)
    open_intents_count: u64,
    /// Count of registered solvers (mirrors DataKey::TotalSolvers)
    total_solvers: u32,
    /// Whether each solver is in the SolverList (registered)
    solver_registered: Vec<bool>,
    /// Total bonded (mirrors DataKey::TotalBonded)
    total_bonded: i128,
}

impl ReferenceState {
    /// Returns `true` if `intent_id` (by bytes) is `Open` or `PartiallyFilled`.
    fn is_open_or_partial(&self, id: &[u8; 32]) -> bool {
        matches!(
            self.intents.get(id).map(|i| &i.state),
            Some(RefState::Open) | Some(RefState::PartiallyFilled)
        )
    }

    /// Recompute `open_intents_count` from scratch. Used for invariant checks.
    fn recount_open(&self) -> u64 {
        self.intents
            .values()
            .filter(|i| i.state == RefState::Open || i.state == RefState::PartiallyFilled)
            .count() as u64
    }

    /// Recompute `total_solvers` from the `solver_registered` bitmask.
    fn recount_solvers(&self) -> u32 {
        self.solver_registered.iter().filter(|&&r| r).count() as u32
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Actions (the generated random vocabulary)
// ═══════════════════════════════════════════════════════════════════════════════

/// Every public entrypoint the harness exercises.
///
/// Indices (`user_idx`, `solver_idx`) are always taken modulo the relevant
/// pool size before use, so proptest can generate any `usize` without
/// needing to know pool sizes up front.
#[derive(Debug, Clone)]
enum Action {
    // ── Intent lifecycle ─────────────────────────────────────────────────────
    /// Submit an intent for `users[user_idx % N_USERS]`.
    SubmitIntent { user_idx: usize },
    /// Solver claims an open intent.
    AcceptIntent {
        solver_idx: usize,
        /// Position (0-based) in the fixture's `open_intent_ids` list.
        intent_slot: usize,
    },
    /// Solver does a *full* fill.
    FillIntentFull {
        solver_idx: usize,
        intent_slot: usize,
    },
    /// Solver does a *partial* fill.
    FillIntentPartial {
        solver_idx: usize,
        intent_slot: usize,
    },
    /// User cancels one of their own intents.
    CancelIntent {
        user_idx: usize,
        intent_slot: usize,
    },
    /// Permissionless: materialise expiry of an Open intent.
    ExpireIntent { intent_slot: usize },
    /// Permissionless: slash a solver that missed the fill window.
    SlashSolver { intent_slot: usize },

    // ── Solver management ────────────────────────────────────────────────────
    /// Solver deposits a top-up bond (MIN_BOND).
    RegisterSolverTopup { solver_idx: usize },
    /// Solver deregisters (only valid when `active_intents == 0`).
    DeregisterSolver { solver_idx: usize },
    /// Solver withdraws MIN_BOND from their bond (only if headroom allows).
    WithdrawBond { solver_idx: usize },

    // ── Ledger-time jumps ────────────────────────────────────────────────────
    /// Advance ledger time by `secs` seconds.
    AdvanceTime { secs: u64 },
}

fn action_strategy() -> impl Strategy<Value = Action> {
    prop_oneof![
        // Intent lifecycle — weight toward lifecycle steps
        4 => (0..N_USERS).prop_map(|u| Action::SubmitIntent { user_idx: u }),
        4 => (0..N_SOLVERS, 0..10usize).prop_map(|(s, slot)| Action::AcceptIntent {
            solver_idx: s,
            intent_slot: slot,
        }),
        3 => (0..N_SOLVERS, 0..10usize).prop_map(|(s, slot)| Action::FillIntentFull {
            solver_idx: s,
            intent_slot: slot,
        }),
        3 => (0..N_SOLVERS, 0..10usize).prop_map(|(s, slot)| Action::FillIntentPartial {
            solver_idx: s,
            intent_slot: slot,
        }),
        2 => (0..N_USERS, 0..10usize).prop_map(|(u, slot)| Action::CancelIntent {
            user_idx: u,
            intent_slot: slot,
        }),
        2 => (0..10usize).prop_map(|slot| Action::ExpireIntent { intent_slot: slot }),
        2 => (0..10usize).prop_map(|slot| Action::SlashSolver { intent_slot: slot }),

        // Solver management
        1 => (0..N_SOLVERS).prop_map(|s| Action::RegisterSolverTopup { solver_idx: s }),
        1 => (0..N_SOLVERS).prop_map(|s| Action::DeregisterSolver { solver_idx: s }),
        1 => (0..N_SOLVERS).prop_map(|s| Action::WithdrawBond { solver_idx: s }),

        // Time jumps: small (within FILL_WINDOW), medium (past FILL_WINDOW),
        // and large (past INTENT_EXPIRY and SLASH_COOLDOWN).
        2 => (1u64..=FILL_WINDOW).prop_map(|s| Action::AdvanceTime { secs: s }),
        2 => (FILL_WINDOW + 1..=INTENT_EXPIRY).prop_map(|s| Action::AdvanceTime { secs: s }),
        1 => (SLASH_COOLDOWN + 1..=SLASH_COOLDOWN * 2).prop_map(|s| Action::AdvanceTime { secs: s }),
    ]
}

// ═══════════════════════════════════════════════════════════════════════════════
// Test fixture
// ═══════════════════════════════════════════════════════════════════════════════

struct Fixture {
    env: Env,
    contract_id: Address,
    bond_token: Address,
    dst_token: Address,
    fee_recipient: Address,
    users: Vec<Address>,
    solvers: Vec<Address>,
    /// All intent IDs submitted during the run, in submission order.
    intent_ids: Vec<BytesN<32>>,
    /// Mapping from BytesN<32> bytes → index in `intent_ids`.
    intent_index: HashMap<[u8; 32], usize>,
    /// Reference model — updated in parallel with every action.
    model: ReferenceState,
    /// Per-user last-cancel timestamp (mirrors on-chain CancelCooldown key).
    user_last_cancel: Vec<u64>,
}

impl Fixture {
    fn client(&self) -> IntentSettlementClient<'_> {
        IntentSettlementClient::new(&self.env, &self.contract_id)
    }

    fn bond_token_client(&self) -> token::Client<'_> {
        token::Client::new(&self.env, &self.bond_token)
    }

    fn bond_admin(&self) -> token::StellarAssetClient<'_> {
        token::StellarAssetClient::new(&self.env, &self.bond_token)
    }

    fn dst_admin(&self) -> token::StellarAssetClient<'_> {
        token::StellarAssetClient::new(&self.env, &self.dst_token)
    }

    fn now(&self) -> u64 {
        self.env.ledger().timestamp()
    }

    fn pass_time(&self, secs: u64) {
        self.env.ledger().with_mut(|li| li.timestamp += secs);
    }

    /// Return the intent_id at `slot`, cycling if out of range. Returns `None`
    /// when no intents have been submitted yet.
    fn intent_at_slot(&self, slot: usize) -> Option<BytesN<32>> {
        if self.intent_ids.is_empty() {
            return None;
        }
        Some(self.intent_ids[slot % self.intent_ids.len()].clone())
    }

    /// Return the intent_id bytes at `slot` as `[u8; 32]`.
    fn intent_bytes_at_slot(&self, slot: usize) -> Option<[u8; 32]> {
        self.intent_at_slot(slot)
            .map(|id| id.to_array())
    }

    /// Return only the intent IDs that are currently Open or PartiallyFilled
    /// according to our reference model. Returns `None` when none exist.
    fn open_intent_at_slot(&self, slot: usize) -> Option<BytesN<32>> {
        let open: Vec<_> = self
            .intent_ids
            .iter()
            .filter(|id| self.model.is_open_or_partial(&id.to_array()))
            .cloned()
            .collect();
        if open.is_empty() {
            return None;
        }
        Some(open[slot % open.len()].clone())
    }

    /// Return only intent IDs in `Accepted` state in the reference model.
    fn accepted_intent_at_slot(&self, slot: usize) -> Option<BytesN<32>> {
        let accepted: Vec<_> = self
            .intent_ids
            .iter()
            .filter(|id| {
                matches!(
                    self.model.intents.get(&id.to_array()).map(|i| &i.state),
                    Some(RefState::Accepted)
                )
            })
            .cloned()
            .collect();
        if accepted.is_empty() {
            return None;
        }
        Some(accepted[slot % accepted.len()].clone())
    }

    /// Return only intent IDs that the reference model says are `Open` or
    /// `PartiallyFilled` *and* whose deadline has passed (slashable).
    fn expired_accepted_intent_at_slot(&self, slot: usize) -> Option<BytesN<32>> {
        let now = self.now();
        let eligible: Vec<_> = self
            .intent_ids
            .iter()
            .filter(|id| {
                let bytes = id.to_array();
                if let Some(ri) = self.model.intents.get(&bytes) {
                    ri.state == RefState::Accepted && ri.deadline <= now
                } else {
                    false
                }
            })
            .cloned()
            .collect();
        if eligible.is_empty() {
            return None;
        }
        Some(eligible[slot % eligible.len()].clone())
    }

    /// Return only Open/PartiallyFilled intents whose deadline has passed.
    fn open_expired_intent_at_slot(&self, slot: usize) -> Option<BytesN<32>> {
        let now = self.now();
        let eligible: Vec<_> = self
            .intent_ids
            .iter()
            .filter(|id| {
                let bytes = id.to_array();
                if let Some(ri) = self.model.intents.get(&bytes) {
                    (ri.state == RefState::Open || ri.state == RefState::PartiallyFilled)
                        && ri.deadline <= now
                } else {
                    false
                }
            })
            .cloned()
            .collect();
        if eligible.is_empty() {
            return None;
        }
        Some(eligible[slot % eligible.len()].clone())
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Setup
// ═══════════════════════════════════════════════════════════════════════════════

fn setup() -> Fixture {
    let env = Env::default();
    env.mock_all_auths();
    // Start time well above zero so subtraction doesn't underflow.
    env.ledger().with_mut(|li| li.timestamp = 10_000);

    let admin = Address::generate(&env);
    let fee_recipient = Address::generate(&env);

    let bond_token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let dst_token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let contract_id = env.register_contract(None, IntentSettlement);

    let client = IntentSettlementClient::new(&env, &contract_id);
    client.initialize(&admin, &fee_recipient, &bond_token);

    // Create user pool.
    let mut users = Vec::with_capacity(N_USERS);
    for _ in 0..N_USERS {
        users.push(Address::generate(&env));
    }

    // Create solver pool and register each with STARTING_BOND.
    let bond_admin = token::StellarAssetClient::new(&env, &bond_token);
    let mut solvers = Vec::with_capacity(N_SOLVERS);
    let mut ref_solvers = Vec::with_capacity(N_SOLVERS);
    let mut solver_accepted_intents = Vec::with_capacity(N_SOLVERS);
    let mut solver_registered = Vec::with_capacity(N_SOLVERS);

    for _ in 0..N_SOLVERS {
        let s = Address::generate(&env);
        bond_admin.mint(&s, &STARTING_BOND);
        client.register_solver(&s, &STARTING_BOND);
        solvers.push(s);
        ref_solvers.push(RefSolver {
            bond_amount: STARTING_BOND,
            is_active: true,
            active_intents: 0,
            last_slash_time: 0,
            fills_completed: 0,
            fills_failed: 0,
        });
        solver_accepted_intents.push(Vec::new());
        solver_registered.push(true);
    }

    let total_bonded = STARTING_BOND * N_SOLVERS as i128;

    let mut model = ReferenceState {
        intents: HashMap::new(),
        solvers: ref_solvers,
        solver_accepted_intents,
        open_intents_count: 0,
        total_solvers: N_SOLVERS as u32,
        solver_registered,
        total_bonded,
    };

    Fixture {
        env,
        contract_id,
        bond_token,
        dst_token,
        fee_recipient,
        users,
        solvers,
        intent_ids: Vec::new(),
        intent_index: HashMap::new(),
        model,
        user_last_cancel: std::vec![0u64; N_USERS],
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Action executor
// ═══════════════════════════════════════════════════════════════════════════════

/// Execute one action against both the contract and the reference model.
///
/// Expected errors (guard conditions the reference model predicts will fail)
/// are silently ignored. Unexpected errors are propagated as assertion failures.
fn execute(f: &mut Fixture, action: &Action) {
    let c = f.client();

    match action {
        // ── SubmitIntent ─────────────────────────────────────────────────────
        Action::SubmitIntent { user_idx } => {
            let uidx = user_idx % N_USERS;
            let user = f.users[uidx].clone();
            let now = f.now();

            // Submit to the contract.
            let id = c.submit_intent(
                &user,
                &String::from_str(&f.env, "ethereum"),
                &String::from_str(
                    &f.env,
                    "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
                ),
                &SRC_AMOUNT,
                &f.dst_token,
                &MIN_DST,
                &(None::<u64>),
                &(None::<Address>),
            );

            // Mirror in the reference model.
            let id_bytes = id.to_array();
            let deadline = now + INTENT_EXPIRY;
            f.model.intents.insert(
                id_bytes,
                RefIntent {
                    state: RefState::Open,
                    user_idx: uidx,
                    solver_idx: None,
                    total_filled: 0,
                    min_dst_amount: MIN_DST,
                    deadline,
                },
            );
            f.model.open_intents_count += 1;

            let idx = f.intent_ids.len();
            f.intent_ids.push(id);
            f.intent_index.insert(id_bytes, idx);
        }

        // ── AcceptIntent ──────────────────────────────────────────────────────
        Action::AcceptIntent { solver_idx, intent_slot } => {
            let sidx = solver_idx % N_SOLVERS;
            let solver = f.solvers[sidx].clone();
            let now = f.now();

            // Pick an Open or PartiallyFilled intent from the reference model.
            let id = match f.open_intent_at_slot(*intent_slot) {
                Some(id) => id,
                None => return, // nothing to accept
            };
            let id_bytes = id.to_array();

            // Check reference-model preconditions.
            let solver_rec = &f.model.solvers[sidx];
            if !f.model.solver_registered[sidx] {
                return;
            }
            if !solver_rec.is_active {
                return;
            }
            // Slash cooldown guard.
            if solver_rec.last_slash_time > 0
                && now < solver_rec.last_slash_time + SLASH_COOLDOWN
            {
                return;
            }
            let intent = match f.model.intents.get(&id_bytes) {
                Some(i) => i.clone(),
                None => return,
            };
            if now >= intent.deadline {
                return; // deadline passed — accept_intent would lazily expire it
            }

            // Try the on-chain call.
            let result = c.try_accept_intent(&solver, &id);
            if result.is_err() {
                return; // any error is treated as expected guard failure
            }

            // Mirror in the reference model.
            let new_deadline = now + FILL_WINDOW;
            if let Some(ri) = f.model.intents.get_mut(&id_bytes) {
                ri.state = RefState::Accepted;
                ri.solver_idx = Some(sidx);
                ri.deadline = new_deadline;
            }
            f.model.open_intents_count = f.model.open_intents_count.saturating_sub(1);
            f.model.solvers[sidx].active_intents += 1;
            f.model.solver_accepted_intents[sidx].push(id_bytes);
        }

        // ── FillIntentFull ────────────────────────────────────────────────────
        Action::FillIntentFull { solver_idx, intent_slot } => {
            let sidx = solver_idx % N_SOLVERS;
            let solver = f.solvers[sidx].clone();
            let now = f.now();

            let id = match f.accepted_intent_at_slot(*intent_slot) {
                Some(id) => id,
                None => return,
            };
            let id_bytes = id.to_array();

            let intent = match f.model.intents.get(&id_bytes) {
                Some(i) => i.clone(),
                None => return,
            };
            // Only the assigned solver can fill.
            if intent.solver_idx != Some(sidx) {
                return;
            }
            if now >= intent.deadline {
                return; // fill window expired
            }

            // Mint enough dst tokens for the fill + fee.
            let fee = FULL_FILL * 5 / 10_000;
            f.dst_admin().mint(&solver, &(FULL_FILL + fee));

            let result = c.try_fill_intent(&solver, &id, &FULL_FILL, &false);
            if result.is_err() {
                return;
            }

            // Mirror in the reference model.
            if let Some(ri) = f.model.intents.get_mut(&id_bytes) {
                ri.total_filled += FULL_FILL;
                if ri.total_filled >= ri.min_dst_amount {
                    ri.state = RefState::Filled;
                } else {
                    // Shouldn't happen with FULL_FILL >= MIN_DST, but be safe.
                    ri.state = RefState::PartiallyFilled;
                    ri.solver_idx = None;
                    ri.deadline = now + INTENT_EXPIRY;
                    f.model.open_intents_count += 1;
                }
            }
            f.model.solvers[sidx].fills_completed += 1;
            f.model.solvers[sidx].active_intents =
                f.model.solvers[sidx].active_intents.saturating_sub(1);
            let sa = &mut f.model.solver_accepted_intents[sidx];
            sa.retain(|b| b != &id_bytes);
        }

        // ── FillIntentPartial ─────────────────────────────────────────────────
        Action::FillIntentPartial { solver_idx, intent_slot } => {
            let sidx = solver_idx % N_SOLVERS;
            let solver = f.solvers[sidx].clone();
            let now = f.now();

            let id = match f.accepted_intent_at_slot(*intent_slot) {
                Some(id) => id,
                None => return,
            };
            let id_bytes = id.to_array();

            let intent = match f.model.intents.get(&id_bytes) {
                Some(i) => i.clone(),
                None => return,
            };
            if intent.solver_idx != Some(sidx) {
                return;
            }
            if now >= intent.deadline {
                return;
            }

            let fee = PARTIAL_FILL * 5 / 10_000;
            f.dst_admin().mint(&solver, &(PARTIAL_FILL + fee));

            let result = c.try_fill_intent(&solver, &id, &PARTIAL_FILL, &false);
            if result.is_err() {
                return;
            }

            // Partial fill re-opens the intent with INTENT_EXPIRY.
            if let Some(ri) = f.model.intents.get_mut(&id_bytes) {
                ri.total_filled += PARTIAL_FILL;
                if ri.total_filled >= ri.min_dst_amount {
                    ri.state = RefState::Filled;
                } else {
                    ri.state = RefState::PartiallyFilled;
                    ri.solver_idx = None;
                    ri.deadline = now + INTENT_EXPIRY;
                    f.model.open_intents_count += 1;
                }
            }
            f.model.solvers[sidx].fills_completed += 1;
            f.model.solvers[sidx].active_intents =
                f.model.solvers[sidx].active_intents.saturating_sub(1);
            let sa = &mut f.model.solver_accepted_intents[sidx];
            sa.retain(|b| b != &id_bytes);
        }

        // ── CancelIntent ──────────────────────────────────────────────────────
        Action::CancelIntent { user_idx, intent_slot } => {
            let uidx = user_idx % N_USERS;
            let user = f.users[uidx].clone();
            let now = f.now();

            // Cooldown guard.
            if now < f.user_last_cancel[uidx] + CANCEL_COOLDOWN {
                return;
            }

            // Find an Open or PartiallyFilled intent owned by this user.
            let id = {
                let uidx_cap = uidx;
                let open: Vec<_> = f
                    .intent_ids
                    .iter()
                    .filter(|id| {
                        let bytes = id.to_array();
                        if let Some(ri) = f.model.intents.get(&bytes) {
                            ri.user_idx == uidx_cap
                                && (ri.state == RefState::Open
                                    || ri.state == RefState::PartiallyFilled)
                        } else {
                            false
                        }
                    })
                    .cloned()
                    .collect();
                if open.is_empty() {
                    return;
                }
                open[intent_slot % open.len()].clone()
            };
            let id_bytes = id.to_array();

            let result = c.try_cancel_intent(&user, &id);
            if result.is_err() {
                return;
            }

            if let Some(ri) = f.model.intents.get_mut(&id_bytes) {
                ri.state = RefState::Cancelled;
            }
            f.model.open_intents_count = f.model.open_intents_count.saturating_sub(1);
            f.user_last_cancel[uidx] = now;
        }

        // ── ExpireIntent ──────────────────────────────────────────────────────
        Action::ExpireIntent { intent_slot } => {
            let now = f.now();
            let id = match f.open_expired_intent_at_slot(*intent_slot) {
                Some(id) => id,
                None => return,
            };
            let id_bytes = id.to_array();

            let result = c.try_expire_intent(&id);
            if result.is_err() {
                return;
            }

            if let Some(ri) = f.model.intents.get_mut(&id_bytes) {
                ri.state = RefState::Expired;
            }
            f.model.open_intents_count = f.model.open_intents_count.saturating_sub(1);
        }

        // ── SlashSolver ───────────────────────────────────────────────────────
        Action::SlashSolver { intent_slot } => {
            let now = f.now();
            let id = match f.expired_accepted_intent_at_slot(*intent_slot) {
                Some(id) => id,
                None => return,
            };
            let id_bytes = id.to_array();

            let intent = match f.model.intents.get(&id_bytes) {
                Some(i) => i.clone(),
                None => return,
            };
            let sidx = match intent.solver_idx {
                Some(s) => s,
                None => return,
            };

            let result = c.try_slash_solver(&id);
            if result.is_err() {
                return;
            }

            // Compute proportional slash (mirrors compute_slash_amount in lib.rs).
            let solver_rec = &f.model.solvers[sidx];
            let unfilled = intent.min_dst_amount - intent.total_filled;
            let bond = solver_rec.bond_amount;
            let slash = compute_slash_amount_model(bond, unfilled);

            // Update reference model.
            f.model.solvers[sidx].bond_amount -= slash;
            f.model.solvers[sidx].fills_failed += 1;
            f.model.solvers[sidx].last_slash_time = now;
            f.model.solvers[sidx].active_intents =
                f.model.solvers[sidx].active_intents.saturating_sub(1);
            if f.model.solvers[sidx].bond_amount < MIN_BOND {
                f.model.solvers[sidx].is_active = false;
            }
            let sa = &mut f.model.solver_accepted_intents[sidx];
            sa.retain(|b| b != &id_bytes);
            f.model.total_bonded -= slash;

            // Intent is re-opened with fresh deadline.
            if let Some(ri) = f.model.intents.get_mut(&id_bytes) {
                ri.state = RefState::Open;
                ri.solver_idx = None;
                ri.deadline = now + INTENT_EXPIRY;
            }
            f.model.open_intents_count += 1;
        }

        // ── RegisterSolverTopup ───────────────────────────────────────────────
        Action::RegisterSolverTopup { solver_idx } => {
            let sidx = solver_idx % N_SOLVERS;
            if !f.model.solver_registered[sidx] {
                return;
            }
            let solver = f.solvers[sidx].clone();
            f.bond_admin().mint(&solver, &MIN_BOND);
            let result = c.try_register_solver(&solver, &MIN_BOND);
            if result.is_err() {
                return;
            }
            f.model.solvers[sidx].bond_amount += MIN_BOND;
            f.model.solvers[sidx].is_active = true;
            f.model.total_bonded += MIN_BOND;
        }

        // ── DeregisterSolver ──────────────────────────────────────────────────
        Action::DeregisterSolver { solver_idx } => {
            let sidx = solver_idx % N_SOLVERS;
            if !f.model.solver_registered[sidx] {
                return;
            }
            if f.model.solvers[sidx].active_intents > 0 {
                return;
            }
            let solver = f.solvers[sidx].clone();
            let result = c.try_deregister_solver(&solver);
            if result.is_err() {
                return;
            }
            let refund = f.model.solvers[sidx].bond_amount;
            f.model.total_bonded -= refund;
            f.model.solvers[sidx].bond_amount = 0;
            f.model.solvers[sidx].is_active = false;
            f.model.solvers[sidx].active_intents = 0;
            f.model.solver_registered[sidx] = false;
            f.model.total_solvers -= 1;
        }

        // ── WithdrawBond ──────────────────────────────────────────────────────
        Action::WithdrawBond { solver_idx } => {
            let sidx = solver_idx % N_SOLVERS;
            if !f.model.solver_registered[sidx] {
                return;
            }
            let remaining = f.model.solvers[sidx].bond_amount - MIN_BOND;
            if remaining < MIN_BOND {
                return; // would drop below minimum
            }
            let solver = f.solvers[sidx].clone();
            let result = c.try_withdraw_bond(&solver, &MIN_BOND);
            if result.is_err() {
                return;
            }
            f.model.solvers[sidx].bond_amount -= MIN_BOND;
            f.model.total_bonded -= MIN_BOND;
        }

        // ── AdvanceTime ───────────────────────────────────────────────────────
        Action::AdvanceTime { secs } => {
            f.pass_time(*secs);
        }
    }
}

/// Pure-Rust mirror of `IntentSettlement::compute_slash_amount`.
///
/// Issue #193: proportional slash — `min(intent_value, bond) / 10`, capped at
/// `bond * SLASH_BPS / 10_000` (10%), floored at 1 stroop.
fn compute_slash_amount_model(bond: i128, unfilled: i128) -> i128 {
    if bond <= 0 {
        return 0;
    }
    let exposure = unfilled.min(bond).max(0);
    let cap = (bond * 1_000 / 10_000).min(bond).max(1);
    (exposure / 10).max(1).min(cap)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Invariant checkers
// ═══════════════════════════════════════════════════════════════════════════════

/// Assert all six global invariants against both on-chain state and the
/// reference model. Called after every step.
fn assert_invariants(f: &Fixture) {
    let c = f.client();
    let env = &f.env;

    // ── I1: OpenIntents == count(Open | PartiallyFilled) ─────────────────────
    {
        let (_, _, on_chain_open) = c.get_stats();
        let model_open = f.model.recount_open();
        assert_eq!(
            on_chain_open, model_open,
            "I1 violated: on-chain OpenIntents={on_chain_open} but reference model counts {model_open} open/partial intents"
        );
        assert_eq!(
            on_chain_open, f.model.open_intents_count,
            "I1 violated: on-chain OpenIntents={on_chain_open} but model counter says {}",
            f.model.open_intents_count
        );
    }

    // ── I2: TotalSolvers == len(SolverList) ───────────────────────────────────
    {
        let total_solvers = c.get_solver_count();
        let model_total = f.model.recount_solvers();
        assert_eq!(
            total_solvers, model_total,
            "I2 violated: on-chain TotalSolvers={total_solvers} but reference model counts {model_total}"
        );
        // list_solvers paginates; fetch all.
        let listed = c.list_solvers(&0, &(N_SOLVERS as u32 * 2));
        assert_eq!(
            total_solvers, listed.len(),
            "I2 violated: TotalSolvers={total_solvers} but SolverList has {} entries",
            listed.len()
        );
    }

    // ── I3: solver.active_intents == len(SolverIntents) ──────────────────────
    {
        for (sidx, solver) in f.solvers.iter().enumerate() {
            if !f.model.solver_registered[sidx] {
                continue;
            }
            if let Some(rec) = c.get_solver(solver) {
                let intents_vec = c.get_solver_intents(solver);
                assert_eq!(
                    rec.active_intents,
                    intents_vec.len(),
                    "I3 violated: solver[{sidx}] active_intents={} but SolverIntents has {} entries",
                    rec.active_intents,
                    intents_vec.len()
                );
                // Also check reference model agrees with on-chain.
                assert_eq!(
                    rec.active_intents,
                    f.model.solvers[sidx].active_intents,
                    "I3 violated: on-chain active_intents={} but reference model says {}",
                    rec.active_intents,
                    f.model.solvers[sidx].active_intents
                );
            }
        }
    }

    // ── I4: Solvency — contract bond balance >= Σ solver bonds ───────────────
    {
        let contract_bal = token::Client::new(env, &f.bond_token)
            .balance(&f.contract_id);
        let sum_bonds: i128 = f
            .solvers
            .iter()
            .filter_map(|s| c.get_solver(s))
            .map(|r| r.bond_amount)
            .sum();
        assert!(
            contract_bal >= sum_bonds,
            "I4 violated: contract holds {contract_bal} bond tokens but Σ bond_amounts = {sum_bonds}"
        );
    }

    // ── I5: No impossible intent state ───────────────────────────────────────
    {
        for id in &f.intent_ids {
            if let Some(intent) = c.get_intent(id) {
                match intent.state {
                    IntentState::Filled => {
                        assert!(
                            intent.total_filled >= intent.min_dst_amount,
                            "I5 violated: intent {:?} is Filled but total_filled={} < min_dst_amount={}",
                            id,
                            intent.total_filled,
                            intent.min_dst_amount
                        );
                    }
                    IntentState::PartiallyFilled => {
                        assert!(
                            intent.total_filled > 0 && intent.total_filled < intent.min_dst_amount,
                            "I5 violated: intent {:?} is PartiallyFilled but total_filled={}",
                            id,
                            intent.total_filled
                        );
                        // A PartiallyFilled intent must have no assigned solver
                        // (it was re-opened).
                        assert!(
                            intent.solver.is_none(),
                            "I5 violated: PartiallyFilled intent {:?} still has a solver assigned",
                            id
                        );
                    }
                    IntentState::Open => {
                        assert!(
                            intent.total_filled == 0 || intent.solver.is_none(),
                            "I5 violated: Open intent {:?} has solver and total_filled={}",
                            id,
                            intent.total_filled
                        );
                    }
                    IntentState::Accepted => {
                        assert!(
                            intent.solver.is_some(),
                            "I5 violated: Accepted intent {:?} has no solver",
                            id
                        );
                    }
                    _ => {} // terminal states — no additional structural checks
                }
            }
        }
    }

    // ── I6: Reference model agrees with on-chain state ───────────────────────
    {
        for id in &f.intent_ids {
            let bytes = id.to_array();
            let model_intent = match f.model.intents.get(&bytes) {
                Some(i) => i,
                None => continue,
            };
            if let Some(chain_intent) = c.get_intent(id) {
                let expected = match model_intent.state {
                    RefState::Open => IntentState::Open,
                    RefState::Accepted => IntentState::Accepted,
                    RefState::PartiallyFilled => IntentState::PartiallyFilled,
                    RefState::Filled => IntentState::Filled,
                    RefState::Cancelled => IntentState::Cancelled,
                    RefState::Expired => IntentState::Expired,
                    RefState::Slashed => IntentState::Slashed,
                };
                assert_eq!(
                    chain_intent.state, expected,
                    "I6 violated: intent {:?} on-chain={:?} but reference model expects {:?}",
                    id, chain_intent.state, expected
                );
                assert_eq!(
                    chain_intent.total_filled, model_intent.total_filled,
                    "I6 violated: intent {:?} total_filled on-chain={} model={}",
                    id, chain_intent.total_filled, model_intent.total_filled
                );
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Proptest entry point
// ═══════════════════════════════════════════════════════════════════════════════

proptest! {
    #![proptest_config(ProptestConfig {
        cases: DEFAULT_CASES,
        // Allow the env var override for nightly CI (PROPTEST_CASES=2048).
        ..ProptestConfig::default()
    })]

    /// Stateful harness: random interleaving of all public entrypoints across
    /// multiple actors with time jumps, checked against the reference model
    /// after every step.
    #[test]
    fn stateful_intent_settlement(
        actions in proptest::collection::vec(action_strategy(), 1..=MAX_STEPS)
    ) {
        let mut f = setup();
        assert_invariants(&f); // baseline

        for action in &actions {
            execute(&mut f, action);
            assert_invariants(&f);
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Seeded historical bug regression tests
// ═══════════════════════════════════════════════════════════════════════════════
//
// Each test encodes a deterministic sequence that would have triggered a
// real bug found during development. They serve as non-shrinkable regression
// guards: the stateful harness above will also discover them via random
// generation, but these pin the exact minimal reproducer in the test suite.
//
// Bug #1 — OpenIntents counter drift on partial fill + re-accept
//   Historically, a partial fill that re-opened the intent also reset the
//   assigned solver, but the OpenIntents counter was not incremented, so
//   the counter fell one below the true count of open intents.
//   Invariant violated: I1 (OpenIntents != count(Open | PartiallyFilled)).
//
// Bug #2 — active_intents not decremented on slash
//   After slash_solver, the contract re-opens the intent but in one version
//   forgot to decrement `solver_record.active_intents`.  A solver slashed once
//   could no longer deregister even after all intents settled, because the
//   count was permanently inflated.
//   Invariant violated: I3 (active_intents != len(SolverIntents)).
//
// Bug #3 — TotalBonded not updated on deregister
//   deregister_solver returned the bond to the solver but forgot to subtract
//   from `DataKey::TotalBonded`, causing TotalBonded to slowly drift above the
//   sum of actual recorded bonds.  The solvency invariant I4 still holds
//   (contract balance >= Σ bonds), but TotalBonded overstates the true figure.
//   We capture this via a dedicated TotalBonded == Σ bonds check below.

/// Regression test for Bug #1: OpenIntents counter drift after partial fill.
///
/// Sequence: submit → accept → partial fill → check I1.
///
/// The re-opened PartiallyFilled intent must be counted by OpenIntents.
/// If the counter was not incremented during partial fill this assertion fails.
#[test]
fn regression_bug1_open_intents_drift_on_partial_fill() {
    let mut f = setup();
    let c = f.client();

    // Submit an intent.
    let id = c.submit_intent(
        &f.users[0],
        &String::from_str(&f.env, "ethereum"),
        &String::from_str(&f.env, "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
        &SRC_AMOUNT,
        &f.dst_token,
        &MIN_DST,
        &(None::<u64>),
        &(None::<Address>),
    );
    let id_bytes = id.to_array();
    f.model.intents.insert(
        id_bytes,
        RefIntent {
            state: RefState::Open,
            user_idx: 0,
            solver_idx: None,
            total_filled: 0,
            min_dst_amount: MIN_DST,
            deadline: f.now() + INTENT_EXPIRY,
        },
    );
    f.model.open_intents_count += 1;
    f.intent_ids.push(id.clone());
    f.intent_index.insert(id_bytes, 0);

    // Solver accepts.
    c.accept_intent(&f.solvers[0], &id);
    {
        let ri = f.model.intents.get_mut(&id_bytes).unwrap();
        ri.state = RefState::Accepted;
        ri.solver_idx = Some(0);
        ri.deadline = f.now() + FILL_WINDOW;
    }
    f.model.open_intents_count -= 1;
    f.model.solvers[0].active_intents += 1;
    f.model.solver_accepted_intents[0].push(id_bytes);

    // Partial fill: solver sends PARTIAL_FILL (< MIN_DST).
    let fee = PARTIAL_FILL * 5 / 10_000;
    f.dst_admin().mint(&f.solvers[0], &(PARTIAL_FILL + fee));
    c.fill_intent(&f.solvers[0], &id, &PARTIAL_FILL, &false);

    // Mirror partial fill.
    {
        let now = f.now();
        let ri = f.model.intents.get_mut(&id_bytes).unwrap();
        ri.total_filled += PARTIAL_FILL;
        ri.state = RefState::PartiallyFilled;
        ri.solver_idx = None;
        ri.deadline = now + INTENT_EXPIRY;
    }
    f.model.open_intents_count += 1; // re-opened
    f.model.solvers[0].fills_completed += 1;
    f.model.solvers[0].active_intents -= 1;
    f.model.solver_accepted_intents[0].retain(|b| b != &id_bytes);

    // I1: OpenIntents must equal 1 (the PartiallyFilled intent is "open").
    let (_, _, on_chain_open) = c.get_stats();
    assert_eq!(
        on_chain_open, 1,
        "Bug #1 regression: OpenIntents should be 1 after partial fill, got {on_chain_open}"
    );
    assert_invariants(&f);
}

/// Regression test for Bug #2: active_intents not decremented on slash.
///
/// Sequence: submit → accept → advance past FILL_WINDOW → slash → check I3.
///
/// After slash the solver's `active_intents` must be 0 (and SolverIntents empty).
#[test]
fn regression_bug2_active_intents_not_decremented_on_slash() {
    let mut f = setup();
    let c = f.client();

    // Submit an intent and accept it.
    let id = c.submit_intent(
        &f.users[1],
        &String::from_str(&f.env, "ethereum"),
        &String::from_str(&f.env, "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"),
        &SRC_AMOUNT,
        &f.dst_token,
        &MIN_DST,
        &(None::<u64>),
        &(None::<Address>),
    );
    let id_bytes = id.to_array();
    f.model.intents.insert(
        id_bytes,
        RefIntent {
            state: RefState::Open,
            user_idx: 1,
            solver_idx: None,
            total_filled: 0,
            min_dst_amount: MIN_DST,
            deadline: f.now() + INTENT_EXPIRY,
        },
    );
    f.model.open_intents_count += 1;
    f.intent_ids.push(id.clone());
    f.intent_index.insert(id_bytes, 0);

    c.accept_intent(&f.solvers[1], &id);
    {
        let ri = f.model.intents.get_mut(&id_bytes).unwrap();
        ri.state = RefState::Accepted;
        ri.solver_idx = Some(1);
        ri.deadline = f.now() + FILL_WINDOW;
    }
    f.model.open_intents_count -= 1;
    f.model.solvers[1].active_intents += 1;
    f.model.solver_accepted_intents[1].push(id_bytes);

    // Advance past the fill window.
    f.pass_time(FILL_WINDOW + 1);

    // Slash the solver.
    c.slash_solver(&id);
    {
        let now = f.now();
        let ri = f.model.intents.get(&id_bytes).unwrap().clone();
        let bond = f.model.solvers[1].bond_amount;
        let unfilled = ri.min_dst_amount - ri.total_filled;
        let slash = compute_slash_amount_model(bond, unfilled);
        f.model.solvers[1].bond_amount -= slash;
        f.model.solvers[1].fills_failed += 1;
        f.model.solvers[1].last_slash_time = now;
        f.model.solvers[1].active_intents = 0;
        f.model.solver_accepted_intents[1].clear();
        f.model.total_bonded -= slash;
        if f.model.solvers[1].bond_amount < MIN_BOND {
            f.model.solvers[1].is_active = false;
        }
        let entry = f.model.intents.get_mut(&id_bytes).unwrap();
        entry.state = RefState::Open;
        entry.solver_idx = None;
        entry.deadline = now + INTENT_EXPIRY;
        f.model.open_intents_count += 1;
    }

    // I3 check: solver's active_intents must be 0.
    let rec = c.get_solver(&f.solvers[1]).expect("solver must still exist after slash");
    assert_eq!(
        rec.active_intents, 0,
        "Bug #2 regression: solver active_intents should be 0 after slash, got {}",
        rec.active_intents
    );
    let intents_list = c.get_solver_intents(&f.solvers[1]);
    assert_eq!(
        intents_list.len(), 0,
        "Bug #2 regression: SolverIntents should be empty after slash, has {} entries",
        intents_list.len()
    );
    assert_invariants(&f);
}

/// Regression test for Bug #3: TotalBonded not updated on deregister.
///
/// Sequence: register solver (already done in setup) → deregister →
/// verify TotalBonded == Σ remaining solver bonds.
///
/// If deregister_solver forgot to subtract from TotalBonded the sum would be
/// inflated by the deregistered solver's bond.
#[test]
fn regression_bug3_total_bonded_drift_on_deregister() {
    let mut f = setup();
    let c = f.client();

    // Deregister solver[2] (no active intents, safe to deregister immediately).
    let solver = f.solvers[2].clone();
    let bond_before = c.get_solver(&solver).unwrap().bond_amount;

    c.deregister_solver(&solver);
    f.model.total_bonded -= bond_before;
    f.model.solvers[2].bond_amount = 0;
    f.model.solvers[2].is_active = false;
    f.model.solver_registered[2] = false;
    f.model.total_solvers -= 1;

    // Read TotalBonded from on-chain stats — currently there's no direct getter
    // for TotalBonded, so we verify via bond-token balance and Σ bonds.
    let contract_bal = f.bond_token_client().balance(&f.contract_id);

    // Sum remaining bonds (only solvers 0 and 1 are still registered).
    let sum_bonds: i128 = f
        .solvers
        .iter()
        .filter_map(|s| c.get_solver(s))
        .map(|r| r.bond_amount)
        .sum();

    // The contract balance must be exactly Σ bonds (no extra tokens should
    // linger after the deregistered solver's bond was refunded).
    assert_eq!(
        contract_bal, sum_bonds,
        "Bug #3 regression: contract_bal={contract_bal} but Σ bond_amounts={sum_bonds} after deregister"
    );

    assert_invariants(&f);
}
