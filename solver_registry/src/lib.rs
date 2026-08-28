#![no_std]

//! Vortex Protocol — Solver Registry (`solver_registry`)
//!
//! Canonical store for solver **tier** state and the source of truth for the
//! per-tier behavioural perks that `intent_settlement` enforces:
//!
//! * a fill-window **extension bonus** applied in `accept_intent`, and
//! * a **reduced slash percentage** applied in `slash_solver`.
//!
//! The tier table is [`docs/solver-registry-design.md`] §3 (Unranked → Platinum).
//! `intent_settlement` reads a solver's tier with a single cross-contract call
//! to [`SolverRegistry::get_tier`] and maps the tier to perk values locally, so
//! the two contracts only need a stable one-method interface between them
//! (issue #197).
//!
//! ## Scope (issue #186 is broader)
//!
//! This crate implements the **tier lookup + perk schedule** that #197 needs.
//! Tiers are set by the admin (`set_tier`). Score-gated automatic promotion —
//! porting `intent_settlement::compute_reputation_score`, `record_fill` /
//! `record_failure`, staking, and migration — is the remaining scope of #186
//! and is intentionally not here. The read interface (`get_tier`,
//! `get_fill_window_bonus_bps`, `get_slash_bps`) is designed to stay stable
//! when that lands.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, Address, Env, Symbol,
};

#[cfg(test)]
mod test;

// ─── Tier table (docs/solver-registry-design.md §3, §6, §7) ───────────────────
//
// Index = tier number. These MUST match the design doc and the copy that
// `intent_settlement` keeps for enforcement (see its `TIER_FILL_WINDOW_BONUS_BPS`
// / `TIER_SLASH_BPS`). A change here is a protocol-parameter change.

/// Highest defined tier (Platinum). Tiers are `0..=MAX_TIER`.
pub const MAX_TIER: u32 = 4;

/// Fill-window extension bonus per tier, in basis points (10_000 = +100%).
/// Unranked +0%, Bronze +10%, Silver +20%, Gold +30%, Platinum +50%.
const TIER_FILL_WINDOW_BONUS_BPS: [u32; 5] = [0, 1_000, 2_000, 3_000, 5_000];

/// Slash percentage per tier, in basis points of the bond (10_000 = 100%).
/// Unranked/Bronze 10%, Silver 8%, Gold 6%, Platinum 5% (the 5% floor).
const TIER_SLASH_BPS: [i128; 5] = [1_000, 1_000, 800, 600, 500];

// ─── Storage Keys ────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum RegistryKey {
    /// Admin address (set in `initialize`); may call `set_tier` / `clear_tier`.
    Admin,
    /// Per-solver tier: `Address` → `u32` in `0..=MAX_TIER`. Absent ⇒ Unranked.
    Tier(Address),
}

// ─── Errors ──────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    /// `initialize` called on an already-initialized registry.
    AlreadyInitialized = 1,
    /// Caller is not the admin.
    Unauthorized = 2,
    /// Contract not initialized (`Admin` key absent).
    NotInitialized = 3,
    /// `set_tier` was given a tier greater than `MAX_TIER`.
    InvalidTier = 4,
}

// ─── Contract ────────────────────────────────────────────────────────────────

#[contract]
pub struct SolverRegistry;

#[contractimpl]
impl SolverRegistry {
    /// Deploy-time setup. Records `admin`. Must be called exactly once.
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&RegistryKey::Admin) {
            panic_with_error!(&env, Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&RegistryKey::Admin, &admin);
    }

    /// Admin-only: set `solver`'s tier. `tier` must be in `0..=MAX_TIER`.
    /// Setting tier `0` is equivalent to `clear_tier`.
    pub fn set_tier(env: Env, solver: Address, tier: u32) {
        Self::require_admin(&env);
        if tier > MAX_TIER {
            panic_with_error!(&env, Error::InvalidTier);
        }
        if tier == 0 {
            env.storage()
                .persistent()
                .remove(&RegistryKey::Tier(solver.clone()));
        } else {
            env.storage()
                .persistent()
                .set(&RegistryKey::Tier(solver.clone()), &tier);
        }
        env.events()
            .publish((Symbol::new(&env, "tier_set"), solver), tier);
    }

    /// Admin-only: drop `solver` back to Unranked (tier 0).
    pub fn clear_tier(env: Env, solver: Address) {
        Self::require_admin(&env);
        env.storage()
            .persistent()
            .remove(&RegistryKey::Tier(solver.clone()));
        env.events()
            .publish((Symbol::new(&env, "tier_set"), solver), 0u32);
    }

    /// `solver`'s current tier, or `0` (Unranked) if none is set. This is the
    /// single method `intent_settlement` calls on the hot path.
    pub fn get_tier(env: Env, solver: Address) -> u32 {
        env.storage()
            .persistent()
            .get(&RegistryKey::Tier(solver))
            .unwrap_or(0)
    }

    /// Fill-window extension bonus for `tier`, in basis points (10_000 = +100%).
    /// Unknown tiers return `0` (no bonus) so callers degrade safely.
    pub fn get_fill_window_bonus_bps(_env: Env, tier: u32) -> u32 {
        TIER_FILL_WINDOW_BONUS_BPS
            .get(tier as usize)
            .copied()
            .unwrap_or(0)
    }

    /// Slash percentage for `tier`, in basis points of the bond. Unknown tiers
    /// return `1_000` (the harshest, Unranked rate) so callers degrade safely.
    /// Intentionally public so off-chain solvers can price it into quotes
    /// (`docs/solver-registry-design.md` §7).
    pub fn get_slash_bps(_env: Env, tier: u32) -> i128 {
        TIER_SLASH_BPS.get(tier as usize).copied().unwrap_or(1_000)
    }

    /// The admin address, or `None` before `initialize`.
    pub fn admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&RegistryKey::Admin)
    }

    // ── Internal ─────────────────────────────────────────────────────────────

    fn require_admin(env: &Env) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&RegistryKey::Admin)
            .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized));
        admin.require_auth();
    }
}
