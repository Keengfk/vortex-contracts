# Supported Source Chains and Token Address Formats

**Issue:** [#132](https://github.com/stellar-vortex-protocol/vortex-contracts/issues/132)  
**Branch:** `docs/supported-chains-table`  
**Status:** Living document — update when the `add_allowed_src_chain` allowlist is populated for each deployment

---

## 1. Overview

Vortex intents have a `src_chain` field (a plain string, e.g. `"ethereum"`) and
a `src_token` field (the token's address on that chain). Off-chain tooling must
use the canonical strings listed here, or `SrcChainAllowlistEnabled` will reject
the intent when enforcement is active.

The destination chain is always **Stellar** — `dst_token` is a Stellar SAC or
SEP-41 address. This document covers the source-chain side only.

---

## 2. Canonical `src_chain` Strings

These are the values the contract recognises via `add_allowed_src_chain()`:

| `src_chain` value | Network | Chain type | Wormhole chain ID | Status |
|---|---|---|---|---|
| `"ethereum"` | Ethereum Mainnet | EVM | 2 | Supported |
| `"base"` | Base Mainnet | EVM (L2, Coinbase) | 30 | Supported |
| `"polygon"` | Polygon PoS | EVM | 5 | Supported |
| `"arbitrum"` | Arbitrum One | EVM (L2, Offchain Labs) | 23 | Supported |
| `"optimism"` | OP Mainnet | EVM (L2, Optimism) | 24 | Supported |
| `"avalanche"` | Avalanche C-Chain | EVM | 6 | Supported |
| `"bsc"` | BNB Smart Chain | EVM | 4 | Supported |
| `"solana"` | Solana Mainnet Beta | SVM | 1 | Planned |

> **Case-sensitive.** The contract stores and compares these strings literally.
> `"Ethereum"` and `"ETHEREUM"` are not the same as `"ethereum"`.

---

## 3. Source Token Address Formats by Chain

### 3.1 EVM Chains (Ethereum, Base, Polygon, Arbitrum, Optimism, Avalanche, BSC)

EVM token addresses are 20-byte hex strings prefixed with `0x`, checksummed
per [EIP-55](https://eips.ethereum.org/EIPS/eip-55). Vortex accepts both
checksummed and lowercase variants (the contract stores them as strings and
does not validate checksum on-chain; off-chain tooling should normalise to
checksummed form for human readability).

**Format:**
```
0x<40 hex digits>
```

**Example CLI usage:**
```bash
--src_chain '"ethereum"' \
--src_token '"0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"'
```

> Note the escaped inner quotes — the Stellar CLI requires string arguments to
> be wrapped in `'"…"'`.

### 3.2 Solana (Planned)

Solana token addresses are base58-encoded 32-byte public keys.

**Format:**
```
<base58-encoded mint address>
```

**Example:**
```
EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v   # USDC on Solana
```

---

## 4. Common Token Addresses by Chain

The table below lists the most commonly used source tokens. Always verify
addresses against the official project sources before using in production —
token contracts can be migrated or deprecated.

### Ethereum

| Token | Contract address | Decimals |
|---|---|---|
| WETH (Wrapped ETH) | `0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2` | 18 |
| USDC | `0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48` | 6 |
| USDT | `0xdAC17F958D2ee523a2206206994597C13D831ec7` | 6 |
| WBTC | `0x2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599` | 8 |
| DAI | `0x6B175474E89094C44Da98b954EedeAC495271d0F` | 18 |

### Base

| Token | Contract address | Decimals |
|---|---|---|
| WETH | `0x4200000000000000000000000000000000000006` | 18 |
| USDC | `0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913` | 6 |
| cbETH | `0x2Ae3F1Ec7F1F5012CFEab0185bfc7aa3cf0DEc22` | 18 |

### Polygon

| Token | Contract address | Decimals |
|---|---|---|
| WMATIC | `0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270` | 18 |
| USDC.e (bridged) | `0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174` | 6 |
| USDC (native) | `0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359` | 6 |
| WETH | `0x7ceB23fD6bC0adD59E62ac25578270cFf1b9f619` | 18 |

### Arbitrum One

| Token | Contract address | Decimals |
|---|---|---|
| WETH | `0x82aF49447D8a07e3bd95BD0d56f35241523fBab1` | 18 |
| USDC | `0xaf88d065e77c8cC2239327C5EDb3A432268e5831` | 6 |
| USDC.e (bridged) | `0xFF970A61A04b1cA14834A43f5dE4533eBDDB5CC8` | 6 |
| ARB | `0x912CE59144191C1204E64559FE8253a0e49E6548` | 18 |

### OP Mainnet (Optimism)

| Token | Contract address | Decimals |
|---|---|---|
| WETH | `0x4200000000000000000000000000000000000006` | 18 |
| USDC | `0x0b2C639c533813f4Aa9D7837CAf62653d097Ff85` | 6 |
| USDC.e (bridged) | `0x7F5c764cBc14f9669B88837ca1490cCa17c31607` | 6 |
| OP | `0x4200000000000000000000000000000000000042` | 18 |

### Avalanche C-Chain

| Token | Contract address | Decimals |
|---|---|---|
| WAVAX | `0xB31f66AA3C1e785363F0875A1B74E27b85FD66c7` | 18 |
| USDC | `0xB97EF9Ef8734C71904D8002F8b6Bc66Dd9c48a6E` | 6 |
| USDT.e | `0xc7198437980c041c805A1EDcbA50c1Ce5db95118` | 6 |

### BNB Smart Chain

| Token | Contract address | Decimals |
|---|---|---|
| WBNB | `0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c` | 18 |
| USDT | `0x55d398326f99059fF775485246999027B3197955` | 18 |
| USDC | `0x8AC76a51cc950d9822D68b83fE1Ad97B32Cd580d` | 18 |
| BTCB | `0x7130d2A12B9BCbFAe4f2634d864A1Ee1Ce3Ead9c` | 18 |

> **BSC stablecoin pitfall:** USDT and USDC on BSC use **18 decimals**, not 6.
> See [Decimal Normalization](../README.md#decimal-normalization-for-src_amount)
> in the README for the full worked-example table.

---

## 5. Allowlist Management

The contract's `src_chain` allowlist is off by default. To enforce it:

```bash
# Add each supported chain
stellar contract invoke --id <CONTRACT_ID> --source <ADMIN_SECRET> --network testnet -- \
  add_allowed_src_chain --chain '"ethereum"'

stellar contract invoke --id <CONTRACT_ID> --source <ADMIN_SECRET> --network testnet -- \
  add_allowed_src_chain --chain '"base"'

# (repeat for each chain in §2)

# Enable enforcement
stellar contract invoke --id <CONTRACT_ID> --source <ADMIN_SECRET> --network testnet -- \
  set_src_chain_allowlist_enabled --enabled true
```

To remove a chain (e.g., if it's deprecated):

```bash
stellar contract invoke --id <CONTRACT_ID> --source <ADMIN_SECRET> --network testnet -- \
  remove_allowed_src_chain --chain '"optimism"'
```

After removal, any new `submit_intent` call with `src_chain = "optimism"` will
fail with `Error::SrcChainNotAllowed`. Existing already-accepted intents are
unaffected.

---

## 6. Adding a New Chain

To add support for a new source chain:

1. Choose a lowercase `src_chain` string (e.g. `"scroll"`).
2. Identify its Wormhole chain ID (see [Wormhole chain IDs](https://docs.wormhole.com/wormhole/reference/constants)).
3. Add the mapping to the chain-ID lookup table in `fill_intent`'s proof
   validation block (see [#129](./129-proof-mismatch-fallback.md) §4).
4. Call `add_allowed_src_chain()` on the deployed contract.
5. Update this document with the new row in §2 and token addresses in §4.
6. Deploy and verify the source-chain `VortexDeposit` contract (see
   [#124](./124-proof-verification-interface.md) §5).

---

## 7. Relationship to Proof Verification

When `fill_intent` runs proof validation (Phase 2 and Phase 3 of the rollout
in [#124](./124-proof-verification-interface.md)), it maps `intent.src_chain`
to a Wormhole chain ID and compares it against `proof.src_chain_id`. The
mapping table lives in §4 of [129-proof-mismatch-fallback.md](./129-proof-mismatch-fallback.md)
and must be kept in sync with the canonical strings listed in §2 of this
document.

---

*Closes #132*
