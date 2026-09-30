#![cfg(test)]

//! Resource-cost harness for `solver_registry` (issue #149).
//!
//! Runs each public entrypoint once, from an isolated fixture, under
//! `soroban_sdk`'s test-mode [`Budget`] and records:
//!
//!   * `cpu` — CPU instructions consumed (`Budget::cpu_instruction_cost`)
//!   * `mem` — memory bytes consumed (`Budget::memory_bytes_cost`)
//!
//! ## Ceiling assertions
//!
//! Every entrypoint has a hard ceiling on its CPU and memory cost, set at
//! +10 % above the baseline snapshot in `docs/149-solver-registry.md`.
//! If a change causes an entrypoint to exceed its ceiling, the corresponding
//! `bench_ceiling_*` test fails immediately in CI.
//!
//! To update a ceiling after a deliberate cost increase:
//!
//! 1. Run `cargo test --features testutils bench::resource_cost_report -- --nocapture`
//!    to get the new measured values.
//! 2. Set the corresponding `CPU_CEIL_*` / `MEM_CEIL_*` constant to
//!    `new_value * 110 / 100` (round up to the nearest 1 000).
//! 3. Update `docs/149-solver-registry.md` and
//!    `docs/149-resource-cost-per-entrypoint.md` with the new baseline.
//!
//! ## Methodology & caveats
//!
//! * The SDK runs the contract **natively as Rust**, not as Wasm. CPU and
//!   memory figures are approximate; treat them as relative rankings and lower
//!   bounds, not as fee quotes.
//! * Token transfers inside `register_solver`, `stake`, `unstake`,
//!   `deregister_solver`, and `slash` invoke the Stellar Asset Contract;
//!   that cost is included in each row.
//! * `compute_reputation_score` is a pure-function entrypoint (no env I/O
//!   other than the function dispatch itself).
//!
//! Regenerate the published tables with:
//! ```text
//! cargo test --features testutils bench::resource_cost_report -- --nocapture
//! ```

extern crate std;

use soroban_sdk::{
    testutils::Address as _,
    token, Address, Env,
};
use std::{format, string::String as StdString, vec::Vec as StdVec};

use crate::{SolverRecord, SolverRegistry, SolverRegistryClient, USDC};

/// Tier-0 (Unranked) bond floor: 50 USDC.
const FLOOR: i128 = 50 * USDC;
/// A generous Platinum-tier bond: 50,000 USDC.
const LARGE_BOND: i128 = 50_000 * USDC;

// ─── Ceiling constants ────────────────────────────────────────────────────────
//
// Ceilings are set at +10 % above the first measured baseline (captured at
// soroban-sdk 21.0.0 on stable Rust).  See docs/149-solver-registry.md for
// the exact baseline values.
//
// State-changing entrypoints (write-path):

// initialize — one-time setup with 5-row tier table seed
const CPU_CEIL_INITIALIZE: u64 = 350_000;
const MEM_CEIL_INITIALIZE: u64 = 55_000;

// set_writer — single instance write + event
const CPU_CEIL_SET_WRITER: u64 = 160_000;
const MEM_CEIL_SET_WRITER: u64 = 28_000;

// set_tier_threshold — read/modify/write 5-row Vec + event
const CPU_CEIL_SET_TIER_THRESHOLD: u64 = 200_000;
const MEM_CEIL_SET_TIER_THRESHOLD: u64 = 35_000;

// register_solver — persistent write + token transfer + event
const CPU_CEIL_REGISTER: u64 = 380_000;
const MEM_CEIL_REGISTER: u64 = 60_000;

// stake — persistent read/write + token transfer + event
const CPU_CEIL_STAKE: u64 = 340_000;
const MEM_CEIL_STAKE: u64 = 52_000;

// unstake — persistent read/write + token transfer + bounds check + event
const CPU_CEIL_UNSTAKE: u64 = 340_000;
const MEM_CEIL_UNSTAKE: u64 = 52_000;

// deregister_solver — persistent delete + token transfer + event
const CPU_CEIL_DEREGISTER: u64 = 350_000;
const MEM_CEIL_DEREGISTER: u64 = 55_000;

// record_fill — persistent read/write + event (no token transfer)
const CPU_CEIL_RECORD_FILL: u64 = 200_000;
const MEM_CEIL_RECORD_FILL: u64 = 35_000;

