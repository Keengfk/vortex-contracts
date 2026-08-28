#![no_std]

//! Vortex Protocol — Cross-Chain Intent Settlement
//!
//! Users submit swap intents (e.g. "swap 1 ETH on Ethereum for ~3500 USDC on Stellar").
//! Solvers compete to fill these intents off-chain, then settle on-chain via this contract.
//! Settlement is guaranteed by a solver bond; failing to fill within the deadline slashes the bond.

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, token, xdr::ToXdr,
    Address, Bytes, BytesN, Env, String, Symbol, Vec,
};

#[cfg(test)]
mod test;

#[cfg(test)]
mod proptest_bond;

// ─── Constants ────────────────────────────────────────────────────────────────

const INTENT_EXPIRY: u64 = 1800; // 30 minutes
const FILL_WINDOW: u64 = 300; // 5 minutes to fill after intent accepted
const MIN_BOND: i128 = 50 * 10_000_000; // 50 USDC minimum solver bond
const PROTOCOL_FEE_BPS: i128 = 5; // 0.05%
/// Duration of the competitive bid-collection window when bid-window mode is
/// enabled.  Solvers have this many seconds after `submit_intent` to submit
/// competing quotes via `bid_intent`; the best quote wins once the window
/// closes.
const BID_WINDOW: u64 = 120; // 2 minutes

/// Baseline slash rate in basis points (1 000 bps = 10%).
///
/// Issue #193: `slash_solver` no longer slashes a flat 10% of the bond.
/// Instead it slashes `min(intent_value, bond) / 10` — an amount proportional
/// to the size of the intent the solver failed to fill — and then *caps* the
/// result at `bond * SLASH_BPS / 10_000` so a slash is never more punitive
/// than the old flat-10% baseline for a well-matched bond-to-intent ratio.
/// The floor of 1 stroop (issue #32) is preserved so a non-zero bond is
/// always economically punished.
const SLASH_BPS: i128 = 1_000; // 10%

/// Issue #188 — dispute-resolution flow (docs/dispute-resolution-design.md).
///
/// `DISPUTE_WINDOW` is the period, starting at `begin_fill`, during which the
/// user may contest a fill via `dispute_fill`.  Output tokens sit in contract
/// escrow for its full duration; once it elapses with no dispute anyone may
/// call `release_fill` to pay the user and close the intent.
const DISPUTE_WINDOW: u64 = 3_600; // 1 hour

/// Issue #188 — after a dispute is raised the arbiter has this long to call
/// `resolve_dispute`.  If it elapses unresolved, `release_fill` becomes a
/// permissionless timeout that releases the escrow to the user (the
/// conservative default from the design doc) without slashing the solver.
const ARBITER_WINDOW: u64 = 86_400; // 24 hours

/// Issue #187 — a solver may hold bonds in at most this many distinct
/// approved tokens.  Bounds the work done by `deregister_solver` (which must
/// refund every token) and the storage cost of the per-token bond entries.
const MAX_BOND_TOKENS: u32 = 8;

/// Delay enforced between proposing and executing a sensitive admin change
/// (admin transfer, fee recipient handover, dst_token allowlist changes).
/// Gives users and solvers a window to notice and react before the change
/// takes effect (#115). Proposing also emits a distinct event immediately,
/// so off-chain monitors get advance notice even before the delay elapses
/// (#116).
const ADMIN_TIMELOCK_DELAY: u64 = 172_800; // 48 hours

// Upper sanity bound for src_amount and min_dst_amount.
//
// Largest realistic token amounts use 18-decimal ETH units.
// 1e12 tokens × 1e18 units/token = 1e30, well within i128 range (~1.7e38),
// but downstream arithmetic (fee = amount * 5 / 10_000) multiplies first and
// then divides. To guarantee `amount * PROTOCOL_FEE_BPS` never overflows i128,
// the bound is i128::MAX / PROTOCOL_FEE_BPS ≈ 3.4e37. We choose a round,
// economically implausible threshold: 10^30 (one trillion 18-decimal tokens).
// That is a comfortable safety margin while rejecting only fat-fingered inputs.
pub const MAX_AMOUNT: i128 = 1_000_000_000_000_000_000_000_000_000_000i128; // 10^30

// Soroban archives ledger entries that go too long without being touched.
// Persistent Intent/Solver records get their TTL bumped on every write so
// they don't need to be manually restored before later calls can read them.
const DAY_IN_LEDGERS: u32 = 17280; // ~5s per ledger
const PERSISTENT_TTL_THRESHOLD: u32 = DAY_IN_LEDGERS * 14;
const PERSISTENT_TTL_EXTEND_TO: u32 = DAY_IN_LEDGERS * 30;

// The contract instance entry (Admin/FeeRecipient/BondToken/TotalIntents/
// TotalVolume, plus the contract's own code) is a single ledger entry and
// needs the same treatment, or the whole contract becomes unreachable.
const INSTANCE_TTL_THRESHOLD: u32 = DAY_IN_LEDGERS * 30;
const INSTANCE_TTL_EXTEND_TO: u32 = DAY_IN_LEDGERS * 60;

// ─── Storage Keys ─────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// **Instance storage.** The admin `Address` that may call privileged
    /// functions (`pause`, `unpause`, `propose_fee_recipient`,
    /// `propose_admin_transfer`, `propose_add_dst_token`, etc.).  Written
    /// once by `initialize` and rotated by `accept_admin_transfer`.  Lives as
    /// long as the contract instance.
    Admin,

    /// **Instance storage.** The `Address` that receives protocol fees
    /// (collected in `fill_intent`) and slashed bond amounts (collected in
    /// `slash_solver`).  Written by `initialize` and updated by
    /// `set_fee_recipient`.  Lives as long as the contract instance.
    FeeRecipient,
    /// Proposed-but-not-yet-accepted new fee recipient plus the ledger
    /// timestamp at which `accept_fee_recipient` may execute it (issue #30,
    /// timelock added by #115): `(Address, u64)`.
    PendingFeeRecipient,
    BondToken,          // USDC address for bonds
    Intent(BytesN<32>), // intent_id -> IntentRecord
    Solver(Address),    // address -> SolverRecord
    TotalIntents,

    /// **Instance storage.** Count of intents currently in `Open` or
    /// `PartiallyFilled` state (`u64`).  Incremented by `submit_intent` and
    /// by `slash_solver` (which re-opens the intent).  Decremented by
    /// `accept_intent`, `cancel_intent`, `expire_intent`, and `fill_intent`
    /// (only on a full fill that closes the intent).
    ///
    /// Trade-off (#109): maintaining this counter on-chain costs one extra
    /// instance-storage read+write on every state-changing call but gives
    /// dashboards an O(1) open-intent count without replaying events.  The
    /// alternative — leaving the computation entirely to indexers — is cheaper
    /// on-chain but forces every dashboard to run a full event replay.  Given
    /// that the counter sits in instance storage (one ledger entry, already
    /// loaded on every call) the marginal cost is a single integer increment/
    /// decrement, which is negligible compared to the persistent-storage reads
    /// for `IntentRecord` and `SolverRecord`.
    OpenIntents,

    /// **Instance storage.** Cumulative `dst_token` volume (`i128`) across
    /// all successfully filled intents.  Incremented by `fill_intent`.
    TotalVolume,

    /// **Instance storage.** Count of currently registered solvers (`u32`).
    /// Incremented by `register_solver` on first registration, decremented
    /// by `deregister_solver`.
    TotalSolvers,

    /// **Instance storage.** Boolean flag (`true` = paused).  Set by
    /// `pause()` and cleared by `unpause()`.  When `true`,
    /// `submit_intent`, `accept_intent`, and `fill_intent` reject all
    /// calls.  Absent until first `pause()` call (defaults to `false`).
    Paused,

    /// **Instance storage.** Presence-flag (value `true`) indicating that
    /// `token` is on the allowed-destination list.  Added by
    /// `add_allowed_dst_token` and removed by `remove_allowed_dst_token`.
    /// Only checked by `submit_intent` when `DstAllowlistEnabled` is `true`.
    AllowedDstToken(Address),

    /// **Instance storage.** Boolean toggle (`true` = enforced).  Set via
    /// `set_dst_allowlist_enabled`.  When `false` (the default), the
    /// `AllowedDstToken` list is populated but not enforced by
    /// `submit_intent`, letting an admin pre-populate the list before
    /// switching enforcement on.
    DstAllowlistEnabled,
    UserNonce(Address),       // per-user submit counter to widen intent_id preimage
    AllowedSrcChain(String), // src_chain name -> present if allowed
    SrcChainAllowlistEnabled,

    /// **Instance storage.** The `Address` authorized to call `pause` in
    /// addition to `Admin` (issue #120). Lets an operator hand a hot key to
    /// an incident-response process without exposing the admin key that
    /// also controls fee routing and admin transfer. Absent until the admin
    /// calls `set_pauser`, in which case `pause` remains admin-only.
    /// `unpause` is intentionally *not* reachable via this role (narrow
    /// unpause access) -- resuming the protocol always needs the full
    /// admin's judgment.
    Pauser,

    /// **Instance storage.** Boolean toggle (`true` = bid-window mode active).
    /// Issue #191: replaces the previous placeholder that reused
    /// `DstAllowlistEnabled`.  When `true`, `submit_intent` opens new intents
    /// in `Bidding` state and solvers compete via `bid_intent` for `BID_WINDOW`
    /// seconds before `settle_bids` assigns the winner.  Set via
    /// `set_bid_window_enabled`; defaults to `false` so first-accept-wins
    /// behaviour is preserved on every deployment that predates the feature.
    BidWindowEnabled,

    /// **Persistent storage.** `intent_id` -> `BestBidRecord`.  Issue #191:
    /// the current leading bid for an intent in `Bidding` state.  Written by
    /// `bid_intent` (only when the new quote is strictly higher), read and
    /// removed by `settle_bids`.  Absent when no solver has bid yet.
    BestBid(BytesN<32>),

    /// **Instance storage.** The `Address` authorized to call `resolve_dispute`
    /// (issue #188).  Absent until `set_arbiter` is called, in which case the
    /// `Admin` address acts as arbiter (the design doc's v1 default).
    Arbiter,

    /// **Instance storage.** Presence-flag (value `true`) marking `token` as an
    /// approved solver-bond token (issue #187, docs/60-multi-bond-token-design.md).
    /// Added by `add_allowed_bond_token`, removed by `remove_allowed_bond_token`.
    /// The original `BondToken` from `initialize` is always treated as approved
    /// even without an explicit entry (migration safety).
    AllowedBondToken(Address),

    /// **Persistent storage.** `(solver, token)` -> `i128` bond balance
    /// (issue #187).  The per-token replacement for the single
    /// `SolverRecord.bond_amount` scalar.  For the legacy default bond token
    /// the value is kept mirrored in `SolverRecord.bond_amount` so pre-#187
    /// reads keep working; for every other approved token this key is the sole
    /// record of the balance.
    SolverBond(Address, Address),

    /// **Instance storage.** `token` -> `i128` minimum bond for that bond token
    /// (issue #187).  Falls back to the effective `min_bond` from
    /// `ProtocolConfig` for the legacy default bond token, and to
    /// `MIN_BOND` for any other token with no explicit entry.
    MinBond(Address),
}

// ─── Data Structs ─────────────────────────────────────────────────────────────

/// Admin-configurable protocol parameters.  Stored as a single instance-storage
/// entry so all four values are read/written atomically.
#[contracttype]
#[derive(Clone)]
pub struct ProtocolConfig {
    /// Minimum solver bond in bond_token's smallest unit.
    pub min_bond: i128,
    /// Seconds a solver has to fill after accepting an intent.
    pub fill_window: u64,
    /// Default intent lifetime in seconds (used when submit_intent deadline is None).
    pub intent_expiry: u64,
    /// Protocol fee in basis points charged on each fill (0.01% per bps).
    pub protocol_fee_bps: i128,
}

/// A user's cross-chain swap intent
#[contracttype]
#[derive(Clone)]
pub struct IntentRecord {
    pub intent_id: BytesN<32>,
    pub user: Address,

    /// Source chain details (off-chain reference)
    pub src_chain: String, // "ethereum" | "base" | "polygon" etc.
    pub src_token: String, // token address on source chain
    pub src_amount: i128,  // amount in source token's smallest unit

    /// Destination (always Stellar)
    pub dst_token: Address, // SAC/SEP-41 token on Stellar
    pub min_dst_amount: i128, // minimum acceptable output per fill (floor per partial)

    pub solver: Option<Address>, // assigned solver
    pub state: IntentState,

    pub created_at: u64,
    pub deadline: u64,
    pub filled_at: Option<u64>,
    pub fill_amount: Option<i128>, // cumulative dst tokens received across all fills

    /// Issue #187: the approved bond token that backs this intent's fill
    /// guarantee.  Set to the solver's chosen token in `accept_intent` /
    /// `settle_bids` and consulted by `slash_solver` so the slash is taken
    /// from — and paid out in — the same token the solver actually bonded.
    /// Defaults to the legacy `DataKey::BondToken` for intents that never
    /// reach an `Accepted`-family state.
    pub bond_token: Address,

    /// Issue #188: end of the escrow/dispute window, set by `begin_fill`.
    /// `None` for intents that took the legacy one-shot `fill_intent` path.
    pub dispute_deadline: Option<u64>,
    /// Issue #188: timestamp `dispute_fill` was called, if a dispute is open.
    pub dispute_raised_at: Option<u64>,
    /// Issue #188: the arbiter's decision once `resolve_dispute` has run.
    pub resolution: Option<DisputeResolution>,

    /// Cumulative dst tokens delivered so far; intent completes when this
    /// reaches or exceeds `min_dst_amount * num_fills_needed`, but in the
    /// partial-fill model the intent is fully settled once the solver
    /// delivering a fill brings `total_filled` to at least `min_dst_amount`.
    ///
    /// More precisely: each individual partial fill must be > 0, and the
    /// intent transitions to `Filled` as soon as `total_filled` satisfies
    /// the user's `min_dst_amount` requirement.
    pub total_filled: i128,
}

#[contracttype]
#[derive(Clone, PartialEq, Debug)]
pub enum IntentState {
    Open,            // awaiting solver
    Accepted,        // solver claimed it
    PartiallyFilled, // one or more partial fills delivered; still open for more
    Filled,          // user received total output >= min_dst_amount
    Cancelled,       // user cancelled before fill
    Expired,         // deadline passed, no fill
    Slashed,         // solver failed to fill after accepting
    /// Bid-window mode: intent has been submitted and is collecting competing
    /// solver bids.  No solver has exclusive fill rights yet.  Once the
    /// `BID_WINDOW` elapses the best bid is settled and the intent transitions
    /// to `Accepted`.
    Bidding,
    /// Issue #188: solver has called `begin_fill`; the output tokens are held
    /// in contract escrow and the user has until `dispute_deadline` to contest
    /// via `dispute_fill`.  `release_fill` moves this to `Filled` once the
    /// window closes without a dispute.
    Filling,
    /// Issue #188: the user contested the fill during the dispute window.
    /// Escrow is frozen until the arbiter calls `resolve_dispute` (or the
    /// `ARBITER_WINDOW` timeout releases it to the user).
    Disputed,
    /// Issue #188: the arbiter (or the arbiter-timeout) closed a dispute.
    /// The outcome is recorded in `IntentRecord.resolution`.
    Resolved,
}

/// Issue #188: the arbiter's ruling on a disputed fill.
/// docs/dispute-resolution-design.md — in both outcomes the user receives the
/// escrowed tokens; the ruling only decides whether the solver is slashed.
#[contracttype]
#[derive(Clone, PartialEq, Debug)]
pub enum DisputeResolution {
    /// Arbiter sided with the user: escrow goes to the user and the solver's
    /// bond is slashed by the same proportional formula `slash_solver` uses.
    Upheld,
    /// Arbiter sided with the solver: escrow still goes to the user (the fill
    /// was delivered) but no slash is applied and the protocol fee is taken.
    Dismissed,
}

