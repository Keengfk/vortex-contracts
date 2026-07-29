# Design Doc: Multi-Sig Admin Control

**Issue:** [#114](https://github.com/stellar-vortex-protocol/vortex-contracts/issues/114)  
**Branch:** `docs/task-spike`  
**Status:** Design complete — recommendation in §5

---

## 1. Problem Statement

`DataKey::Admin` is a single `Address` that controls:

- `set_fee_recipient` — changes where protocol fees and slashed bonds go.
- `add_allowed_dst_token` / `remove_allowed_dst_token` / `set_dst_allowlist_enabled` — controls which tokens users can request.
- `pause` / `unpause` — halts or restores the protocol.
- `transfer_admin` — transfers admin control itself.

A single compromised or lost key is a total loss of protocol control. A
compromised key could drain fee revenue to an attacker's address, brick the
allowlist, or pause the protocol indefinitely. A lost key means no one can
ever unpause, transfer admin, or update fee routing.

The goal of this spike is to evaluate whether Stellar's native multi-signature
mechanism is sufficient, or whether in-contract multi-sig logic is needed.

---

## 2. How Stellar Native Multi-Sig Works

Stellar accounts have a built-in multi-sig model at the protocol level:

- **Signers**: An account can have multiple signers, each with a weight (1–255).
- **Thresholds**: Separate thresholds for Low, Medium, and High operations.
  A transaction's required threshold depends on its operation type.
- **Account-level enforcement**: The Stellar network itself rejects transactions
  that don't meet the threshold; no contract logic is needed.
- **Multi-sig account as a contract signer**: A Stellar account used as a
  contract's `Admin` address can itself be a multi-sig account. When
  `admin.require_auth()` is called inside the contract, Stellar validates that
  the transaction carries enough signer weight to meet the account's threshold.

This means: **if the `Admin` address is a Stellar multi-sig account, the
contract already enforces multi-sig without any changes to `intent_settlement`.**

### 2.1 Soroban `require_auth` and multi-sig accounts

`Address::require_auth()` in Soroban works for both:
1. Regular accounts — a single ed25519 signature.
2. Multi-sig accounts — the transaction must include signatures from enough
   signers to meet the account's Medium or High threshold (depending on
   operation type).

The verification happens at the Stellar protocol layer before the contract
invocation, so the contract code is agnostic — it just calls `require_auth()`.

**Conclusion: No contract code changes are needed to support native multi-sig.**

---

## 3. Native Multi-Sig: Capabilities and Limitations

### 3.1 What it handles well

| Requirement | Native multi-sig |
|-------------|-----------------|
| K-of-N approval for admin operations | ✅ Yes (weight + threshold) |
| Signers can be individual Stellar keypairs | ✅ Yes |
| Signers can be hardware wallet keys | ✅ Yes (Ledger, Trezor, etc.) |
| Time-locks between approval and execution | ❌ No |
| On-chain proposal tracking (who approved what) | ❌ No (off-chain coordination) |
| Signers as smart contract addresses (e.g., a DAO) | ⚠️ Partial (see §3.2) |
| Emergency single-key pause (fast response) | ⚠️ Possible with weight split (see §4.2) |

### 3.2 Signer types

Stellar native signers must be ed25519 keypairs or pre-auth transactions
(hash-of-tx signers). They cannot be arbitrary smart contract addresses.
If Vortex eventually wants a DAO or governance module to be a signer, a
custom in-contract multi-sig would be required. For the near term (team-controlled
multi-sig), native multi-sig is sufficient.

### 3.3 No time-locks

Stellar's native multi-sig has no built-in time-lock between when a quorum is
reached and when the transaction executes. If a time-lock is needed (e.g., 24-hour
delay for `set_fee_recipient`), an in-contract implementation or a Gnosis
Safe-style guardian contract is required.

---

## 4. Options

### Option A — Native multi-sig account as Admin (Recommended)

Set `Admin` to a Stellar account controlled by a 2-of-3 (or similar) multi-sig
during `initialize`. The contract requires zero changes.

**Setup example** (Stellar CLI):

```bash
# Create a fresh admin account
stellar keys generate admin-multisig

# Add signer 1 (weight 1) — team member Alice
stellar account set-options \
  --source admin-multisig \
  --signer-key <ALICE_PUBLIC_KEY> \
  --signer-weight 1 \
  --network mainnet

# Add signer 2 (weight 1) — team member Bob
stellar account set-options \
  --source admin-multisig \
  --signer-key <BOB_PUBLIC_KEY> \
  --signer-weight 1 \
  --network mainnet

# Add signer 3 (weight 1) — team member Carol
stellar account set-options \
  --source admin-multisig \
  --signer-key <CAROL_PUBLIC_KEY> \
  --signer-weight 1 \
  --network mainnet

# Set medium threshold = 2 (2-of-3 required for most operations)
stellar account set-options \
  --source admin-multisig \
  --med-threshold 2 \
  --network mainnet

# Set high threshold = 2 as well (for key rotation)
stellar account set-options \
  --source admin-multisig \
  --high-threshold 2 \
  --network mainnet

# Remove the master key (so the account truly requires multi-sig)
stellar account set-options \
  --source admin-multisig \
  --master-weight 0 \
  --network mainnet
```

After setup, `admin-multisig` is the address passed to `initialize`. Any call
to a contract function protected by `admin.require_auth()` requires 2 of the 3
signers to co-sign the Stellar transaction.

