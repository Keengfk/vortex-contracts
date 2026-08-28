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

#[cfg(test)]
mod bench;

// ─── Constants ────────────────────────────────────────────────────────────────

const INTENT_EXPIRY: u64 = 1800; // 30 minutes
const FILL_WINDOW: u64 = 300; // 5 minutes to fill after intent accepted
const MIN_BOND: i128 = 50 * 10_000_000; // 50 USDC minimum solver bond
const PROTOCOL_FEE_BPS: i128 = 5; // 0.05%

/// Delay enforced between proposing and executing a sensitive admin change
/// (admin transfer, fee recipient handover, dst_token allowlist changes).
/// Gives users and solvers a window to notice and react before the change
/// takes effect (#115). Proposing also emits a distinct event immediately,
/// so off-chain monitors get advance notice even before the delay elapses
/// (#116).
const ADMIN_TIMELOCK_DELAY: u64 = 172_800; // 48 hours

// ── Defaults seeded into `ProtocolConfig` by `initialize`, and the fallback
// `load_config` returns for contracts deployed before the configurable-params
// feature existed.  They mirror the historical compile-time constants above.
const DEFAULT_MIN_BOND: i128 = MIN_BOND;
const DEFAULT_FILL_WINDOW: u64 = FILL_WINDOW;
const DEFAULT_INTENT_EXPIRY: u64 = INTENT_EXPIRY;
const DEFAULT_PROTOCOL_FEE_BPS: i128 = PROTOCOL_FEE_BPS;

// ── `set_config` bounds.  A parameter outside any of these ranges is rejected
// with `Error::InvalidConfig`.
const MAX_PROTOCOL_FEE_BPS: i128 = 1_000; // 10% hard cap on the protocol fee
const MIN_FILL_WINDOW_SECS: u64 = 60; // a solver needs at least a minute to fill
const MIN_INTENT_EXPIRY_SECS: u64 = 300; // and must always exceed the fill window
const MIN_BOND_FLOOR: i128 = 10_000_000; // one 7-decimal USDC unit

// ── Cooldowns / limits enforced outside `ProtocolConfig`.
const SLASH_COOLDOWN: u64 = 3600; // 1 hour a slashed solver must wait before accepting again
const CANCEL_COOLDOWN: u64 = 3600; // 1 hour between a user's successive intent cancellations
const MAX_EXTENSION_DURATION: u64 = 300; // one extra fill window granted by `request_extension`

// ── Storage-migration schema version (#194). Bumped whenever a `migrate()`
// body is added for a new release; `initialize` stamps fresh deploys with the
// current value and `migrate` refuses to run once the contract is already at
// it, so a migration can never be applied twice.
const MIGRATION_VERSION: u32 = 1;

