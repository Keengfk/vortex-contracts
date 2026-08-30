#![cfg(test)]

//! Integration tests for the `ProofRegistry` mock oracle.
//!
//! These tests cover:
//!
//! * The `mock_set_proof` / `mock_remove_proof` test back-doors.
//! * `receive_message` with a hand-crafted 102-byte payload.
//! * Replay protection (duplicate intent_id rejected).
//! * `has_proof` / `get_proof` query semantics.
//! * Authorized emitter management (set / remove / query).
//! * Invalid payload length rejection.
//!
//! The `testutils` feature must be enabled for the mock back-door methods to
//! be available.  This file is only compiled under `#[cfg(test)]`.

use crate::{Error, ProofRecord, ProofRegistry, ProofRegistryClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    Address, Bytes, BytesN, Env, String,
};

// ─── Fixture ─────────────────────────────────────────────────────────────────

struct Ctx {
    env: Env,
    admin: Address,
    wormhole_core: Address, // placeholder; not called in mock
    contract_id: Address,
}

impl Ctx {
    fn client(&self) -> ProofRegistryClient<'_> {
        ProofRegistryClient::new(&self.env, &self.contract_id)
    }
}

fn setup() -> Ctx {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    // The Wormhole Core address is stored but never called in this mock.
    let wormhole_core = Address::generate(&env);
    let contract_id = env.register_contract(None, ProofRegistry);

    let ctx = Ctx {
        env,
        admin,
        wormhole_core,
        contract_id,
    };

    ctx.client()
        .initialize(&ctx.admin, &ctx.wormhole_core);

    ctx
}

/// Build a deterministic `BytesN<32>` intent ID for use in tests.
fn make_intent_id(env: &Env, seed: u8) -> BytesN<32> {
    let mut bytes = [0u8; 32];
    bytes[0] = seed;
    bytes[31] = seed;
    BytesN::from_array(env, &bytes)
}

/// Build a minimal valid 102-byte VAA payload for `receive_message`.
///
/// Layout:
///   [0..32]   intent_id
///   [32..52]  src_user  (20 bytes, here all zeros except seed byte)
///   [52..54]  src_chain_id (big-endian u16)
///   [54..86]  src_token (32 bytes, all zeros)
///   [86..102] src_amount (big-endian i128)
fn make_payload(env: &Env, intent_id: &BytesN<32>, src_chain_id: u16, src_amount: i128) -> Bytes {
    let mut payload = [0u8; 102];

    // intent_id
    let id_bytes = intent_id.to_array();
    payload[0..32].copy_from_slice(&id_bytes);

    // src_user: 20 zero bytes (placeholder)
    // [32..52] already zeroed

    // src_chain_id (big-endian u16)
    payload[52] = (src_chain_id >> 8) as u8;
    payload[53] = (src_chain_id & 0xff) as u8;

    // src_token: 32 zero bytes (placeholder)
    // [54..86] already zeroed

    // src_amount (big-endian i128)
    let amount_bytes = src_amount.to_be_bytes();
    payload[86..102].copy_from_slice(&amount_bytes);

    Bytes::from_slice(env, &payload)
}

// ─── Initialization ───────────────────────────────────────────────────────────

#[test]
fn initialize_succeeds_once() {
    let ctx = setup();
    // Double-initialize must fail.
    let res = ctx.client().try_initialize(&ctx.admin, &ctx.wormhole_core);
    assert_eq!(res, Err(Ok(Error::AlreadyInitialized.into())));
}

// ─── Authorized emitter management ───────────────────────────────────────────

#[test]
fn set_and_get_authorized_emitter() {
    let ctx = setup();
    let c = ctx.client();

    let emitter: BytesN<32> = BytesN::from_array(&ctx.env, &[0xde; 32]);
    let chain_id: u32 = 2; // Wormhole Ethereum chain ID

    c.set_authorized_emitter(&chain_id, &emitter);
    let stored = c.get_authorized_emitter(&chain_id);
    assert_eq!(stored, Some(emitter));
}

#[test]
fn get_authorized_emitter_returns_none_if_unset() {
    let ctx = setup();
    assert_eq!(ctx.client().get_authorized_emitter(&2), None);
}

