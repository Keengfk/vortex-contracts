# Bridge / Messaging Protocol Comparison for Source-Chain Proof Delivery

**Issue:** #128  
**Author:** Vortex Protocol Research  
**Date:** July 2026  
**Branch:** `docs/bridge-protocol-comparison`

---

## Overview

Vortex Protocol settles cross-chain swap intents on Stellar. To slash a solver
for non-delivery or to verify that a user actually deposited on the source
chain, the `intent_settlement` contract must be able to trust a claim about a
transaction that happened on another chain (e.g., Ethereum, Base, Arbitrum).

This document evaluates general-purpose cross-chain messaging/bridge protocols
as one path to deliver that proof to Stellar. It is **complementary** to the
Stellar-oracle spike (which covers native oracle infrastructure such as DIA,
Reflector, and Band). See that document for the oracle-only path; the
recommendation section cross-references both.

The core question is: **which protocol can relay a verifiable proof that
"tx X on chain A locked/sent Y tokens from address Z" into a Soroban contract
on Stellar Mainnet, at acceptable cost, latency, and trust level?**

---

## Scope

| In scope | Out of scope |
|---|---|
| Protocols with live or announced Stellar Mainnet support | Protocols with no Stellar roadmap |
| General message passing (arbitrary payloads) | Token-bridge-only products |
| Trust model analysis relevant to proof delivery | Full DeFi bridge feature comparison |
| Security incident history | Tokenomics, governance token speculation |

Protocols evaluated: **Axelar**, **LayerZero**, **Wormhole**, **Chainlink CCIP**, **Hyperlane**.

---

## Protocol Summaries

### 1. Axelar (Mobius Development Stack)

#### Stellar Support Maturity: ✅ Live Mainnet (February 2026)