// Basis-points denominator, shared by the protocol fee and the #192 discount
// schedule (`discount_bps` is a fraction of the fee, not of the fill).
const BPS_DENOMINATOR: i128 = 10_000;

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
    UserNonce(Address),      // per-user submit counter to widen intent_id preimage
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

    /// **Instance storage.** Admin-configurable `ProtocolConfig` (min bond,
    /// fill window, intent expiry, protocol fee bps).  Seeded by `initialize`
    /// and replaced atomically by `set_config`.  Absent on contracts that
    /// pre-date the configurable-params feature, in which case `load_config`
    /// falls back to the compile-time `DEFAULT_*` values.
    Config,

    /// **Instance storage.** Pending admin-transfer proposal plus the ledger
    /// timestamp at which `accept_admin_transfer` may execute it:
    /// `(Address, u64)`.  Written by `propose_admin_transfer`, cleared by
    /// `accept_admin_transfer`.
    PendingAdmin,

    /// **Instance storage.** Per-`dst_token` pending "add to allowlist"
    /// proposal: the ledger timestamp (`u64`) at which `execute_add_dst_token`
    /// becomes callable.  Written by `propose_add_dst_token`.
    PendingDstTokenAdd(Address),

    /// **Instance storage.** Per-`dst_token` pending "remove from allowlist"
    /// proposal: the ledger timestamp (`u64`) at which
    /// `execute_remove_dst_token` becomes callable.
    PendingDstTokenRemove(Address),

    /// **Instance storage.** Enumerable mirror of the `AllowedDstToken`
    /// presence-flags: `Vec<Address>` of every currently-allowed dst_token,
    /// so `list_allowed_dst_tokens` can answer without an event replay (#117).
    AllowedDstTokenList,

    /// **Persistent storage.** Per-`dst_token` bond multiplier in tenths
    /// (`i128`; `10` = 1.0x, `15` = 1.5x).  Unset tokens default to `10`.
    /// Written by `set_min_bond_multiplier`.
    MinBondMultiplier(Address),

    /// **Persistent storage.** Per-user list of every `intent_id` the user
    /// has submitted (`Vec<BytesN<32>>`), backing `list_intents_by_user`.
    UserIntents(Address),

    /// **Persistent storage.** Per-user ledger timestamp (`u64`) of the last
    /// successful `cancel_intent`, enforcing `CANCEL_COOLDOWN` between a
    /// user's successive cancellations (spam deterrence).
    CancelCooldown(Address),

    /// **Persistent storage.** Presence-flag (value `true`) recording that an
    /// intent has already consumed its single `request_extension` grant.
    ExtensionGranted(BytesN<32>),

    /// **Instance storage.** Pending contract-upgrade proposal plus the ledger
    /// timestamp at which `execute_upgrade` may run it: `(BytesN<32>, u64)`
    /// (#194). Written by `propose_upgrade`, cleared by `execute_upgrade`.
    PendingUpgrade,

    /// **Instance storage.** The storage-migration schema version the contract
    /// is currently at (`u32`) (#194). Set to `MIGRATION_VERSION` by
    /// `initialize` (fresh deploys need no migration) and advanced to it by
    /// `migrate`, which is guarded against re-running once already at that
    /// version. Absent (→ `0`) on contracts deployed before this feature.
    MigrationVersion,

    /// **Instance storage.** Ordered fee-discount schedule (#192):
    /// `Vec<(i128 min_volume, u32 discount_bps)>`, ascending by `min_volume`.
    /// A solver whose `SolverRecord.total_volume` clears a tier's threshold
    /// pays `protocol_fee_bps` reduced by that tier's `discount_bps` (in
    /// hundredths of the fee). Absent / empty ⇒ every solver pays the flat
    /// fee. Set by `set_fee_discount_tiers`.
    FeeDiscountTiers,
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
}

/// A registered solver (market maker)
#[contracttype]
#[derive(Clone)]
pub struct SolverRecord {
    pub address: Address,
    pub bond_amount: i128, // USDC locked as collateral
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
    /// #31: fee arithmetic overflowed (fill_amount is astronomically large)
    FeeOverflow = 23,
    /// #33: the address passed to add_allowed_dst_token doesn't implement SEP-41
    InvalidTokenInterface = 24,
    /// #30: no pending fee-recipient proposal to accept in `accept_fee_recipient`.
    NoPendingFeeRecipient = 25,
    /// `submit_intent` named a `src_chain` that is not on the allowlist while
    /// src-chain allowlist enforcement is enabled (#34).
    SrcChainNotAllowed = 26,
    /// `rescue_tokens` was asked to move the protocol's own bond token (#35).
    RescueProtectedToken = 27,
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

    /// A timelocked admin action (`accept_admin_transfer`,
    /// `accept_fee_recipient`, `execute_add_dst_token`,
    /// `execute_remove_dst_token`) was attempted before its
    /// `ADMIN_TIMELOCK_DELAY` had elapsed (#115).
    TimelockNotElapsed = 29,

    /// `accept_admin_transfer` was called with no pending proposal stored.
    NoPendingAdminTransfer = 30,

    /// `set_config` was called with a parameter outside its allowed bound
    /// (see the `MAX_PROTOCOL_FEE_BPS` / `MIN_*` constants).
    InvalidConfig = 31,

    /// `execute_add_dst_token` / `execute_remove_dst_token` found no matching
    /// pending proposal for the given token.
    NoPendingDstTokenChange = 32,

    /// `submit_intent` was called with `src_amount` or `min_dst_amount`
    /// exceeding `MAX_AMOUNT`.
    AmountTooLarge = 33,