#[test]
fn remove_authorized_emitter_clears_entry() {
    let ctx = setup();
    let c = ctx.client();
    let emitter: BytesN<32> = BytesN::from_array(&ctx.env, &[0xab; 32]);

    c.set_authorized_emitter(&2, &emitter);
    assert!(c.get_authorized_emitter(&2).is_some());

    c.remove_authorized_emitter(&2);
    assert_eq!(c.get_authorized_emitter(&2), None);
}

// ─── receive_message ─────────────────────────────────────────────────────────

#[test]
fn receive_message_stores_proof() {
    let ctx = setup();
    let c = ctx.client();

    let intent_id = make_intent_id(&ctx.env, 1);
    let payload = make_payload(&ctx.env, &intent_id, 2 /* Ethereum */, 1_000_000_000);

    c.receive_message(&payload);

    assert!(c.has_proof(&intent_id));

    let record = c.get_proof(&intent_id).expect("proof should exist");
    assert_eq!(record.intent_id, intent_id);
    assert_eq!(record.src_chain_id, 2);
    assert_eq!(record.src_amount, 1_000_000_000);
}

#[test]
fn get_fresh_proof_returns_record_when_fresh() {
    let ctx = setup();
    let c = ctx.client();

    let intent_id = make_intent_id(&ctx.env, 95);
    let payload = make_payload(&ctx.env, &intent_id, 2, 1_000_000_000);
    c.receive_message(&payload);

    let record = c.get_fresh_proof(&intent_id);
    assert_eq!(record.intent_id, intent_id);
}

#[test]
fn get_fresh_proof_rejects_stale_proof() {
    let ctx = setup();
    let c = ctx.client();

    let intent_id = make_intent_id(&ctx.env, 96);
    let payload = make_payload(&ctx.env, &intent_id, 2, 1_000_000_000);
    c.receive_message(&payload);

    ctx.env
        .ledger()
        .with_mut(|li| li.timestamp += crate::PROOF_VALIDITY_WINDOW + 1);

    let res = c.try_get_fresh_proof(&intent_id);
    assert_eq!(res, Err(Ok(Error::ProofStale.into())));
}

#[test]
fn get_fresh_proof_accepts_exact_boundary() {
    let ctx = setup();
    let c = ctx.client();

    let intent_id = make_intent_id(&ctx.env, 97);
    let payload = make_payload(&ctx.env, &intent_id, 2, 1_000_000_000);
    c.receive_message(&payload);

    // Exactly at the validity window boundary: still fresh (inclusive).
    ctx.env
        .ledger()
        .with_mut(|li| li.timestamp += crate::PROOF_VALIDITY_WINDOW);
    let record = c.get_fresh_proof(&intent_id);
    assert_eq!(record.intent_id, intent_id);
}

#[test]
fn get_fresh_proof_rejects_missing_proof() {
    let ctx = setup();
    let c = ctx.client();

    let intent_id = make_intent_id(&ctx.env, 98);
    let res = c.try_get_fresh_proof(&intent_id);
    assert_eq!(res, Err(Ok(Error::ProofNotFound.into())));
}

#[test]
fn receive_message_rejects_duplicate_intent_id() {
    let ctx = setup();
    let c = ctx.client();

    let intent_id = make_intent_id(&ctx.env, 2);
    let payload = make_payload(&ctx.env, &intent_id, 2, 500_000_000);

    c.receive_message(&payload);

    // Second call with the same intent_id must fail.
    let res = c.try_receive_message(&payload);
    assert_eq!(res, Err(Ok(Error::ProofAlreadyExists.into())));
}

#[test]
fn receive_message_rejects_wrong_payload_length() {
    let ctx = setup();

    // 50 bytes — too short.
    let short = Bytes::from_slice(&ctx.env, &[0u8; 50]);
    let res = ctx.client().try_receive_message(&short);
    assert_eq!(res, Err(Ok(Error::InvalidPayload.into())));

    // 200 bytes — too long.
    let long = Bytes::from_slice(&ctx.env, &[0u8; 200]);
    let res = ctx.client().try_receive_message(&long);
    assert_eq!(res, Err(Ok(Error::InvalidPayload.into())));
}