// record_failure — persistent read/write + event
const CPU_CEIL_RECORD_FAILURE: u64 = 190_000;
const MEM_CEIL_RECORD_FAILURE: u64 = 33_000;

// slash — persistent read/write + token transfer + reputation calc + event
const CPU_CEIL_SLASH: u64 = 400_000;
const MEM_CEIL_SLASH: u64 = 62_000;

// Read-only entrypoints (no persistent writes):

// get_tier — persistent read + tier calculation
const CPU_CEIL_GET_TIER: u64 = 170_000;
const MEM_CEIL_GET_TIER: u64 = 28_000;

// tier_for — pure computation on instance data
const CPU_CEIL_TIER_FOR: u64 = 150_000;
const MEM_CEIL_TIER_FOR: u64 = 25_000;

// get_reputation_score — persistent read + score computation
const CPU_CEIL_GET_REPUTATION_SCORE: u64 = 160_000;
const MEM_CEIL_GET_REPUTATION_SCORE: u64 = 27_000;

// get_solver — persistent read only
const CPU_CEIL_GET_SOLVER: u64 = 150_000;
const MEM_CEIL_GET_SOLVER: u64 = 25_000;

// get_solver_count — instance read only
const CPU_CEIL_GET_SOLVER_COUNT: u64 = 120_000;
const MEM_CEIL_GET_SOLVER_COUNT: u64 = 20_000;

// get_tier_table — instance read + Vec construction (5 rows)
const CPU_CEIL_GET_TIER_TABLE: u64 = 180_000;
const MEM_CEIL_GET_TIER_TABLE: u64 = 30_000;

// get_fill_window_bonus_pct — instance read + array index
const CPU_CEIL_GET_FILL_WINDOW_BONUS_PCT: u64 = 130_000;
const MEM_CEIL_GET_FILL_WINDOW_BONUS_PCT: u64 = 22_000;

// get_slash_bps — instance read + array index
const CPU_CEIL_GET_SLASH_BPS: u64 = 130_000;
const MEM_CEIL_GET_SLASH_BPS: u64 = 22_000;

// get_fee_rebate_bps — instance read + array index
const CPU_CEIL_GET_FEE_REBATE_BPS: u64 = 130_000;
const MEM_CEIL_GET_FEE_REBATE_BPS: u64 = 22_000;

// get_admin — single instance read
const CPU_CEIL_GET_ADMIN: u64 = 120_000;
const MEM_CEIL_GET_ADMIN: u64 = 20_000;

// get_bond_token — single instance read
const CPU_CEIL_GET_BOND_TOKEN: u64 = 120_000;
const MEM_CEIL_GET_BOND_TOKEN: u64 = 20_000;

// compute_reputation_score — pure function, no env I/O
const CPU_CEIL_COMPUTE_REPUTATION_SCORE: u64 = 110_000;
const MEM_CEIL_COMPUTE_REPUTATION_SCORE: u64 = 18_000;

// ─── Infrastructure ───────────────────────────────────────────────────────────

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Measurement {
    cpu: u64,
    mem: u64,
}

fn measure<T>(env: &Env, f: impl FnOnce() -> T) -> (T, Measurement) {
    env.budget().reset_default();
    let out = f();
    let b = env.budget();
    let m = Measurement {
        cpu: b.cpu_instruction_cost(),
        mem: b.memory_bytes_cost(),
    };
    (out, m)
}

struct Fixture {
    env: Env,
    admin: Address,
    fee_recipient: Address,
    solver: Address,
    bond_token: Address,
    contract_id: Address,
}

impl Fixture {
    fn new() -> Self {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let fee_recipient = Address::generate(&env);
        let solver = Address::generate(&env);
        let bond_token = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let contract_id = env.register_contract(None, SolverRegistry);

        let f = Fixture {
            env,
            admin,
            fee_recipient,
            solver,
            bond_token,
            contract_id,
        };
        f.client()
            .initialize(&f.admin, &f.bond_token, &f.fee_recipient);
        f
    }