/// A registered solver (market maker)
#[contracttype]
#[derive(Clone)]
pub struct SolverRecord {
    pub address: Address,
    /// Legacy scalar bond, denominated in the original `DataKey::BondToken`.
    ///
    /// Issue #187 introduced per-token bonds stored under
    /// `DataKey::SolverBond(solver, token)`.  This field is retained as the
    /// mirror of that entry *for the default bond token only*, so every
    /// pre-#187 reader (`get_solver`, `is_solver_eligible`, the bond-conservation
    /// proptest) keeps working unchanged.  Bonds in any other approved token
    /// live solely in `SolverBond` and are enumerated via `bond_tokens`.
    pub bond_amount: i128,
    pub fills_completed: u32,
    pub fills_failed: u32,
    pub total_volume: i128,
    pub is_active: bool,
    pub registered_at: u64,
    /// Number of intents currently Accepted by this solver (not yet filled or slashed).
    /// Bond stays locked behind these obligations, so it must be zero before deregistration.
    pub active_intents: u32,
    /// Timestamp of last slash; cooldown applies after a slash.
    pub last_slash_time: u64,
    /// Issue #187: every approved token this solver currently holds a non-zero
    /// bond in, including the default token.  Bounded by `MAX_BOND_TOKENS`.
    /// `deregister_solver` walks this list to refund every token in one call.
    pub bond_tokens: Vec<Address>,
}

/// Return type for `get_protocol_params`.
/// Exposes the four effective protocol values as named fields so integrators
/// don't have to rely on source-code comments for the constant definitions.
#[contracttype]
#[derive(Clone)]
pub struct ProtocolParams {
    /// Minimum USDC bond (in token's smallest unit) a solver must hold.
    pub min_bond: i128,
    /// Seconds a solver has to fill an intent after accepting it.
    pub fill_window: u64,
    /// Default intent lifetime in seconds (when no explicit deadline is passed).
    pub intent_expiry: u64,
    /// Protocol fee charged on each fill, in basis points (1 bps = 0.01%).
    pub protocol_fee_bps: i128,
}

/// Tracks the leading bid for an intent that is in the `Bidding` state.
/// Only the current best bid is kept — a new submission replaces it only
/// if it quotes a strictly higher `quoted_dst_amount`.
#[contracttype]
#[derive(Clone)]
pub struct BestBidRecord {
    pub solver: Address,
    pub quoted_dst_amount: i128,
}

/// Aggregate protocol-wide health snapshot, returned by `get_protocol_health`.
/// Bundles the fields that previously required three separate calls
/// (`is_paused`, `get_stats`, `get_solver_count`) into one, so
/// dashboard/monitoring integrations need a single round-trip.
#[contracttype]
#[derive(Clone)]
pub struct ProtocolHealth {
    /// Mirrors `is_paused()` — true when submit/accept/fill are halted.
    pub paused: bool,
    /// Mirrors `get_stats().0` — total intents ever submitted.
    pub total_intents: u64,
    /// Mirrors `get_stats().1` — cumulative dst_token volume across all fills.
    pub total_volume: i128,
    /// Mirrors `get_solver_count()` — currently registered solvers.
    pub total_solvers: u32,
}

// ─── Errors ───────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Error {
    /// `initialize` was called on a contract that already has an `Admin` key
    /// in instance storage. Raised exclusively by `initialize`.
    AlreadyInitialized = 1,

    /// A privileged operation was attempted by a caller who is not the
    /// required authority.  Raised by `fill_intent` when the caller is not
    /// the solver that accepted the intent, and by `cancel_intent` when the
    /// caller is not the intent's owner.
    Unauthorized = 2,

    /// The supplied `intent_id` has no corresponding `IntentRecord` in
    /// persistent storage.  Raised by `accept_intent`, `fill_intent`,
    /// `cancel_intent`, `slash_solver`, and `expire_intent`.
    IntentNotFound = 3,

    /// The intent's `state` is not `Open` at a point where `Open` is
    /// required.  Raised by `cancel_intent` (non-`Open`/non-`Accepted`
    /// guard) and by `expire_intent` (which only operates on `Open` intents).
    IntentNotOpen = 4,

    /// The current ledger timestamp has reached or passed the intent's
    /// `deadline` when a solver tries to accept it via `accept_intent`.
    /// The intent's state is lazily updated to `Expired` before the panic.
    IntentExpired = 5,

    /// `fill_intent` or `slash_solver` requires the intent to be in state
    /// `Accepted`, but it is in a different terminal or intermediate state.
    /// Also raised by `slash_solver` when `intent.state != Accepted`.
    IntentNotAccepted = 6,

    /// An operation that requires a registered solver (e.g. `deregister_solver`,
    /// `withdraw_bond`, `accept_intent`) was called for an address that has no
    /// `SolverRecord` in persistent storage.
    SolverNotRegistered = 7,

    /// `register_solver` was called with a `bond_amount` that, when added to
    /// any existing bond, does not reach `MIN_BOND` (500_000_000 stroops /
    /// 50 USDC).  Also raised by `withdraw_bond` when the post-withdrawal
    /// balance would fall below `MIN_BOND`.
    SolverBondTooLow = 8,

    /// `fill_intent` was called with a `fill_amount` less than the intent's
    /// `min_dst_amount`.  Raised only in `fill_intent`.
    InsufficientOutput = 9,

    /// `fill_intent` was called after the intent's `deadline` (i.e. the fill
    /// window that starts when the solver calls `accept_intent` and lasts
    /// `FILL_WINDOW` seconds) has already elapsed.  Also (confusingly) used
    /// in `slash_solver` as a guard label when the fill window has *not yet*
    /// expired — the intent cannot be slashed before its deadline.
    FillWindowExpired = 10,

    /// `cancel_intent` was called on an intent in state `Accepted`.  Users
    /// may only cancel `Open` intents; once a solver has accepted, the
    /// `slash_solver` path must be used if the solver fails to fill.
    CannotCancelAccepted = 11,

    /// `accept_intent` was called for a solver whose `is_active` flag is
    /// `false` (set when the bond falls below `MIN_BOND` after a slash, or
    /// after calling `deregister_solver`).
    SolverInactive = 12,

    /// A numeric input that must be strictly positive was zero or negative.
    /// Raised by `submit_intent` (`src_amount` or `min_dst_amount ≤ 0`) and
    /// by `register_solver` / `withdraw_bond` (`bond_amount ≤ 0`).
    ZeroAmount = 13,

    /// `submit_intent` was called with a `deadline` that is already in the
    /// past (i.e. `deadline ≤ env.ledger().timestamp()`).
    InvalidDeadline = 14,

    /// `fill_intent` was called on an intent that is already in state
    /// `Filled`.
    IntentAlreadyFilled = 15,

    /// An operation that requires the contract to be initialized (i.e. needs
    /// `Admin` in instance storage) was called before `initialize`.  Raised
    /// by `require_admin` and by `propose_fee_recipient` /
    /// `propose_admin_transfer`.
    NotInitialized = 16,

    /// `deregister_solver` was called while the solver's `active_intents`
    /// counter is greater than zero, meaning at least one intent is currently
    /// in state `Accepted` by this solver.  The solver must wait for those
    /// intents to reach a terminal state first.
    SolverHasActiveIntents = 17,

    /// `submit_intent`, `accept_intent`, or `fill_intent` was called while
    /// the contract's `Paused` flag is `true`.  Raised by
    /// `require_not_paused`.
    ContractPaused = 18,

    /// `expire_intent` was called before the intent's `deadline` has been
    /// reached (i.e. `env.ledger().timestamp() < intent.deadline`).
    DeadlineNotReached = 19,

    /// `withdraw_bond` was called with an `amount` greater than the solver's
    /// current `bond_amount`.
    InsufficientBond = 20,

    /// `submit_intent` was called with a `dst_token` that is not present in
    /// the `AllowedDstToken` allowlist while `DstAllowlistEnabled` is `true`.
    DstTokenNotAllowed = 21,

    /// Duplicate `intent_id` detected in `submit_intent` (hash collision guard).
    IntentAlreadyExists = 22,
    /// #30: no pending fee-recipient proposal to accept
    NoPendingFeeRecipient = 22,
    /// #31: fee arithmetic overflowed (fill_amount is astronomically large)
    FeeOverflow = 23,
    /// #33: the address passed to add_allowed_dst_token doesn't implement SEP-41
    InvalidTokenInterface = 24,
    SrcChainNotAllowed = 22,
    RescueProtectedToken = 23,
    /// #127: `submit_intent` was called with a `src_token` whose format does
    /// not match the conventions of the declared `src_chain`.
    ///
    /// EVM chains (ethereum, base, polygon, arbitrum, optimism): expect a
    /// `0x`-prefixed 42-character hex string (e.g. `"0xA0b86991…"`).
    ///
    /// Solana: expects a base58 string between 32 and 44 characters long with
    /// no `0x` prefix.
    ///
    /// If `src_chain` is unknown this error is never raised — unknown chains
    /// bypass token-format validation so the allowlist remains the sole gate.
    InvalidSrcToken = 28,

    // ── Issue #191 — competitive bid window ──────────────────────────────────

    /// `bid_intent` was called on an intent whose state is not `Bidding`.
    IntentNotBidding = 30,
    /// `bid_intent` was called after the bid window (`intent.deadline`) closed.
    BidWindowClosed = 31,
    /// `bid_intent`'s `quoted_dst_amount` did not strictly exceed the current
    /// `BestBidRecord.quoted_dst_amount`.
    BidNotHigher = 32,
    /// `settle_bids` was called before the bid window closed.
    BidWindowStillOpen = 33,

    // ── Issue #188 — dispute-resolution flow ────────────────────────────────

    /// `begin_fill` requires the intent to be in state `Accepted`.
    IntentNotAcceptedForFill = 34,
    /// `dispute_fill` / `release_fill` requires state `Filling`.
    IntentNotFilling = 35,
    /// `dispute_fill` was called after `dispute_deadline` elapsed.
    DisputeWindowClosed = 36,
    /// `resolve_dispute` requires state `Disputed`.
    IntentNotDisputed = 37,
    /// `release_fill` was called while the escrow/dispute window is still open
    /// and no dispute has been raised.
    DisputeWindowStillOpen = 38,
    /// `resolve_dispute` was called by an address that is neither the
    /// configured `Arbiter` nor the `Admin`.
    NotArbiter = 39,

    // ── Issue #187 — multi-bond-token support ───────────────────────────────

    /// `register_solver` / `accept_intent` was given a bond token that is not
    /// in the `AllowedBondToken` set (and is not the legacy default token).
    BondTokenNotAllowed = 40,
    /// `register_solver` would push the solver past `MAX_BOND_TOKENS` distinct
    /// bond tokens.
    TooManyBondTokens = 41,
}

// ─── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct IntentSettlement;

#[contractimpl]
impl IntentSettlement {
    // ── Initialization ────────────────────────────────────────────────────────