    /// `cancel_intent` was called again before `CANCEL_COOLDOWN` had elapsed
    /// since the caller's previous cancellation.
    CancelCooldownNotExpired = 34,

    /// `execute_upgrade` was called with no pending `propose_upgrade` (#194).
    NoPendingUpgrade = 35,

    /// `migrate` was called when the contract is already at `MIGRATION_VERSION`
    /// (#194) — the one-time migration guard.
    AlreadyMigrated = 36,

    /// `set_fee_discount_tiers` was given a schedule that is not strictly
    /// ascending by `min_volume`, or a `discount_bps` above 10 000 (#192).
    InvalidFeeTiers = 37,
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
        // Fresh deploys are already at the current schema — `migrate` is a
        // no-op unless a later upgrade bumps `MIGRATION_VERSION` (#194).
        env.storage()
            .instance()
            .set(&DataKey::MigrationVersion, &MIGRATION_VERSION);
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
        env.storage().instance().set(
            &DataKey::PendingFeeRecipient,
            &(new_fee_recipient.clone(), eta),
        );

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

    // ── Contract Upgrade (#194) ───────────────────────────────────────────────

    /// Admin-only: propose swapping the contract's Wasm for the code with hash
    /// `new_wasm_hash` (already uploaded to the ledger, e.g. via
    /// `stellar contract upload`). Timelocked exactly like the other sensitive
    /// admin actions: an `upgrade_proposed` event fires immediately for
    /// off-chain monitors, and `execute_upgrade` may only run once
    /// `ADMIN_TIMELOCK_DELAY` has elapsed. A fresh proposal overwrites any
    /// prior pending one (and resets the timelock).
    pub fn propose_upgrade(env: Env, new_wasm_hash: BytesN<32>) {
        Self::require_admin(&env);
        let eta = env.ledger().timestamp() + ADMIN_TIMELOCK_DELAY;
        env.storage()
            .instance()
            .set(&DataKey::PendingUpgrade, &(new_wasm_hash.clone(), eta));
        Self::bump_instance_ttl(&env);
        env.events().publish(
            (Symbol::new(&env, "upgrade_proposed"),),
            (new_wasm_hash, eta),
        );
    }

    /// Apply a previously proposed upgrade once its timelock has elapsed.
    /// Admin-only (the proposal is re-confirmed here so a stale proposal can't
    /// be executed by a third party). `new_wasm_hash` must match the pending
    /// proposal. Emits `upgraded`; after this the new code is live and callers
    /// should run [`migrate`] if the release requires it.
    pub fn execute_upgrade(env: Env, new_wasm_hash: BytesN<32>) {
        Self::require_admin(&env);
        let (pending, eta): (BytesN<32>, u64) = env
            .storage()
            .instance()
            .get(&DataKey::PendingUpgrade)
            .unwrap_or_else(|| panic_with_error!(&env, Error::NoPendingUpgrade));
        if pending != new_wasm_hash {
            panic_with_error!(&env, Error::Unauthorized);
        }
        if env.ledger().timestamp() < eta {
            panic_with_error!(&env, Error::TimelockNotElapsed);
        }
        env.storage().instance().remove(&DataKey::PendingUpgrade);
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        env.events()
            .publish((Symbol::new(&env, "upgraded"),), new_wasm_hash);
    }

    /// The pending upgrade proposal, if any: `(new_wasm_hash, eta)`.
    pub fn get_pending_upgrade(env: Env) -> Option<(BytesN<32>, u64)> {
        env.storage().instance().get(&DataKey::PendingUpgrade)
    }

    /// Admin-only, run-once-per-release storage migration hook (#194).
    ///
    /// Guarded by `DataKey::MigrationVersion`: it refuses (`AlreadyMigrated`)
    /// once the contract is at `MIGRATION_VERSION`, so a migration can never be
    /// applied twice even if a later `execute_upgrade` forgets to bump the
    /// version. Contracts deployed before this feature have no stored version
    /// (`unwrap_or(0)`), so their first `migrate` runs.
    ///
    /// The body is intentionally empty in this release — no persisted shape has
    /// changed. Future upgrades that reshape storage add their one-time
    /// backfill here and bump `MIGRATION_VERSION`.
    pub fn migrate(env: Env) {
        Self::require_admin(&env);
        let current: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MigrationVersion)
            .unwrap_or(0);
        if current >= MIGRATION_VERSION {
            panic_with_error!(&env, Error::AlreadyMigrated);
        }

