#![no_std]

//! Vortex Protocol — Mock Cross-Chain Proof Oracle (`proof_registry`)
//!
//! This crate implements the `ProofRegistry` contract as defined in
//! [`docs/124-proof-verification-interface.md`].  It provides:
//!
//! 1. **Production interface** — the same storage layout and public API that
//!    a real Wormhole-backed registry will expose, so `intent_settlement` can
//!    be written against a stable interface today.
//!
//! 2. **Test-controllable back-door** — a `mock_set_proof` entry-point
//!    (compiled only when the `testutils` feature is active) that lets tests
//!    inject arbitrary `ProofRecord` values directly into storage, bypassing
//!    VAA verification.  This allows integration tests to exercise
//!    proof-gated `fill_intent` paths without a real Wormhole guardian quorum.
//!
//! ## Design rationale
//!
//! Keeping mock behaviour behind a Cargo feature flag means:
//! - The mock back-door is provably absent from the release WASM binary.
//! - CI tests use the exact same contract ABI as production.
//! - The mock can be replaced by the real implementation by removing the
//!   `mock_set_proof` method and wiring up actual VAA verification, with
//!   no changes required to `intent_settlement` or its tests.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, Address, Bytes,
    BytesN, Env, String, Symbol,
};

/// Issue #254: how long (in seconds) a `ProofRecord` remains usable to gate a
/// `fill_intent` call after `receive_message` stores it. Chosen to comfortably
/// exceed `intent_settlement`'s 300-second `FILL_WINDOW` plus realistic
/// VAA-relay latency (1–20 minutes across the bridge protocols compared in
/// `docs/bridge-protocol-comparison.md`), so a proof arriving even somewhat
/// late is never spuriously rejected as stale. This is distinct from Soroban
/// storage-TTL archival (issue #51) — this is business-logic staleness, not
/// ledger-entry expiry.
pub const PROOF_VALIDITY_WINDOW: u64 = 3600;

#[cfg(test)]
mod test;

// ─── Storage Keys ─────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum ProofKey {
    /// Admin address (set in `initialize`).
    Admin,
    /// Wormhole Core contract address used for VAA verification in production.
    /// Stored but not called in this mock — present so the storage layout
    /// matches the future production contract.
    WormholeCore,
    /// Authorized emitter address on a given Wormhole source-chain ID.
    /// Key: `(chain_id: u16)` → `emitter: BytesN<32>`.
    AuthorizedEmitter(u32), // u32 wraps u16 — Soroban contracttype requires u32
    /// Verified proof record keyed by Vortex `intent_id`.
    Proof(BytesN<32>),
    /// Boolean flag (`true` = paused). Set by `pause()` and cleared by
    /// `unpause()`. When `true`, `receive_message` rejects new proofs.
    /// Absent until first `pause()` call (defaults to `false`).
    Paused,
}

// ─── Data Types ───────────────────────────────────────────────────────────────

/// A verified record that a source-chain deposit occurred for `intent_id`.
///
/// In production this is populated by `receive_message` after Guardian
/// signature verification.  In the mock it may also be populated by
/// `mock_set_proof` (testutils feature) for test-controlled scenarios.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct ProofRecord {
    /// Vortex intent ID this proof corresponds to.
    pub intent_id: BytesN<32>,
    /// User's address on the source chain (hex string for EVM, base58 for Solana).
    pub src_user: String,
    /// Wormhole chain ID of the source chain (e.g. 2 = Ethereum, 30 = Base).
    pub src_chain_id: u32,
    /// Source token address on the source chain.
    pub src_token: String,
    /// Amount deposited on the source chain in that token's smallest unit.
    pub src_amount: i128,
    /// Wormhole VAA sequence number — used for replay protection.
    pub vaa_sequence: u64,
    /// Ledger timestamp when this proof was registered on Stellar.
    pub received_at: u64,
}