Axelar is the most mature option for Stellar today. The integration went live
on **February 16, 2026**, with Solv, Stronghold, and Squid already running
production traffic on the Stellar↔Ethereum corridor. Stellar's official
developer docs (`developers.stellar.org/docs/tools/infra-tools/cross-chain`,
last updated July 24, 2026) list Axelar as the **only** cross-chain protocol
currently documented for Soroban integration. The Soroban gateway contracts are
published at
[`axelarnetwork/axelar-cgp-soroban`](https://github.com/axelarnetwork/axelar-cgp-soroban).

**What it provides:**

- **General Message Passing (GMP):** Soroban contracts can send and receive
  arbitrary byte payloads to/from Ethereum, Avalanche, Base, Polygon, and all
  other Axelar-connected chains (~70+).
- **Interchain Token Service (ITS):** Deploy tokens that exist natively on
  multiple chains with burn-and-mint semantics—relevant if Vortex eventually
  issues a canonical cross-chain receipt token.

**How proof delivery would work:**

A relayer monitors the source chain (e.g., Ethereum) and, upon detecting the
user's deposit transaction, constructs an Axelar GMP call with the relevant
fields (tx hash, sender, amount, token, intent ID). Axelar validators reach
consensus and deliver the message to the Vortex Soroban receiver contract,
which can then validate the claim and update intent state.

#### Trust Model

Axelar is a **proof-of-stake chain** with its own validator set. Validators
stake AXL and sign cross-chain messages after reaching Byzantine fault-tolerant
consensus. This is an external-validator trust model: you trust that
two-thirds-plus of Axelar's validator set is honest. Axelar validators
re-attest the other chain's state rather than verifying it with light clients,
which is a weaker cryptographic assumption than native IBC light clients.

**Organizational risk (important):** In February 2026, Circle acquired the
team and intellectual property of **Interop Labs**, Axelar's primary core
developer. Axelar co-founder Sergey Gorbunov joined Circle. The Axelar
network, Foundation, and AXL token remain under community governance, with
**Common Prefix** taking over development. Axelar is not shutting down, but
its founding dev shop moving to a direct competitor is a genuine long-term
maintainability concern.

| Property | Detail |
|---|---|
| Trust model | PoS validator consensus (external, not light-client) |
| Validator count | ~75 active validators; threshold 2/3 stake |
| Slashing | Yes – validators are slashable for misbehavior |
| Exploit history | No direct protocol exploit to date |
| Org risk | Interop Labs acquired by Circle (Feb 2026) |

#### Cost

- GMP calls: typically **$0.10–$0.50** per message depending on destination gas
  prices. Fees are paid in the source-chain native token via a gas service.
- An extra consensus hop adds overhead vs. lighter messaging protocols.
- Stellar fees for execution are very low (Soroban resource model).

#### Latency

- **~1–2 minutes** end-to-end on most routes (source finality + Axelar
  consensus + Stellar ledger close).
- Stellar's ~5–6 second ledger close adds negligible overhead.

#### Developer Experience

- Best-in-class Stellar documentation; official Stellar docs link directly to
  Axelar.
- Soroban SDK and example contracts available.
- Active production usage means real-world issues are already being surfaced
  and fixed.

---

### 2. LayerZero v2

#### Stellar Support Maturity: ✅ Live Mainnet (2026)

LayerZero has live Stellar Soroban support with dedicated documentation at
`docs.layerzero.network/v2/developers/stellar`. Contracts implement the OApp
(Omnichain Application) and OFT (Omnichain Fungible Token) standards in Rust
using Soroban macros (`#[lz_contract]`, `#[oapp]`). An independent security
audit was conducted via Code4rena (April 2026 contest: `2026-04-layerzero`).

**Coverage:** 100+ chains including EVM, Solana, TON, Aptos, Sui, and Stellar.

#### Trust Model

LayerZero v2 uses a **Decentralized Verifier Network (DVN)** model. Each
application configures its own X-of-Y-of-N security stack from independent
DVNs. Messages are not delivered until the required threshold of DVNs attest
the source-chain event. This gives Vortex full control over the security
configuration—but also full responsibility for it.

Notable DVN operators include Google Cloud, Polyhedra, Nethermind, Fidelity's
FCAT, and Worldpay.

**Critical security incident (April 18, 2026):** The KelpDAO rsETH bridge was
drained of approximately **$292 million** in the largest DeFi exploit of 2026.
Root cause: KelpDAO had configured a **single LayerZero DVN** as their entire
verification stack. Attackers (attributed to North Korea's Lazarus Group)
compromised the RPC infrastructure of that single DVN, fed it falsified data,
and forged a cross-chain message that released 116,500 rsETH from the bridge
escrow. The protocol itself was not broken; the flaw was in the integrator's
configuration choosing a 1-of-1 DVN. The incident proved that the DVN model's
security guarantee is only as strong as the weakest configuration in production.

**Implication for Vortex:** A multi-DVN configuration (minimum 2-of-3 with
diverse operators) is non-negotiable. Do not ship with a single DVN.

| Property | Detail |
|---|---|
| Trust model | Configurable DVN set (X-of-Y-of-N attestation) |
| Slashing | No protocol-level economic backstop; DVN reputation only |
| Exploit history | KelpDAO $292M (Apr 2026) — config flaw, not protocol flaw |
| Decentralization | High with a well-chosen DVN set; low with lazy defaults |

#### Cost

- Generally **low to medium**: DVN fees + destination gas.
- Per-message cost on EVM→Stellar routes: approximately **$0.05–$0.30** for
  typical message sizes, depending on DVN configuration and Ethereum gas prices.
- Source-chain gas is often the dominant cost component on L1 sources.

#### Latency

- **~30 seconds to 2 minutes** with fast DVN configurations.
- Fastest of the protocols with confirmed Stellar support when DVNs have
  efficient source-chain watchers.

#### Developer Experience

- Thorough Stellar-specific docs; the Soroban EVM-vs-Stellar differences are
  well documented (storage TTL management, Soroban auth framework, fee payment
  differences).
- Multiple audits published on GitHub.
- Relatively new on Stellar (less battle-tested than Axelar here), but the
  OApp/OFT standard is widely used on other chains.

---

### 3. Chainlink CCIP

#### Stellar Support Maturity: ⚠️ Announced, Not Yet Live

In mid-2025, Stellar announced joining **Chainlink Scale** and adopting CCIP,
Data Feeds, and Data Streams. As of July 2026, CCIP integration on Stellar
Mainnet is announced but there are no live Soroban CCIP contracts in the
official Chainlink docs or in the Stellar developer docs. The partnership is
strategically significant (SDF explicitly chose Chainlink for institutional DeFi
infrastructure), but production readiness on Stellar is pending.

**Coverage once live:** 57+ mainnets including Solana, Aptos, TON (EVM-heavy
but expanding).

#### Trust Model

CCIP uses Chainlink's **Decentralized Oracle Network (DON)** for message
relaying, plus a separate **Risk Management Network (RMN)** that independently
monitors transfers and can pause anomalous activity. This defense-in-depth
architecture is the strongest institutional security posture among the
evaluated protocols. The RMN is operated by Chainlink (not fully independent
third parties), providing code-diversity rather than org-diversity, but no
protocol-level exploit has occurred to date.

| Property | Detail |
|---|---|
| Trust model | Chainlink DON + independent Risk Management Network |
| Slashing | N/A (Chainlink-operated nodes) |
| Exploit history | None at protocol level |
| Decentralization | Medium — gated onboarding, Chainlink-operated networks |

#### Cost

- **Higher** than competitors: typically **$2–$10** per message on mainnet
  routes, partly because CCIP waits for more source-chain confirmations.
- Best justified for high-value, infrequent transfers (e.g., large institutional
  fills) rather than high-frequency small proofs.

#### Latency

- **10–20 minutes** by default — deliberately conservative to allow the Risk
  Management Network to review and halt suspicious transfers.
- Configurable in some contexts, but the safety checks impose a floor.

#### Developer Experience

- Excellent documentation; large ecosystem of integrators.
- **Not yet available on Stellar Mainnet.** Cannot be used today.
- Best choice once live if Vortex prioritizes institutional-grade security over
  cost and speed.

---

### 4. Wormhole

#### Stellar Support Maturity: ❌ No Confirmed Integration

No Stellar Soroban integration was found in Wormhole's documentation, GitHub
repositories, or public announcements as of July 2026. Wormhole's network
update blog (April 2026) focused on deprecating Scroll support, with no mention
of Stellar. Wormhole's comparative strengths are **Solana** and broad non-EVM
reach (Sui, Aptos, Sei, Cosmos via IBC), not Stellar.

Wormhole is **not viable for Vortex** at this time without custom integration
work.

| Property | Detail |
|---|---|
| Trust model | 19 institutionally-run Guardians, 13-of-19 threshold |
| Slashing | None — proof-of-authority, no staking |
| Exploit history | $326M Solana exploit (Feb 2022, refunded by Jump Crypto) |
| Stellar support | ❌ None confirmed |

**Security note:** The 2022 exploit forged a signature-verification path to
mint 120,000 wETH on Solana. The architecture has been hardened significantly.
The structural risk remains the fixed, permissioned guardian set—compromise
13-of-19 and you can forge any message.

---

### 5. Hyperlane

#### Stellar Support Maturity: ❌ No Confirmed Integration

Hyperlane supports **150+ chains** across 7 virtual machines (EVM, Solana,
Cosmos, and others), but no Stellar Soroban integration is confirmed in their
documentation or GitHub as of July 2026. Hyperlane's permissionless model means
a community contributor *could* deploy Hyperlane contracts on Stellar without
foundation approval—but no such deployment has been announced or documented.

Hyperlane is **not available for Stellar today**, but worth monitoring given
its permissionless deployment model.

| Property | Detail |
|---|---|
| Trust model | Modular ISM (multisig, optimistic, ZK — app-chosen) |
| Chain coverage | 150+ (widest of evaluated protocols) |
| Stellar support | ❌ None confirmed |
| Exploit history | No major protocol exploit |

**If/when Hyperlane deploys on Stellar**, it would be the most flexible option
for Vortex: application-level ISM composition means the proof-delivery security
module could be tailored exactly to Vortex's requirements (e.g., an optimistic
ISM for low-value intents + multisig ISM for high-value ones).

---

## Side-by-Side Comparison

| Protocol | Stellar Mainnet | Trust Model | Finality | Cost/msg | Exploit History | Stellar Docs |
|---|---|---|---|---|---|---|
| **Axelar** | ✅ Live (Feb 2026) | PoS validator consensus | ~1–2 min | ~$0.10–$0.50 | None at protocol level | Official Stellar docs |
| **LayerZero** | ✅ Live (2026) | Configurable DVN (X-of-Y-of-N) | ~30s–2 min | ~$0.05–$0.30 | KelpDAO $292M (config flaw) | Dedicated Stellar section |
| **Chainlink CCIP** | ⚠️ Announced | DON + Risk Mgmt Network | ~10–20 min | ~$2–$10 | None | Not live yet |
| **Wormhole** | ❌ None | 19 Guardians (13/19 PoA) | ~1–5 min | ~$0.10–$0.50 | $326M (2022, refunded) | ❌ |
| **Hyperlane** | ❌ None | Modular ISM (app-chosen) | ~1–3 min | ~$0.01–$0.10 | None | ❌ |

---

## Evaluation Against Vortex Requirements

Vortex needs to:

1. **Verify that a user deposited on the source chain** (e.g., locked ETH or
   USDC on Ethereum) — proof of a specific transaction.
2. **Do so within the fill window** (current: 5 minutes) — or at least quickly
   enough that slashing remains economically meaningful.
3. **Operate at a cost that doesn't price out small intents** — per-proof cost
   matters.
4. **Not introduce a new centralization risk** that could be exploited to forge
   proofs and drain solver bonds.

### Requirement 1: Source-Chain Proof Delivery

Both **Axelar GMP** and **LayerZero OApp** satisfy this: an off-chain relayer
observes the source-chain event and delivers an attested message to the Soroban
contract. The Soroban contract must be designed to accept only messages from
the authorized bridge endpoint (Axelar gateway address or LayerZero endpoint).

Neither protocol does ZK proof verification on-chain today — the trust shifts
to the attesting validators/DVNs. This is a trust trade-off: validators are
trusted intermediaries, not cryptographic proofs. See the oracle spike for a
comparison with native Stellar oracle paths.

### Requirement 2: Latency vs. Fill Window

| Protocol | Fits 5-minute fill window? |
|---|---|
| LayerZero | ✅ Yes (30s–2 min) |
| Axelar | ✅ Yes (1–2 min) |
| Wormhole | ✅ Yes, but no Stellar support |
| CCIP | ❌ 10–20 min exceeds window |
| Hyperlane | ✅ Yes, but no Stellar support |

CCIP is incompatible with the current 5-minute fill window unless the window
is extended. The fill window could be revisited in a future contract version
if CCIP becomes the chosen protocol.

### Requirement 3: Cost Per Proof

Rough per-proof costs assuming Ethereum as source chain:

| Protocol | Approx. cost (Eth L1 source) | Viability for small intents |
|---|---|---|
| LayerZero | ~$0.05–$0.30 | ✅ Viable |
| Axelar | ~$0.10–$0.50 | ✅ Viable |
| CCIP | ~$2–$10 | ⚠️ Only viable for larger fills |
| Wormhole | N/A (no Stellar) | — |
| Hyperlane | N/A (no Stellar) | — |

On L2 source chains (Base, Arbitrum, Optimism), source-chain gas drops
significantly and total cost for LayerZero and Axelar falls to cents.

### Requirement 4: Trust / Attack Surface

| Protocol | Risk | Mitigation |
|---|---|---|
| Axelar | Validator set compromise; Circle acquisition of core dev | Monitor validator decentralization; multi-sig on contract admin |
| LayerZero | DVN misconfiguration (proven by KelpDAO $292M) | Mandate ≥2-of-3 diverse DVNs; periodic DVN set review |
| CCIP | Chainlink org dependency; higher centralization | Acceptable for institutional use; diversify over time |

For Vortex, the most actionable risk is the **LayerZero DVN config flaw pattern**.
If LayerZero is chosen, the DVN configuration must be:
- Minimum 2-of-3 independent DVNs (e.g., Google Cloud DVN + Nethermind DVN +
  a ZK-based DVN such as Polyhedra)
- A security configuration review process before any DVN changes
- Rate limits on the Soroban receiver to cap worst-case loss from forged proofs

---

## Recommendation

### Primary path (today): Axelar

Axelar is the recommended starting point for Vortex's source-chain proof
delivery for the following reasons:

1. **Only production-ready option on Stellar with official documentation.** The
   Stellar Development Foundation's own docs list Axelar as the integration
   path. This means library quality, example contracts, and upstream bug fixes
   are already in motion.
2. **GMP is purpose-built for arbitrary payload delivery** — not just token
   bridging. A proof message (`{intent_id, src_chain, src_tx_hash, sender,
   token, amount}`) is exactly the kind of payload GMP was designed for.
3. **Latency (1–2 min) fits within the 5-minute fill window** with headroom.
4. **Active ecosystem on Stellar** (Solv, Stronghold, Squid) means protocol
   behavior on Stellar is observable and tested.

**Caveats to monitor:**
- The Circle/Interop Labs acquisition is a medium-term maintainability risk.
  Track Common Prefix's development velocity over the next 6 months.
- Axelar's validator set is smaller than Ethereum's; economic security is
  bounded by AXL stake, not ETH stake.

### Alternative path (ready to switch): LayerZero

LayerZero is a close second and becomes the preference if:

- Axelar's post-acquisition development velocity drops
- A route requires a chain Axelar doesn't support (LayerZero covers 100+ vs.
  Axelar's ~70+)
- Vortex wants fine-grained per-route DVN configuration

The **mandatory condition** for LayerZero adoption is a multi-DVN security
configuration (≥ 2-of-3 independent DVNs). Do not ship a single-DVN
configuration under any circumstances.

### Future watch: Chainlink CCIP

Once CCIP is live on Stellar Mainnet, it becomes the preferred choice for
high-value fills (large solver payouts, institutional intents) due to its
defense-in-depth Risk Management Network. The 10–20 minute latency can be
addressed by extending the fill window for intents above a configurable value
threshold. A hybrid architecture (Axelar/LayerZero for standard intents, CCIP
for high-value intents) is worth exploring at that point.

### Future watch: Hyperlane

Monitor Hyperlane's ecosystem for a community Stellar Soroban deployment. Its
permissionless ISM model is architecturally ideal for Vortex: the proof-delivery
security module could be tuned per-intent-size. If deployed, it would likely
be the lowest-cost option.

### Cross-reference: Stellar Oracle Spike

The oracle-focused spike evaluates DIA, Reflector, and Band as native
Stellar oracle infrastructure. The key architectural distinction is:

- **Bridge/messaging protocols (this document):** A relayer actively watches
  the source chain and pushes a proof message to Stellar when a specific
  event occurs. Suitable for event-driven, real-time proof delivery.
- **Oracles:** A data provider periodically publishes price feeds or
  state data on-chain. Suitable for price references; less suited to
  proving specific transaction inclusion.

For intent settlement, **bridge/messaging protocols are more appropriate** for
source-chain proof delivery because they can deliver a specific, transaction-level
attestation on demand. Oracles are better suited to supplementary roles (e.g.,
verifying that the solver delivered the correct amount of a token whose price
was quoted off-chain).

A hybrid model—oracle for price sanity checks + messaging protocol for
tx-inclusion proofs—is likely the most robust architecture.

---

## Open Questions for Interface Design

This research feeds into the cross-chain proof verification interface design.
The following questions should be addressed in that issue:

1. **Who runs the relayer?** Bridge protocols require an off-chain relayer to
   watch the source chain and submit GMP calls. Options: Vortex runs it, the
   solver runs it, or a permissionless bounty model incentivizes anyone to
   submit.

2. **What is the minimum proof payload?** The Soroban receiver contract needs
   to validate: `(intent_id, src_chain, src_tx_hash, sender, token_address,
   amount)`. Are there additional fields needed for slashing evidence?

3. **How do we handle proof forgery risk?** Both Axelar and LayerZero are
   validator-trust models, not cryptographic proofs. Rate limits and
   per-intent value caps on the Soroban receiver are necessary mitigations.

4. **Should the fill window be parameterized by source chain?** Ethereum L1
   has ~12s block times plus confirmation depth; CCIP requires 10–20 min.
   A parameterizable `proof_deadline` per source chain would future-proof
   the interface.

5. **Multi-protocol redundancy?** A Soroban receiver that accepts proofs from
   either Axelar or LayerZero (with the same payload schema) provides
   resilience against one protocol having an outage.

---

## References

- Axelar Stellar integration announcement: https://www.axelar.network/blog/axelar-stellar-integration (Feb 16, 2026)
- Axelar Soroban contracts: https://github.com/axelarnetwork/axelar-cgp-soroban
- Stellar developer docs — Cross-Chain: https://developers.stellar.org/docs/tools/infra-tools/cross-chain (updated Jul 24, 2026)
- LayerZero Stellar getting started: https://docs.layerzero.network/v2/developers/stellar/getting-started
- LayerZero Code4rena audit: https://github.com/code-423n4/2026-04-layerzero
- Chainlink CCIP × Stellar announcement: https://stellar.org/blog/foundation-news/stellar-to-join-chainlink-scale-and-adopt-data-feeds-data-streams-and-ccip-to-power-next-gen-defi-applications
- KelpDAO $292M exploit (LayerZero DVN config flaw): https://www.chainalysis.com/blog/kelpdao-bridge-exploit-april-2026/
- Wormhole $326M exploit (2022): https://www.halborn.com/blog/post/explained-the-wormhole-hack-february-2022
- Circle acquires Interop Labs (Axelar): https://www.circle.com/blog/circle-signs-agreement-to-acquire-interop-labs-team-intellectual-property
- Protofire cross-chain comparison 2026: https://protofire.io/guides/cross-chain-messaging/
- Eco cross-chain messaging protocols 2026: https://eco.com/support/en/articles/14729258-8-best-cross-chain-messaging-protocols-2026