    fn client(&self) -> SolverRegistryClient<'_> {
        SolverRegistryClient::new(&self.env, &self.contract_id)
    }

    fn bond_admin(&self) -> token::StellarAssetClient<'_> {
        token::StellarAssetClient::new(&self.env, &self.bond_token)
    }

    /// Mint `amount` to the default solver and register them.
    fn register(&self, bond: i128) {
        self.bond_admin().mint(&self.solver, &bond);
        self.client().register_solver(&self.solver, &bond);
    }

    /// Build a SolverRecord with `fills` completed fills and `vol` total volume,
    /// used for testing compute_reputation_score.
    fn record_with_fills(&self, fills: u32, vol: i128) -> SolverRecord {
        SolverRecord {
            address: self.solver.clone(),
            bond_amount: LARGE_BOND,
            fills_completed: fills,
            fills_failed: 0,
            total_volume: vol,
            registered_at: self.env.ledger().timestamp(),
            last_slash_time: 0,
            slashed_total: 0,
        }
    }
}

// ─── Reporting ────────────────────────────────────────────────────────────────

type Row = (StdString, Measurement);

fn push(rows: &mut StdVec<Row>, label: &str, m: Measurement) {
    rows.push((StdString::from(label), m));
}

fn fmt_table(rows: &[Row]) -> StdString {
    let mut out = StdString::new();
    out.push_str("| Entrypoint | CPU insns | CPU ceil | Mem bytes | Mem ceil |\n|---|--:|--:|--:|--:|\n");
    for (label, m) in rows {
        out.push_str(&format!("| `{}` | {} | — | {} | — |\n", label, m.cpu, m.mem));
    }
    out
}

fn collect_rows() -> StdVec<Row> {
    let mut rows: StdVec<Row> = StdVec::new();

    // initialize (measured from a fresh, un-initialized contract)
    {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let fee_recipient = Address::generate(&env);
        let bond_token = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let contract_id = env.register_contract(None, SolverRegistry);
        let client = SolverRegistryClient::new(&env, &contract_id);
        let (_, m) = measure(&env, || client.initialize(&admin, &bond_token, &fee_recipient));
        push(&mut rows, "initialize", m);
    }

    // Admin write path
    {
        let f = Fixture::new();
        let writer = Address::generate(&f.env);
        let (_, m) = measure(&f.env, || f.client().set_writer(&writer));
        push(&mut rows, "set_writer", m);
    }

    {
        let f = Fixture::new();
        // Tune tier 2 (Silver): must stay between tier 1 (Bronze) and tier 3 (Gold)
        // Bronze: min_bond=500 USDC, min_score=1000 | Gold: min_bond=10000 USDC, min_score=7000
        let (_, m) = measure(&f.env, || {
            f.client()
                .set_tier_threshold(&2, &(1_000 * USDC), &3_500)
        });
        push(&mut rows, "set_tier_threshold", m);
    }

    // Solver self-service
    {
        let f = Fixture::new();
        f.bond_admin().mint(&f.solver, &(LARGE_BOND * 2));
        let (_, m) = measure(&f.env, || {
            f.client().register_solver(&f.solver, &LARGE_BOND)
        });
        push(&mut rows, "register_solver", m);
    }

    {
        let f = Fixture::new();
        f.register(FLOOR);
        f.bond_admin().mint(&f.solver, &FLOOR);
        let (_, m) = measure(&f.env, || f.client().stake(&f.solver, &FLOOR));
        push(&mut rows, "stake", m);
    }

    {
        let f = Fixture::new();
        // Register with 2× floor so there's room to unstake floor without going below floor
        f.register(FLOOR * 2);
        let (_, m) = measure(&f.env, || f.client().unstake(&f.solver, &FLOOR));
        push(&mut rows, "unstake", m);
    }

    {
        let f = Fixture::new();
        f.register(FLOOR);
        let (_, m) = measure(&f.env, || f.client().deregister_solver(&f.solver));
        push(&mut rows, "deregister_solver", m);
    }

    // Settlement write path (writer = admin for these measurements)
    {
        let f = Fixture::new();
        f.register(LARGE_BOND);
        let fill_amount: i128 = 1_000 * USDC;
        let (_, m) = measure(&f.env, || {
            f.client().record_fill(&f.admin, &f.solver, &fill_amount)
        });
        push(&mut rows, "record_fill", m);
    }

    {
        let f = Fixture::new();
        f.register(LARGE_BOND);
        let (_, m) = measure(&f.env, || f.client().record_failure(&f.admin, &f.solver));
        push(&mut rows, "record_failure", m);
    }

    {
        let f = Fixture::new();
        f.register(LARGE_BOND);
        let (_, m) = measure(&f.env, || f.client().slash(&f.admin, &f.solver));
        push(&mut rows, "slash", m);
    }

    // Read-only views
    {
        let f = Fixture::new();
        f.register(LARGE_BOND);
        let (_, m) = measure(&f.env, || f.client().get_tier(&f.solver));
        push(&mut rows, "get_tier", m);
    }

    {
        let f = Fixture::new();
        f.register(LARGE_BOND);
        // Worst-case score: a solver with many fills at Platinum threshold
        let score_bps: u32 = 9_500;
        let (_, m) = measure(&f.env, || f.client().tier_for(&score_bps, &LARGE_BOND));
        push(&mut rows, "tier_for", m);
    }

    {
        let f = Fixture::new();
        f.register(LARGE_BOND);
        let (_, m) = measure(&f.env, || f.client().get_reputation_score(&f.solver));
        push(&mut rows, "get_reputation_score", m);
    }

    {
        let f = Fixture::new();
        f.register(LARGE_BOND);
        let (_, m) = measure(&f.env, || f.client().get_solver(&f.solver));
        push(&mut rows, "get_solver", m);
    }

    {
        let f = Fixture::new();
        f.register(LARGE_BOND);
        let (_, m) = measure(&f.env, || f.client().get_solver_count());
        push(&mut rows, "get_solver_count", m);
    }

    {
        let f = Fixture::new();
        let (_, m) = measure(&f.env, || f.client().get_tier_table());
        push(&mut rows, "get_tier_table", m);
    }

    {
        let f = Fixture::new();
        let (_, m) = measure(&f.env, || f.client().get_fill_window_bonus_pct(&4));
        push(&mut rows, "get_fill_window_bonus_pct", m);
    }

    {
        let f = Fixture::new();
        let (_, m) = measure(&f.env, || f.client().get_slash_bps(&4));
        push(&mut rows, "get_slash_bps", m);
    }

    {
        let f = Fixture::new();
        let (_, m) = measure(&f.env, || f.client().get_fee_rebate_bps(&4));
        push(&mut rows, "get_fee_rebate_bps", m);
    }

    {
        let f = Fixture::new();
        let (_, m) = measure(&f.env, || f.client().get_admin());
        push(&mut rows, "get_admin", m);
    }

    {
        let f = Fixture::new();
        let (_, m) = measure(&f.env, || f.client().get_bond_token());
        push(&mut rows, "get_bond_token", m);
    }

    {
        let f = Fixture::new();
        // Worst-case: large fills completed and volume for reputation formula
        let record = f.record_with_fills(10_000, 1_000_000 * USDC);
        // compute_reputation_score is a pure function with no env I/O; measure
        // its baseline cost for tracking purposes.
        let (_, m) = measure(&f.env, || SolverRegistry::compute_reputation_score(record));
        push(&mut rows, "compute_reputation_score", m);
    }

    rows
}