// ─── Errors ───────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    /// `initialize` called on an already-initialized registry.
    AlreadyInitialized = 1,
    /// Caller is not the admin.
    Unauthorized = 2,
    /// `receive_message` received a VAA whose emitter is not in the authorized
    /// list for the claimed source chain.
    EmitterNotAuthorized = 3,
    /// A proof for this `intent_id` already exists (replay protection).
    ProofAlreadyExists = 4,
    /// `get_proof` or `has_proof` was called for an `intent_id` with no record.
    ProofNotFound = 5,
    /// VAA payload could not be decoded (wrong length or malformed).
    InvalidPayload = 6,
    /// Contract not initialized (`Admin` key absent).
    NotInitialized = 7,
    /// `receive_message` called while the registry is paused.
    ContractPaused = 8,
    /// `get_fresh_proof` found a `ProofRecord` older than
    /// `PROOF_VALIDITY_WINDOW`. Distinct from `ProofNotFound` — the proof
    /// exists but is too stale to gate a fill.
    ProofStale = 9,
}

// ─── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct ProofRegistry;

#[contractimpl]
impl ProofRegistry {
    // ── Initialization ────────────────────────────────────────────────────────

    /// Deploy-time setup.  Records `admin` and the Wormhole Core contract
    /// address.  Must be called exactly once.
    pub fn initialize(env: Env, admin: Address, wormhole_core: Address) {
        if env.storage().instance().has(&ProofKey::Admin) {
            panic_with_error!(&env, Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&ProofKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&ProofKey::WormholeCore, &wormhole_core);
    }

    // ── Admin ─────────────────────────────────────────────────────────────────

    /// Admin-only: register a trusted emitter for a given Wormhole source-chain
    /// ID.  Only VAAs originating from this emitter on `chain_id` will be
    /// accepted by `receive_message`.
    pub fn set_authorized_emitter(env: Env, chain_id: u32, emitter: BytesN<32>) {
        Self::require_admin(&env);
        env.storage()
            .instance()
            .set(&ProofKey::AuthorizedEmitter(chain_id), &emitter);
        env.events().publish(
            (Symbol::new(&env, "emitter_authorized"),),
            (chain_id, emitter),
        );
    }

    /// Admin-only: remove a trusted emitter (e.g. after a source contract
    /// upgrade).
    pub fn remove_authorized_emitter(env: Env, chain_id: u32) {
        Self::require_admin(&env);
        env.storage()
            .instance()
            .remove(&ProofKey::AuthorizedEmitter(chain_id));
        env.events().publish(
            (Symbol::new(&env, "emitter_removed"),),
            chain_id,
        );
    }

    /// Return the authorized emitter for `chain_id`, or `None` if unset.
    pub fn get_authorized_emitter(env: Env, chain_id: u32) -> Option<BytesN<32>> {
        env.storage()
            .instance()
            .get(&ProofKey::AuthorizedEmitter(chain_id))
    }

    /// Admin-only: halt `receive_message` for incident response (issue #264),
    /// mirroring `intent_settlement`'s `pause`/`unpause` mechanism. Unlike
    /// `intent_settlement` (issue #120), there is no separate narrow-scoped
    /// pauser role here — admin-only is sufficient for this registry's first
    /// version. `get_proof`/`has_proof` remain available during a pause.
    pub fn pause(env: Env) {
        Self::require_admin(&env);
        env.storage().instance().set(&ProofKey::Paused, &true);
        env.events().publish((Symbol::new(&env, "paused"),), true);
    }

    /// Admin-only: lift a pause and resume accepting proofs.
    pub fn unpause(env: Env) {
        Self::require_admin(&env);
        env.storage().instance().set(&ProofKey::Paused, &false);
        env.events().publish((Symbol::new(&env, "paused"),), false);
    }

    /// Whether `receive_message` is currently halted.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&ProofKey::Paused)
            .unwrap_or(false)
    }

    // ── Message Receipt ───────────────────────────────────────────────────────

