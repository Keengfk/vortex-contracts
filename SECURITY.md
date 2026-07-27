# Security Policy — `vortex-contracts`

> For general vulnerability reporting instructions, see the org-level
> [SECURITY.md](https://github.com/vortex-protocol/.github/blob/main/SECURITY.md).

---

## Threat Model — `intent_settlement`

This document describes the trust assumptions, assets at risk, and known
limitations of the `intent_settlement` contract ahead of a mainnet deployment.
It is intended for auditors and integrators.

---

### Assets at Risk

| Asset | Where held | Value |
|-------|-----------|-------|
| Solver bonds | `intent_settlement` contract account (USDC) | ≥ 50 USDC per solver |
| User swap output | Solver's token balance (transferred in `fill_intent`) | Unbounded |
| Protocol fees | `FeeRecipient` address | 0.05 % of filled volume |
| Admin privileges | `Admin` key in instance storage | Full contract control |

---

### Trust Assumptions

#### 1. Solver self-reporting

The contract has **no on-chain proof** that a solver actually sent tokens on the
source chain. `fill_intent` trusts the solver to transfer `dst_token` to the
user on Stellar; the economic incentive not to default is the bond slash
(`slash_solver` takes 10 % of the bond permissionlessly once the fill window
expires).

**Implication:** A solver with a large enough bond can accept an intent, fail to
fill, absorb the 10 % slash, and profit if the spread on the source-chain trade
is more valuable than 10 % of their bond.  The bond size is the primary
economic deterrent.

#### 2. Admin key custody

A single `Admin` address controls:

- `pause` / `unpause` — can halt all new swaps.
- `set_fee_recipient` — redirects fee and slash proceeds.
- `transfer_admin` — rotates the admin key (requires both old and new admin to
  sign).
- `add_allowed_dst_token` / `set_dst_allowlist_enabled` — controls which tokens
  users can target.

**Implication:** A compromised admin key can grief the protocol (pause, fee
redirection) but **cannot steal user funds or solver bonds directly**, because
no admin function transfers tokens out of user or solver custody.  Key rotation
requires dual authorization, reducing the single-key-failure surface.

#### 3. Destination token allowlist (default: off)

By default, `submit_intent` accepts any `dst_token` address.  If an admin
enables the allowlist (`set_dst_allowlist_enabled(true)`), only pre-approved
tokens may be used as destinations.

**Implication (allowlist disabled):** A user or solver could name a malicious
contract as `dst_token`.  The risk lands on the user who submitted the intent —
they would receive tokens from a contract they did not vet.  Enabling the
allowlist before mainnet is strongly recommended.

**Implication (allowlist enabled):** The allowlist is stored in instance
storage, so adding/removing tokens is cheap but centralized under the admin
key.

#### 4. Fill-window timing

The fill window (`FILL_WINDOW = 300 s`) starts from `accept_intent`.  The
deadline stored on-chain is `now + 300`.  A solver and a block producer
colluding to manipulate `ledger().timestamp()` could extend the fill window
slightly, but Stellar's timestamp drift is bounded by consensus rules and is not
a meaningful attack surface in practice.

#### 5. Permissionless slash and expire

`slash_solver` and `expire_intent` are callable by anyone.  This is intentional
— it prevents a solver from dodging accountability by waiting out a pause or
ensuring only friendly callers settle the intent.  A griefing scenario where
someone repeatedly calls `expire_intent` on an already-expired intent is
defended by the `IntentNotOpen` guard (idempotent after first call).

---

### Known Limitations

- **No cross-chain proof.** The contract cannot verify the source-chain
  transaction. Trust is economic, not cryptographic.
- **Single admin key.** There is no multi-sig or timelock on admin operations.
  Protocol operators should secure the admin key with a hardware wallet or
  multi-sig wrapper before mainnet.
- **Bond slash is fixed at 10 %.** A solver with a very large bond can default
  cheaply.  A dynamic slash proportional to intent size is on the roadmap.
- **Intent re-open after slash.** After `slash_solver` the intent is reset to
  `Open` with a fresh `INTENT_EXPIRY` deadline.  There is currently no cap on
  how many times an intent can cycle through `Open → Accepted → Slashed`.
- **No allowlist by default.** Until an admin calls `set_dst_allowlist_enabled(true)`,
  any token address — including malicious contracts — can be used as `dst_token`.

---

### Reporting a Vulnerability

Please do **not** open a public GitHub issue for security vulnerabilities.
Follow the responsible-disclosure process described in the org-level
[SECURITY.md](https://github.com/vortex-protocol/.github/blob/main/SECURITY.md).