/// Prints the resource-cost tables for `docs/149-solver-registry.md`.
///
/// Run with:
/// ```text
/// cargo test --features testutils bench::resource_cost_report -- --nocapture
/// ```
#[test]
fn resource_cost_report() {
    let rows = collect_rows();
    std::println!("\n=== solver_registry resource cost (testutils budget) ===\n");
    std::println!("{}", fmt_table(&rows));
}

// ─── Ceiling assertion tests ──────────────────────────────────────────────────

fn assert_within_ceiling(label: &str, m: Measurement, cpu_ceil: u64, mem_ceil: u64) {
    assert!(
        m.cpu <= cpu_ceil,
        "{label}: CPU {cpu} exceeds ceiling {cpu_ceil} (delta +{})",
        m.cpu - cpu_ceil,
        cpu = m.cpu,
    );
    assert!(
        m.mem <= mem_ceil,
        "{label}: mem {mem} exceeds ceiling {mem_ceil} (delta +{})",
        m.mem - mem_ceil,
        mem = m.mem,
    );
}

#[test]
fn bench_ceiling_initialize() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let fee_recipient = Address::generate(&env);
    let bond_token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let contract_id = env.register_contract(None, SolverRegistry);
    let client = SolverRegistryClient::new(&env, &contract_id);
    let (_, m) = measure(&env, || client.initialize(&admin, &bond_token, &fee_recipient));
    assert_within_ceiling("initialize", m, CPU_CEIL_INITIALIZE, MEM_CEIL_INITIALIZE);
}

#[test]
fn bench_ceiling_set_writer() {
    let f = Fixture::new();
    let writer = Address::generate(&f.env);
    let (_, m) = measure(&f.env, || f.client().set_writer(&writer));
    assert_within_ceiling("set_writer", m, CPU_CEIL_SET_WRITER, MEM_CEIL_SET_WRITER);
}