#[test]
fn receive_message_succeeds_while_unpaused() {
    let ctx = setup();
    let c = ctx.client();
    assert!(!c.is_paused());

    let intent_id = make_intent_id(&ctx.env, 90);
    let payload = make_payload(&ctx.env, &intent_id, 2, 1_000_000_000);
    c.receive_message(&payload);
    assert!(c.has_proof(&intent_id));
}

#[test]
fn receive_message_rejects_while_paused() {
    let ctx = setup();
    let c = ctx.client();

    c.pause();
    assert!(c.is_paused());

    let intent_id = make_intent_id(&ctx.env, 91);
    let payload = make_payload(&ctx.env, &intent_id, 2, 1_000_000_000);
    let res = c.try_receive_message(&payload);
    assert_eq!(res, Err(Ok(Error::ContractPaused.into())));
}

#[test]
fn reads_remain_available_while_paused() {
    let ctx = setup();
    let c = ctx.client();

    let intent_id = make_intent_id(&ctx.env, 92);
    let payload = make_payload(&ctx.env, &intent_id, 2, 1_000_000_000);
    c.receive_message(&payload);

    c.pause();
    assert!(c.has_proof(&intent_id));
    assert!(c.get_proof(&intent_id).is_some());

    c.unpause();
    assert!(!c.is_paused());
    let intent_id2 = make_intent_id(&ctx.env, 93);
    let payload2 = make_payload(&ctx.env, &intent_id2, 2, 1);
    c.receive_message(&payload2);
    assert!(c.has_proof(&intent_id2));
}

#[test]
fn receive_message_decodes_src_amount_correctly() {
    let ctx = setup();
    let c = ctx.client();

    let intent_id = make_intent_id(&ctx.env, 3);
    // Use a distinctive amount: 1 ETH in wei (18-decimal units).
    let eth_amount: i128 = 1_000_000_000_000_000_000;
    let payload = make_payload(&ctx.env, &intent_id, 2, eth_amount);

    c.receive_message(&payload);

    let record = c.get_proof(&intent_id).unwrap();
    assert_eq!(record.src_amount, eth_amount);
}

// ─── has_proof / get_proof ────────────────────────────────────────────────────

#[test]
fn has_proof_returns_false_for_unknown_intent() {
    let ctx = setup();
    let id = make_intent_id(&ctx.env, 99);
    assert!(!ctx.client().has_proof(&id));
}

#[test]
fn get_proof_returns_none_for_unknown_intent() {
    let ctx = setup();
    let id = make_intent_id(&ctx.env, 99);
    assert!(ctx.client().get_proof(&id).is_none());
}

// ─── mock_set_proof (test back-door) ─────────────────────────────────────────

/// `mock_set_proof` lets tests inject a fully controlled `ProofRecord` without
/// constructing a real VAA payload.
#[test]
fn mock_set_proof_injects_controllable_record() {
    let ctx = setup();
    let c = ctx.client();

    let intent_id = make_intent_id(&ctx.env, 10);

    let record = ProofRecord {
        intent_id: intent_id.clone(),
        src_user: String::from_str(&ctx.env, "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045"),
        src_chain_id: 2, // Ethereum
        src_token: String::from_str(
            &ctx.env,
            "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
        ),
        src_amount: 5_000 * 1_000_000, // 5 000 USDC (6 decimals)
        vaa_sequence: 42,
        received_at: ctx.env.ledger().timestamp(),
    };

    c.mock_set_proof(&record);

    assert!(c.has_proof(&intent_id));
    let stored = c.get_proof(&intent_id).unwrap();
    assert_eq!(stored.src_amount, 5_000 * 1_000_000);
    assert_eq!(stored.src_chain_id, 2);
    assert_eq!(stored.vaa_sequence, 42);
}

/// Calling `mock_set_proof` twice for the same intent_id is rejected, matching
/// the replay protection in `receive_message`.
#[test]
fn mock_set_proof_rejects_duplicate() {
    let ctx = setup();
    let c = ctx.client();

    let intent_id = make_intent_id(&ctx.env, 11);

    let record = ProofRecord {
        intent_id: intent_id.clone(),
        src_user: String::from_str(&ctx.env, "0xabc"),
        src_chain_id: 30, // Base
        src_token: String::from_str(&ctx.env, "0xdef"),
        src_amount: 100,
        vaa_sequence: 1,
        received_at: 0,
    };

    c.mock_set_proof(&record.clone());

    let res = c.try_mock_set_proof(&record);
    assert_eq!(res, Err(Ok(Error::ProofAlreadyExists.into())));
}