    /// One-time contract setup. Records the `admin`, `fee_recipient`, and
    /// `bond_token` (USDC) addresses, seeds protocol stats to zero, writes the
    /// default `ProtocolConfig`, and extends the instance TTL.
    /// Panics with `AlreadyInitialized` if called a second time.
    pub fn initialize(env: Env, admin: Address, fee_recipient: Address, bond_token: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic_with_error!(&env, Error::AlreadyInitialized);
        }
        // Auth audit: require_auth() is correct here. `admin` must sign the
        // initialization tx to prove ownership of the address being recorded as
        // admin. require_auth_for_args is not needed because there are no
        // separate per-argument capabilities to scope — the signer IS the admin.
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::FeeRecipient, &fee_recipient);
        env.storage()
            .instance()
            .set(&DataKey::BondToken, &bond_token);
        env.storage().instance().set(&DataKey::TotalIntents, &0u64);
        env.storage().instance().set(&DataKey::TotalVolume, &0i128);
        env.storage().instance().set(&DataKey::TotalSolvers, &0u32);
        env.storage().instance().set(&DataKey::OpenIntents, &0u64);
        // Seed Config with defaults so the contract is immediately usable
        // without a follow-up admin call.
        env.storage().instance().set(
            &DataKey::Config,
            &ProtocolConfig {
                min_bond: DEFAULT_MIN_BOND,
                fill_window: DEFAULT_FILL_WINDOW,
                intent_expiry: DEFAULT_INTENT_EXPIRY,
                protocol_fee_bps: DEFAULT_PROTOCOL_FEE_BPS,
            },
        );
        Self::bump_instance_ttl(&env);
    }

    // ── Admin ──────────────────────────────────────────────────────────────────

    /// Admin-only: propose a new fee recipient address. The proposal is stored
    /// (with the ledger timestamp at which it becomes executable) but not yet
    /// active, and a `fee_recipient_proposed` event fires immediately so
    /// off-chain monitors have advance notice (#116). The new address must
    /// wait out the timelock and then call `accept_fee_recipient` to confirm,
    /// mirroring `transfer_admin`'s two-step pattern so a typo'd or
    /// unreachable address can never silently misroute protocol fees, and
    /// giving affected parties a window to react before it's live (#115).
    ///
    /// A new proposal overwrites any prior pending proposal (and resets the
    /// timelock), so the admin can correct a mistake before the recipient has
    /// accepted.
    pub fn propose_fee_recipient(env: Env, new_fee_recipient: Address) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NotInitialized));
        // Auth audit: require_auth() is correct. The stored admin address must
        // sign. require_auth_for_args would add no security here — there's no
        // meaningful sub-scope within "being admin".
        admin.require_auth();

        let eta = env.ledger().timestamp() + ADMIN_TIMELOCK_DELAY;
        env.storage()
            .instance()
            .set(&DataKey::PendingFeeRecipient, &(new_fee_recipient.clone(), eta));

        env.events().publish(
            (Symbol::new(&env, "fee_recipient_proposed"),),
            (new_fee_recipient, eta),
        );
    }

    /// The pending fee recipient confirms the handover once the timelock
    /// delay since `propose_fee_recipient` has elapsed. Until this is called
    /// the current fee recipient remains unchanged.
    pub fn accept_fee_recipient(env: Env, new_fee_recipient: Address) {
        let (pending, eta): (Address, u64) = env
            .storage()
            .instance()
            .get(&DataKey::PendingFeeRecipient)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NoPendingFeeRecipient));

        if pending != new_fee_recipient {
            panic_with_error!(&env, Error::Unauthorized);
        }
        if env.ledger().timestamp() < eta {
            panic_with_error!(&env, Error::TimelockNotElapsed);
        }
        new_fee_recipient.require_auth();

        env.storage()
            .instance()
            .set(&DataKey::FeeRecipient, &new_fee_recipient);
        env.storage()
            .instance()
            .remove(&DataKey::PendingFeeRecipient);

        env.events().publish(
            (Symbol::new(&env, "fee_recipient_updated"),),
            new_fee_recipient,
        );
    }

    /// Admin-only: propose transferring the admin role to a new address. A
    /// `admin_transfer_proposed` event fires immediately for off-chain
    /// monitors (#116); the transfer itself only takes effect once
    /// `new_admin` calls `accept_admin_transfer` after the timelock delay
    /// has elapsed (#115), so a typo'd address can't accidentally brick
    /// admin control and affected parties get advance notice.
    ///
    /// A new proposal overwrites any prior pending proposal (and resets the
    /// timelock).
    pub fn propose_admin_transfer(env: Env, new_admin: Address) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NotInitialized));
        // Auth audit: require_auth() is correct here — the stored admin
        // address must sign to propose handing off its own role.
        admin.require_auth();

        let eta = env.ledger().timestamp() + ADMIN_TIMELOCK_DELAY;
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &(new_admin.clone(), eta));

        env.events().publish(
            (Symbol::new(&env, "admin_transfer_proposed"),),
            (new_admin, eta),
        );
    }

    /// The pending new admin confirms the handover once the timelock delay
    /// since `propose_admin_transfer` has elapsed. Requiring the incoming
    /// admin's own signature prevents accidentally handing the role to a
    /// typo'd or uncontrolled address.
    pub fn accept_admin_transfer(env: Env, new_admin: Address) {
        let (pending, eta): (Address, u64) = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NoPendingAdminTransfer));

        if pending != new_admin {
            panic_with_error!(&env, Error::Unauthorized);
        }
        if env.ledger().timestamp() < eta {
            panic_with_error!(&env, Error::TimelockNotElapsed);
        }
        new_admin.require_auth();

        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);

        env.events()
            .publish((Symbol::new(&env, "admin_transferred"),), new_admin);
    }

    // ── Protocol Config ───────────────────────────────────────────────────────

    /// Read the effective protocol config.  Falls back to compile-time defaults
    /// for contracts that existed before this upgrade (upgrade safety).
    pub fn get_config(env: Env) -> ProtocolConfig {
        Self::load_config(&env)
    }

    /// Admin-only: update the four configurable protocol parameters atomically.
    ///
    /// Bounds (any violation returns `InvalidConfig`):
    /// * `protocol_fee_bps`  ≤ 1 000 (10%)
    /// * `fill_window`       ≥ 60 s
    /// * `intent_expiry`     ≥ 300 s and > fill_window
    /// * `min_bond`          ≥ 1 token unit (10_000_000 for 7-decimal USDC)
    pub fn set_config(
        env: Env,
        min_bond: i128,
        fill_window: u64,
        intent_expiry: u64,
        protocol_fee_bps: i128,
    ) {
        Self::require_admin(&env);

        if !(0..=MAX_PROTOCOL_FEE_BPS).contains(&protocol_fee_bps) {
            panic_with_error!(&env, Error::InvalidConfig);
        }
        if fill_window < MIN_FILL_WINDOW_SECS {
            panic_with_error!(&env, Error::InvalidConfig);
        }
        if intent_expiry < MIN_INTENT_EXPIRY_SECS || intent_expiry <= fill_window {
            panic_with_error!(&env, Error::InvalidConfig);
        }
        if min_bond < MIN_BOND_FLOOR {
            panic_with_error!(&env, Error::InvalidConfig);
        }

        let cfg = ProtocolConfig {
            min_bond,
            fill_window,
            intent_expiry,
            protocol_fee_bps,
        };
        env.storage().instance().set(&DataKey::Config, &cfg);
        Self::bump_instance_ttl(&env);

        env.events().publish(
            (Symbol::new(&env, "config_updated"),),
            (min_bond, fill_window, intent_expiry, protocol_fee_bps),
        );
    }

    // ── Destination Token Allowlist ───────────────────────────────────────────

    /// Admin-only: propose allowing a dst_token to be targeted by new
    /// intents. submit_intent had no validation on dst_token at all --
    /// any address, including a bogus or malicious "token" contract, could
    /// be named as the destination.
    ///
    /// We call `decimals()` on the candidate address as a lightweight SEP-41
    /// interface probe (issue #33) at proposal time. If the address doesn't
    /// implement the token interface the call traps and the transaction
    /// reverts, surfacing the error at admin time rather than silently
    /// storing a proposal that would only fail later.
    ///
    /// This only records the proposal and fires a `dst_token_add_proposed`
    /// event for off-chain monitors (#116); the token isn't actually
    /// allowed until `execute_add_dst_token` is called after the timelock
    /// delay has elapsed (#115, #118), giving users and solvers a window to
    /// notice and react before the allowlist changes.
    ///
    /// Note: `decimals()` is a read-only view, so this probe has no side
    /// effects on the token's state.
    pub fn propose_add_dst_token(env: Env, token: Address) {
        Self::require_admin(&env);

        // Probe the SEP-41 interface: if `token` isn't a real token contract
        // this will trap and revert the transaction before we store anything.
        let token_client = token::Client::new(&env, &token);
        // decimals() is a pure view with no side-effects; we discard the value.
        let _decimals = token_client.decimals();

        let eta = env.ledger().timestamp() + ADMIN_TIMELOCK_DELAY;
        env.storage()
            .instance()
            .set(&DataKey::PendingDstTokenAdd(token.clone()), &eta);

        env.events().publish(
            (Symbol::new(&env, "dst_token_add_proposed"),),
            (token, eta),
        );
    }

    /// Apply a previously proposed `propose_add_dst_token` once its timelock
    /// delay has elapsed. Callable by anyone -- the change was already
    /// authorized by the admin at proposal time, so there's nothing left to
    /// gate once the delay has passed.
    pub fn execute_add_dst_token(env: Env, token: Address) {
        let eta: u64 = env
            .storage()
            .instance()
            .get(&DataKey::PendingDstTokenAdd(token.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::NoPendingDstTokenChange));
        if env.ledger().timestamp() < eta {
            panic_with_error!(&env, Error::TimelockNotElapsed);
        }

        env.storage()
            .instance()
            .remove(&DataKey::PendingDstTokenAdd(token.clone()));
        env.storage()
            .instance()
            .set(&DataKey::AllowedDstToken(token.clone()), &true);
        Self::add_to_dst_token_list(&env, &token);

        env.events()
            .publish((Symbol::new(&env, "dst_token_allowed"),), token);
    }

    /// Admin-only: propose disallowing a dst_token. Fires a
    /// `dst_token_remove_proposed` event immediately (#116); the token stays
    /// allowed until `execute_remove_dst_token` is called after the timelock
    /// delay elapses (#115).
    pub fn propose_remove_dst_token(env: Env, token: Address) {
        Self::require_admin(&env);

        let eta = env.ledger().timestamp() + ADMIN_TIMELOCK_DELAY;
        env.storage()
            .instance()
            .set(&DataKey::PendingDstTokenRemove(token.clone()), &eta);

        env.events().publish(
            (Symbol::new(&env, "dst_token_remove_proposed"),),
            (token, eta),
        );
    }

    /// Apply a previously proposed `propose_remove_dst_token` once its
    /// timelock delay has elapsed. Callable by anyone, for the same reason
    /// as `execute_add_dst_token`.
    pub fn execute_remove_dst_token(env: Env, token: Address) {
        let eta: u64 = env
            .storage()
            .instance()
            .get(&DataKey::PendingDstTokenRemove(token.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::NoPendingDstTokenChange));
        if env.ledger().timestamp() < eta {
            panic_with_error!(&env, Error::TimelockNotElapsed);
        }

        env.storage()
            .instance()
            .remove(&DataKey::PendingDstTokenRemove(token.clone()));
        env.storage()
            .instance()
            .remove(&DataKey::AllowedDstToken(token.clone()));
        Self::remove_from_dst_token_list(&env, &token);

        env.events()
            .publish((Symbol::new(&env, "dst_token_disallowed"),), token);
    }

    /// Returns `true` if `token` is on the dst_token allowlist.
    /// Does not check whether allowlist enforcement is currently active.
    pub fn is_dst_token_allowed(env: Env, token: Address) -> bool {
        env.storage()
            .instance()
            .has(&DataKey::AllowedDstToken(token))
    }

    /// Admin-only: turn allowlist enforcement in submit_intent on/off.
    /// Off by default -- an admin opts in once they've populated the list
    /// via add_allowed_dst_token, rather than every intent submission
    /// suddenly requiring one.
    ///
    /// Issue #119: emits an event like every other admin toggle in this
    /// contract (pause/unpause, fee_recipient_updated, admin_transferred),
    /// so off-chain indexers can observe enforcement flips without polling
    /// `is_dst_allowlist_enabled`.
    pub fn set_dst_allowlist_enabled(env: Env, enabled: bool) {
        Self::require_admin(&env);
        env.storage()
            .instance()
            .set(&DataKey::DstAllowlistEnabled, &enabled);
        env.events()
            .publish((Symbol::new(&env, "dst_allowlist_enabled"),), enabled);
    }

    /// Returns `true` if the dst_token allowlist is currently being enforced
    /// by `submit_intent`. Defaults to `false` on a fresh deployment.
    pub fn is_dst_allowlist_enabled(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::DstAllowlistEnabled)
            .unwrap_or(false)
    }

    /// List every dst_token currently present in the allowlist (#117).
    /// `is_dst_token_allowed` only answers one-token-at-a-time queries; this
    /// gives integrators and auditors a complete picture without replaying
    /// every `dst_token_allowed` / `dst_token_disallowed` event. Returns an
    /// empty `Vec` if nothing has ever been allowed.
    pub fn list_allowed_dst_tokens(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::AllowedDstTokenList)
            .unwrap_or_else(|| Vec::new(&env))
    }

    // ── Per-Token Bond Multiplier ──────────────────────────────────────────────

    /// Admin-only: set a custom bond multiplier for a dst_token.
    /// Multiplier is stored as i128 where 10 = 1.0x, 15 = 1.5x, 20 = 2.0x.
    /// Unset tokens default to 10 (1.0x).
    pub fn set_min_bond_multiplier(env: Env, token: Address, multiplier: i128) {
        Self::require_admin(&env);
        if multiplier <= 0 {
            panic_with_error!(&env, Error::ZeroAmount);
        }
        env.storage()
            .persistent()
            .set(&DataKey::MinBondMultiplier(token.clone()), &multiplier);
        env.events().publish(
            (Symbol::new(&env, "bond_multiplier_set"),),
            (token, multiplier),
        );
    }

    /// Get the bond multiplier for a dst_token, or 10 (1.0x) if unset.
    pub fn get_min_bond_multiplier(env: Env, token: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::MinBondMultiplier(token))
            .unwrap_or(10)
    }

    // ── Source Chain Allowlist ────────────────────────────────────────────────

        /// Admin-only: add a chain name to the src_chain allowlist.
    ///
    /// Issue #34: submit_intent accepted src_chain as free-text with zero
    /// validation, so a typo ("etherium") or unsupported name would create an
    /// intent that solvers can never match. This allowlist mirrors the
    /// AllowedDstToken pattern: an admin populates the list, then enables
    /// enforcement via set_src_chain_allowlist_enabled.
    pub fn add_allowed_src_chain(env: Env, chain: String) {
        Self::require_admin(&env);
        env.storage()
            .instance()
            .set(&DataKey::AllowedSrcChain(chain.clone()), &true);
        env.events()
            .publish((Symbol::new(&env, "src_chain_allowed"),), chain);
    }

    /// Admin-only: remove a chain name from the src_chain allowlist.
    pub fn remove_allowed_src_chain(env: Env, chain: String) {
        Self::require_admin(&env);
        env.storage()
            .instance()
            .remove(&DataKey::AllowedSrcChain(chain.clone()));
        env.events()
            .publish((Symbol::new(&env, "src_chain_disallowed"),), chain);
    }

    /// Returns true if `chain` is on the allowlist.
    pub fn is_src_chain_allowed(env: Env, chain: String) -> bool {
        env.storage()
            .instance()
            .has(&DataKey::AllowedSrcChain(chain))
    }

    /// Admin-only: toggle src_chain validation in submit_intent.
    ///
    /// Defaults to false so existing deployments keep working until an admin
    /// has populated the list and is ready to enforce it. Set to true before
    /// mainnet launch after calling add_allowed_src_chain for every chain the
    /// protocol supports.
    pub fn set_src_chain_allowlist_enabled(env: Env, enabled: bool) {
        Self::require_admin(&env);
        env.storage()
            .instance()
            .set(&DataKey::SrcChainAllowlistEnabled, &enabled);
    }

    /// Whether src_chain validation is currently active.
    pub fn is_src_chain_allowlist_enabled(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::SrcChainAllowlistEnabled)
            .unwrap_or(false)
    }

    // ── Pause Control ─────────────────────────────────────────────────────────

    /// Admin-only: designate (or rotate) the address that may call `pause`
    /// in addition to the admin (issue #120). This lets incident response
    /// use a narrower-scoped hot key instead of the full admin key, which
    /// also controls fee routing and admin transfer. Calling this again
    /// with a new address replaces the previous pauser; there is no way to
    /// clear it back to "admin-only" other than pointing it at the admin's
    /// own address.
    pub fn set_pauser(env: Env, pauser: Address) {
        Self::require_admin(&env);
        env.storage().instance().set(&DataKey::Pauser, &pauser);
        env.events()
            .publish((Symbol::new(&env, "pauser_updated"),), pauser);
    }

    /// The current pauser address, if the admin has set one.
    pub fn get_pauser(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Pauser)
    }

    /// Admin- or pauser-only: halt new intent submission, acceptance, and
    /// fills for incident response. slash_solver stays permissionless
    /// throughout, so a solver already holding an Accepted intent can't
    /// dodge accountability by waiting out the pause.
    ///
    /// Issue #36 — pause scope decision: register_solver, deregister_solver,
    /// and withdraw_bond are also gated here. During a live incident an admin
    /// may need to freeze the entire protocol state to investigate; allowing
    /// solvers to withdraw their bonds mid-incident would let them shed
    /// collateral exactly when the protocol most needs it as a backstop.
    /// cancel_intent is intentionally left open so users can always reclaim
    /// their Open intents.
    ///
    /// Issue #120 — `caller` must be either the admin or the address set via
    /// `set_pauser`, so fast incident response doesn't require exposing the
    /// full admin key.
    pub fn pause(env: Env, caller: Address) {
        Self::require_admin_or_pauser(&env, &caller);
        env.storage().instance().set(&DataKey::Paused, &true);
        env.events().publish((Symbol::new(&env, "paused"),), true);
    }

    /// Admin-only: lift a pause and restore normal operation.
    ///
    /// Issue #120 — deliberately narrower than `pause`: the pauser role can
    /// freeze the protocol but cannot unfreeze it. Resuming money movement
    /// after an incident always needs the full admin's judgment, not just
    /// whoever holds the pause hot key.
    pub fn unpause(env: Env) {
        Self::require_admin(&env);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.events().publish((Symbol::new(&env, "paused"),), false);
    }

    /// Whether submit_intent/accept_intent/fill_intent and solver bond
    /// management are currently halted.
    pub fn is_paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    // ── Token Rescue ──────────────────────────────────────────────────────────

    /// Admin-only: recover SEP-41 tokens accidentally sent to the contract.
    ///
    /// Issue #35 — trust model: rescue is restricted to tokens that are
    /// neither the bond_token nor any token currently referenced by an active
    /// (Accepted) intent as its dst_token. This prevents the rescue path from
    /// being misused to drain live solver collateral or in-flight intent
    /// output from under active protocol participants.
    ///
    /// If you need to move bond_token you must wait until all active intents
    /// have settled (filled, slashed, or cancelled), then handle any
    /// accounting off-chain.
    pub fn rescue_tokens(env: Env, token: Address, to: Address, amount: i128) {
        Self::require_admin(&env);

        if amount <= 0 {
            panic_with_error!(&env, Error::ZeroAmount);
        }

        // Refuse to rescue the protocol's own bond/collateral token.
        let bond_token: Address = env
            .storage()
            .instance()
            .get(&DataKey::BondToken)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NotInitialized));
        if token == bond_token {
            panic_with_error!(&env, Error::RescueProtectedToken);
        }

        let client = token::Client::new(&env, &token);
        client.transfer(&env.current_contract_address(), &to, &amount);

        env.events()
            .publish((Symbol::new(&env, "tokens_rescued"), to), (token, amount));
    }

    // ── Solver Management ─────────────────────────────────────────────────────

    /// Solvers register by depositing a bond in the protocol's default bond
    /// token (USDC). Existing solvers may top up with any positive amount --
    /// the minimum is enforced on the resulting total, not on each individual
    /// deposit.
    ///
    /// Issue #187: this is now a thin wrapper over `register_solver_with_token`
    /// pinned to the legacy default token, kept so pre-#187 callers and tests
    /// work unchanged.
    pub fn register_solver(env: Env, solver: Address, bond_amount: i128) {
        let bond_token = Self::load_bond_token(&env);
        Self::register_solver_inner(env, solver, bond_token, bond_amount);
    }

    /// Issue #187: register (or top up) a solver bond in `bond_token`, which
    /// must be the legacy default token or on the `AllowedBondToken` set.
    /// Per-token minimums come from `min_bond_for_token`; a solver may hold
    /// bonds in up to `MAX_BOND_TOKENS` distinct tokens.
    pub fn register_solver_with_token(
        env: Env,
        solver: Address,
        bond_token: Address,
        bond_amount: i128,
    ) {
        Self::register_solver_inner(env, solver, bond_token, bond_amount);
    }

    fn register_solver_inner(env: Env, solver: Address, bond_token: Address, bond_amount: i128) {
        // Auth audit: require_auth() is correct. The solver must sign to
        // consent to locking their own funds as bond.
        solver.require_auth();
        Self::require_not_paused(&env);
        Self::bump_instance_ttl(&env);

        if bond_amount <= 0 {
            panic_with_error!(&env, Error::ZeroAmount);
        }

        if !Self::is_bond_token_allowed(&env, &bond_token) {
            panic_with_error!(&env, Error::BondTokenNotAllowed);
        }

        let existing: Option<SolverRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Solver(solver.clone()));

        let is_new_solver = existing.is_none();

        // ── Effects first (CEI) ──────────────────────────────────────────────
        // Build and persist the SolverRecord *before* pulling funds in so the
        // contract's storage is always consistent with what it holds.
        let mut record = match existing {
            Some(mut s) => {
                s.is_active = true;
                s
            }
            None => SolverRecord {
                address: solver.clone(),
                bond_amount: 0,
                fills_completed: 0,
                fills_failed: 0,
                total_volume: 0,
                is_active: true,
                registered_at: env.ledger().timestamp(),
                active_intents: 0,
                last_slash_time: 0,
                bond_tokens: Vec::new(&env),
            },
        };

        let existing_bond = Self::get_solver_bond_amount(&env, &record, &bond_token);
        let new_bond = existing_bond + bond_amount;
        if new_bond < Self::min_bond_for_token(&env, &bond_token) {
            panic_with_error!(&env, Error::SolverBondTooLow);
        }

        // Enforce the per-solver bond-token cap before adding a brand-new token.
        if existing_bond == 0 && record.bond_tokens.len() >= MAX_BOND_TOKENS {
            panic_with_error!(&env, Error::TooManyBondTokens);
        }

        Self::set_solver_bond_amount(&env, &mut record, &bond_token, new_bond);

        env.storage()
            .persistent()
            .set(&DataKey::Solver(solver.clone()), &record);
        Self::bump_solver_ttl(&env, &solver);

        if is_new_solver {
            let total: u32 = env
                .storage()
                .instance()
                .get(&DataKey::TotalSolvers)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::TotalSolvers, &(total + 1));
        }

        // ── Interaction: pull bond in ────────────────────────────────────────
        let client = token::Client::new(&env, &bond_token);
        client.transfer(&solver, &env.current_contract_address(), &bond_amount);

        env.events().publish(
            (Symbol::new(&env, "solver_registered"), solver),
            (bond_token, bond_amount),
        );
    }

    /// Solver voluntarily exits the protocol. Returns the full bond — in every
    /// token they hold one (issue #187) — to the solver and removes their
    /// record. Requires no active (Accepted) intents — use `slash_solver` to
    /// clear those first.
    pub fn deregister_solver(env: Env, solver: Address) {
        // Auth audit: require_auth() is correct. Only the solver themselves
        // may deregister and trigger bond return. require_auth_for_args is not
        // useful — the sole action is "deregister this exact address".
        solver.require_auth();
        Self::require_not_paused(&env);
        Self::bump_instance_ttl(&env);

        let record: SolverRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Solver(solver.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::SolverNotRegistered));

        if record.active_intents > 0 {
            panic_with_error!(&env, Error::SolverHasActiveIntents);
        }

        let default_token = Self::load_bond_token(&env);

        // Snapshot every (token, amount) pair to refund. The legacy default
        // token may not appear in `bond_tokens` for a pre-#187 record, so it is
        // handled explicitly.
        let mut refunds: Vec<(Address, i128)> = Vec::new(&env);
        if record.bond_amount > 0 {
            refunds.push_back((default_token.clone(), record.bond_amount));
        }
        for i in 0..record.bond_tokens.len() {
            let t = record.bond_tokens.get(i).unwrap();
            if t == default_token {
                continue; // already captured above
            }
            let amt: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::SolverBond(solver.clone(), t.clone()))
                .unwrap_or(0);
            if amt > 0 {
                refunds.push_back((t, amt));
            }
        }

        // ── Effects first (CEI) ──────────────────────────────────────────────
        // Remove all per-token entries and the record *before* the external
        // token transfers so that any re-entrant call sees no record and would
        // panic with SolverNotRegistered rather than processing a double-refund.
        for i in 0..refunds.len() {
            let (t, _) = refunds.get(i).unwrap();
            if t != default_token {
                env.storage()
                    .persistent()
                    .remove(&DataKey::SolverBond(solver.clone(), t));
            }
        }
        env.storage()
            .persistent()
            .remove(&DataKey::Solver(solver.clone()));

        let total: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TotalSolvers)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalSolvers, &total.saturating_sub(1));

        // ── Interaction: return every bond ──────────────────────────────────
        let mut total_default_refund = 0i128;
        for i in 0..refunds.len() {
            let (t, amt) = refunds.get(i).unwrap();
            token::Client::new(&env, &t).transfer(
                &env.current_contract_address(),
                &solver,
                &amt,
            );
            if t == default_token {
                total_default_refund = amt;
            }
        }

        env.events().publish(
            (Symbol::new(&env, "solver_deregistered"), solver),
            total_default_refund,
        );
    }

    /// Solver withdraws part of their default-token bond without fully
    /// deregistering. The remaining bond must still clear the minimum -- to go
    /// below that, use deregister_solver instead (which also requires no active
    /// intents).
    ///
    /// Issue #187: thin wrapper over `withdraw_bond_token` pinned to the legacy
    /// default token.
    pub fn withdraw_bond(env: Env, solver: Address, amount: i128) {
        let bond_token = Self::load_bond_token(&env);
        Self::withdraw_bond_inner(env, solver, bond_token, amount);
    }

    /// Issue #187: withdraw part of a solver's bond held in `bond_token`. The
    /// remaining balance in that token must still clear
    /// `min_bond_for_token(bond_token)`.
    pub fn withdraw_bond_token(env: Env, solver: Address, bond_token: Address, amount: i128) {
        Self::withdraw_bond_inner(env, solver, bond_token, amount);
    }

    fn withdraw_bond_inner(env: Env, solver: Address, bond_token: Address, amount: i128) {
        // Auth audit: require_auth() is correct. Only the solver may withdraw
        // their own bond.
        solver.require_auth();
        Self::require_not_paused(&env);
        Self::bump_instance_ttl(&env);

        if amount <= 0 {
            panic_with_error!(&env, Error::ZeroAmount);
        }

        let mut record: SolverRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Solver(solver.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::SolverNotRegistered));

        let current = Self::get_solver_bond_amount(&env, &record, &bond_token);
        if amount > current {
            panic_with_error!(&env, Error::InsufficientBond);
        }

        let remaining = current - amount;
        if remaining < Self::min_bond_for_token(&env, &bond_token) {
            panic_with_error!(&env, Error::SolverBondTooLow);
        }

        Self::set_solver_bond_amount(&env, &mut record, &bond_token, remaining);
        env.storage()
            .persistent()
            .set(&DataKey::Solver(solver.clone()), &record);
        Self::bump_solver_ttl(&env, &solver);

        let client = token::Client::new(&env, &bond_token);
        client.transfer(&env.current_contract_address(), &solver, &amount);

        // Issue #108: include the post-withdrawal remaining balance so indexers
        // can maintain a solver's bond ledger without a separate get_solver call.
        // data: (amount: i128, remaining: i128)
        env.events().publish(
            (Symbol::new(&env, "bond_withdrawn"), solver),
            (amount, remaining),
        );
    }

    // ── Intent Lifecycle ──────────────────────────────────────────────────────

    /// User submits a swap intent. No funds are locked on Stellar at this point —
    /// the user initiates the source-chain tx separately.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_intent(
        env: Env,
        user: Address,
        src_chain: String,
        src_token: String,
        src_amount: i128,
        dst_token: Address,
        min_dst_amount: i128,
        deadline: Option<u64>,
    ) -> BytesN<32> {
        // Auth audit: require_auth() is correct. The user must sign to assert
        // ownership of the address receiving output tokens (dst). If a third-party
        // contract were ever to call submit_intent on a user's behalf, switching to
        // require_auth_for_args scoped to (user, dst_token, min_dst_amount) would
        // limit the scope of delegated authorisation — noted as a future hardening
        // opportunity if composable intent submission is added.
        user.require_auth();
        Self::require_not_paused(&env);
        Self::bump_instance_ttl(&env);

        if src_amount <= 0 || min_dst_amount <= 0 {
            panic_with_error!(&env, Error::ZeroAmount);
        }

        if src_amount > MAX_AMOUNT || min_dst_amount > MAX_AMOUNT {
            panic_with_error!(&env, Error::AmountTooLarge);
        }

        if Self::is_dst_allowlist_enabled(env.clone())
            && !Self::is_dst_token_allowed(env.clone(), dst_token.clone())
        {
            panic_with_error!(&env, Error::DstTokenNotAllowed);
        }

        // #34 — validate src_chain when the allowlist is enabled.
        if Self::is_src_chain_allowlist_enabled(env.clone())
            && !Self::is_src_chain_allowed(env.clone(), src_chain.clone())
        {
            panic_with_error!(&env, Error::SrcChainNotAllowed);
        }

        // #127 — validate src_token address format against the declared chain's
        // conventions (EVM: 0x + 40 hex chars; Solana: base58 32–44 chars).
        // This runs even when the src_chain allowlist is disabled so obviously
        // malformed tokens are always caught at submission time.
        Self::validate_src_token(&env, &src_chain, &src_token);

        let now = env.ledger().timestamp();
        let cfg = Self::load_config(&env);
        let expiry = deadline.unwrap_or(now + cfg.intent_expiry);

        if expiry <= now {
            panic_with_error!(&env, Error::InvalidDeadline);
        }

        // Widen the preimage with a per-user nonce so that two intents from
        // the same user with identical (src_chain, src_amount) in the same
        // ledger close produce distinct ids rather than colliding silently.
        let nonce: u64 = env
            .storage()
            .instance()
            .get(&DataKey::UserNonce(user.clone()))
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::UserNonce(user.clone()), &(nonce + 1));

        // Deterministic intent_id = hash(user, src_chain, src_token, src_amount, now, nonce)
        let intent_id = Self::compute_intent_id(&env, &user, &src_chain, src_amount, now, nonce);

        // Guard against an extremely unlikely hash collision: if a record with
        // this id somehow already exists, reject rather than silently overwrite.
        if env
            .storage()
            .persistent()
            .has(&DataKey::Intent(intent_id.clone()))
        {
            panic_with_error!(&env, Error::IntentAlreadyExists);
        }

        let intent = IntentRecord {
            intent_id: intent_id.clone(),
            user: user.clone(),
            src_chain,
            src_token,
            src_amount,
            dst_token,
            min_dst_amount,
            solver: None,
            // When bid-window mode is active, the intent opens in Bidding state
            // so solvers can compete before one is assigned exclusive fill rights.
            // The bid-window deadline is BID_WINDOW seconds from now, not the
            // full intent expiry — settle_bids extends it to FILL_WINDOW once a
            // winner is picked.  The original expiry is stored separately in
            // deadline and reset after settlement.
            state: if Self::is_bid_window_enabled(env.clone()) {
                IntentState::Bidding
            } else {
                IntentState::Open
            },
            created_at: now,
            // In bidding mode, deadline tracks the end of the bid window.
            // In first-accept-wins mode, deadline tracks the intent expiry.
            deadline: if Self::is_bid_window_enabled(env.clone()) {
                now + BID_WINDOW
            } else {
                expiry
            },
            filled_at: None,
            fill_amount: None,
            total_filled: 0,
            // Issue #187: placeholder until a solver accepts and names the token
            // that backs their obligation. Defaults to the legacy bond token so
            // `slash_solver` has a valid target even on paths that skip accept.
            bond_token: Self::load_bond_token(&env),
            // Issue #188: no escrow/dispute state until begin_fill runs.
            dispute_deadline: None,
            dispute_raised_at: None,
            resolution: None,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Intent(intent_id.clone()), &intent);
        Self::bump_intent_ttl(&env, &intent_id);

        let mut user_intents: Vec<BytesN<32>> = env
            .storage()
            .persistent()
            .get(&DataKey::UserIntents(user.clone()))
            .unwrap_or_else(|| Vec::new(&env));
        user_intents.push_back(intent_id.clone());
        env.storage()
            .persistent()
            .set(&DataKey::UserIntents(user.clone()), &user_intents);

        let total: u64 = env
            .storage()
            .instance()
            .get(&DataKey::TotalIntents)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalIntents, &(total + 1));

        // Increment open_intents: every new submission starts as Open (or Bidding,
        // which also counts as an unfilled intent awaiting a solver).
        let open: u64 = env
            .storage()
            .instance()
            .get(&DataKey::OpenIntents)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::OpenIntents, &(open + 1));

        env.events().publish(
            (Symbol::new(&env, "intent_submitted"), user),
            (intent_id.clone(), min_dst_amount, expiry),
        );

        intent_id
    }

    /// Solver claims an intent (exclusive fill right for FILL_WINDOW seconds),
    /// backing the obligation with their default-token bond.
    ///
    /// Issue #187: thin wrapper over `accept_intent_with_bond` pinned to the
    /// legacy default bond token.
    pub fn accept_intent(env: Env, solver: Address, intent_id: BytesN<32>) {
        let bond_token = Self::load_bond_token(&env);
        Self::accept_intent_inner(env, solver, intent_id, bond_token);
    }

    /// Issue #187: claim an intent, backing it with the solver's bond in
    /// `bond_token`. The token is recorded on the intent so `slash_solver`
    /// takes the penalty from — and pays it out in — the same token.
    pub fn accept_intent_with_bond(
        env: Env,
        solver: Address,
        intent_id: BytesN<32>,
        bond_token: Address,
    ) {
        Self::accept_intent_inner(env, solver, intent_id, bond_token);
    }

    fn accept_intent_inner(
        env: Env,
        solver: Address,
        intent_id: BytesN<32>,
        bond_token: Address,
    ) {
        // Auth audit: require_auth() is correct. The solver must sign to
        // voluntarily take on the fill obligation and bond risk.
        solver.require_auth();
        Self::require_not_paused(&env);
        Self::bump_instance_ttl(&env);

        let mut solver_record: SolverRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Solver(solver.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::SolverNotRegistered));

        if !solver_record.is_active {
            panic_with_error!(&env, Error::SolverInactive);
        }

        let now = env.ledger().timestamp();
        if solver_record.last_slash_time > 0 && now < solver_record.last_slash_time + SLASH_COOLDOWN {
            panic_with_error!(&env, Error::SolverInactive);
        }

        if !Self::is_bond_token_allowed(&env, &bond_token) {
            panic_with_error!(&env, Error::BondTokenNotAllowed);
        }

        let mut intent: IntentRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Intent(intent_id.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::IntentNotFound));

        // The dst_token multiplier scales the *default*-token minimum; for any
        // other bond token we require at least that token's own minimum. This
        // keeps the #187 non-goal (no cross-token price comparison) intact.
        let required_bond = if bond_token == Self::load_bond_token(&env) {
            Self::get_adjusted_min_bond(&env, &intent.dst_token)
        } else {
            Self::min_bond_for_token(&env, &bond_token)
        };
        if Self::get_solver_bond_amount(&env, &solver_record, &bond_token) < required_bond {
            panic_with_error!(&env, Error::SolverBondTooLow);
        }

        // Boundary semantics: deadline is EXCLUSIVE for acceptance.
        // `now >= intent.deadline` rejects at the boundary second (`now == deadline`)
        // so the full [created_at, deadline) half-open window is available for solvers.
        if now >= intent.deadline {
            env.storage()
                .persistent()
                .set(&DataKey::Intent(intent_id.clone()), &intent);
            Self::bump_intent_ttl(&env, &intent_id);
            panic_with_error!(&env, Error::IntentExpired);
        }

        if intent.state != IntentState::Open && intent.state != IntentState::PartiallyFilled {
            panic_with_error!(&env, Error::IntentNotOpen);
        }

        intent.solver = Some(solver.clone());
        intent.state = IntentState::Accepted;
        intent.bond_token = bond_token.clone();
        // Extend deadline to fill window from now
        let cfg = Self::load_config(&env);
        intent.deadline = now + cfg.fill_window;

        solver_record.active_intents += 1;
        env.storage()
            .persistent()
            .set(&DataKey::Solver(solver.clone()), &solver_record);

        // Decrement open_intents: the intent is no longer open (a solver owns it).
        let open: u64 = env
            .storage()
            .instance()
            .get(&DataKey::OpenIntents)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::OpenIntents, &open.saturating_sub(1));

        env.storage()
            .persistent()
            .set(&DataKey::Intent(intent_id.clone()), &intent);
        Self::bump_intent_ttl(&env, &intent_id);

        env.events().publish(
            (Symbol::new(&env, "intent_accepted"), solver),
            (intent_id, intent.deadline),
        );
    }

    /// Solver fills the intent by sending dst_token to the user.
    ///
    /// Partial fills are supported: `fill_amount` must be > 0 but may be less
    /// than `min_dst_amount`.  The intent transitions to `PartiallyFilled` after
    /// each sub-fill and is re-opened so another solver (or the same one) can
    /// accept and deliver the remainder.  Once the cumulative `total_filled`
    /// reaches or exceeds `min_dst_amount` the intent transitions to `Filled`.
    ///
    /// The protocol fee is taken on each individual fill so the fee accounting
    /// stays consistent regardless of how many fills it takes.
    pub fn fill_intent(env: Env, solver: Address, intent_id: BytesN<32>, fill_amount: i128) {
        // Auth audit: require_auth() is correct. The solver must sign to
        // authorise the token transfer from their address to the user and fee
        // recipient. This is the highest-value call site: the solver authorises
        // a token transfer, so the auth is load-bearing. require_auth_for_args
        // scoped to (solver, intent_id, fill_amount) would meaningfully tighten
        // the scope if a delegated-execution pattern is ever introduced — noted
        // as the strongest candidate for future hardening.
        solver.require_auth();
        Self::require_not_paused(&env);
        Self::bump_instance_ttl(&env);

        let mut intent: IntentRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Intent(intent_id.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::IntentNotFound));

        let now = env.ledger().timestamp();
        // Boundary semantics: the fill-window deadline is EXCLUSIVE for filling.
        // `now >= intent.deadline` rejects at the boundary second (`now == deadline`)
        // so the full [accepted_at, accepted_at + FILL_WINDOW) window is available
        // to the solver.
        if now >= intent.deadline {
            panic_with_error!(&env, Error::FillWindowExpired);
        }

        match &intent.state {
            IntentState::Accepted => {}
            IntentState::Filled => panic_with_error!(&env, Error::IntentAlreadyFilled),
            _ => panic_with_error!(&env, Error::IntentNotAccepted),
        }

        if intent.solver.as_ref() != Some(&solver) {
            panic_with_error!(&env, Error::Unauthorized);
        }

        if fill_amount <= 0 {
            panic_with_error!(&env, Error::ZeroAmount);
        }

        // Protocol fee for this fill. Explicit checked_mul/checked_div makes the
        // overflow-safety property visible in code, rather than relying solely on
        // the Cargo.toml `overflow-checks = true` release-profile setting (#31).
        let fee = fill_amount
            .checked_mul(PROTOCOL_FEE_BPS)
            .unwrap_or_else(|| panic_with_error!(&env, Error::FeeOverflow))
            .checked_div(10_000)
            .unwrap_or_else(|| panic_with_error!(&env, Error::FeeOverflow));

        // ── Effects first (CEI) ──────────────────────────────────────────────
        // Mark every state change and write it to storage *before* any external
        // token transfer executes. A hostile SEP-41 token that tries to re-enter
        // fill_intent or slash_solver during the transfer sees the already-
        // updated intent state and is rejected by the guards above.
        intent.total_filled += fill_amount;
        let cumulative = intent.total_filled;
        intent.fill_amount = Some(cumulative);

        let mut solver_record: SolverRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Solver(solver.clone()))
            .unwrap();
        solver_record.total_volume += fill_amount;

        if cumulative >= intent.min_dst_amount {
            // Intent is fully satisfied — close it out. open_intents was already
            // decremented when the intent was accepted; no adjustment needed.
            intent.state = IntentState::Filled;
            intent.filled_at = Some(now);
            solver_record.fills_completed += 1;
            solver_record.active_intents = solver_record.active_intents.saturating_sub(1);
        } else {
            // Partial fill: re-open so another solver (or the same) can claim the
            // remaining amount. Reset solver assignment and deadline back to the
            // full intent expiry window; the intent is back in Open rotation, so
            // increment open_intents again.
            intent.state = IntentState::PartiallyFilled;
            intent.solver = None;
            intent.deadline = now + INTENT_EXPIRY;
            solver_record.active_intents = solver_record.active_intents.saturating_sub(1);

            let open: u64 = env
                .storage()
                .instance()
                .get(&DataKey::OpenIntents)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::OpenIntents, &(open + 1));
        }

        env.storage()
            .persistent()
            .set(&DataKey::Solver(solver.clone()), &solver_record);
        Self::bump_solver_ttl(&env, &solver);

        let total_vol: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalVolume)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalVolume, &(total_vol + fill_amount));

        env.storage()
            .persistent()
            .set(&DataKey::Intent(intent_id.clone()), &intent);
        Self::bump_intent_ttl(&env, &intent_id);

        // ── Interactions: token transfers ────────────────────────────────────
        // Solver delivers this fill's output to the user, then pays the protocol
        // fee (priced into their quote). Taking the fee from the solver — rather
        // than clawing it back from the user — keeps the user's received amount
        // at or above `min_dst_amount`, and keeps every transfer authorized by
        // the solver who signed this call.
        let dst_client = token::Client::new(&env, &intent.dst_token);
        dst_client.transfer(&solver, &intent.user, &fill_amount);
        if fee > 0 {
            let fee_recipient: Address = env
                .storage()
                .instance()
                .get(&DataKey::FeeRecipient)
                .unwrap();
            dst_client.transfer(&solver, &fee_recipient, &fee);
        }

        env.events().publish(
            (Symbol::new(&env, "intent_filled"), solver),
            (intent_id, fill_amount, fee),
        );
    }

    /// User can cancel an Open intent (not yet accepted)
    pub fn cancel_intent(env: Env, user: Address, intent_id: BytesN<32>) {
        // Auth audit: require_auth() is correct. Only the intent owner may
        // cancel. An additional ownership check (`intent.user != user`) follows
        // immediately after the intent is loaded, providing defence-in-depth.
        // require_auth_for_args is not needed here — the action is simply
        // "cancel intent for this user".
        user.require_auth();
        Self::bump_instance_ttl(&env);

        let now = env.ledger().timestamp();

        // Check cancellation cooldown for spam-deterrence
        if let Some(last_cancel_time) = env
            .storage()
            .persistent()
            .get::<_, u64>(&DataKey::CancelCooldown(user.clone()))
        {
            if now < last_cancel_time + CANCEL_COOLDOWN {
                panic_with_error!(&env, Error::CancelCooldownNotExpired);
            }
        }

        let mut intent: IntentRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Intent(intent_id.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::IntentNotFound));

        if intent.user != user {
            panic_with_error!(&env, Error::Unauthorized);
        }

        if intent.state == IntentState::Accepted {
            panic_with_error!(&env, Error::CannotCancelAccepted);
        }

        if intent.state != IntentState::Open && intent.state != IntentState::PartiallyFilled {
            panic_with_error!(&env, Error::IntentNotOpen);
        }

        intent.state = IntentState::Cancelled;
        env.storage()
            .persistent()
            .set(&DataKey::Intent(intent_id.clone()), &intent);
        Self::bump_intent_ttl(&env, &intent_id);

        // Decrement open_intents: intent is no longer open.
        let open: u64 = env
            .storage()
            .instance()
            .get(&DataKey::OpenIntents)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::OpenIntents, &open.saturating_sub(1));
        // Update cancellation cooldown
        env.storage()
            .persistent()
            .set(&DataKey::CancelCooldown(user.clone()), &now);

        env.events()
            .publish((Symbol::new(&env, "intent_cancelled"), user), intent_id);
    }

    /// Permissionless: slash a solver that accepted but didn't fill within FILL_WINDOW
    pub fn slash_solver(env: Env, intent_id: BytesN<32>) {
        Self::bump_instance_ttl(&env);

        let mut intent: IntentRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Intent(intent_id.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::IntentNotFound));

        let now = env.ledger().timestamp();

        if intent.state != IntentState::Accepted {
            panic_with_error!(&env, Error::IntentNotAccepted);
        }

        // Boundary semantics: the fill-window deadline is INCLUSIVE for slashing.
        // The guard `now < intent.deadline` is false when `now == deadline`, so
        // slashing becomes valid at the deadline second itself (not strictly after).
        // Fill window available to solver: [accepted_at, accepted_at + FILL_WINDOW).
        // Slash window: [accepted_at + FILL_WINDOW, ∞).
        if now < intent.deadline {
            panic_with_error!(&env, Error::FillWindowExpired); // not expired yet
        }

        let solver_addr = intent.solver.clone().unwrap();
        let mut solver_record: SolverRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Solver(solver_addr.clone()))
            .unwrap();

        let bond_token = intent.bond_token.clone();
        // Issue #193: proportional slash — the amount is a function of *both*
        // the solver's bond and the size of the intent they failed to fill
        // (`min_dst_amount` minus any partial progress), capped at the old flat
        // 10% baseline and floored at 1 stroop (issue #32).  See
        // `compute_slash_amount` for the formula and its edge-case proof.
        let unfilled = intent.min_dst_amount - intent.total_filled;
        let bond_before = Self::get_solver_bond_amount(&env, &solver_record, &bond_token);
        let slash_amount = Self::compute_slash_amount(bond_before, unfilled);
        Self::set_solver_bond_amount(
            &env,
            &mut solver_record,
            &bond_token,
            bond_before - slash_amount,
        );
        solver_record.fills_failed += 1;
        solver_record.last_slash_time = now;
        solver_record.active_intents = solver_record.active_intents.saturating_sub(1);

        let cfg = Self::load_config(&env);
        // A solver whose bond no longer covers the minimum for the token that
        // backed this intent can't credibly back further fills -- take them out
        // of rotation until they top back up.
        if Self::get_solver_bond_amount(&env, &solver_record, &bond_token)
            < Self::min_bond_for_token(&env, &bond_token)
        {
            solver_record.is_active = false;
        }

        // Re-open the intent, preserving partial-fill progress if any.
        // The intent transitions back to Open/PartiallyFilled, so increment open_intents.
        intent.state = if intent.total_filled > 0 {
            IntentState::PartiallyFilled
        } else {
            IntentState::Open
        };
        intent.solver = None;
        intent.deadline = now + cfg.intent_expiry;

        let open: u64 = env
            .storage()
            .instance()
            .get(&DataKey::OpenIntents)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::OpenIntents, &(open + 1));

        // Persist both records BEFORE any token transfer so that a re-entrant
        // or back-to-back call on the same intent_id is rejected by the
        // IntentNotAccepted guard above (the state is already Open by then).
        env.storage()
            .persistent()
            .set(&DataKey::Solver(solver_addr.clone()), &solver_record);
        Self::bump_solver_ttl(&env, &solver_addr);
        env.storage()
            .persistent()
            .set(&DataKey::Intent(intent_id.clone()), &intent);
        Self::bump_intent_ttl(&env, &intent_id);

        // Send slash to fee recipient, in the same token the solver bonded
        // (issue #187), with state already committed above.
        if slash_amount > 0 {
            let fee_recipient: Address = env
                .storage()
                .instance()
                .get(&DataKey::FeeRecipient)
                .unwrap();
            let client = token::Client::new(&env, &bond_token);
            client.transfer(
                &env.current_contract_address(),
                &fee_recipient,
                &slash_amount,
            );
        }

        env.events().publish(
            (Symbol::new(&env, "solver_slashed"), solver_addr),
            (intent_id, slash_amount),
        );
    }

    /// Permissionless: materialize an Open intent's Expired state once its
    /// deadline has passed. Expiry was previously only ever realized lazily
    /// inside accept_intent, so an intent nobody tried to accept could sit
    /// indefinitely showing state Open in storage despite being unfillable.
    pub fn expire_intent(env: Env, intent_id: BytesN<32>) {
        Self::bump_instance_ttl(&env);

        let mut intent: IntentRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Intent(intent_id.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::IntentNotFound));

        if intent.state != IntentState::Open && intent.state != IntentState::PartiallyFilled {
            panic_with_error!(&env, Error::IntentNotOpen);
        }

        let now = env.ledger().timestamp();
        // Boundary semantics: the intent deadline is INCLUSIVE for expiry.
        // The guard `now < intent.deadline` is false when `now == deadline`, so
        // expiry becomes valid at the deadline second itself (not strictly after).
        // Intent is live in [created_at, deadline); caller can expire at deadline+.
        if now < intent.deadline {
            panic_with_error!(&env, Error::DeadlineNotReached);
        }

        intent.state = IntentState::Expired;
        env.storage()
            .persistent()
            .set(&DataKey::Intent(intent_id.clone()), &intent);
        Self::bump_intent_ttl(&env, &intent_id);

        // Decrement open_intents: intent is no longer open.
        let open: u64 = env
            .storage()
            .instance()
            .get(&DataKey::OpenIntents)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::OpenIntents, &open.saturating_sub(1));

        env.events()
            .publish((Symbol::new(&env, "intent_expired"),), intent_id);
    }

    // ── Competitive Bid Window (#191) ─────────────────────────────────────────

    /// Admin-only: turn bid-window mode on or off.
    ///
    /// Issue #191: replaces the placeholder that reused
    /// `DataKey::DstAllowlistEnabled`.  When enabled, `submit_intent` opens new
    /// intents in `Bidding` state; solvers submit competing quotes via
    /// `bid_intent` for `BID_WINDOW` seconds, then anyone calls `settle_bids`
    /// to assign the highest bidder (or re-open the intent if nobody bid).
    /// Off by default; toggling it has no effect on intents already created.
    pub fn set_bid_window_enabled(env: Env, enabled: bool) {
        Self::require_admin(&env);
        env.storage()
            .instance()
            .set(&DataKey::BidWindowEnabled, &enabled);
        env.events()
            .publish((Symbol::new(&env, "bid_window_enabled"),), enabled);
    }

    /// A solver submits (or improves) a competing quote for an intent that is
    /// in the `Bidding` state.
    ///
    /// * The solver must currently satisfy `is_solver_eligible` (registered,
    ///   active, bonded at or above the minimum) — the same gate `accept_intent`
    ///   applies.
    /// * `quoted_dst_amount` must be strictly greater than the current best
    ///   bid, per `BestBidRecord`'s doc comment.  **Tie-break:** the first
    ///   solver to reach a given amount keeps the lead; a later equal quote does
    ///   *not* displace it.
    /// * Only callable while `now < intent.deadline` (the bid-window end).
    pub fn bid_intent(env: Env, solver: Address, intent_id: BytesN<32>, quoted_dst_amount: i128) {
        solver.require_auth();
        Self::require_not_paused(&env);
        Self::bump_instance_ttl(&env);

        if quoted_dst_amount <= 0 {
            panic_with_error!(&env, Error::ZeroAmount);
        }

        let intent: IntentRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Intent(intent_id.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::IntentNotFound));

        if intent.state != IntentState::Bidding {
            panic_with_error!(&env, Error::IntentNotBidding);
        }

        let now = env.ledger().timestamp();
        // Boundary semantics: the bid window is EXCLUSIVE for bidding, matching
        // `accept_intent` — `now >= deadline` rejects at the boundary second.
        if now >= intent.deadline {
            panic_with_error!(&env, Error::BidWindowClosed);
        }

        if !Self::is_solver_eligible(env.clone(), solver.clone()) {
            panic_with_error!(&env, Error::SolverInactive);
        }

        if let Some(best) = env
            .storage()
            .persistent()
            .get::<_, BestBidRecord>(&DataKey::BestBid(intent_id.clone()))
        {
            // Strictly higher only (ties keep the incumbent).
            if quoted_dst_amount <= best.quoted_dst_amount {
                panic_with_error!(&env, Error::BidNotHigher);
            }
        }

        env.storage().persistent().set(
            &DataKey::BestBid(intent_id.clone()),
            &BestBidRecord {
                solver: solver.clone(),
                quoted_dst_amount,
            },
        );
        env.storage().persistent().extend_ttl(
            &DataKey::BestBid(intent_id.clone()),
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND_TO,
        );

        env.events().publish(
            (Symbol::new(&env, "bid_submitted"), solver),
            (intent_id, quoted_dst_amount),
        );
    }

    /// Permissionless: close the bid window for a `Bidding` intent and act on
    /// the result, mirroring `expire_intent`'s permissionless-materialization
    /// pattern.
    ///
    /// * **A winning bid exists and the solver is still eligible:** the intent
    ///   moves to `Accepted` with a fresh `FILL_WINDOW` deadline and
    ///   `accept_intent`'s bookkeeping (`solver`, `active_intents`,
    ///   `OpenIntents`).
    /// * **No bid was received** (or the leading bidder's bond has since
    ///   dropped below the eligibility floor): the intent is re-opened as
    ///   `Open` with a fresh `INTENT_EXPIRY` deadline rather than getting stuck
    ///   in `Bidding` forever.
    pub fn settle_bids(env: Env, intent_id: BytesN<32>) {
        Self::bump_instance_ttl(&env);

        let mut intent: IntentRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Intent(intent_id.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::IntentNotFound));

        if intent.state != IntentState::Bidding {
            panic_with_error!(&env, Error::IntentNotBidding);
        }

        let now = env.ledger().timestamp();
        // INCLUSIVE for settlement, matching `expire_intent`: valid at the
        // deadline second itself.
        if now < intent.deadline {
            panic_with_error!(&env, Error::BidWindowStillOpen);
        }

        let cfg = Self::load_config(&env);
        let best = env
            .storage()
            .persistent()
            .get::<_, BestBidRecord>(&DataKey::BestBid(intent_id.clone()));
        env.storage()
            .persistent()
            .remove(&DataKey::BestBid(intent_id.clone()));

        let winner = best.filter(|b| Self::is_solver_eligible(env.clone(), b.solver.clone()));

        match winner {
            Some(b) => {
                // Mirror accept_intent.
                let mut solver_record: SolverRecord = env
                    .storage()
                    .persistent()
                    .get(&DataKey::Solver(b.solver.clone()))
                    .unwrap();
                solver_record.active_intents += 1;
                env.storage()
                    .persistent()
                    .set(&DataKey::Solver(b.solver.clone()), &solver_record);
                Self::bump_solver_ttl(&env, &b.solver);

                intent.solver = Some(b.solver.clone());
                intent.state = IntentState::Accepted;
                intent.bond_token = Self::load_bond_token(&env);
                intent.deadline = now + cfg.fill_window;

                // The intent leaves the open pool (a solver now owns it).
                let open: u64 = env
                    .storage()
                    .instance()
                    .get(&DataKey::OpenIntents)
                    .unwrap_or(0);
                env.storage()
                    .instance()
                    .set(&DataKey::OpenIntents, &open.saturating_sub(1));

                env.storage()
                    .persistent()
                    .set(&DataKey::Intent(intent_id.clone()), &intent);
                Self::bump_intent_ttl(&env, &intent_id);

                env.events().publish(
                    (Symbol::new(&env, "intent_accepted"), b.solver),
                    (intent_id.clone(), intent.deadline),
                );
                env.events().publish(
                    (Symbol::new(&env, "bids_settled"),),
                    (intent_id, b.quoted_dst_amount),
                );
            }
            None => {
                // No usable bid: re-open as Open. OpenIntents already counts
                // this intent (submit_intent incremented it for Bidding), so the
                // counter is left unchanged.
                intent.state = IntentState::Open;
                intent.deadline = now + cfg.intent_expiry;
                env.storage()
                    .persistent()
                    .set(&DataKey::Intent(intent_id.clone()), &intent);
                Self::bump_intent_ttl(&env, &intent_id);

                env.events()
                    .publish((Symbol::new(&env, "bids_settled_no_winner"),), intent_id);
            }
        }
    }

    /// The current leading bid for an intent in `Bidding` state, if any.
    pub fn get_best_bid(env: Env, intent_id: BytesN<32>) -> Option<BestBidRecord> {
        env.storage().persistent().get(&DataKey::BestBid(intent_id))
    }

    // ── Dispute Resolution (#188) ────────────────────────────────────────────

    /// Admin-only: set the address allowed to call `resolve_dispute`.
    /// Until this is called the `Admin` acts as arbiter (the design doc's v1
    /// default — docs/dispute-resolution-design.md §"Arbiter role").
    pub fn set_arbiter(env: Env, arbiter: Address) {
        Self::require_admin(&env);
        env.storage().instance().set(&DataKey::Arbiter, &arbiter);
        env.events()
            .publish((Symbol::new(&env, "arbiter_updated"),), arbiter);
    }

    /// The current arbiter (explicit `set_arbiter` value, else the `Admin`).
    pub fn get_arbiter(env: Env) -> Address {
        Self::load_arbiter(&env)
    }

    /// Solver delivers a completing fill into contract **escrow**, starting the
    /// dispute window (issue #188).  Unlike `fill_intent`, the output tokens are
    /// held by the contract — not sent straight to the user — until either the
    /// window closes cleanly (`release_fill`) or the arbiter rules on a dispute
    /// (`resolve_dispute`).
    ///
    /// Only a single completing fill is supported on this path: `fill_amount`
    /// must bring `total_filled` to at least `min_dst_amount`.  Partial fills
    /// keep using `fill_intent`.
    pub fn begin_fill(env: Env, solver: Address, intent_id: BytesN<32>, fill_amount: i128) {
        solver.require_auth();
        Self::require_not_paused(&env);
        Self::bump_instance_ttl(&env);

        let mut intent: IntentRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Intent(intent_id.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::IntentNotFound));

        if intent.state != IntentState::Accepted {
            panic_with_error!(&env, Error::IntentNotAcceptedForFill);
        }
        if intent.solver.as_ref() != Some(&solver) {
            panic_with_error!(&env, Error::Unauthorized);
        }

        let now = env.ledger().timestamp();
        // Fill-window deadline is EXCLUSIVE, matching `fill_intent`.
        if now >= intent.deadline {
            panic_with_error!(&env, Error::FillWindowExpired);
        }
        if fill_amount <= 0 {
            panic_with_error!(&env, Error::ZeroAmount);
        }
        if intent.total_filled + fill_amount < intent.min_dst_amount {
            panic_with_error!(&env, Error::InsufficientOutput);
        }

        // ── Effects first (CEI) ──────────────────────────────────────────────
        // `fill_amount` holds the escrowed amount while state is Filling /
        // Disputed; it is folded into `total_filled` only on resolution.
        intent.state = IntentState::Filling;
        intent.dispute_deadline = Some(now + DISPUTE_WINDOW);
        intent.fill_amount = Some(fill_amount);
        env.storage()
            .persistent()
            .set(&DataKey::Intent(intent_id.clone()), &intent);
        Self::bump_intent_ttl(&env, &intent_id);

        // ── Interaction: pull the output into escrow ─────────────────────────
        token::Client::new(&env, &intent.dst_token).transfer(
            &solver,
            &env.current_contract_address(),
            &fill_amount,
        );

        env.events().publish(
            (Symbol::new(&env, "fill_begun"), solver),
            (intent_id, fill_amount, now + DISPUTE_WINDOW),
        );
    }

    /// User contests an escrowed fill during the dispute window (issue #188).
    /// Freezes the escrow until the arbiter rules (`resolve_dispute`) or the
    /// arbiter window times out (`release_fill`).
    pub fn dispute_fill(env: Env, user: Address, intent_id: BytesN<32>) {
        user.require_auth();
        Self::bump_instance_ttl(&env);

        let mut intent: IntentRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Intent(intent_id.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::IntentNotFound));

        if intent.state != IntentState::Filling {
            panic_with_error!(&env, Error::IntentNotFilling);
        }
        if intent.user != user {
            panic_with_error!(&env, Error::Unauthorized);
        }

        let now = env.ledger().timestamp();
        let deadline = intent.dispute_deadline.unwrap_or(0);
        // Dispute window is EXCLUSIVE: at `now == deadline` it has closed.
        if now >= deadline {
            panic_with_error!(&env, Error::DisputeWindowClosed);
        }

        intent.state = IntentState::Disputed;
        intent.dispute_raised_at = Some(now);
        env.storage()
            .persistent()
            .set(&DataKey::Intent(intent_id.clone()), &intent);
        Self::bump_intent_ttl(&env, &intent_id);

        env.events()
            .publish((Symbol::new(&env, "fill_disputed"), user), intent_id);
    }

    /// Arbiter-only: rule on a disputed fill (issue #188).
    ///
    /// In **both** outcomes the escrowed tokens are delivered to the user (the
    /// design doc is explicit that the dispute only decides the solver's fate):
    /// * `Upheld` — solver misconduct: user receives the **full** escrow (no
    ///   protocol fee) and the solver's bond is slashed by the same
    ///   proportional formula `slash_solver` uses.
    /// * `Dismissed` — fill was legitimate: user receives `escrow − fee`, the
    ///   protocol fee is taken, and the solver is credited a completed fill.
    pub fn resolve_dispute(
        env: Env,
        arbiter: Address,
        intent_id: BytesN<32>,
        resolution: DisputeResolution,
    ) {
        arbiter.require_auth();
        Self::bump_instance_ttl(&env);

        if arbiter != Self::load_arbiter(&env) {
            panic_with_error!(&env, Error::NotArbiter);
        }

        let mut intent: IntentRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Intent(intent_id.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::IntentNotFound));

        if intent.state != IntentState::Disputed {
            panic_with_error!(&env, Error::IntentNotDisputed);
        }

        let now = env.ledger().timestamp();
        let escrow = intent.fill_amount.unwrap_or(0);
        let solver_addr = intent.solver.clone().unwrap();
        let mut solver_record: SolverRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Solver(solver_addr.clone()))
            .unwrap();
        solver_record.active_intents = solver_record.active_intents.saturating_sub(1);

        let fee_recipient: Address = env
            .storage()
            .instance()
            .get(&DataKey::FeeRecipient)
            .unwrap();
        let dst_client = token::Client::new(&env, &intent.dst_token);

        let mut slash_amount = 0i128;
        let mut fee = 0i128;
        match &resolution {
            DisputeResolution::Upheld => {
                let bond_token = intent.bond_token.clone();
                let unfilled = intent.min_dst_amount - intent.total_filled;
                let bond_before =
                    Self::get_solver_bond_amount(&env, &solver_record, &bond_token);
                slash_amount = Self::compute_slash_amount(bond_before, unfilled);
                Self::set_solver_bond_amount(
                    &env,
                    &mut solver_record,
                    &bond_token,
                    bond_before - slash_amount,
                );
                solver_record.fills_failed += 1;
                solver_record.last_slash_time = now;
                if Self::get_solver_bond_amount(&env, &solver_record, &bond_token)
                    < Self::min_bond_for_token(&env, &bond_token)
                {
                    solver_record.is_active = false;
                }
            }
            DisputeResolution::Dismissed => {
                fee = escrow
                    .checked_mul(PROTOCOL_FEE_BPS)
                    .unwrap_or_else(|| panic_with_error!(&env, Error::FeeOverflow))
                    .checked_div(10_000)
                    .unwrap_or_else(|| panic_with_error!(&env, Error::FeeOverflow));
                solver_record.fills_completed += 1;
                solver_record.total_volume += escrow;
            }
        }

        // ── Effects ─────────────────────────────────────────────────────────
        intent.total_filled += escrow;
        intent.fill_amount = Some(intent.total_filled);
        intent.filled_at = Some(now);
        intent.state = IntentState::Resolved;
        intent.resolution = Some(resolution.clone());

        env.storage()
            .persistent()
            .set(&DataKey::Solver(solver_addr.clone()), &solver_record);
        Self::bump_solver_ttl(&env, &solver_addr);
        env.storage()
            .persistent()
            .set(&DataKey::Intent(intent_id.clone()), &intent);
        Self::bump_intent_ttl(&env, &intent_id);

        if resolution == DisputeResolution::Dismissed {
            let total_vol: i128 = env
                .storage()
                .instance()
                .get(&DataKey::TotalVolume)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::TotalVolume, &(total_vol + escrow));
        }

        // ── Interactions ────────────────────────────────────────────────────
        let contract = env.current_contract_address();
        dst_client.transfer(&contract, &intent.user, &(escrow - fee));
        if fee > 0 {
            dst_client.transfer(&contract, &fee_recipient, &fee);
        }
        if slash_amount > 0 {
            token::Client::new(&env, &intent.bond_token).transfer(
                &contract,
                &fee_recipient,
                &slash_amount,
            );
        }

        env.events().publish(
            (Symbol::new(&env, "dispute_resolved"), solver_addr),
            (intent_id, escrow - fee, slash_amount),
        );
    }

    /// Permissionless (issue #188): settle an escrowed fill once its window has
    /// elapsed.
    ///
    /// * `Filling` + `now >= dispute_deadline` — no dispute was raised: the
    ///   user receives `escrow − fee`, the protocol fee is taken, and the
    ///   solver is credited a completed fill (intent → `Filled`).
    /// * `Disputed` + `now >= dispute_raised_at + ARBITER_WINDOW` — the arbiter
    ///   failed to rule in time: the user receives the **full** escrow with no
    ///   fee and no slash (the conservative default), intent → `Resolved` with
    ///   `resolution == None` marking the timeout.
    pub fn release_fill(env: Env, intent_id: BytesN<32>) {
        Self::bump_instance_ttl(&env);

        let mut intent: IntentRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Intent(intent_id.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::IntentNotFound));

        let now = env.ledger().timestamp();
        let escrow = intent.fill_amount.unwrap_or(0);
        let solver_addr = intent.solver.clone().unwrap();
        let mut solver_record: SolverRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Solver(solver_addr.clone()))
            .unwrap();
        solver_record.active_intents = solver_record.active_intents.saturating_sub(1);

        let fee_recipient: Address = env
            .storage()
            .instance()
            .get(&DataKey::FeeRecipient)
            .unwrap();
        let dst_client = token::Client::new(&env, &intent.dst_token);
        let contract = env.current_contract_address();

        let fee = match intent.state {
            IntentState::Filling => {
                let deadline = intent.dispute_deadline.unwrap_or(0);
                if now < deadline {
                    panic_with_error!(&env, Error::DisputeWindowStillOpen);
                }
                let fee = escrow
                    .checked_mul(PROTOCOL_FEE_BPS)
                    .unwrap_or_else(|| panic_with_error!(&env, Error::FeeOverflow))
                    .checked_div(10_000)
                    .unwrap_or_else(|| panic_with_error!(&env, Error::FeeOverflow));
                solver_record.fills_completed += 1;
                solver_record.total_volume += escrow;
                intent.state = IntentState::Filled;
                fee
            }
            IntentState::Disputed => {
                let raised = intent.dispute_raised_at.unwrap_or(0);
                if now < raised + ARBITER_WINDOW {
                    panic_with_error!(&env, Error::DisputeWindowStillOpen);
                }
                intent.state = IntentState::Resolved;
                intent.resolution = None; // marks an arbiter timeout
                0
            }
            _ => panic_with_error!(&env, Error::IntentNotFilling),
        };

        intent.total_filled += escrow;
        intent.fill_amount = Some(intent.total_filled);
        intent.filled_at = Some(now);

        env.storage()
            .persistent()
            .set(&DataKey::Solver(solver_addr.clone()), &solver_record);
        Self::bump_solver_ttl(&env, &solver_addr);
        env.storage()
            .persistent()
            .set(&DataKey::Intent(intent_id.clone()), &intent);
        Self::bump_intent_ttl(&env, &intent_id);

        // Tokens reach the user in both branches, so cumulative volume grows by
        // the escrowed amount regardless of outcome.
        let total_vol: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalVolume)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalVolume, &(total_vol + escrow));

        dst_client.transfer(&contract, &intent.user, &(escrow - fee));
        if fee > 0 {
            dst_client.transfer(&contract, &fee_recipient, &fee);
        }

        env.events().publish(
            (Symbol::new(&env, "fill_released"), solver_addr),
            (intent_id, escrow - fee),
        );
    }

    // ── Multi-Bond-Token Admin (#187) ────────────────────────────────────────

    /// Admin-only (issue #187): approve `token` for use as a solver bond.
    /// Probes the SEP-41 interface via `decimals()` — a bad address traps and
    /// reverts before anything is stored, mirroring `propose_add_dst_token`.
    pub fn add_allowed_bond_token(env: Env, token: Address) {
        Self::require_admin(&env);
        let _decimals = token::Client::new(&env, &token).decimals();
        env.storage()
            .instance()
            .set(&DataKey::AllowedBondToken(token.clone()), &true);
        env.events()
            .publish((Symbol::new(&env, "bond_token_allowed"),), token);
    }

    /// Admin-only (issue #187): remove `token` from the approved bond set.
    /// Solvers already bonded in it keep their funds and can still withdraw or
    /// deregister; they simply cannot add more bond in this token.
    pub fn remove_allowed_bond_token(env: Env, token: Address) {
        Self::require_admin(&env);
        env.storage()
            .instance()
            .remove(&DataKey::AllowedBondToken(token.clone()));
        env.events()
            .publish((Symbol::new(&env, "bond_token_disallowed"),), token);
    }

    /// `true` if `token` may currently be used as a solver bond (the legacy
    /// default token, or an explicitly approved one).
    pub fn is_allowed_bond_token(env: Env, token: Address) -> bool {
        Self::is_bond_token_allowed(&env, &token)
    }

    /// Admin-only (issue #187): set the minimum bond for `token`.  Ignored for
    /// the legacy default token, whose minimum always comes from
    /// `ProtocolConfig` / `set_config`.
    pub fn set_bond_token_min(env: Env, token: Address, amount: i128) {
        Self::require_admin(&env);
        if amount <= 0 {
            panic_with_error!(&env, Error::ZeroAmount);
        }
        env.storage()
            .instance()
            .set(&DataKey::MinBond(token.clone()), &amount);
        env.events()
            .publish((Symbol::new(&env, "bond_token_min_set"),), (token, amount));
    }

    /// The effective minimum bond for `token` (issue #187).
    pub fn get_bond_token_min(env: Env, token: Address) -> i128 {
        Self::min_bond_for_token(&env, &token)
    }

    /// A solver's bond balance in a specific token (issue #187).
    pub fn get_solver_bond(env: Env, solver: Address, token: Address) -> i128 {
        match env
            .storage()
            .persistent()
            .get::<_, SolverRecord>(&DataKey::Solver(solver))
        {
            Some(record) => Self::get_solver_bond_amount(&env, &record, &token),
            None => 0,
        }
    }

    /// Every `(token, amount)` bond a solver currently holds (issue #187).
    pub fn get_solver_bonds(env: Env, solver: Address) -> Vec<(Address, i128)> {
        let mut out: Vec<(Address, i128)> = Vec::new(&env);
        let record: SolverRecord = match env
            .storage()
            .persistent()
            .get(&DataKey::Solver(solver.clone()))
        {
            Some(r) => r,
            None => return out,
        };
        let default_token = Self::load_bond_token(&env);
        if record.bond_amount > 0 {
            out.push_back((default_token.clone(), record.bond_amount));
        }
        for i in 0..record.bond_tokens.len() {
            let t = record.bond_tokens.get(i).unwrap();
            if t == default_token {
                continue;
            }
            let amt: i128 = env
                .storage()
                .persistent()
                .get(&DataKey::SolverBond(solver.clone(), t.clone()))
                .unwrap_or(0);
            if amt > 0 {
                out.push_back((t, amt));
            }
        }
        out
    }

    // ── Batch Operations ──────────────────────────────────────────────────────

    /// Submit multiple intents in a single transaction.
    /// Processes all intents in the batch; a failure partway through will
    /// revert the entire batch (Soroban transaction atomicity).
    /// Bounded by MAX_BATCH_SIZE to prevent resource exhaustion.
    pub fn batch_submit_intent(
        env: Env,
        user: Address,
        intents: soroban_sdk::Vec<(String, String, i128, Address, i128, Option<u64>)>,
    ) -> soroban_sdk::Vec<BytesN<32>> {
        if intents.len() > MAX_BATCH_SIZE as usize {
            panic_with_error!(&env, Error::ZeroAmount); // No dedicated error; reuse nearest
        }

        let mut result = soroban_sdk::Vec::new(&env);
        for (src_chain, src_token, src_amount, dst_token, min_dst_amount, deadline) in intents {
            let intent_id = Self::submit_intent(
                env.clone(),
                user.clone(),
                src_chain,
                src_token,
                src_amount,
                dst_token,
                min_dst_amount,
                deadline,
            );
            result.push_back(intent_id);
        }
        result
    }

    /// Accept multiple intents in a single transaction.
    /// Processes all intents in the batch; a failure partway through will
    /// revert the entire batch (Soroban transaction atomicity).
    /// Bounded by MAX_BATCH_SIZE to prevent resource exhaustion.
    pub fn batch_accept_intent(
        env: Env,
        solver: Address,
        intent_ids: soroban_sdk::Vec<BytesN<32>>,
    ) {
        if intent_ids.len() > MAX_BATCH_SIZE as usize {
            panic_with_error!(&env, Error::ZeroAmount); // No dedicated error; reuse nearest
        }

        for intent_id in intent_ids {
            Self::accept_intent(env.clone(), solver.clone(), intent_id);
        }
    }

    // ── Fill Window Extension ─────────────────────────────────────────────────

    /// Solver requests a grace-period extension on an Accepted intent.
    /// Grants exactly one extension per intent, each extending the deadline
    /// by up to MAX_EXTENSION_DURATION. Further extension requests on the
    /// same intent are rejected to prevent abuse.
    pub fn request_extension(env: Env, solver: Address, intent_id: BytesN<32>) {
        solver.require_auth();
        Self::bump_instance_ttl(&env);

        let mut intent: IntentRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Intent(intent_id.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, Error::IntentNotFound));

        // Only Accepted intents can be extended
        if intent.state != IntentState::Accepted {
            panic_with_error!(&env, Error::IntentNotAccepted);
        }

        // Only the assigned solver can request an extension
        if intent.solver.as_ref() != Some(&solver) {
            panic_with_error!(&env, Error::Unauthorized);
        }

        // Each intent gets exactly one extension
        if env
            .storage()
            .persistent()
            .has(&DataKey::ExtensionGranted(intent_id.clone()))
        {
            panic_with_error!(&env, Error::ZeroAmount); // No dedicated error; reuse nearest
        }

        let now = env.ledger().timestamp();

        // Extend the deadline by the full extension duration
        intent.deadline = now + MAX_EXTENSION_DURATION;

        // Record that this intent has used its one extension
        env.storage()
            .persistent()
            .set(&DataKey::ExtensionGranted(intent_id.clone()), &true);

        env.storage()
            .persistent()
            .set(&DataKey::Intent(intent_id.clone()), &intent);
        Self::bump_intent_ttl(&env, &intent_id);

        env.events().publish(
            (Symbol::new(&env, "extension_granted"), solver),
            (intent_id, intent.deadline),
        );
    }

    // ── Views ─────────────────────────────────────────────────────────────────

    /// Read-only: returns the current effective protocol parameters.
    ///
    /// Useful for integrators who need to know MIN_BOND, FILL_WINDOW,
    /// INTENT_EXPIRY, and PROTOCOL_FEE_BPS without reading source code.
    /// Returns the values as a dedicated struct so each field is named at
    /// the call site rather than relying on tuple-position conventions.
    pub fn get_protocol_params(env: Env) -> ProtocolParams {
        let _ = env; // view — no storage read needed; values are compile-time constants
        ProtocolParams {
            min_bond: MIN_BOND,
            fill_window: FILL_WINDOW,
            intent_expiry: INTENT_EXPIRY,
            protocol_fee_bps: PROTOCOL_FEE_BPS,
        }
    }

    /// Fetch an intent's full record by id, or None if it was never submitted.
    pub fn get_intent(env: Env, intent_id: BytesN<32>) -> Option<IntentRecord> {
        env.storage().persistent().get(&DataKey::Intent(intent_id))
    }

    /// Fetch a solver's full record by address, or None if never registered.
    pub fn get_solver(env: Env, solver: Address) -> Option<SolverRecord> {
        env.storage().persistent().get(&DataKey::Solver(solver))
    }

    /// Returns the reputation score (0–10_000 basis points) for `solver`,
    /// or None if the solver has never registered.
    ///
    /// Callers that only need the numeric value and already hold the
    /// SolverRecord can call `compute_reputation_score` directly.
    pub fn get_reputation_score(env: Env, solver: Address) -> Option<u32> {
        let record: SolverRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Solver(solver))?;
        Some(Self::compute_reputation_score(&record))
    }

    /// Whether `solver` currently meets accept_intent's requirements
    /// (registered, active, bonded above MIN_BOND). Lets off-chain solver
    /// bots self-check eligibility without independently reimplementing
    /// the same logic accept_intent enforces.
    pub fn is_solver_eligible(env: Env, solver: Address) -> bool {
        let cfg = Self::load_config(&env);
        match env
            .storage()
            .persistent()
            .get::<_, SolverRecord>(&DataKey::Solver(solver))
        {
            Some(record) => record.is_active && record.bond_amount >= cfg.min_bond,
            None => false,
        }
    }

    /// Returns the current fee recipient address, or `None` before initialization.
    pub fn get_fee_recipient(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::FeeRecipient)
    }

    /// Pending fee-recipient proposal, if any: `(new_fee_recipient, eta)`
    /// where `eta` is the ledger timestamp at which `accept_fee_recipient`
    /// may execute it.
    pub fn get_pending_fee_recipient(env: Env) -> Option<(Address, u64)> {
        env.storage().instance().get(&DataKey::PendingFeeRecipient)
    }

    /// Returns the bond token address (USDC SAC), or `None` before initialization.
    pub fn get_bond_token(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::BondToken)
    }

    /// Returns the current admin address, or `None` before initialization.
    pub fn get_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }

    /// Returns `(total_intents, total_volume, open_intents)`.
    ///
    /// - `total_intents` — cumulative count of intents ever submitted.
    /// - `total_volume`  — cumulative dst-token units delivered across all fills.
    /// - `open_intents`  — intents currently in `Open` or `PartiallyFilled` state.
    ///
    /// **Trade-off (#109):** `open_intents` is maintained as an on-chain
    /// counter in instance storage (the same ledger entry that already holds
    /// `TotalIntents` and `TotalVolume`).  This means every state-changing
    /// call (`submit_intent`, `accept_intent`, `fill_intent`,
    /// `cancel_intent`, `expire_intent`, `slash_solver`) pays one extra
    /// integer read + write inside the instance entry, which is already
    /// loaded on every call.  The marginal cost is negligible compared to the
    /// persistent-storage I/O for `IntentRecord` and `SolverRecord`.
    ///
    /// The alternative — leaving `open_intents` entirely to indexers — would
    /// keep on-chain logic simpler, but would force every dashboard to replay
    /// the full event history for an O(N) count.  Storing the counter on-chain
    /// makes it O(1) for any caller.
    ///
    /// Note: the counter can transiently under-count if the contract is
    /// upgraded from a version that did not track it (pre-#109 deployments
    /// will have `OpenIntents` absent, which `unwrap_or(0)` handles gracefully
    /// — the counter will be accurate from the upgrade ledger forward).
    pub fn get_stats(env: Env) -> (u64, i128, u64) {
        let intents: u64 = env
            .storage()
            .instance()
            .get(&DataKey::TotalIntents)
            .unwrap_or(0);
        let volume: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalVolume)
            .unwrap_or(0);
        let open: u64 = env
            .storage()
            .instance()
            .get(&DataKey::OpenIntents)
            .unwrap_or(0);
        (intents, volume, open)
    }

    /// Minimum bond required for solver registration.
    pub fn get_min_bond(_env: Env) -> i128 {
        MIN_BOND
    }

    /// List all intent IDs for a given user. Returns empty Vec if user has no intents.
    pub fn list_intents_by_user(env: Env, user: Address) -> Vec<BytesN<32>> {
        env.storage()
            .persistent()
            .get(&DataKey::UserIntents(user))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Total number of solvers ever registered.
    pub fn get_solver_count(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::TotalSolvers)
            .unwrap_or(0)
    }

    /// Aggregate health snapshot combining `is_paused`, `get_stats`, and
    /// `get_solver_count` into a single call, for dashboard/monitoring
    /// integrations that would otherwise need three separate round-trips.
    pub fn get_protocol_health(env: Env) -> ProtocolHealth {
        let paused: bool = env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false);
        let total_intents: u64 = env
            .storage()
            .instance()
            .get(&DataKey::TotalIntents)
            .unwrap_or(0);
        let total_volume: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalVolume)
            .unwrap_or(0);
        let total_solvers: u32 = env
            .storage()
            .instance()
            .get(&DataKey::TotalSolvers)
            .unwrap_or(0);

        ProtocolHealth {
            paused,
            total_intents,
            total_volume,
            total_solvers,
        }
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    /// Compute a reputation score (0–10 000 bps) for a solver.
    ///
    /// Formula:
    ///   base  = fills_completed / (fills_completed + fills_failed)  [0–1]
    ///   decay = 1 / (1 + total_volume / VOLUME_SCALE)               [0–1]
    ///   score = base * (1 - 0.1 * decay) * 10_000
    ///
    /// Rationale:
    /// - `base` is the raw success rate.
    /// - `decay` gives a small bonus (up to 10%) to high-volume solvers who
    ///   demonstrate consistent execution: at zero volume the score is 90% of
    ///   the success rate; at very high volume it approaches 100%.
    /// - All arithmetic is integer-only and cannot panic — division by zero is
    ///   guarded, and intermediate values stay within i128/u64 range.
    ///
    /// Edge cases:
    ///   zero fills  → 0
    ///   all failures → 0
    ///   perfect rate, no volume → 9 000  (90% × 10 000)
    ///   perfect rate, high vol  → approaches 10 000
    pub fn compute_reputation_score(record: &SolverRecord) -> u32 {
        let total_fills = record.fills_completed as u64 + record.fills_failed as u64;
        if total_fills == 0 {
            return 0;
        }

        // base_bps ∈ [0, 10_000]
        let base_bps = (record.fills_completed as u64 * 10_000) / total_fills;

        // Volume scale: 1 000 fills × 100 dst tokens (7 dp) is the knee of
        // the curve. Only the shape matters — the constant can be tuned later.
        const VOLUME_SCALE: i128 = 1_000 * 100 * 10_000_000;

        // decay_bps = VOLUME_SCALE / (VOLUME_SCALE + vol + 1) × 10_000
        // ∈ (0, 10_000].  High volume → low decay_bps.
        let vol = record.total_volume.max(0);
        let decay_bps = ((VOLUME_SCALE as u64) * 10_000)
            / ((VOLUME_SCALE + vol + 1) as u64);

        // volume_multiplier_bps ∈ [9_000, 10_000)
        // At zero volume: decay_bps = ~10_000, multiplier = 9_000
        // At high  volume: decay_bps → 0,      multiplier → 10_000
        let multiplier_bps = 10_000u64 - decay_bps / 10;

        let score = base_bps * multiplier_bps / 10_000;
        score as u32
    }

    /// #127: Validate `src_token` address format against the conventions of
    /// `src_chain`.
    ///
    /// Rules:
    /// * EVM chains (`"ethereum"`, `"base"`, `"polygon"`, `"arbitrum"`,
    ///   `"optimism"`): token must be a `0x`-prefixed 42-character ASCII string
    ///   (2 + 40 hex digits).
    /// * `"solana"`: token must be a base58-encoded public key — ASCII, no `0x`
    ///   prefix, between 32 and 44 characters inclusive.
    /// * Any other `src_chain` value: validation is skipped (forward-compatible).
    ///
    /// Called from `submit_intent` unconditionally so that even when the
    /// src_chain allowlist is disabled, obviously malformed tokens are rejected
    /// early.
    fn validate_src_token(env: &Env, src_chain: &String, src_token: &String) {
        let token_len = src_token.len();
        let chain_len = src_chain.len();

        // Compare `src_chain` byte-by-byte against a known ASCII literal.
        let chain_is = |literal: &[u8]| -> bool {
            if chain_len as usize != literal.len() {
                return false;
            }
            let mut i = 0u32;
            while i < chain_len {
                if src_chain.get(i) != literal[i as usize] as u32 {
                    return false;
                }
                i += 1;
            }
            true
        };

        let is_evm = chain_is(b"ethereum")
            || chain_is(b"base")
            || chain_is(b"polygon")
            || chain_is(b"arbitrum")
            || chain_is(b"optimism");

        if is_evm {
            // EVM token address: exactly "0x" + 40 hex chars = 42 characters.
            if token_len != 42 {
                panic_with_error!(env, Error::InvalidSrcToken);
            }
            // Must start with "0x".
            if src_token.get(0) != b'0' as u32 || src_token.get(1) != b'x' as u32 {
                panic_with_error!(env, Error::InvalidSrcToken);
            }
            // Remaining 40 characters must all be hex digits [0-9a-fA-F].
            let mut i = 2u32;
            while i < 42 {
                let ch = src_token.get(i);
                let is_hex = (ch >= b'0' as u32 && ch <= b'9' as u32)
                    || (ch >= b'a' as u32 && ch <= b'f' as u32)
                    || (ch >= b'A' as u32 && ch <= b'F' as u32);
                if !is_hex {
                    panic_with_error!(env, Error::InvalidSrcToken);
                }
                i += 1;
            }
            return;
        }

        if chain_is(b"solana") {
            // Solana token (SPL mint): base58-encoded public key, 32–44 chars,
            // no "0x" prefix.
            if token_len < 32 || token_len > 44 {
                panic_with_error!(env, Error::InvalidSrcToken);
            }
            if token_len >= 2
                && src_token.get(0) == b'0' as u32
                && src_token.get(1) == b'x' as u32
            {
                panic_with_error!(env, Error::InvalidSrcToken);
            }
            // Validate base58 alphabet:
            // 123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz
            // (excludes: '0', 'I', 'O', 'l')
            let mut i = 0u32;
            while i < token_len {
                let ch = src_token.get(i);
                let is_b58 = (ch >= b'1' as u32 && ch <= b'9' as u32)
                    || (ch >= b'A' as u32 && ch <= b'H' as u32)
                    || (ch >= b'J' as u32 && ch <= b'N' as u32)
                    || (ch >= b'P' as u32 && ch <= b'Z' as u32)
                    || (ch >= b'a' as u32 && ch <= b'k' as u32)
                    || (ch >= b'm' as u32 && ch <= b'z' as u32);
                if !is_b58 {
                    panic_with_error!(env, Error::InvalidSrcToken);
                }
                i += 1;
            }
        }
        // Unknown chain: skip validation — forward-compatible with future chains.
    }

    fn require_admin(env: &Env) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized));
        // Auth audit: require_auth() is correct. All callers of require_admin
        // are admin-only functions (unpause, set_pauser,
        // add/remove_allowed_dst_token, set_dst_allowlist_enabled). The admin
        // is a single address with uniform authority over these functions;
        // require_auth_for_args would add no meaningful scope reduction.
        admin.require_auth();
    }

    /// Issue #120: `pause` accepts either the admin or the address set via
    /// `set_pauser`. `caller` is an explicit argument (rather than looked up
    /// implicitly, as `require_admin` does for the single-admin case)
    /// because there are now two addresses that could legitimately be the
    /// signer, so the contract needs to know which one is authorizing this
    /// call before it can require that specific address's auth.
    fn require_admin_or_pauser(env: &Env, caller: &Address) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized));
        let is_pauser = env
            .storage()
            .instance()
            .get::<_, Address>(&DataKey::Pauser)
            .map(|pauser| pauser == *caller)
            .unwrap_or(false);
        if *caller != admin && !is_pauser {
            panic_with_error!(env, Error::Unauthorized);
        }
        caller.require_auth();
    }

    fn require_not_paused(env: &Env) {
        if Self::is_paused(env.clone()) {
            panic_with_error!(env, Error::ContractPaused);
        }
    }

    /// Add `token` to the enumerable allowlist (#117), if not already present.
    fn add_to_dst_token_list(env: &Env, token: &Address) {
        let mut list: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::AllowedDstTokenList)
            .unwrap_or_else(|| Vec::new(env));
        let mut already_present = false;
        for i in 0..list.len() {
            if list.get(i).unwrap() == *token {
                already_present = true;
                break;
            }
        }
        if !already_present {
            list.push_back(token.clone());
            env.storage()
                .instance()
                .set(&DataKey::AllowedDstTokenList, &list);
        }
    }

    /// Remove `token` from the enumerable allowlist (#117), if present.
    fn remove_from_dst_token_list(env: &Env, token: &Address) {
        let list: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::AllowedDstTokenList)
            .unwrap_or_else(|| Vec::new(env));
        let mut new_list: Vec<Address> = Vec::new(env);
        for i in 0..list.len() {
            let item = list.get(i).unwrap();
            if item != *token {
                new_list.push_back(item);
            }
        }
        env.storage()
            .instance()
            .set(&DataKey::AllowedDstTokenList, &new_list);
    }

    fn get_adjusted_min_bond(env: &Env, dst_token: &Address) -> i128 {
        let multiplier = env
            .storage()
            .persistent()
            .get::<_, i128>(&DataKey::MinBondMultiplier(dst_token.clone()))
            .unwrap_or(10);
        (MIN_BOND * multiplier) / 10
    }

    /// Load the protocol config from storage, falling back to defaults for
    /// contracts that pre-date this upgrade (upgrade-safe).
    fn load_config(env: &Env) -> ProtocolConfig {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .unwrap_or(ProtocolConfig {
                min_bond: DEFAULT_MIN_BOND,
                fill_window: DEFAULT_FILL_WINDOW,
                intent_expiry: DEFAULT_INTENT_EXPIRY,
                protocol_fee_bps: DEFAULT_PROTOCOL_FEE_BPS,
            })
    }

    fn load_bond_token(env: &Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::BondToken)
            .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized))
    }

    /// Returns `true` when bid-window mode is active.
    ///
    /// Issue #191: this now reads a dedicated `DataKey::BidWindowEnabled` flag
    /// set via `set_bid_window_enabled`.  It previously reused
    /// `DataKey::DstAllowlistEnabled` "as a placeholder", which meant toggling
    /// the destination-token allowlist would silently also toggle bidding mode —
    /// a storage-key collision that is now closed.  Defaults to `false` so
    /// first-accept-wins behaviour is preserved on every deployment that
    /// pre-dates this feature.
    ///
    /// Bid-window mode changes `submit_intent` so newly created intents start
    /// in the `Bidding` state instead of `Open`, giving solvers a fixed
    /// `BID_WINDOW`-second window to submit competing quotes via `bid_intent`
    /// before `settle_bids` assigns the winner.
    pub fn is_bid_window_enabled(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::BidWindowEnabled)
            .unwrap_or(false)
    }

    /// Issue #187 — the minimum bond required for a given bond token.
    ///
    /// * Legacy default token → the effective `min_bond` from `ProtocolConfig`
    ///   (so pre-#187 behaviour and any admin `set_config` override are
    ///   preserved).
    /// * Any other approved token → an admin-set `DataKey::MinBond(token)`
    ///   entry, falling back to the compile-time `MIN_BOND` constant when the
    ///   admin has not set one.
    fn min_bond_for_token(env: &Env, token: &Address) -> i128 {
        if *token == Self::load_bond_token(env) {
            return Self::load_config(env).min_bond;
        }
        env.storage()
            .instance()
            .get(&DataKey::MinBond(token.clone()))
            .unwrap_or(MIN_BOND)
    }

    /// Issue #187 — `true` if `token` may be used as a solver bond: either it
    /// is the legacy default token (always allowed) or it has an explicit
    /// `DataKey::AllowedBondToken` entry.
    fn is_bond_token_allowed(env: &Env, token: &Address) -> bool {
        *token == Self::load_bond_token(env)
            || env
                .storage()
                .instance()
                .has(&DataKey::AllowedBondToken(token.clone()))
    }

    /// Issue #187 — read a solver's bond in a specific token.
    ///
    /// For the legacy default token the source of truth is
    /// `SolverRecord.bond_amount` (kept mirrored for pre-#187 readers); for
    /// every other token it is the `DataKey::SolverBond(solver, token)` entry.
    fn get_solver_bond_amount(env: &Env, record: &SolverRecord, token: &Address) -> i128 {
        if *token == Self::load_bond_token(env) {
            record.bond_amount
        } else {
            env.storage()
                .persistent()
                .get(&DataKey::SolverBond(record.address.clone(), token.clone()))
                .unwrap_or(0)
        }
    }

    /// Issue #187 — write a solver's bond in a specific token, keeping the
    /// legacy `bond_amount` mirror and the `bond_tokens` enumeration in sync.
    /// A zero balance drops the token from `bond_tokens` (and, for non-default
    /// tokens, removes the storage entry entirely).
    fn set_solver_bond_amount(
        env: &Env,
        record: &mut SolverRecord,
        token: &Address,
        amount: i128,
    ) {
        let default_token = Self::load_bond_token(env);
        if *token == default_token {
            record.bond_amount = amount;
        } else if amount > 0 {
            env.storage().persistent().set(
                &DataKey::SolverBond(record.address.clone(), token.clone()),
                &amount,
            );
        } else {
            env.storage()
                .persistent()
                .remove(&DataKey::SolverBond(record.address.clone(), token.clone()));
        }

        let mut present = false;
        for i in 0..record.bond_tokens.len() {
            if record.bond_tokens.get(i).unwrap() == *token {
                present = true;
                break;
            }
        }
        if amount > 0 && !present {
            record.bond_tokens.push_back(token.clone());
        } else if amount == 0 && present {
            // Rebuild without `token`, mirroring `remove_from_dst_token_list`.
            let mut rebuilt: Vec<Address> = Vec::new(env);
            for i in 0..record.bond_tokens.len() {
                let t = record.bond_tokens.get(i).unwrap();
                if t != *token {
                    rebuilt.push_back(t);
                }
            }
            record.bond_tokens = rebuilt;
        }
    }

    /// Issue #193 — proportional bond slash.
    ///
    /// Returns the amount to slash from `bond` for a solver that failed to
    /// deliver an intent whose outstanding output is `unfilled_amount`
    /// (`min_dst_amount - total_filled`, floored at 0):
    ///
    /// ```text
    ///   exposure   = min(unfilled_amount, bond)   // same-token comparability
    ///   proportional = exposure / 10              // 10% of what was at stake
    ///   cap          = bond * SLASH_BPS / 10_000  // never worse than flat 10%
    ///   slash        = clamp(proportional, 1, min(cap, bond))
    /// ```
    ///
    /// Properties (mirroring `compute_reputation_score`'s edge-case discipline):
    /// * Integer-only, cannot panic (all operands ≥ 0, no division by zero).
    /// * Floor of 1 stroop preserves issue #32's "non-zero bond is always
    ///   punished" guarantee.
    /// * Cap at `bond * 10%` means a well-matched bond is never slashed harder
    ///   than the old flat rate; a solver who over-bonds relative to the intent
    ///   is slashed *less*, and a solver who under-bonds is still capped at
    ///   100% of bond (via `exposure ≤ bond`) and never panics.
    /// * `unfilled_amount == 0` (shouldn't happen for an Accepted intent, but
    ///   guarded) still yields the floor of 1.
    fn compute_slash_amount(bond: i128, unfilled_amount: i128) -> i128 {
        if bond <= 0 {
            return 0;
        }
        let exposure = unfilled_amount.max(0).min(bond);
        let proportional = exposure / 10;
        let cap = (bond / 10_000) * SLASH_BPS;
        let cap = cap.min(bond).max(1);
        proportional.max(1).min(cap)
    }

    /// Issue #188 — the address allowed to call `resolve_dispute`: the
    /// `DataKey::Arbiter` entry if set, otherwise the `Admin` (the design
    /// doc's v1 default).
    fn load_arbiter(env: &Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Arbiter)
            .unwrap_or_else(|| {
                env.storage()
                    .instance()
                    .get(&DataKey::Admin)
                    .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized))
            })
    }

    /// Returns the effective fee in basis points for a given `fill_amount`,
    /// consulting the stored `ProtocolConfig` for the per-contract rate.
    ///
    /// Future work (tiered-fee feature): this function can be extended to
    /// accept a solver address and apply volume-tier discounts based on the
    /// solver's historical `total_volume`.  For now it returns the flat
    /// `protocol_fee_bps` from config so all existing call-sites get a single
    /// source of truth for fee calculation.
    fn get_tiered_fee_bps(env: &Env) -> i128 {
        Self::load_config(env).protocol_fee_bps
    }

    fn bump_instance_ttl(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
    }

    fn bump_intent_ttl(env: &Env, intent_id: &BytesN<32>) {
        env.storage().persistent().extend_ttl(
            &DataKey::Intent(intent_id.clone()),
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND_TO,
        );
    }

    fn bump_solver_ttl(env: &Env, solver: &Address) {
        env.storage().persistent().extend_ttl(
            &DataKey::Solver(solver.clone()),
            PERSISTENT_TTL_THRESHOLD,
            PERSISTENT_TTL_EXTEND_TO,
        );
    }

    fn compute_intent_id(
        env: &Env,
        user: &Address,
        src_chain: &String,
        amount: i128,
        timestamp: u64,
        nonce: u64,
    ) -> BytesN<32> {
        // Build a collision-resistant preimage from the full intent context, then
        // hash to a 32-byte id. Including the user, source chain, and a
        // per-user nonce ensures two otherwise-identical intents from the same
        // user in the same ledger always produce distinct ids.
        let mut preimage = Bytes::new(env);
        preimage.append(&user.clone().to_xdr(env));
        preimage.append(&src_chain.clone().to_xdr(env));
        preimage.extend_from_array(&amount.to_be_bytes());
        preimage.extend_from_array(&timestamp.to_be_bytes());
        preimage.extend_from_array(&nonce.to_be_bytes());
        env.crypto().sha256(&preimage).into()
    }
}