**Pros:**
- Zero contract code changes.
- Battle-tested Stellar primitive.
- Hardware wallet support (Ledger/Trezor).
- Immediate deployability.

**Cons:**
- No on-chain audit trail of who approved what (must rely on transaction history).
- No time-locks.
- Signer rotation requires the admin account itself to send a `set-options`
  transaction (still requires quorum, so it is safe).
- Off-chain coordination tool needed (e.g., Stellar Multisig Coordinator, Lobstr
  multisig, or a simple shared Notion + Stellar CLI flow).

---

### Option B — In-Contract Multi-Sig Logic

Add a `MultiSigConfig` struct to instance storage:

```rust
struct MultiSigConfig {
    pub signers: Vec<Address>,
    pub threshold: u32,
}
```

Admin operations would accumulate approvals in a `Proposal` storage entry
until `threshold` is reached, then execute.

**Pros:**
- On-chain proposal tracking.
- Signers can be any `Address` including contract addresses (future DAO).
- Time-locks are implementable.

**Cons:**
- Significant new contract surface (Proposals, expiry, replay protection).
- More code = more attack surface.
- Higher gas cost per admin operation.
- Not needed today — the team is small and native multi-sig is sufficient.

**Verdict:** Defer to a future upgrade if/when governance requirements demand it.

---

### Option C — External Guardian/Timelock Contract

Deploy a separate `AdminGuardian` contract that:
1. Accepts signed proposals from K-of-N signers.
2. Enforces a time-lock between proposal approval and execution.
3. Calls `transfer_admin` on `intent_settlement` to rotate admin if needed.

`Admin` in `intent_settlement` is set to the `AdminGuardian` contract address.

**Pros:**
- Time-locks without in-contract logic.
- Clean separation of concerns.

**Cons:**
- Two contracts to maintain and upgrade.
- `AdminGuardian` itself needs a multi-sig or trusted deployer.
- Overkill for current protocol stage.

**Verdict:** Consider for mainnet v2 if protocol TVL warrants it.

---

## 5. Recommendation

**Implement Option A: use a Stellar native multi-sig account as `Admin`.**

No contract code changes are required. This is an **operational recommendation**
rather than a code change.

Specific actions:

1. **Before testnet deployment:** Set up a 2-of-3 Stellar multi-sig account
   (see §4, Option A setup script). Use this address for `initialize`'s `admin`
   parameter.

2. **Key holder policy:** Each signer should use a hardware wallet (Ledger or
   Trezor). Store the 3rd key in a cold storage device (air-gapped). Document
   key-holder identity in a private ops runbook.

3. **Pause authority split (optional):** To allow fast incident response without
   full multi-sig coordination, consider a separate `PauseGuardian` address
   stored alongside `Admin`:
   - `Admin` (2-of-3) controls all sensitive operations.
   - `PauseGuardian` (single key, hot) can only call `pause()`.
   - `Admin` can always call `unpause()` and rotate `PauseGuardian`.
   This requires a small contract change to store `DataKey::PauseGuardian` and
   split `pause()` from the full `require_admin()` guard.

4. **Signer rotation ceremony:** Document the process for replacing a
   compromised signer: (a) two remaining signers co-sign a `set-options` tx
   on the admin account to add the replacement key and remove the compromised
   one, (b) verify the new signer configuration, (c) update ops runbook.

5. **Future upgrade path:** If the protocol evolves toward on-chain governance
   (DAO vote → admin action), revisit Option B or C. The `transfer_admin`
   function already allows migrating admin to a new contract at any time.

---

## 6. Contract Changes Required

**Option A (recommended):** None. The recommendation is operational.

**Optional PauseGuardian split (small change):**

```rust
// Add to DataKey:
PauseGuardian,

// New function:
pub fn set_pause_guardian(env: Env, guardian: Address) {
    Self::require_admin(&env);
    env.storage().instance().set(&DataKey::PauseGuardian, &guardian);
}

// Modify pause() to accept either admin OR pause guardian:
pub fn pause(env: Env) {
    let admin: Address = ...; // existing
    let guardian: Option<Address> = env.storage().instance().get(&DataKey::PauseGuardian);
    
    // Either admin or the designated guardian can pause
    if let Some(g) = guardian {
        // Try guardian auth first; fall back to admin
        // (In practice: the caller signs with whichever key they hold)
        let caller_is_guardian = /* check invocation auth */;
        if !caller_is_guardian {
            admin.require_auth();
        } else {
            g.require_auth();
        }
    } else {
        admin.require_auth();
    }
    // ... rest of pause logic
}
```

This is low-risk and directly addresses the incident-response concern. Full
implementation detail is left to the implementation PR.

---

## 7. Summary

| Approach | Contract changes | Time-locks | On-chain audit | DAO-ready | Recommended |
|----------|-----------------|------------|----------------|-----------|-------------|
| Native multi-sig (A) | None | No | No | No | ✅ Now |
| In-contract multi-sig (B) | Large | Yes | Yes | Yes | Future |
| External guardian (C) | Small | Yes | Partial | Partial | Future |

The native multi-sig account approach satisfies Vortex's current security
requirements (eliminating single-key risk) with zero contract changes and
immediate deployability. The optional `PauseGuardian` split is the only code
change worth considering at this stage.

---

*Closes #114*
