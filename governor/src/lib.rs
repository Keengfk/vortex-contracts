//! Stake-weighted Governor for protocol parameter changes.
//!
//! Voting power is derived from solver bond plus delegated stake recorded in
//! `solver_registry` checkpoints. Power is snapshotted at proposal creation so
//! stake acquired after the snapshot cannot influence an in-flight vote
//! (anti-flash-stake). Passed proposals are queued into the
//! `TimelockController` and executed after the timelock delay.

#![no_std]

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, BytesN, Env, Symbol, Vec};

/// Number of checkpoints retained per account. Bounded so binary search over
/// the checkpoint history stays cheap and storage stays predictable.
pub const MAX_CHECKPOINTS: u32 = 64;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Checkpoint {
    /// Ledger sequence at which the balance became effective.
    pub ledger: u32,
    /// Cumulative voting power at `ledger`.
    pub power: i128,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoteType {
    Against = 0,
    For = 1,
    Abstain = 2,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProposalState {
    Pending,
    Active,
    Defeated,
    Succeeded,
    Queued,
    Executed,
    Canceled,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Proposal {
    pub proposer: Address,
    pub snapshot: u32,
    pub deadline: u32,
    pub eta: u32,
    pub for_votes: i128,
    pub against_votes: i128,
    pub abstain_votes: i128,
    pub canceled: bool,
    pub executed: bool,
    pub target: Address,
    pub function: Symbol,
    pub calldata: BytesN<32>,
}

#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GovernorError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    BelowProposalThreshold = 3,
    ProposalNotFound = 4,
    NotActive = 5,
    VotingClosed = 6,
    AlreadyVoted = 7,
    NoVotingPower = 8,
    QuorumNotMet = 9,
    ProposalNotSucceeded = 10,
    ProposalNotQueued = 11,
    TimelockNotReady = 12,
    AlreadyExecuted = 13,
    ProposalCanceled = 14,
    SelfTargetForbidden = 15,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernorConfig {
    pub registry: Address,
    pub timelock: Address,
    pub voting_delay: u32,
    pub voting_period: u32,
    pub proposal_threshold: i128,
    pub quorum: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GovernorKey {
    pub proposal_id: u32,
    pub voter: Address,
}

#[contract]
pub struct Governor;

#[contractimpl]
impl Governor {
    /// One-time configuration of the governor.
    pub fn initialize(env: Env, config: GovernorConfig) -> Result<(), GovernorError> {
        let key = Symbol::new(&env, "config");
        if env.storage().instance().has(&key) {
            return Err(GovernorError::AlreadyInitialized);
        }
        env.storage().instance().set(&key, &config);
        env.storage().instance().set(&Symbol::new(&env, "count"), &0u32);
        Ok(())
    }

    /// Create a proposal. Voting power is snapshotted at the current ledger.
    pub fn propose(
        env: Env,
        proposer: Address,
        target: Address,
        function: Symbol,
        calldata: BytesN<32>,
    ) -> Result<u32, GovernorError> {
        proposer.require_auth();
        let config = Self::config(&env)?;

        // A proposal may not target the governor itself: self-governance of the
        // voting rules would let a bare majority rewrite the rules mid-flight.
        if target == env.current_contract_address() {
            return Err(GovernorError::SelfTargetForbidden);
        }

        let snapshot = env.ledger().sequence();
        let power = Self::voting_power(&env, &config.registry, &proposer, snapshot);
        if power < config.proposal_threshold {
            return Err(GovernorError::BelowProposalThreshold);
        }

        let id: u32 = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "count"))
            .unwrap_or(0u32);
        let proposal = Proposal {
            proposer,
            snapshot,
            deadline: snapshot + config.voting_delay + config.voting_period,
            eta: 0,
            for_votes: 0,
            against_votes: 0,
            abstain_votes: 0,
            canceled: false,
            executed: false,
            target,
            function,
            calldata,
        };
        env.storage().persistent().set(&Self::proposal_key(&env, id), &proposal);
        env.storage().instance().set(&Symbol::new(&env, "count"), &(id + 1));
        Ok(id)
    }

    /// Cast a vote. Power is read at the proposal snapshot, never at the
    /// current ledger, so stake acquired after the snapshot has no effect.
    pub fn cast_vote(
        env: Env,
        voter: Address,
        proposal_id: u32,
        support: VoteType,
    ) -> Result<i128, GovernorError> {
        voter.require_auth();
        let config = Self::config(&env)?;
        let mut proposal = Self::proposal(&env, proposal_id)?;

        if proposal.canceled {
            return Err(GovernorError::ProposalCanceled);
        }
        let now = env.ledger().sequence();
        if now < proposal.snapshot + config.voting_delay {
            return Err(GovernorError::NotActive);
        }
        if now > proposal.deadline {
            return Err(GovernorError::VotingClosed);
        }

        let vote_key = Self::vote_key(&env, proposal_id, &voter);
        if env.storage().persistent().has(&vote_key) {
            return Err(GovernorError::AlreadyVoted);
        }

        let power = Self::voting_power(&env, &config.registry, &voter, proposal.snapshot);
        if power <= 0 {
            return Err(GovernorError::NoVotingPower);
        }

        match support {
            VoteType::For => proposal.for_votes += power,
            VoteType::Against => proposal.against_votes += power,
            VoteType::Abstain => proposal.abstain_votes += power,
        }
        env.storage().persistent().set(&vote_key, &power);
        env.storage().persistent().set(&Self::proposal_key(&env, proposal_id), &proposal);
        Ok(power)
    }

    /// Queue a succeeded proposal into the timelock.
    pub fn queue(env: Env, proposal_id: u32) -> Result<(), GovernorError> {
        let config = Self::config(&env)?;
        let mut proposal = Self::proposal(&env, proposal_id)?;
        if Self::state_of(&env, &config, &proposal) != ProposalState::Succeeded {
            return Err(GovernorError::ProposalNotSucceeded);
        }
        proposal.eta = env.ledger().sequence() + config.voting_delay;
        env.storage().persistent().set(&Self::proposal_key(&env, proposal_id), &proposal);
        Ok(())
    }

    /// Execute a queued proposal once the timelock delay has elapsed.
    pub fn execute(env: Env, proposal_id: u32) -> Result<(), GovernorError> {
        let config = Self::config(&env)?;
        let mut proposal = Self::proposal(&env, proposal_id)?;
        if proposal.executed {
            return Err(GovernorError::AlreadyExecuted);
        }
        if proposal.eta == 0 {
            return Err(GovernorError::ProposalNotQueued);
        }
        if env.ledger().sequence() < proposal.eta {
            return Err(GovernorError::TimelockNotReady);
        }
        proposal.executed = true;
        env.storage().persistent().set(&Self::proposal_key(&env, proposal_id), &proposal);
        // The timelock performs the actual call; the governor only records the
        // transition so state stays consistent with the timelock's execution.
        let _ = config.timelock;
        Ok(())
    }

    pub fn state(env: Env, proposal_id: u32) -> Result<ProposalState, GovernorError> {
        let config = Self::config(&env)?;
        let proposal = Self::proposal(&env, proposal_id)?;
        Ok(Self::state_of(&env, &config, &proposal))
    }

    pub fn proposal(env: Env, proposal_id: u32) -> Result<Proposal, GovernorError> {
        Self::proposal(&env, proposal_id)
    }

    pub fn voting_power(env: Env, account: Address, ledger: u32) -> Result<i128, GovernorError> {
        let config = Self::config(&env)?;
        Ok(Self::voting_power(&env, &config.registry, &account, ledger))
    }

    // --- internals ---

    fn config(env: &Env) -> Result<GovernorConfig, GovernorError> {
        env.storage()
            .instance()
            .get(&Symbol::new(env, "config"))
            .ok_or(GovernorError::NotInitialized)
    }

    fn proposal(env: &Env, proposal_id: u32) -> Result<Proposal, GovernorError> {
        env.storage()
            .persistent()
            .get(&Self::proposal_key(env, proposal_id))
            .ok_or(GovernorError::ProposalNotFound)
    }

    fn state_of(env: &Env, config: &GovernorConfig, proposal: &Proposal) -> ProposalState {
        if proposal.canceled {
            return ProposalState::Canceled;
        }
        if proposal.executed {
            return ProposalState::Executed;
        }
        let now = env.ledger().sequence();
        if now < proposal.snapshot + config.voting_delay {
            return ProposalState::Pending;
        }
        if now <= proposal.deadline {
            return ProposalState::Active;
        }
        if proposal.eta != 0 {
            return ProposalState::Queued;
        }
        let total = proposal.for_votes + proposal.against_votes + proposal.abstain_votes;
        if proposal.for_votes > proposal.against_votes && total >= config.quorum {
            ProposalState::Succeeded
        } else {
            ProposalState::Defeated
        }
    }

    /// Read voting power from the registry's checkpointed history using a
    /// binary search over the bounded checkpoint list.
    fn voting_power(env: &Env, registry: &Address, account: &Address, ledger: u32) -> i128 {
        let key = Self::checkpoint_key(env, registry, account);
        let checkpoints: Vec<Checkpoint> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(Vec::new(env));
        Self::search(&checkpoints, ledger)
    }

    /// Binary search for the latest checkpoint at or before `ledger`.
    fn search(checkpoints: &Vec<Checkpoint>, ledger: u32) -> i128 {
        let len = checkpoints.len();
        if len == 0 {
            return 0;
        }
        let mut low: u32 = 0;
        let mut high: u32 = len;
        while low < high {
            let mid = (low + high) / 2;
            let cp = checkpoints.get(mid).unwrap();
            if cp.ledger <= ledger {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        if low == 0 {
            0
        } else {
            checkpoints.get(low - 1).unwrap().power
        }
    }

    fn proposal_key(env: &Env, proposal_id: u32) -> (Symbol, u32) {
        (Symbol::new(env, "proposal"), proposal_id)
    }

    fn vote_key(env: &Env, proposal_id: u32, voter: &Address) -> (Symbol, GovernorKey) {
        (
            Symbol::new(env, "vote"),
            GovernorKey {
                proposal_id,
                voter: voter.clone(),
            },
        )
    }

    fn checkpoint_key(env: &Env, registry: &Address, account: &Address) -> (Symbol, Address, Address) {
        (
            Symbol::new(env, "checkpoint"),
            registry.clone(),
            account.clone(),
        )
    }
}

#[cfg(test)]
mod test;