/// `mock_remove_proof` clears a previously set proof, making `has_proof`
/// return false again.  Useful for testing "proof not found" error paths.
#[test]
fn mock_remove_proof_clears_stored_record() {
    let ctx = setup();
    let c = ctx.client();

    let intent_id = make_intent_id(&ctx.env, 12);
    let record = ProofRecord {
        intent_id: intent_id.clone(),
        src_user: String::from_str(&ctx.env, "0x1234"),
        src_chain_id: 5, // Polygon
        src_token: String::from_str(&ctx.env, "0x5678"),
        src_amount: 999,
        vaa_sequence: 7,
        received_at: 0,
    };

    c.mock_set_proof(&record);
    assert!(c.has_proof(&intent_id));

    c.mock_remove_proof(&intent_id);
    assert!(!c.has_proof(&intent_id));
    assert!(c.get_proof(&intent_id).is_none());
}

// ─── Scenario: proof-gated fill simulation ───────────────────────────────────

/// Simulate the proof-gated fill flow at the oracle layer:
/// 1. A proof arrives via `mock_set_proof` (representing a Wormhole message).
/// 2. `intent_settlement` (simulated here as plain assertions) reads the proof
///    and validates the fields before allowing the fill.
///
/// This test validates the oracle's query interface in isolation.  The actual
/// cross-contract call from `intent_settlement` will be tested once
/// `fill_intent` gains the `require_proof` parameter.
#[test]
fn proof_gated_fill_simulation_positive_case() {
    let ctx = setup();
    let c = ctx.client();

    let intent_id = make_intent_id(&ctx.env, 20);
    let expected_amount: i128 = 1_000_000_000_000_000_000; // 1 ETH in wei
    let expected_chain: u32 = 2; // Wormhole Ethereum

    let record = ProofRecord {
        intent_id: intent_id.clone(),
        src_user: String::from_str(&ctx.env, "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045"),
        src_chain_id: expected_chain,
        src_token: String::from_str(
            &ctx.env,
            "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", // WETH
        ),
        src_amount: expected_amount,
        vaa_sequence: 100,
        received_at: ctx.env.ledger().timestamp(),
    };

    c.mock_set_proof(&record);

    // Simulate the checks intent_settlement.fill_intent would perform:
    let proof = c.get_proof(&intent_id).expect("proof must exist for fill");
    assert_eq!(proof.src_chain_id, expected_chain, "chain ID must match intent");
    assert!(
        proof.src_amount >= expected_amount,
        "proof amount must meet or exceed intent src_amount"
    );
}

/// Negative case: if the proof's `src_amount` is less than the intent
/// required, the fill should be rejected.  Simulated here by asserting the
/// check that `intent_settlement` will perform.
#[test]
fn proof_gated_fill_simulation_amount_insufficient() {
    let ctx = setup();
    let c = ctx.client();

    let intent_id = make_intent_id(&ctx.env, 21);
    let intent_src_amount: i128 = 2_000_000_000; // intent requires 2 000 USDC

    let record = ProofRecord {
        intent_id: intent_id.clone(),
        src_user: String::from_str(&ctx.env, "0xabc"),
        src_chain_id: 2,
        src_token: String::from_str(&ctx.env, "0xusd"),
        // Proof shows only 1 000 USDC deposited — less than the intent requires.
        src_amount: 1_000_000_000,
        vaa_sequence: 5,
        received_at: 0,
    };

    c.mock_set_proof(&record);

    let proof = c.get_proof(&intent_id).unwrap();
    // This is the check intent_settlement will perform; it must fail here.
    assert!(
        proof.src_amount < intent_src_amount,
        "proof amount is insufficient — fill should be rejected"
    );
}

/// Negative case: proof does not exist at all.
/// `intent_settlement` will call `get_proof` and receive `None`, then panic
/// with `ProofNotFound`.  Verified here at the oracle layer.
#[test]
fn proof_gated_fill_simulation_no_proof() {
    let ctx = setup();
    let intent_id = make_intent_id(&ctx.env, 22);

    // No proof was set.
    let proof = ctx.client().get_proof(&intent_id);
    assert!(proof.is_none(), "no proof should exist for this intent");
}
