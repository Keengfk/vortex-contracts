# Research Spike: Cross-Chain Proof Verification via Stellar Oracle/Messaging Infra

**Issue:** [#49](https://github.com/stellar-vortex-protocol/vortex-contracts/issues/49)  
**Branch:** `docs/task-spike`  
**Status:** Spike complete — recommendation in §6  
**Follow-up:** [#124](https://github.com/stellar-vortex-protocol/vortex-contracts/issues/124) — concrete interface design

---

## 1. Problem Statement

Vortex's `fill_intent` currently trusts the solver's claim that the source-chain
transaction occurred. A solver calls `fill_intent`, transfers `dst_token` to the
user, and the contract marks the intent `Filled`. The bond + slash mechanism
creates an economic disincentive to lie, but it does not provide cryptographic
proof that a corresponding deposit happened on the source chain.

Goals of this spike:

1. Survey available options for verifying a source-chain tx on Stellar.
2. Assess each option's feasibility, latency, cost, and alignment with the
   existing trust model.
3. Produce a recommendation that drives the design in issue #124.

---

## 2. Current Trust Model (Baseline)

```
User locks funds on source chain (off-chain)
    │
    ▼
Solver observes the lock, calls accept_intent()
    │
    ▼
Solver calls fill_intent() — transfers dst_token to user on Stellar
    │
    ▼
If solver fails → slash_solver() burns 10% of their bond
```

The guarantee is **economic, not cryptographic**. A solver that behaves
honestly earns revenue; one that accepts and does not fill loses 10% of
their USDC bond and has the intent re-opened. The design works at lower
TVL but becomes insufficient as intent sizes or solver counts grow, because:

- A well-capitalised solver could absorb slash costs and still profit from
  taking intents it never fills (griefing users).
- Users have no on-chain recourse if the fill is disputed — there is no
  proof either way.

---

## 3. Options Surveyed

### 3.1 Acurast (Trusted Execution Environment oracles)

**What it is:** A decentralised compute layer where TEE-attested processors
run arbitrary JavaScript. Results are pushed on-chain via Stellar transactions.

**How proof delivery would work:**
1. An Acurast job monitors a source-chain RPC endpoint.
2. When a deposit tx matching `(user, src_token, src_amount, intent_id)` reaches
   finality, the TEE produces a signed attestation.
3. The attestation is written to a Stellar contract (`ProofRegistry`) via
   Acurast's on-chain delivery mechanism.
4. `fill_intent` (or a new `verify_proof` helper) checks `ProofRegistry` before
   releasing payment.

**Finality assumptions:** Ethereum ~12 minutes (2 epochs), Base ~2 minutes
(L1 confirmation), Polygon ~5 minutes.

**Latency:** On-chain proof availability ≈ source finality + Acurast job polling
interval (configurable, min ~30 s). End-to-end median estimate: 5–15 min.

**Cost:** Acurast charges per-job-execution (processor fees); Stellar tx fees are
negligible. Processor fee on mainnet is roughly $0.001–$0.01 per execution,
depending on compute time. Competitive for intents above ~$50.

**Trust model alignment:** TEE attestations reduce trust in the operator; the
verifier still trusts that the TEE firmware is uncompromised. This is a
meaningful improvement over pure economic trust but is not a ZK proof.

**Maturity / Stellar support:** Acurast has a live Stellar integration
(canister-style deployments). Production-ready for Soroban as of early 2025.

**Risks:**
- TEE supply centralisation (limited processor set on testnets).
- Acurast itself is an external dependency; an outage delays all fills.

---

### 3.2 DIA Oracles (Data & Event Oracles)

**What it is:** DIA provides customisable on-chain data feeds sourced from
chain APIs and pushed by a decentralised set of data providers.

**How proof delivery would work:**
1. A DIA "event oracle" feed is configured to publish source-chain transfer
   events matching Vortex's deposit contract ABI.
2. A Stellar-deployed DIA consumer contract reads the published data.
3. `fill_intent` queries the DIA consumer before marking an intent filled.

**Latency:** DIA feeds are configurable; heartbeat intervals range from 60 s
to 24 h. A dedicated event feed could be tuned to 60–120 s update frequency.

**Cost:** DIA charges a service fee for custom feeds. Estimated $500–$2,000/month
for a dedicated event feed, making this unsuitable unless volume justifies it.

**Trust model alignment:** DIA uses a multisig of data providers, not a TEE.
Trust is in the consortium not misbehaving. Weaker than TEE attestation but
widely used in DeFi.

**Maturity / Stellar support:** DIA does not have a production Stellar
integration as of mid-2025. Would require Vortex-funded integration work.

**Risks:**
- No native Stellar support today — significant custom integration cost.
- Data feed model is designed for prices, not arbitrary event proofs; shoehorning
  event proofs is non-standard.

**Verdict:** Not recommended for this use case.

---

### 3.3 Chainlink CCIP (Cross-Chain Interoperability Protocol)

**What it is:** Chainlink's official cross-chain messaging protocol. A source
chain contract sends a CCIP message; CCIP DON relays it; a destination chain
contract receives it.

**How proof delivery would work:**
1. Vortex deploys a `VortexDeposit` contract on each source chain (Ethereum,
   Base, Polygon).
2. When a user deposits, `VortexDeposit` emits a CCIP message containing
   `(intent_id, user, src_token, src_amount)`.
3. CCIP DON relays the message to a Stellar receiver contract.
4. The Stellar receiver writes the proof into a `ProofRegistry` storage entry.

**Latency:** CCIP confirmation time: Ethereum → any chain ≈ 15–20 min
(waits for Ethereum finality). Base/Polygon → Stellar ≈ 5–10 min.

**Cost:** CCIP charges in LINK on the source chain (~$0.50–$2.00 per message
for Ethereum mainnet). This cost is passed to the user or absorbed in the
protocol fee.

**Trust model alignment:** CCIP's Risk Management Network provides an
independent watchdog layer. Strong, well-audited security model. Used in
production by major DeFi protocols.

**Maturity / Stellar support:** **CCIP does not support Stellar as a destination
chain as of mid-2025.** Stellar/Soroban is not in Chainlink's published roadmap.

**Risks:**
- No Stellar lane exists today and timeline is unknown.
- Requires source-chain contracts, adding significant deployment surface.

**Verdict:** Best-in-class security but blocked on Stellar support. Monitor for
future availability.

---

### 3.4 Wormhole Messaging

**What it is:** Wormhole's generic message passing (not just token bridging).
A contract on the source chain emits a Wormhole message (VAA); Wormhole
Guardians sign it; any chain with a Wormhole core contract can verify the VAA.

**How proof delivery would work:**
1. `VortexDeposit` on each source chain calls `wormhole.publishMessage(payload)`.
2. A Guardian quorum (19 of 19 at the time of writing) produces a signed VAA.
3. A Stellar `ProofVerifier` contract calls the Wormhole Stellar core contract,
   verifies the Guardian signatures, and stores the parsed proof in
   `ProofRegistry`.
4. `fill_intent` (or a new entry point) checks `ProofRegistry` before
   authorising the transfer.

**Latency:** Guardian signing takes ~1–2 min after source finality. End-to-end
(including Ethereum finality): ~15 min. L2 chains (Base, Polygon): ~5 min.

**Cost:** Wormhole messaging is free (no per-message fee from the protocol
itself). Solvers pay only source-chain and Stellar tx fees.

**Trust model alignment:** Relies on the 19-of-19 Guardian set not colluding.
Guardian set is publicly known and includes Certus One, Jump Crypto, Everstake,
etc. Robust for DeFi at scale.

**Maturity / Stellar support:** Wormhole has a **production Stellar integration**
(native token transfers via Wormhole NTT live on Stellar mainnet since Q1 2025).
The core contract and Guardian VAA verification are deployed and battle-tested
on Stellar. Generic messaging to Stellar is supported.

**Risks:**
- Guardian collusion (19 signers, practically very low risk).
- Requires source-chain `VortexDeposit` contracts on every supported chain.
- VAA relay must be triggered (usually by solver or a relayer bot).

---

### 3.5 LayerZero V2

**What it is:** LayerZero's modular omnichain messaging. Source chain contracts
emit messages; a configurable DVN (Decentralised Verification Network) attests;
destination chain contracts receive.

**How proof delivery would work:**
Similar to Wormhole — source-chain deposit contract emits an LZ message; DVN
attests; Stellar receiver verifies and stores proof.

**Latency:** ~2–5 min for L2 source chains, ~15 min for Ethereum mainnet.

**Cost:** LZ charges a small fee in native gas on source chain, plus a verifier
fee (~$0.10–$0.50 per message).

**Maturity / Stellar support:** **LayerZero does not have a Stellar endpoint as
of mid-2025.** No public roadmap item. Same blocker as Chainlink CCIP.

**Verdict:** Blocked on Stellar support, same as CCIP.

---

### 3.6 Optimistic / ZK Light Client (DIY or via Herodotus / Succinct)

**What it is:** Deploy a light client of the source chain inside a Soroban
contract that can verify block headers and Merkle inclusion proofs. ZK variants
use a proof system (PLONK/STARK) to compress the verification.

**Feasibility on Soroban:**
Soroban has a `env.crypto()` interface exposing SHA-256, keccak256, and ed25519.
It **does not** expose secp256k1 ECDSA verification natively, which is required
to verify Ethereum block headers signed by the validator set. A workaround would
be to use a custom WASM implementation, but Soroban's WASM execution budget
(CPU instructions) is constrained. Ethereum header verification would likely
exceed the budget for anything more than a single keccak check.

ZK proof verification is similarly impractical in pure Soroban today — there is
no BN254 pairing precompile. Stellar's roadmap includes cryptographic
precompile expansions but no firm date.

**Verdict:** Technically infeasible on Soroban today without significant
protocol-level additions.

---

## 4. Comparison Matrix

| Option              | Stellar Support | Latency       | Cost/msg   | Trust Model          | Maturity |
|---------------------|-----------------|---------------|------------|----------------------|----------|
| Acurast (TEE)       | ✅ Production    | 5–15 min      | ~$0.01     | TEE attestation      | Medium   |
| DIA Event Oracle    | ❌ None          | 60–120 s      | ~$1,500/mo | Data provider msig   | Low      |
| Chainlink CCIP      | ❌ None          | 15–20 min     | ~$1.00     | Risk Mgmt Network    | High     |
| Wormhole Messaging  | ✅ Production    | 5–15 min      | Free       | Guardian quorum      | High     |
| LayerZero V2        | ❌ None          | 2–15 min      | ~$0.25     | DVN quorum           | High     |
| ZK/Light Client     | ⚠️ Infeasible    | N/A           | N/A        | Cryptographic        | N/A      |

---

## 5. Impact on `fill_intent` Trust Model

Today `fill_intent` has no proof gate. With proof verification, the flow becomes:

```
User locks funds on source chain
    │
    ▼
Messaging protocol relays proof to Stellar ProofRegistry
    │
    ▼
Solver calls fill_intent(intent_id, proof_id)
    │
    ▼
fill_intent checks ProofRegistry.has_proof(intent_id) == true
    │
    ▼  (proof valid)
Solver transfers dst_token to user → intent marked Filled
```

Key changes:
- `fill_intent` gains a `proof_id: BytesN<32>` parameter.
- A new `ProofVerifier` contract (or internal function) must be trusted by
  `intent_settlement`.
- The bond + slash mechanism can be relaxed for solvers using proof-verified
  fills; pure-economic trust remains for a legacy mode during migration.
- `accept_intent` can remain as-is; solvers still claim exclusive fill rights.

---

## 6. Recommendation

**Adopt Wormhole generic messaging as the primary proof transport.**

Rationale:
1. Only viable option with production Stellar support and sufficient maturity.
2. No per-message fee makes it economically neutral at any intent size.
3. The Guardian security model (19-of-19 quorum) is well-understood and broadly
   accepted in DeFi.
4. Wormhole NTT is already live on Stellar mainnet, meaning the core contract
   and team support exist.

**Secondary recommendation:** Monitor Acurast as an alternative for chains
where Wormhole source-chain deployment is impractical (e.g., lesser-known EVMs
or non-EVM chains). TEE attestation is a useful fallback.

**Do not pursue** DIA, CCIP, or LayerZero until they publish a Stellar
endpoint.

---

## 7. Rough Integration Shape (Input to #124)

```
[Source chain]                          [Stellar]
VortexDeposit contract                  ProofRegistry contract
  - emit WormholeMessage(               - receive_message(vaa: Bytes)
      intent_id,                        - has_proof(intent_id) -> bool
      user,
      src_token,
      src_amount
    )

intent_settlement (existing)
  fill_intent(solver, intent_id, fill_amount, proof_id)
    └─ call ProofRegistry.has_proof(intent_id)
    └─ proceed only if true
```

The full interface design (exact types, error handling, ProofRegistry storage
layout, migration path) is specified in issue #124.

---

## 8. Open Questions for #124

1. Who triggers the VAA relay to Stellar — the solver, a Vortex relayer bot, or
   a permissionless anyone-can-relay approach?
2. Should the ProofRegistry be a separate contract or a module inside
   `intent_settlement`?
3. How does the system handle a fill where the proof arrives *after* the
   fill window but the solver genuinely filled? (Grace period? Off-chain
   dispute?)
4. Migration: should proof verification be optional (flag per intent) or
   mandatory for all intents above a threshold amount?

---

*Closes #49*