    /// Receive and verify a Wormhole VAA, then store the decoded proof.
    ///
    /// **Production behaviour (not yet implemented here):**
    /// 1. Call the Wormhole Core contract to verify Guardian signatures on `vaa`.
    /// 2. Decode the VAA body: extract `emitter_chain`, `emitter_address`,
    ///    and the 102-byte payload defined in §3.4 of the design doc.
    /// 3. Check `emitter_address` is in `AuthorizedEmitter(emitter_chain)`.
    /// 4. Decode the payload into a `ProofRecord`.
    /// 5. Reject if a proof for this `intent_id` already exists.
    /// 6. Store the `ProofRecord` under `ProofKey::Proof(intent_id)`.
    ///
    /// **Mock behaviour (this implementation):**
    /// VAA verification is skipped.  The method parses the raw 102-byte
    /// payload directly without checking Guardian signatures or emitter
    /// authorization.  This is intentional: tests call this with a hand-crafted
    /// payload rather than a real signed VAA.
    ///
    /// Payload layout (102 bytes, big-endian):
    /// ```
    ///  [0..32]   intent_id   (BytesN<32>)
    ///  [32..52]  src_user    (20-byte EVM address, zero-padded on Solana)
    ///  [52..54]  src_chain_id (u16)
    ///  [54..86]  src_token   (32 bytes, address padded)
    ///  [86..102] src_amount  (i128, big-endian)
    /// ```
    pub fn receive_message(env: Env, vaa: Bytes) {
        if Self::is_paused(env.clone()) {
            panic_with_error!(&env, Error::ContractPaused);
        }

        // Payload must be exactly 102 bytes.
        if vaa.len() != 102 {
            panic_with_error!(&env, Error::InvalidPayload);
        }

        // Decode intent_id (bytes 0..32).
        let intent_id: BytesN<32> = vaa.slice(0..32).try_into().unwrap_or_else(|_| {
            panic_with_error!(&env, Error::InvalidPayload)
        });

        // Reject replays.
        if env
            .storage()
            .persistent()
            .has(&ProofKey::Proof(intent_id.clone()))
        {
            panic_with_error!(&env, Error::ProofAlreadyExists);
        }

        // Decode src_chain_id (bytes 52..54) as big-endian u16 → u32.
        let chain_hi = vaa.get(52) as u32;
        let chain_lo = vaa.get(53) as u32;
        let src_chain_id: u32 = (chain_hi << 8) | chain_lo;

        // In production: check emitter authorization here.
        // Mock: skip that check entirely.

        // Decode src_amount (bytes 86..102) as big-endian i128.
        let mut amount_bytes = [0u8; 16];
        let mut idx = 0usize;
        while idx < 16 {
            amount_bytes[idx] = vaa.get((86 + idx) as u32) as u8;
            idx += 1;
        }
        let src_amount = i128::from_be_bytes(amount_bytes);

        // vaa_sequence is not present in the payload (it lives in the VAA
        // envelope, not the application payload).  Use 0 for the mock; the
        // real implementation will extract it from the VAA header.
        let now = env.ledger().timestamp();

        // src_user and src_token are stored as hex strings of the raw bytes
        // for simplicity in the mock.  Production will use the actual encoding.
        let src_user = Self::bytes_to_hex_string(&env, &vaa.slice(32..52));
        let src_token = Self::bytes_to_hex_string(&env, &vaa.slice(54..86));

        let record = ProofRecord {
            intent_id: intent_id.clone(),
            src_user,
            src_chain_id,
            src_token,
            src_amount,
            vaa_sequence: 0,
            received_at: now,
        };

        env.storage()
            .persistent()
            .set(&ProofKey::Proof(intent_id.clone()), &record);

        env.events().publish(
            (Symbol::new(&env, "proof_received"),),
            (intent_id, src_chain_id, src_amount),
        );
    }

    // ── Proof Queries ─────────────────────────────────────────────────────────

    /// Return the stored `ProofRecord` for `intent_id`, or `None` if not yet
    /// received.
    pub fn get_proof(env: Env, intent_id: BytesN<32>) -> Option<ProofRecord> {
        env.storage()
            .persistent()
            .get(&ProofKey::Proof(intent_id))
    }

