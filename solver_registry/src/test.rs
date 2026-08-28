#![cfg(test)]

//! Tests for `solver_registry`: the tier table, `get_tier` defaulting,
//! admin-gated `set_tier` / `clear_tier`, and the perk-schedule views.

use crate::{Error, SolverRegistry, SolverRegistryClient, MAX_TIER};
use soroban_sdk::{testutils::Address as _, Address, Env};

struct Ctx {
    env: Env,
    admin: Address,
    contract_id: Address,
}

impl Ctx {
    fn client(&self) -> SolverRegistryClient<'_> {
        SolverRegistryClient::new(&self.env, &self.contract_id)
    }
}

fn setup() -> Ctx {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let contract_id = env.register_contract(None, SolverRegistry);
    let ctx = Ctx {
        env,
        admin,
        contract_id,
    };
    ctx.client().initialize(&ctx.admin);
    ctx
}

#[test]
fn initialize_is_one_shot() {
    let ctx = setup();
    let res = ctx.client().try_initialize(&ctx.admin);
    assert_eq!(res, Err(Ok(Error::AlreadyInitialized.into())));
}

#[test]
fn get_tier_defaults_to_unranked() {
    let ctx = setup();
    let stranger = Address::generate(&ctx.env);
    assert_eq!(ctx.client().get_tier(&stranger), 0);
}

#[test]
fn set_and_get_tier_roundtrips_all_tiers() {
    let ctx = setup();
    let c = ctx.client();
    let solver = Address::generate(&ctx.env);

    for tier in 0..=MAX_TIER {
        c.set_tier(&solver, &tier);
        assert_eq!(c.get_tier(&solver), tier);
    }
}

#[test]
fn set_tier_zero_clears_the_entry() {
    let ctx = setup();
    let c = ctx.client();
    let solver = Address::generate(&ctx.env);

    c.set_tier(&solver, &3);
    assert_eq!(c.get_tier(&solver), 3);
    c.set_tier(&solver, &0);
    assert_eq!(c.get_tier(&solver), 0);

    c.set_tier(&solver, &4);
    c.clear_tier(&solver);
    assert_eq!(c.get_tier(&solver), 0);
}

#[test]
fn set_tier_above_max_is_rejected() {
    let ctx = setup();
    let res = ctx
        .client()
        .try_set_tier(&Address::generate(&ctx.env), &(MAX_TIER + 1));
    assert_eq!(res, Err(Ok(Error::InvalidTier.into())));
}

#[test]
fn set_tier_requires_admin_auth() {
    let ctx = setup();
    let solver = Address::generate(&ctx.env);
    ctx.client().set_tier(&solver, &2);

    // Under mock_all_auths every call is authorized; assert the admin address
    // is the one whose auth was required.
    let authed_by_admin = ctx.env.auths().iter().any(|(addr, _)| *addr == ctx.admin);
    assert!(authed_by_admin, "set_tier must require admin auth");
}

#[test]
fn fill_window_bonus_bps_matches_design_doc() {
    let ctx = setup();
    let c = ctx.client();
    // docs/solver-registry-design.md §3/§6: +0 / +10 / +20 / +30 / +50 %.
    assert_eq!(c.get_fill_window_bonus_bps(&0), 0);
    assert_eq!(c.get_fill_window_bonus_bps(&1), 1_000);
    assert_eq!(c.get_fill_window_bonus_bps(&2), 2_000);
    assert_eq!(c.get_fill_window_bonus_bps(&3), 3_000);
    assert_eq!(c.get_fill_window_bonus_bps(&4), 5_000);
    // Unknown tier → no bonus (safe degrade).
    assert_eq!(c.get_fill_window_bonus_bps(&99), 0);
}

#[test]
fn slash_bps_matches_design_doc_with_five_percent_floor() {
    let ctx = setup();
    let c = ctx.client();
    // docs/solver-registry-design.md §3/§7: 1000 / 1000 / 800 / 600 / 500.
    assert_eq!(c.get_slash_bps(&0), 1_000);
    assert_eq!(c.get_slash_bps(&1), 1_000);
    assert_eq!(c.get_slash_bps(&2), 800);
    assert_eq!(c.get_slash_bps(&3), 600);
    // Platinum — the 5% floor.
    assert_eq!(c.get_slash_bps(&4), 500);
    // Unknown tier → harshest rate (safe degrade).
    assert_eq!(c.get_slash_bps(&99), 1_000);
    // No tier is ever slashed less than the 5% Platinum floor.
    for tier in 0..=MAX_TIER {
        assert!(c.get_slash_bps(&tier) >= 500);
    }
}