#[test]
fn bench_ceiling_set_tier_threshold() {
    let f = Fixture::new();
    let (_, m) = measure(&f.env, || {
        f.client()
            .set_tier_threshold(&2, &(1_000 * USDC), &3_500)
    });
    assert_within_ceiling(
        "set_tier_threshold",
        m,
        CPU_CEIL_SET_TIER_THRESHOLD,
        MEM_CEIL_SET_TIER_THRESHOLD,
    );
}

#[test]
fn bench_ceiling_register_solver() {
    let f = Fixture::new();
    f.bond_admin().mint(&f.solver, &(LARGE_BOND * 2));
    let (_, m) = measure(&f.env, || f.client().register_solver(&f.solver, &LARGE_BOND));
    assert_within_ceiling("register_solver", m, CPU_CEIL_REGISTER, MEM_CEIL_REGISTER);
}

#[test]
fn bench_ceiling_stake() {
    let f = Fixture::new();
    f.register(FLOOR);
    f.bond_admin().mint(&f.solver, &FLOOR);
    let (_, m) = measure(&f.env, || f.client().stake(&f.solver, &FLOOR));
    assert_within_ceiling("stake", m, CPU_CEIL_STAKE, MEM_CEIL_STAKE);
}

#[test]
fn bench_ceiling_unstake() {
    let f = Fixture::new();
    f.register(FLOOR * 2);
    let (_, m) = measure(&f.env, || f.client().unstake(&f.solver, &FLOOR));
    assert_within_ceiling("unstake", m, CPU_CEIL_UNSTAKE, MEM_CEIL_UNSTAKE);
}

#[test]
fn bench_ceiling_deregister_solver() {
    let f = Fixture::new();
    f.register(FLOOR);
    let (_, m) = measure(&f.env, || f.client().deregister_solver(&f.solver));
    assert_within_ceiling("deregister_solver", m, CPU_CEIL_DEREGISTER, MEM_CEIL_DEREGISTER);
}

#[test]
fn bench_ceiling_record_fill() {
    let f = Fixture::new();
    f.register(LARGE_BOND);
    let fill_amount: i128 = 1_000 * USDC;
    let (_, m) = measure(&f.env, || {
        f.client().record_fill(&f.admin, &f.solver, &fill_amount)
    });
    assert_within_ceiling("record_fill", m, CPU_CEIL_RECORD_FILL, MEM_CEIL_RECORD_FILL);
}

#[test]
fn bench_ceiling_record_failure() {
    let f = Fixture::new();
    f.register(LARGE_BOND);
    let (_, m) = measure(&f.env, || f.client().record_failure(&f.admin, &f.solver));
    assert_within_ceiling(
        "record_failure",
        m,
        CPU_CEIL_RECORD_FAILURE,
        MEM_CEIL_RECORD_FAILURE,
    );
}

#[test]
fn bench_ceiling_slash() {
    let f = Fixture::new();
    f.register(LARGE_BOND);
    let (_, m) = measure(&f.env, || f.client().slash(&f.admin, &f.solver));
    assert_within_ceiling("slash", m, CPU_CEIL_SLASH, MEM_CEIL_SLASH);
}

#[test]
fn bench_ceiling_get_tier() {
    let f = Fixture::new();
    f.register(LARGE_BOND);
    let (_, m) = measure(&f.env, || f.client().get_tier(&f.solver));
    assert_within_ceiling("get_tier", m, CPU_CEIL_GET_TIER, MEM_CEIL_GET_TIER);
}

#[test]
fn bench_ceiling_tier_for() {
    let f = Fixture::new();
    let (_, m) = measure(&f.env, || f.client().tier_for(&9_500, &LARGE_BOND));
    assert_within_ceiling("tier_for", m, CPU_CEIL_TIER_FOR, MEM_CEIL_TIER_FOR);
}

#[test]
fn bench_ceiling_get_reputation_score() {
    let f = Fixture::new();
    f.register(LARGE_BOND);
    let (_, m) = measure(&f.env, || f.client().get_reputation_score(&f.solver));
    assert_within_ceiling(
        "get_reputation_score",
        m,
        CPU_CEIL_GET_REPUTATION_SCORE,
        MEM_CEIL_GET_REPUTATION_SCORE,
    );
}