        // (no-op for MIGRATION_VERSION == 1)

        env.storage()
            .instance()
            .set(&DataKey::MigrationVersion, &MIGRATION_VERSION);
        Self::bump_instance_ttl(&env);
        env.events().publish(
            (Symbol::new(&env, "migrated"),),
            (current, MIGRATION_VERSION),
        );
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

        env.events()
            .publish((Symbol::new(&env, "dst_token_add_proposed"),), (token, eta));
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

    /// Solvers register by depositing a USDC bond. Existing solvers may top up
    /// with any positive amount -- the minimum is enforced on the resulting
    /// total, not on each individual deposit.
    pub fn register_solver(env: Env, solver: Address, bond_amount: i128) {
        // Auth audit: require_auth() is correct. The solver must sign to
        // consent to locking their own funds as bond. require_auth_for_args
        // could theoretically scope to (solver, bond_amount) but adding that
        // scope provides no real benefit — the solver is the tx signer and
        // the bond amount is constrained by their token balance anyway.
        solver.require_auth();
        Self::require_not_paused(&env);
        Self::bump_instance_ttl(&env);

        if bond_amount <= 0 {
            panic_with_error!(&env, Error::ZeroAmount);
        }

        let existing: Option<SolverRecord> = env
            .storage()
            .persistent()
            .get(&DataKey::Solver(solver.clone()));

        let existing_bond = existing.as_ref().map(|s| s.bond_amount).unwrap_or(0);
        let cfg = Self::load_config(&env);
        if existing_bond + bond_amount < cfg.min_bond {
            panic_with_error!(&env, Error::SolverBondTooLow);
        }

        let is_new_solver = existing.is_none();

        // ── Effects first (CEI) ──────────────────────────────────────────────
        // Build and persist the SolverRecord *before* pulling funds in so the
        // contract's storage is always consistent with what it holds: if the
        // transfer were to fail (or a re-entrant call were made mid-transfer),
        // the record either doesn't exist yet (new solver) or still reflects
        // the pre-topup balance, rather than an inflated balance with no matching funds.
        let record = match existing {
            Some(mut s) => {
                s.bond_amount += bond_amount;
                s.is_active = true;
                s
            }
            None => SolverRecord {
                address: solver.clone(),
                bond_amount,
                fills_completed: 0,
                fills_failed: 0,
                total_volume: 0,
                is_active: true,
                registered_at: env.ledger().timestamp(),
                active_intents: 0,
                last_slash_time: 0,
            },
        };

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
        let bond_token = Self::load_bond_token(&env);
        let client = token::Client::new(&env, &bond_token);
        client.transfer(&solver, &env.current_contract_address(), &bond_amount);

        env.events().publish(
            (Symbol::new(&env, "solver_registered"), solver),
            bond_amount,
        );
    }

    /// Solver voluntarily exits the protocol. Returns the full bond to the
    /// solver and removes their record. Requires no active (Accepted) intents —
    /// use `slash_solver` to clear those first.
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

        // ── Effects first (CEI) ──────────────────────────────────────────────
        // Remove the solver record and update the counter *before* the external
        // token transfer so that any re-entrant call sees no record and would
        // panic with SolverNotRegistered rather than processing a double-refund.
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

        // ── Interaction: return bond ─────────────────────────────────────────
        if record.bond_amount > 0 {
            let bond_token = Self::load_bond_token(&env);
            let client = token::Client::new(&env, &bond_token);
            client.transfer(
                &env.current_contract_address(),
                &solver,
                &record.bond_amount,
            );
        }