    /// Returns `true` iff a valid proof exists for `intent_id`.
    pub fn has_proof(env: Env, intent_id: BytesN<32>) -> bool {
        env.storage()
            .persistent()
            .has(&ProofKey::Proof(intent_id))
    }

    /// Return `intent_id`'s `ProofRecord` only if it exists and is still
    /// fresh (`now - received_at <= PROOF_VALIDITY_WINDOW`). Panics with
    /// `Error::ProofNotFound` if no proof was received, or
    /// `Error::ProofStale` if one exists but has aged out (issue #254).
    /// This is the entry point `fill_intent`'s proof check (issue #5) is
    /// intended to call — `get_proof`/`has_proof` remain raw, freshness-blind
    /// reads for other callers.
    pub fn get_fresh_proof(env: Env, intent_id: BytesN<32>) -> ProofRecord {
        let record: ProofRecord = env
            .storage()
            .persistent()
            .get(&ProofKey::Proof(intent_id))
            .unwrap_or_else(|| panic_with_error!(&env, Error::ProofNotFound));
        let now = env.ledger().timestamp();
        // Boundary: exactly at the validity window is still fresh (inclusive),
        // matching this codebase's documented inclusive/exclusive convention
        // (issue #26) — validity holds through the boundary second itself.
        if now - record.received_at > PROOF_VALIDITY_WINDOW {
            panic_with_error!(&env, Error::ProofStale);
        }
        record
    }

    // ── Test Back-Door ────────────────────────────────────────────────────────

    /// **Test-only** (available only when the `testutils` Cargo feature is
    /// enabled): directly insert a `ProofRecord` into storage, bypassing
    /// all VAA parsing and Guardian verification.
    ///
    /// This allows integration tests to set up any proof scenario — including
    /// edge cases like proofs with mismatched amounts or chain IDs — without
    /// constructing a real signed VAA.
    ///
    /// The method is intentionally not guarded by admin auth in the mock so
    /// that any test address can call it.  A production implementation would
    /// not expose this method at all.
    ///
    /// Issue #264: deliberately ignores the pause flag. This is test-setup
    /// scaffolding, not the production message-receipt path `pause` protects;
    /// tests that need to assert paused-`receive_message` behavior call
    /// `receive_message` directly.
    #[cfg(feature = "testutils")]
    pub fn mock_set_proof(env: Env, record: ProofRecord) {
        // Reject replays (same as receive_message) so tests that accidentally
        // call this twice get a clear error rather than silent overwrite.
        if env
            .storage()
            .persistent()
            .has(&ProofKey::Proof(record.intent_id.clone()))
        {
            panic_with_error!(&env, Error::ProofAlreadyExists);
        }
        env.storage()
            .persistent()
            .set(&ProofKey::Proof(record.intent_id.clone()), &record);
    }

    /// **Test-only**: remove a stored proof.  Useful for testing the
    /// "proof not found" path after a proof was previously inserted.
    #[cfg(feature = "testutils")]
    pub fn mock_remove_proof(env: Env, intent_id: BytesN<32>) {
        env.storage()
            .persistent()
            .remove(&ProofKey::Proof(intent_id));
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    fn require_admin(env: &Env) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&ProofKey::Admin)
            .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized));
        admin.require_auth();
    }

    /// Convert a raw byte slice into a lowercase hex `String`.
    /// Used to represent `src_user` and `src_token` fields decoded from the
    /// VAA payload without pulling in an external base58/hex crate.
    fn bytes_to_hex_string(env: &Env, bytes: &Bytes) -> String {
        const HEX: &[u8] = b"0123456789abcdef";
        // Each byte becomes two hex characters; prepend "0x".
        let len = bytes.len();
        // Build into a fixed-size Bytes then convert to String.
        let mut out = Bytes::new(env);
        out.push_back(b'0');
        out.push_back(b'x');
        let mut i = 0u32;
        while i < len {
            let byte = bytes.get(i) as u8;
            out.push_back(HEX[(byte >> 4) as usize]);
            out.push_back(HEX[(byte & 0x0f) as usize]);
            i += 1;
        }
        String::from_bytes(env, &out)
    }
}