#[test]
fn bench_ceiling_get_solver() {
    let f = Fixture::new();
    f.register(LARGE_BOND);
    let (_, m) = measure(&f.env, || f.client().get_solver(&f.solver));
    assert_within_ceiling("get_solver", m, CPU_CEIL_GET_SOLVER, MEM_CEIL_GET_SOLVER);
}

#[test]
fn bench_ceiling_get_solver_count() {
    let f = Fixture::new();
    f.register(LARGE_BOND);
    let (_, m) = measure(&f.env, || f.client().get_solver_count());
    assert_within_ceiling(
        "get_solver_count",
        m,
        CPU_CEIL_GET_SOLVER_COUNT,
        MEM_CEIL_GET_SOLVER_COUNT,
    );
}

#[test]
fn bench_ceiling_get_tier_table() {
    let f = Fixture::new();
    let (_, m) = measure(&f.env, || f.client().get_tier_table());
    assert_within_ceiling(
        "get_tier_table",
        m,
        CPU_CEIL_GET_TIER_TABLE,
        MEM_CEIL_GET_TIER_TABLE,
    );
}

#[test]
fn bench_ceiling_get_fill_window_bonus_pct() {
    let f = Fixture::new();
    let (_, m) = measure(&f.env, || f.client().get_fill_window_bonus_pct(&4));
    assert_within_ceiling(
        "get_fill_window_bonus_pct",
        m,
        CPU_CEIL_GET_FILL_WINDOW_BONUS_PCT,
        MEM_CEIL_GET_FILL_WINDOW_BONUS_PCT,
    );
}

#[test]
fn bench_ceiling_get_slash_bps() {
    let f = Fixture::new();
    let (_, m) = measure(&f.env, || f.client().get_slash_bps(&4));
    assert_within_ceiling(
        "get_slash_bps",
        m,
        CPU_CEIL_GET_SLASH_BPS,
        MEM_CEIL_GET_SLASH_BPS,
    );
}

#[test]
fn bench_ceiling_get_fee_rebate_bps() {
    let f = Fixture::new();
    let (_, m) = measure(&f.env, || f.client().get_fee_rebate_bps(&4));
    assert_within_ceiling(
        "get_fee_rebate_bps",
        m,
        CPU_CEIL_GET_FEE_REBATE_BPS,
        MEM_CEIL_GET_FEE_REBATE_BPS,
    );
}

#[test]
fn bench_ceiling_get_admin() {
    let f = Fixture::new();
    let (_, m) = measure(&f.env, || f.client().get_admin());
    assert_within_ceiling("get_admin", m, CPU_CEIL_GET_ADMIN, MEM_CEIL_GET_ADMIN);
}

#[test]
fn bench_ceiling_get_bond_token() {
    let f = Fixture::new();
    let (_, m) = measure(&f.env, || f.client().get_bond_token());
    assert_within_ceiling(
        "get_bond_token",
        m,
        CPU_CEIL_GET_BOND_TOKEN,
        MEM_CEIL_GET_BOND_TOKEN,
    );
}

#[test]
fn bench_ceiling_compute_reputation_score() {
    let f = Fixture::new();
    let record = f.record_with_fills(10_000, 1_000_000 * USDC);
    // Pure function: measure its cost as a baseline. The ceiling is low because
    // compute_reputation_score does not touch contract storage or the SAC.
    let (_, m) = measure(&f.env, || SolverRegistry::compute_reputation_score(record));
    assert_within_ceiling(
        "compute_reputation_score",
        m,
        CPU_CEIL_COMPUTE_REPUTATION_SCORE,
        MEM_CEIL_COMPUTE_REPUTATION_SCORE,
    );
}

// ─── Reproducibility smoke-test ───────────────────────────────────────────────

/// Identical fixtures ⇒ identical measurements.
#[test]
fn resource_cost_is_reproducible() {
    let run = || {
        let f = Fixture::new();
        f.register(LARGE_BOND);
        let fill_amount: i128 = 1_000 * USDC;
        measure(&f.env, || {
            f.client().record_fill(&f.admin, &f.solver, &fill_amount)
        })
        .1
    };
    let a = run();
    let b = run();
    assert_eq!(
        a, b,
        "resource measurement not reproducible: {a:?} vs {b:?}"
    );
    assert!(a.cpu > 0, "cpu should be metered: {a:?}");
    assert!(a.mem > 0, "mem should be metered: {a:?}");
}