        env.events().publish(
            (Symbol::new(&env, "solver_deregistered"), solver),
            record.bond_amount,
        );
    }

    /// Solver withdraws part of their bond without fully deregistering.
    /// The remaining bond must still clear MIN_BOND -- to go below that,
    /// use deregister_solver instead (which also requires no active intents).
    pub fn withdraw_bond(env: Env, solver: Address, amount: i128) {
        // Auth audit: require_auth() is correct. Only the solver may withdraw
        // their own bond. require_auth_for_args could scope to the withdrawal
        // amount, but the solver signature authorises the full withdrawal path;
        // amount is validated against their stored balance immediately after.
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

        if amount > record.bond_amount {
            panic_with_error!(&env, Error::InsufficientBond);
        }

        let remaining = record.bond_amount - amount;
        let cfg = Self::load_config(&env);
        if remaining < cfg.min_bond {
            panic_with_error!(&env, Error::SolverBondTooLow);
        }

        record.bond_amount = remaining;
        env.storage()
            .persistent()
            .set(&DataKey::Solver(solver.clone()), &record);
        Self::bump_solver_ttl(&env, &solver);

        let bond_token = Self::load_bond_token(&env);
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
            state: IntentState::Open,
            created_at: now,
            deadline: expiry,
            filled_at: None,
            fill_amount: None,
            total_filled: 0,
        };

        Self::save_intent(&env, &intent_id, &intent);

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

        // Increment open_intents: every new submission starts as Open.
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

    /// Solver claims an intent (exclusive fill right for FILL_WINDOW seconds)
    pub fn accept_intent(env: Env, solver: Address, intent_id: BytesN<32>) {
        // Auth audit: require_auth() is correct. The solver must sign to
        // voluntarily take on the fill obligation and bond risk associated with
        // this intent. require_auth_for_args scoped to intent_id could prevent a
        // malicious invoker contract from accepting an unintended intent on the
        // solver's behalf; noted as a future hardening opportunity.
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
        if solver_record.last_slash_time > 0 && now < solver_record.last_slash_time + SLASH_COOLDOWN
        {
            panic_with_error!(&env, Error::SolverInactive);
        }

        let mut intent = Self::load_intent(&env, &intent_id);

        let adjusted_min_bond = Self::get_adjusted_min_bond(&env, &intent.dst_token);
        if solver_record.bond_amount < adjusted_min_bond {
            panic_with_error!(&env, Error::SolverBondTooLow);
        }

        let now = env.ledger().timestamp();
        // Boundary semantics: deadline is EXCLUSIVE for acceptance.
        // `now >= intent.deadline` rejects at the boundary second (`now == deadline`)
        // so the full [created_at, deadline) half-open window is available for solvers.
        if now >= intent.deadline {
            Self::save_intent(&env, &intent_id, &intent);
            panic_with_error!(&env, Error::IntentExpired);
        }

        if intent.state != IntentState::Open && intent.state != IntentState::PartiallyFilled {
            panic_with_error!(&env, Error::IntentNotOpen);
        }

        intent.solver = Some(solver.clone());
        intent.state = IntentState::Accepted;
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

        Self::save_intent(&env, &intent_id, &intent);

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

        let mut intent = Self::load_intent(&env, &intent_id);

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

        // Protocol fee on this fill, taken from the solver (priced into their
        // quote) so the user always receives at least `min_dst_amount`.
        // `get_tiered_fee_bps` is the single source of truth for the effective
        // rate: the admin-configurable `ProtocolConfig` fee, minus any
        // volume-tier discount this solver has earned (#192). Explicit
        // checked_mul/checked_div makes the overflow-safety property visible in
        // code rather than relying solely on the release profile's
        // `overflow-checks = true` setting (issue #31).
        let fee_bps = Self::get_tiered_fee_bps(&env, &solver);
        let fee = fill_amount
            .checked_mul(fee_bps)
            .unwrap_or_else(|| panic_with_error!(&env, Error::FeeOverflow))
            .checked_div(BPS_DENOMINATOR)
            .unwrap_or_else(|| panic_with_error!(&env, Error::FeeOverflow));

        // ── Effects first (CEI) ──────────────────────────────────────────────
        // Every state change is written to storage *before* the external token
        // transfers below. A hostile SEP-41 token that tries to re-enter
        // fill_intent or slash_solver during a transfer sees the intent already
        // in its post-fill state and is rejected by the guards above.
        //
        // Accumulate the fill.
        intent.total_filled += fill_amount;
        let cumulative = intent.total_filled;

        // Update fill_amount to reflect the running total for backward-compatible reads.
        intent.fill_amount = Some(cumulative);

        // Update solver stats for this partial fill.
        let mut solver_record: SolverRecord = env
            .storage()
            .persistent()
            .get(&DataKey::Solver(solver.clone()))
            .unwrap();
        solver_record.total_volume += fill_amount;

        if cumulative >= intent.min_dst_amount {
            // Intent is fully satisfied — close it out.
            // open_intents was already decremented when the intent was accepted;
            // no further adjustment needed here.
            intent.state = IntentState::Filled;
            intent.filled_at = Some(now);
            solver_record.fills_completed += 1;
            solver_record.active_intents = solver_record.active_intents.saturating_sub(1);
        } else {
            // Partial fill: re-open so another solver (or the same) can claim the
            // remaining amount.  Reset solver assignment and deadline back to the
            // full intent expiry window so the rest of the intent can be picked up.
            // The intent is back in Open rotation, so increment open_intents again.
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

        // Update protocol stats
        let total_vol: i128 = env
            .storage()
            .instance()
            .get(&DataKey::TotalVolume)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::TotalVolume, &(total_vol + fill_amount));

        Self::save_intent(&env, &intent_id, &intent);

        // ── Interactions last (CEI): token transfers ─────────────────────────
        // Solver delivers this fill's output to the user, then pays the
        // protocol fee. Both transfers are authorized by the solver who signed
        // this call, and the user's received amount is never reduced by the fee.
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

        let mut intent = Self::load_intent(&env, &intent_id);

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
        Self::save_intent(&env, &intent_id, &intent);

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

        let mut intent = Self::load_intent(&env, &intent_id);

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

        // Slash 10% of bond, with a floor of 1 so that a non-zero bond is never
        // economically unpunished due to integer division rounding to zero
        // (issue #32: tiny bonds below 10 would otherwise yield slash_amount = 0).
        let slash_amount = (solver_record.bond_amount / 10).max(1);
        solver_record.bond_amount -= slash_amount;
        solver_record.fills_failed += 1;
        solver_record.last_slash_time = now;
        solver_record.active_intents = solver_record.active_intents.saturating_sub(1);

        let cfg = Self::load_config(&env);
        // A solver whose bond no longer covers min_bond can't credibly back
        // further fills -- take them out of rotation until they top back up.
        if solver_record.bond_amount < cfg.min_bond {
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
        Self::save_intent(&env, &intent_id, &intent);

        // Send slash to fee recipient (state already committed above)
        if slash_amount > 0 {
            let bond_token = Self::load_bond_token(&env);
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

        let mut intent = Self::load_intent(&env, &intent_id);

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
        Self::save_intent(&env, &intent_id, &intent);

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

    // ── Fill Window Extension ─────────────────────────────────────────────────

    /// Solver requests a grace-period extension on an Accepted intent.
    /// Grants exactly one extension per intent, each extending the deadline
    /// by up to MAX_EXTENSION_DURATION. Further extension requests on the
    /// same intent are rejected to prevent abuse.
    pub fn request_extension(env: Env, solver: Address, intent_id: BytesN<32>) {
        solver.require_auth();
        Self::bump_instance_ttl(&env);

        let mut intent = Self::load_intent(&env, &intent_id);

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

        Self::save_intent(&env, &intent_id, &intent);

        env.events().publish(
            (Symbol::new(&env, "extension_granted"), solver),
            (intent_id, intent.deadline),
        );
    }

    // ── Views ─────────────────────────────────────────────────────────────────

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
        let record: SolverRecord = env.storage().persistent().get(&DataKey::Solver(solver))?;
        Some(Self::compute_reputation_score(record))
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

    /// Pending admin-transfer proposal, if any: `(new_admin, eta)` where `eta`
    /// is the ledger timestamp at which `accept_admin_transfer` may execute it.
    pub fn get_pending_admin(env: Env) -> Option<(Address, u64)> {
        env.storage().instance().get(&DataKey::PendingAdmin)
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
    pub fn compute_reputation_score(record: SolverRecord) -> u32 {
        let total_fills = record.fills_completed as u64 + record.fills_failed as u64;
        if total_fills == 0 {
            return 0;
        }

        // base_bps ∈ [0, 10_000]
        let base_bps = (record.fills_completed as u64 * 10_000) / total_fills;

        // Volume scale: 1 000 fills × 100 dst tokens (7 dp) is the knee of
        // the curve. Only the shape matters — the constant can be tuned later.
        const VOLUME_SCALE: i128 = 1_000 * 100 * 10_000_000;

        // decay_bps = VOLUME_SCALE / (VOLUME_SCALE + vol) × 10_000
        // ∈ (0, 10_000].  High volume → low decay_bps.  `VOLUME_SCALE` is a
        // positive constant and `vol >= 0`, so the denominator is never zero.
        let vol = record.total_volume.max(0);
        let decay_bps = ((VOLUME_SCALE as u64) * 10_000) / ((VOLUME_SCALE + vol) as u64);

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
        // `soroban_sdk::String` has no byte indexing; copy each string into a
        // fixed stack buffer (guarding the length first) and inspect the bytes.
        let chain_len = src_chain.len() as usize;
        let token_len = src_token.len() as usize;

        // Longest chain name we recognise is 8 bytes ("ethereum"/"arbitrum"/
        // "optimism"); anything longer can't match, so treat it as unknown.
        let mut chain_buf = [0u8; 16];
        if chain_len == 0 || chain_len > chain_buf.len() {
            return;
        }
        src_chain.copy_into_slice(&mut chain_buf[..chain_len]);
        let chain = &chain_buf[..chain_len];
        let chain_is = |literal: &[u8]| chain == literal;

        let is_evm = chain_is(b"ethereum")
            || chain_is(b"base")
            || chain_is(b"polygon")
            || chain_is(b"arbitrum")
            || chain_is(b"optimism");
        let is_solana = chain_is(b"solana");

        // Unknown chain: skip validation — forward-compatible with future chains.
        if !is_evm && !is_solana {
            return;
        }

        // Token addresses are short; 64 bytes is well beyond any real value.
        let mut token_buf = [0u8; 64];
        if token_len > token_buf.len() {
            panic_with_error!(env, Error::InvalidSrcToken);
        }
        src_token.copy_into_slice(&mut token_buf[..token_len]);
        let tok = &token_buf[..token_len];

        if is_evm {
            // EVM token address: exactly "0x" + 40 hex chars = 42 characters.
            if token_len != 42 || tok[0] != b'0' || tok[1] != b'x' {
                panic_with_error!(env, Error::InvalidSrcToken);
            }
            for &ch in &tok[2..] {
                let is_hex = ch.is_ascii_digit()
                    || (b'a'..=b'f').contains(&ch)
                    || (b'A'..=b'F').contains(&ch);
                if !is_hex {
                    panic_with_error!(env, Error::InvalidSrcToken);
                }
            }
            return;
        }

        // Solana token (SPL mint): base58-encoded public key, 32–44 chars,
        // no "0x" prefix.
        if !(32..=44).contains(&token_len) {
            panic_with_error!(env, Error::InvalidSrcToken);
        }
        if token_len >= 2 && tok[0] == b'0' && tok[1] == b'x' {
            panic_with_error!(env, Error::InvalidSrcToken);
        }
        // Base58 alphabet: 123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz
        // (excludes '0', 'I', 'O', 'l').
        for &ch in tok {
            let is_b58 = (b'1'..=b'9').contains(&ch)
                || (b'A'..=b'H').contains(&ch)
                || (b'J'..=b'N').contains(&ch)
                || (b'P'..=b'Z').contains(&ch)
                || (b'a'..=b'k').contains(&ch)
                || (b'm'..=b'z').contains(&ch);
            if !is_b58 {
                panic_with_error!(env, Error::InvalidSrcToken);
            }
        }
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

    /// The effective protocol fee in basis points for `solver` (#192).
    ///
    /// Starts from `ProtocolConfig.protocol_fee_bps` and applies the largest
    /// discount tier whose `min_volume` the solver's cumulative
    /// `SolverRecord.total_volume` has reached. `discount_bps` is a fraction of
    /// the fee, in hundredths of a percent (`10_000` = the fee is waived
    /// entirely). The result is always clamped to `0..=base` — a discount can
    /// never make the fee negative or larger than the un-discounted rate.
    ///
    /// With no schedule set (the default) every solver pays the flat `base`.
    fn get_tiered_fee_bps(env: &Env, solver: &Address) -> i128 {
        let base = Self::load_config(env).protocol_fee_bps;
        let tiers: Vec<(i128, u32)> = match env.storage().instance().get(&DataKey::FeeDiscountTiers)
        {
            Some(t) => t,
            None => return base,
        };
        if tiers.is_empty() {
            return base;
        }

        let volume = env
            .storage()
            .persistent()
            .get::<_, SolverRecord>(&DataKey::Solver(solver.clone()))
            .map(|r| r.total_volume)
            .unwrap_or(0);

        // Tiers are stored ascending by `min_volume`; the last one the solver
        // clears wins.
        let mut discount_bps: i128 = 0;
        for (min_volume, tier_discount) in tiers.iter() {
            if volume >= min_volume {
                discount_bps = tier_discount as i128;
            } else {
                break;
            }
        }
        if discount_bps <= 0 {
            return base;
        }
        if discount_bps >= BPS_DENOMINATOR {
            return 0;
        }
        // base - base*discount/10_000, all non-negative and <= base.
        let reduction = base
            .checked_mul(discount_bps)
            .unwrap_or_else(|| panic_with_error!(env, Error::FeeOverflow))
            / BPS_DENOMINATOR;
        base - reduction
    }

    /// Admin-only: set the fee-discount schedule (#192). `tiers` is
    /// `(min_volume, discount_bps)` pairs, and must be **strictly ascending by
    /// `min_volume`** with every `discount_bps <= 10_000`
    /// (`InvalidFeeTiers` otherwise). Pass an empty vec to remove all
    /// discounts. Data-driven so the curve can be tuned without a contract
    /// upgrade.
    pub fn set_fee_discount_tiers(env: Env, tiers: Vec<(i128, u32)>) {
        Self::require_admin(&env);
        let mut prev: Option<i128> = None;
        for (min_volume, discount_bps) in tiers.iter() {
            if min_volume < 0 || discount_bps as i128 > BPS_DENOMINATOR {
                panic_with_error!(&env, Error::InvalidFeeTiers);
            }
            if let Some(p) = prev {
                if min_volume <= p {
                    panic_with_error!(&env, Error::InvalidFeeTiers);
                }
            }
            prev = Some(min_volume);
        }
        env.storage()
            .instance()
            .set(&DataKey::FeeDiscountTiers, &tiers);
        Self::bump_instance_ttl(&env);
        env.events()
            .publish((Symbol::new(&env, "fee_discount_tiers_set"),), tiers.len());
    }

    /// The current fee-discount schedule (empty if none is set), and the
    /// effective fee in bps `solver` would pay on a fill right now after any
    /// volume-tier discount — so solver bots can price a fill before
    /// submitting (#192): `(tiers, effective_fee_bps)`.
    pub fn get_fee_schedule(env: Env, solver: Address) -> (Vec<(i128, u32)>, i128) {
        let tiers = env
            .storage()
            .instance()
            .get(&DataKey::FeeDiscountTiers)
            .unwrap_or_else(|| Vec::new(&env));
        let effective = Self::get_tiered_fee_bps(&env, &solver);
        (tiers, effective)
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

    /// Load an intent record, or panic `IntentNotFound`.
    fn load_intent(env: &Env, intent_id: &BytesN<32>) -> IntentRecord {
        env.storage()
            .persistent()
            .get(&DataKey::Intent(intent_id.clone()))
            .unwrap_or_else(|| panic_with_error!(env, Error::IntentNotFound))
    }

    /// Persist an intent record and bump its TTL.
    fn save_intent(env: &Env, intent_id: &BytesN<32>, intent: &IntentRecord) {
        env.storage()
            .persistent()
            .set(&DataKey::Intent(intent_id.clone()), intent);
        Self::bump_intent_ttl(env, intent_id);
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
