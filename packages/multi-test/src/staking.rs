use std::collections::{BTreeSet, VecDeque};

use anyhow::{anyhow, bail, Result as AnyResult};
use schemars::JsonSchema;

use cosmwasm_std::{
    coin, ensure, ensure_eq, to_binary, Addr, AllDelegationsResponse, AllValidatorsResponse, Api,
    BankMsg, Binary, BlockInfo, BondedDenomResponse, Coin, CustomQuery, Decimal, Delegation,
    DelegationResponse, DistributionMsg, Empty, Event, FullDelegation, Querier, StakingMsg,
    StakingQuery, Storage, Timestamp, Uint128, Validator, ValidatorResponse,
};
use cosmwasm_storage::{prefixed, prefixed_read};
use cw_storage_plus::{Item, Map};
use serde::{Deserialize, Serialize};

use crate::app::CosmosRouter;
use crate::executor::AppResponse;
use crate::{BankSudo, Module};

// Contains some general staking parameters
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
pub struct StakingInfo {
    /// The denominator of the staking token
    bonded_denom: String,
    /// Time between unbonding and receiving tokens in seconds
    unbonding_time: u64,
    /// Interest rate per year (60 * 60 * 24 * 365 seconds)
    apr: Decimal,
}

/// The number of (conceptual) shares of this validator the staker has. These can be fractional shares
/// Used to calculate the stake. If the validator is slashed, this might not be the same as the stake.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
struct Shares(Decimal);

impl Shares {
    /// The stake of this delegator. Make sure to pass the correct validator in
    pub fn stake(&self, validator: &ValidatorInfo) -> Uint128 {
        self.0 / validator.total_shares * validator.stake
    }

    pub fn rewards(&self, validator: &ValidatorInfo, rewards: Decimal) -> Decimal {
        self.0 * rewards / validator.total_shares
    }
}

/// Holds some operational data about a validator
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
struct ValidatorInfo {
    /// The stakers that have staked with this validator
    stakers: BTreeSet<Addr>,
    /// The whole stake of all stakers
    stake: Uint128,
    /// The block time when this validator's rewards were last update. This is needed for rewards calculation.
    last_rewards_calculation: Timestamp,
    /// The total number of shares this validator has issued, only used internally for calculating rewards
    total_shares: Decimal,
    /// The number of available rewards. This is updated in `calculate_rewards`.
    /// It is needed to save the current rewards somewhere before adding / removing stake,
    /// since the new stake should only apply to future interest, not past interest.
    calculated_rewards: Decimal,
}

impl ValidatorInfo {
    pub fn new(block_time: Timestamp) -> Self {
        Self {
            stakers: BTreeSet::new(),
            stake: Uint128::zero(),
            last_rewards_calculation: block_time,
            total_shares: Decimal::zero(),
            calculated_rewards: Decimal::zero(),
        }
    }
    /// Returns the amount of shares a delegator gets for staking the given amount of tokens (bonded_denom) at this point in time.
    /// This should usually be `1:1` unless the delegator was slashed.
    pub fn shares_for(&self, stake: Uint128) -> Decimal {
        if self.stake.is_zero() {
            // first staker always gets 1:1
            Decimal::one()
        } else {
            Decimal::from_ratio(stake, 1u128) * self.total_shares
                / Decimal::from_ratio(self.stake, 1u128)
        }
    }
}

const STAKING_INFO: Item<StakingInfo> = Item::new("staking_info");
const STAKES: Map<(&Addr, &Addr), Shares> = Map::new("stakes");
const VALIDATOR_MAP: Map<&Addr, Validator> = Map::new("validator_map");
/// Additional vec of validators, in case the `iterator` feature is disabled
const VALIDATORS: Item<Vec<Validator>> = Item::new("validators");
/// Contains additional info for each validator
const VALIDATOR_INFO: Map<&Addr, ValidatorInfo> = Map::new("validator_info");
/// The queue of unbonding operations. This is needed because unbonding has a waiting time. See [`StakeKeeper`]
/// TODO: replace with `Deque`
const UNBONDING_QUEUE: Item<VecDeque<(Addr, Timestamp, u128)>> = Item::new("unbonding_queue");

pub const NAMESPACE_STAKING: &[u8] = b"staking";

// We need to expand on this, but we will need this to properly test out staking
#[derive(Clone, std::fmt::Debug, PartialEq, Eq, JsonSchema)]
pub enum StakingSudo {
    /// Slashes the given percentage of the validator's stake.
    /// For now, you cannot slash after the fact in tests.
    Slash {
        validator: String,
        percentage: Decimal,
    },
    /// Causes the unbonding queue to be processed.
    /// This needs to be triggered manually, since there is no good place to do this right now.
    /// In cosmos-sdk, this is done in `EndBlock`, but we don't have that here.
    ProcessQueue {},
}

pub trait Staking: Module<ExecT = StakingMsg, QueryT = StakingQuery, SudoT = StakingSudo> {}

pub trait Distribution: Module<ExecT = DistributionMsg, QueryT = Empty, SudoT = Empty> {}

pub struct StakeKeeper {
    module_addr: Addr,
}

impl Default for StakeKeeper {
    fn default() -> Self {
        Self::new()
    }
}

impl StakeKeeper {
    pub fn new() -> Self {
        StakeKeeper {
            // The address of the staking module. This holds all staked tokens.
            module_addr: Addr::unchecked("staking_module"),
        }
    }

    /// Provides some general parameters to the stake keeper
    pub fn setup(&self, storage: &mut dyn Storage, staking_info: StakingInfo) -> AnyResult<()> {
        let mut storage = prefixed(storage, NAMESPACE_STAKING);

        STAKING_INFO.save(&mut storage, &staking_info)?;
        Ok(())
    }

    pub fn init_stake(
        &self,
        storage: &mut dyn Storage,
        block: &BlockInfo,
        account: &Addr,
        validator: &Addr,
        amount: Coin,
    ) -> AnyResult<()> {
        let mut storage = prefixed(storage, NAMESPACE_STAKING);

        self.add_stake(&mut storage, block, account, validator, amount)
    }

    /// Add a new validator available for staking
    pub fn add_validator(
        &self,
        api: &dyn Api,
        storage: &mut dyn Storage,
        block: &BlockInfo,
        validator: Validator,
    ) -> AnyResult<()> {
        let mut storage = prefixed(storage, NAMESPACE_STAKING);

        let val_addr = api.addr_validate(&validator.address)?;
        if VALIDATOR_MAP.may_load(&storage, &val_addr)?.is_some() {
            bail!(
                "Cannot add validator {}, since a validator with that address already exists",
                val_addr
            );
        }

        VALIDATOR_MAP.save(&mut storage, &val_addr, &validator)?;
        let mut vec = VALIDATORS.may_load(&storage)?.unwrap_or_default();
        vec.push(validator);
        VALIDATORS.save(&mut storage, &vec)?;
        VALIDATOR_INFO.save(&mut storage, &val_addr, &ValidatorInfo::new(block.time))?;
        Ok(())
    }

    fn get_staking_info(staking_storage: &dyn Storage) -> AnyResult<StakingInfo> {
        Ok(STAKING_INFO
            .may_load(staking_storage)?
            .unwrap_or_else(|| StakingInfo {
                bonded_denom: "TOKEN".to_string(),
                unbonding_time: 60,
                apr: Decimal::percent(10),
            }))
    }

    /// Returns the rewards of the given delegator at the given validator
    pub fn get_rewards(
        &self,
        storage: &dyn Storage,
        block: &BlockInfo,
        delegator: &Addr,
        validator: &Addr,
    ) -> AnyResult<Option<Coin>> {
        let staking_storage = prefixed_read(storage, NAMESPACE_STAKING);

        let validator_obj = match self.get_validator(&staking_storage, validator)? {
            Some(validator) => validator,
            None => bail!("non-existent validator {}", validator),
        };
        // calculate rewards using fixed ratio
        let shares = match STAKES.load(&staking_storage, (delegator, validator)) {
            Ok(stakes) => stakes,
            Err(_) => {
                return Ok(None);
            }
        };
        let validator_info = VALIDATOR_INFO.load(&staking_storage, validator)?;

        Self::get_rewards_internal(
            &staking_storage,
            block,
            &shares,
            &validator_obj,
            &validator_info,
        )
        .map(Some)
    }

    fn get_rewards_internal(
        staking_storage: &dyn Storage,
        block: &BlockInfo,
        shares: &Shares,
        validator: &Validator,
        validator_info: &ValidatorInfo,
    ) -> AnyResult<Coin> {
        let staking_info = Self::get_staking_info(staking_storage)?;

        println!(
            "old delegator rewards: {} * {} / {}",
            validator_info.calculated_rewards, shares.0, validator_info.total_shares
        );

        // calculate missing rewards without updating the validator to reduce rounding errors
        let missing_validator_rewards = Self::calculate_rewards(
            block.time,
            validator_info.last_rewards_calculation,
            staking_info.apr,
            validator.commission,
            validator_info.stake,
        );
        let validator_rewards = validator_info.calculated_rewards + missing_validator_rewards;

        // calculate the delegator's share of those
        let delegator_rewards = shares.rewards(validator_info, validator_rewards);

        println!(
            "new validator / delegator rewards: {} / {}",
            validator_rewards, delegator_rewards
        );

        Ok(Coin {
            denom: staking_info.bonded_denom,
            amount: Uint128::new(1) * delegator_rewards, // multiplying by 1 to convert Decimal to Uint128
        })
    }

    /// Calculates the rewards that are due since the last calculation.
    fn calculate_rewards(
        current_time: Timestamp,
        since: Timestamp,
        interest_rate: Decimal,
        validator_commission: Decimal,
        stake: Uint128,
    ) -> Decimal {
        // calculate time since last update (in seconds)
        let time_diff = current_time.minus_seconds(since.seconds()).seconds();

        // using decimal here to reduce rounding error when calling this function a lot
        let reward = Decimal::from_ratio(stake, 1u128)
            * interest_rate
            * Decimal::from_ratio(time_diff, 1u128)
            / Decimal::from_ratio(60u128 * 60 * 24 * 365, 1u128);
        let commission = reward * validator_commission;

        println!(
            "calculated new: 10% * {} - 10% comm. = {}",
            stake,
            reward - commission
        );

        reward - commission
    }

    /// Updates the staking reward for the given validator. This mutates the validator info,
    /// but does not save it.
    /// Always call this to update rewards before changing a validator stake.
    fn update_rewards(
        block: &BlockInfo,
        staking_info: &StakingInfo,
        validator_info: &mut ValidatorInfo,
        validator: &Validator,
    ) -> AnyResult<()> {
        if validator_info.last_rewards_calculation >= block.time {
            return Ok(());
        }

        let new_rewards = Self::calculate_rewards(
            block.time,
            validator_info.last_rewards_calculation,
            staking_info.apr,
            validator.commission,
            validator_info.stake,
        );

        // update validator info, but only if there is at least 1 new token
        // Less than one token would not change anything, as only full tokens are presented
        // outside of the keeper.
        if new_rewards >= Decimal::one() {
            validator_info.last_rewards_calculation = block.time;
            validator_info.calculated_rewards += new_rewards;
        }
        Ok(())
    }

    /// Returns the single validator with the given address (or `None` if there is no such validator)
    fn get_validator(
        &self,
        staking_storage: &dyn Storage,
        address: &Addr,
    ) -> AnyResult<Option<Validator>> {
        Ok(VALIDATOR_MAP.may_load(staking_storage, address)?)
    }

    /// Returns all available validators
    fn get_validators(&self, staking_storage: &dyn Storage) -> AnyResult<Vec<Validator>> {
        Ok(VALIDATORS.may_load(staking_storage)?.unwrap_or_default())
    }

    fn get_stake(
        &self,
        staking_storage: &dyn Storage,
        account: &Addr,
        validator: &Addr,
    ) -> AnyResult<Coin> {
        let shares = STAKES.may_load(staking_storage, (account, validator))?;
        let staking_info = Self::get_staking_info(staking_storage)?;
        let validator_info = VALIDATOR_INFO.may_load(staking_storage, validator)?;
        Ok(Coin {
            amount: shares
                .zip(validator_info)
                .map(|(s, validator_info)| s.stake(&validator_info))
                .unwrap_or_default(),
            denom: staking_info.bonded_denom,
        })
    }

    fn add_stake(
        &self,
        staking_storage: &mut dyn Storage,
        block: &BlockInfo,
        to_address: &Addr,
        validator: &Addr,
        amount: Coin,
    ) -> AnyResult<()> {
        self.validate_denom(staking_storage, &amount)?;
        self.validate_nonzero(&amount)?;
        self.update_stake(
            staking_storage,
            block,
            to_address,
            validator,
            amount.amount,
            false,
        )
    }

    fn remove_stake(
        &self,
        staking_storage: &mut dyn Storage,
        block: &BlockInfo,
        from_address: &Addr,
        validator: &Addr,
        amount: Coin,
    ) -> AnyResult<()> {
        self.validate_denom(staking_storage, &amount)?;
        self.validate_nonzero(&amount)?;
        self.update_stake(
            staking_storage,
            block,
            from_address,
            validator,
            amount.amount,
            true,
        )
    }

    fn update_stake(
        &self,
        staking_storage: &mut dyn Storage,
        block: &BlockInfo,
        delegator: &Addr,
        validator: &Addr,
        amount: impl Into<Uint128>,
        sub: bool,
    ) -> AnyResult<()> {
        let amount = amount.into();

        let mut validator_info = VALIDATOR_INFO
            .may_load(staking_storage, validator)?
            .unwrap_or_else(|| ValidatorInfo::new(block.time));
        let mut stake_info = STAKES
            .may_load(staking_storage, (delegator, validator))?
            .unwrap_or_else(|| Shares(Decimal::zero()));

        // update rewards for this validator
        if !amount.is_zero() {
            let validator_obj = VALIDATOR_MAP.load(staking_storage, validator)?;
            let staking_info = Self::get_staking_info(staking_storage)?;
            Self::update_rewards(block, &staking_info, &mut validator_info, &validator_obj)?;
        }

        // now, we can update the stake
        if sub {
            let shares = validator_info.shares_for(amount);
            stake_info.0 -= shares;

            validator_info.stake = validator_info.stake.checked_sub(amount)?;
            validator_info.total_shares -= shares;
        } else {
            let new_shares = validator_info.shares_for(amount);
            stake_info.0 += new_shares;

            validator_info.stake = validator_info.stake.checked_add(amount)?;
            validator_info.total_shares += new_shares;
        }

        // save updated values
        if stake_info.0.is_zero() {
            // no more stake, so remove
            STAKES.remove(staking_storage, (delegator, validator));
            validator_info.stakers.remove(delegator);
        } else {
            STAKES.save(staking_storage, (delegator, validator), &stake_info)?;
            validator_info.stakers.insert(delegator.clone());
        }
        // save updated validator info
        VALIDATOR_INFO.save(staking_storage, validator, &validator_info)?;

        Ok(())
    }

    fn slash(
        &self,
        staking_storage: &mut dyn Storage,
        validator: &Addr,
        percentage: Decimal,
    ) -> AnyResult<()> {
        let mut validator_info = VALIDATOR_INFO
            .may_load(staking_storage, validator)?
            .ok_or_else(|| anyhow!("validator not found"))?;

        // TODO: handle rewards? Either update them before slashing or set them to zero, depending on the slashing logic

        let remaining_percentage = Decimal::one() - percentage;
        validator_info.stake = validator_info.stake * remaining_percentage;

        // if the stake is completely gone, we clear all stakers and reinitialize the validator
        if validator_info.stake.is_zero() {
            // need to remove all stakes
            for delegator in validator_info.stakers.iter() {
                STAKES.remove(staking_storage, (delegator, validator));
            }
            validator_info.stakers.clear();
            validator_info.total_shares = Decimal::zero();
        }
        VALIDATOR_INFO.save(staking_storage, validator, &validator_info)?;
        Ok(())
    }

    fn validate_nonzero(&self, amount: &Coin) -> AnyResult<()> {
        ensure!(!amount.amount.is_zero(), anyhow!("cannot delegate 0 coins"));
        Ok(())
    }

    // Asserts that the given coin has the proper denominator
    fn validate_denom(&self, staking_storage: &dyn Storage, amount: &Coin) -> AnyResult<()> {
        let staking_info = Self::get_staking_info(staking_storage)?;
        ensure_eq!(
            amount.denom,
            staking_info.bonded_denom,
            anyhow!(
                "cannot delegate coins of denominator {}, only of {}",
                amount.denom,
                staking_info.bonded_denom
            )
        );
        Ok(())
    }

    // Asserts that the given coin has the proper denominator
    fn validate_percentage(&self, percentage: Decimal) -> AnyResult<()> {
        ensure!(percentage <= Decimal::one(), anyhow!("expected percentage"));
        Ok(())
    }
}

impl Staking for StakeKeeper {}

impl Module for StakeKeeper {
    type ExecT = StakingMsg;
    type QueryT = StakingQuery;
    type SudoT = StakingSudo;

    fn execute<ExecC, QueryC: CustomQuery>(
        &self,
        api: &dyn Api,
        storage: &mut dyn Storage,
        router: &dyn CosmosRouter<ExecC = ExecC, QueryC = QueryC>,
        block: &BlockInfo,
        sender: Addr,
        msg: StakingMsg,
    ) -> AnyResult<AppResponse> {
        let mut staking_storage = prefixed(storage, NAMESPACE_STAKING);
        match msg {
            StakingMsg::Delegate { validator, amount } => {
                let validator = api.addr_validate(&validator)?;

                // see https://github.com/cosmos/cosmos-sdk/blob/v0.46.1/x/staking/keeper/msg_server.go#L251-L256
                let events = vec![Event::new("delegate")
                    .add_attribute("validator", &validator)
                    .add_attribute("amount", format!("{}{}", amount.amount, amount.denom))
                    .add_attribute("new_shares", amount.amount.to_string())]; // TODO: calculate shares?
                self.add_stake(
                    &mut staking_storage,
                    block,
                    &sender,
                    &validator,
                    amount.clone(),
                )?;
                // move money from sender account to this module (note we can controller sender here)
                router.execute(
                    api,
                    storage,
                    block,
                    sender,
                    BankMsg::Send {
                        to_address: self.module_addr.to_string(),
                        amount: vec![amount],
                    }
                    .into(),
                )?;
                Ok(AppResponse { events, data: None })
            }
            StakingMsg::Undelegate { validator, amount } => {
                let validator = api.addr_validate(&validator)?;
                self.validate_denom(&staking_storage, &amount)?;
                self.validate_nonzero(&amount)?;

                // see https://github.com/cosmos/cosmos-sdk/blob/v0.46.1/x/staking/keeper/msg_server.go#L378-L383
                let events = vec![Event::new("unbond")
                    .add_attribute("validator", &validator)
                    .add_attribute("amount", format!("{}{}", amount.amount, amount.denom))
                    .add_attribute("completion_time", "2022-09-27T14:00:00+00:00")]; // TODO: actual date?
                self.remove_stake(
                    &mut staking_storage,
                    block,
                    &sender,
                    &validator,
                    amount.clone(),
                )?;
                // add tokens to unbonding queue
                let staking_info = Self::get_staking_info(&staking_storage)?;
                let mut queue = UNBONDING_QUEUE
                    .may_load(&staking_storage)?
                    .unwrap_or_default();
                queue.push_back((
                    sender.clone(),
                    block.time.plus_seconds(staking_info.unbonding_time),
                    amount.amount.u128(),
                ));
                Ok(AppResponse { events, data: None })
            }
            StakingMsg::Redelegate {
                src_validator,
                dst_validator,
                amount,
            } => {
                let src_validator = api.addr_validate(&src_validator)?;
                let dst_validator = api.addr_validate(&dst_validator)?;
                // see https://github.com/cosmos/cosmos-sdk/blob/v0.46.1/x/staking/keeper/msg_server.go#L316-L322
                let events = vec![Event::new("redelegate")
                    .add_attribute("source_validator", &src_validator)
                    .add_attribute("destination_validator", &dst_validator)
                    .add_attribute("amount", format!("{}{}", amount.amount, amount.denom))];

                self.remove_stake(
                    &mut staking_storage,
                    block,
                    &sender,
                    &src_validator,
                    amount.clone(),
                )?;
                self.add_stake(&mut staking_storage, block, &sender, &dst_validator, amount)?;

                Ok(AppResponse { events, data: None })
            }
            m => bail!("Unsupported staking message: {:?}", m),
        }
    }

    fn sudo<ExecC, QueryC: CustomQuery>(
        &self,
        api: &dyn Api,
        storage: &mut dyn Storage,
        router: &dyn CosmosRouter<ExecC = ExecC, QueryC = QueryC>,
        block: &BlockInfo,
        msg: StakingSudo,
    ) -> AnyResult<AppResponse> {
        let mut staking_storage = prefixed(storage, NAMESPACE_STAKING);
        match msg {
            StakingSudo::Slash {
                validator,
                percentage,
            } => {
                let validator = api.addr_validate(&validator)?;
                self.validate_percentage(percentage)?;

                self.slash(&mut staking_storage, &validator, percentage)?;

                Ok(AppResponse::default())
            }
            StakingSudo::ProcessQueue {} => {
                let mut queue = UNBONDING_QUEUE
                    .may_load(&staking_storage)?
                    .unwrap_or_default();

                loop {
                    match queue.front() {
                        // assuming the queue is sorted by payout_at
                        Some((_, payout_at, _)) if payout_at <= &block.time => {
                            // remove from queue
                            let (delegator, _, amount) = queue.pop_front().unwrap();

                            let staking_storage = prefixed_read(storage, NAMESPACE_STAKING);
                            let staking_info = Self::get_staking_info(&staking_storage)?;
                            router.execute(
                                api,
                                storage,
                                block,
                                self.module_addr.clone(),
                                BankMsg::Send {
                                    to_address: delegator.into_string(),
                                    amount: vec![coin(amount, &staking_info.bonded_denom)],
                                }
                                .into(),
                            )?;
                        }
                        _ => break,
                    }
                }
                Ok(AppResponse::default())
            }
        }
    }

    fn query(
        &self,
        api: &dyn Api,
        storage: &dyn Storage,
        _querier: &dyn Querier,
        block: &BlockInfo,
        request: StakingQuery,
    ) -> AnyResult<Binary> {
        let staking_storage = prefixed_read(storage, NAMESPACE_STAKING);
        match request {
            StakingQuery::BondedDenom {} => Ok(to_binary(&BondedDenomResponse {
                denom: Self::get_staking_info(&staking_storage)?.bonded_denom,
            })?),
            StakingQuery::AllDelegations { delegator } => {
                let delegator = api.addr_validate(&delegator)?;
                let validators = self.get_validators(&staking_storage)?;

                let res: AnyResult<Vec<Delegation>> = validators
                    .into_iter()
                    .map(|validator| {
                        let delegator = delegator.clone();
                        let amount = self.get_stake(
                            &staking_storage,
                            &delegator,
                            &Addr::unchecked(&validator.address),
                        )?;

                        Ok(Delegation {
                            delegator,
                            validator: validator.address,
                            amount,
                        })
                    })
                    .collect();

                Ok(to_binary(&AllDelegationsResponse { delegations: res? })?)
            }
            StakingQuery::Delegation {
                delegator,
                validator,
            } => {
                let validator_addr = Addr::unchecked(&validator);
                let validator_obj = match self.get_validator(&staking_storage, &validator_addr)? {
                    Some(validator) => validator,
                    None => bail!("non-existent validator {}", validator),
                };
                let delegator = api.addr_validate(&delegator)?;
                // calculate rewards using fixed ratio
                let shares = match STAKES.load(&staking_storage, (&delegator, &validator_addr)) {
                    Ok(stakes) => stakes,
                    Err(_) => {
                        let response = DelegationResponse { delegation: None };
                        return Ok(to_binary(&response)?);
                    }
                };
                let validator_info = VALIDATOR_INFO.load(&staking_storage, &validator_addr)?;
                let stakes = shares.stake(&validator_info);
                let reward = Self::get_rewards_internal(
                    &staking_storage,
                    block,
                    &shares,
                    &validator_obj,
                    &validator_info,
                )?;
                let staking_info = Self::get_staking_info(&staking_storage)?;
                let full_delegation_response = DelegationResponse {
                    delegation: Some(FullDelegation {
                        delegator,
                        validator,
                        amount: coin(stakes.u128(), staking_info.bonded_denom),
                        can_redelegate: coin(0, "testcoin"),
                        accumulated_rewards: vec![reward],
                    }),
                };

                let res = to_binary(&full_delegation_response)?;
                Ok(res)
            }
            StakingQuery::AllValidators {} => Ok(to_binary(&AllValidatorsResponse {
                validators: self.get_validators(&staking_storage)?,
            })?),
            StakingQuery::Validator { address } => Ok(to_binary(&ValidatorResponse {
                validator: self.get_validator(&staking_storage, &Addr::unchecked(address))?,
            })?),
            q => bail!("Unsupported staking sudo message: {:?}", q),
        }
    }
}

#[derive(Default)]
pub struct DistributionKeeper {}

impl DistributionKeeper {
    pub fn new() -> Self {
        DistributionKeeper {}
    }
}

impl Distribution for DistributionKeeper {}

impl Module for DistributionKeeper {
    type ExecT = DistributionMsg;
    type QueryT = Empty;
    type SudoT = Empty;

    fn execute<ExecC, QueryC: CustomQuery>(
        &self,
        api: &dyn Api,
        storage: &mut dyn Storage,
        router: &dyn CosmosRouter<ExecC = ExecC, QueryC = QueryC>,
        block: &BlockInfo,
        sender: Addr,
        msg: DistributionMsg,
    ) -> AnyResult<AppResponse> {
        let mut staking_storage = prefixed(storage, NAMESPACE_STAKING);
        match msg {
            DistributionMsg::WithdrawDelegatorReward { validator } => {
                let validator_addr = api.addr_validate(&validator)?;

                let staking_info = STAKING_INFO.load(&staking_storage)?;
                let mut validator_info = VALIDATOR_INFO.load(&staking_storage, &validator_addr)?;
                let validator_obj = VALIDATOR_MAP.load(&staking_storage, &validator_addr)?;

                // update the validator's rewards
                StakeKeeper::update_rewards(
                    block,
                    &staking_info,
                    &mut validator_info,
                    &validator_obj,
                )?;

                // remove delegator's share of the rewards
                let shares = STAKES.load(&staking_storage, (&sender, &validator_addr))?;
                let rewards = shares.rewards(&validator_info, validator_info.calculated_rewards);
                validator_info.calculated_rewards -= rewards;
                let rewards = Uint128::new(1) * rewards; // convert to Uint128

                // save updated validator_info
                VALIDATOR_INFO.save(&mut staking_storage, &validator_addr, &validator_info)?;

                // directly mint rewards to delegator
                router.sudo(
                    api,
                    storage,
                    block,
                    BankSudo::Mint {
                        to_address: sender.to_string(),
                        amount: vec![Coin {
                            amount: rewards,
                            denom: staking_info.bonded_denom.clone(),
                        }],
                    }
                    .into(),
                )?;

                let events = vec![Event::new("withdraw_delegator_reward")
                    .add_attribute("validator", &validator)
                    .add_attribute("sender", &sender)
                    .add_attribute(
                        "amount",
                        format!("{}{}", rewards, staking_info.bonded_denom),
                    )];
                Ok(AppResponse { events, data: None })
            }
            m => bail!("Unsupported distribution message: {:?}", m),
        }
    }

    fn sudo<ExecC, QueryC>(
        &self,
        _api: &dyn Api,
        _storage: &mut dyn Storage,
        _router: &dyn CosmosRouter<ExecC = ExecC, QueryC = QueryC>,
        _block: &BlockInfo,
        _msg: Empty,
    ) -> AnyResult<AppResponse> {
        bail!("Something went wrong - Distribution doesn't have sudo messages")
    }

    fn query(
        &self,
        _api: &dyn Api,
        _storage: &dyn Storage,
        _querier: &dyn Querier,
        _block: &BlockInfo,
        _request: Empty,
    ) -> AnyResult<Binary> {
        bail!("Something went wrong - Distribution doesn't have query messages")
    }
}

#[cfg(test)]
mod test {
    use crate::{app::MockRouter, BankKeeper, FailingModule, Router, WasmKeeper};

    use super::*;

    use cosmwasm_std::testing::{mock_env, MockApi, MockStorage};

    /// Type alias for default build `Router` to make its reference in typical scenario
    type BasicRouter<ExecC = Empty, QueryC = Empty> = Router<
        BankKeeper,
        FailingModule<ExecC, QueryC, Empty>,
        WasmKeeper<ExecC, QueryC>,
        StakeKeeper,
        DistributionKeeper,
    >;

    fn mock_router() -> BasicRouter {
        Router {
            wasm: WasmKeeper::new(),
            bank: BankKeeper::new(),
            custom: FailingModule::new(),
            staking: StakeKeeper::new(),
            distribution: DistributionKeeper::new(),
        }
    }

    #[test]
    fn add_get_validators() {
        let api = MockApi::default();
        let mut store = MockStorage::new();
        let stake = StakeKeeper::new();
        let block = mock_env().block;

        // add validator
        let valoper1 = Validator {
            address: "testvaloper1".to_string(),
            commission: Decimal::percent(10),
            max_commission: Decimal::percent(20),
            max_change_rate: Decimal::percent(1),
        };
        stake
            .add_validator(&api, &mut store, &block, valoper1.clone())
            .unwrap();

        // get it
        let staking_storage = prefixed_read(&store, NAMESPACE_STAKING);
        let val = stake
            .get_validator(
                &staking_storage,
                &api.addr_validate("testvaloper1").unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(val, valoper1);

        // try to add with same address
        let valoper1_fake = Validator {
            address: "testvaloper1".to_string(),
            commission: Decimal::percent(1),
            max_commission: Decimal::percent(10),
            max_change_rate: Decimal::percent(100),
        };
        stake
            .add_validator(&api, &mut store, &block, valoper1_fake)
            .unwrap_err();

        // should still be original value
        let staking_storage = prefixed_read(&store, NAMESPACE_STAKING);
        let val = stake
            .get_validator(
                &staking_storage,
                &api.addr_validate("testvaloper1").unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(val, valoper1);
    }

    #[test]
    fn validator_slashing() {
        let api = MockApi::default();
        let router = MockRouter::default();
        let mut store = MockStorage::new();
        let stake = StakeKeeper::new();
        let block = mock_env().block;

        let delegator = Addr::unchecked("delegator");
        let validator = api.addr_validate("testvaloper1").unwrap();

        // add validator
        let valoper1 = Validator {
            address: "testvaloper1".to_string(),
            commission: Decimal::percent(10),
            max_commission: Decimal::percent(20),
            max_change_rate: Decimal::percent(1),
        };
        stake
            .add_validator(&api, &mut store, &block, valoper1)
            .unwrap();

        // stake 100 tokens
        let mut staking_storage = prefixed(&mut store, NAMESPACE_STAKING);
        stake
            .add_stake(
                &mut staking_storage,
                &block,
                &delegator,
                &validator,
                coin(100, "TOKEN"),
            )
            .unwrap();

        // slash 50%
        stake
            .sudo(
                &api,
                &mut store,
                &router,
                &block,
                StakingSudo::Slash {
                    validator: "testvaloper1".to_string(),
                    percentage: Decimal::percent(50),
                },
            )
            .unwrap();

        // check stake
        let staking_storage = prefixed(&mut store, NAMESPACE_STAKING);
        let stake_left = stake
            .get_stake(&staking_storage, &delegator, &validator)
            .unwrap();
        assert_eq!(stake_left.amount.u128(), 50, "should have slashed 50%");

        // slash all
        stake
            .sudo(
                &api,
                &mut store,
                &router,
                &block,
                StakingSudo::Slash {
                    validator: "testvaloper1".to_string(),
                    percentage: Decimal::percent(100),
                },
            )
            .unwrap();

        // check stake
        let staking_storage = prefixed(&mut store, NAMESPACE_STAKING);
        let stake_left = stake
            .get_stake(&staking_storage, &delegator, &validator)
            .unwrap();
        assert_eq!(
            stake_left.amount.u128(),
            0,
            "should have slashed whole stake"
        );
    }

    fn setup_test(
        apr: Decimal,
        validator_commission: Decimal,
    ) -> (MockApi, MockStorage, BasicRouter, BlockInfo, Addr) {
        let api = MockApi::default();
        let router = mock_router();
        let mut store = MockStorage::new();
        let block = mock_env().block;

        let validator = api.addr_validate("testvaloper1").unwrap();

        // setup 10% APR
        router
            .staking
            .setup(
                &mut store,
                StakingInfo {
                    bonded_denom: "TOKEN".to_string(),
                    unbonding_time: 60,
                    apr,
                },
            )
            .unwrap();

        // add validator
        let valoper1 = Validator {
            address: "testvaloper1".to_string(),
            commission: validator_commission,
            max_commission: Decimal::percent(100),
            max_change_rate: Decimal::percent(1),
        };
        router
            .staking
            .add_validator(&api, &mut store, &block, valoper1)
            .unwrap();

        (api, store, router, block, validator)
    }

    #[test]
    fn rewards_work_for_single_delegator() {
        let (api, mut store, router, mut block, validator) =
            setup_test(Decimal::percent(10), Decimal::percent(10));
        let stake = &router.staking;
        let distr = &router.distribution;
        let delegator = Addr::unchecked("delegator");

        let mut staking_storage = prefixed(&mut store, NAMESPACE_STAKING);
        // stake 200 tokens
        stake
            .add_stake(
                &mut staking_storage,
                &block,
                &delegator,
                &validator,
                coin(200, "TOKEN"),
            )
            .unwrap();

        // wait 1/2 year
        block.time = block.time.plus_seconds(60 * 60 * 24 * 365 / 2);

        // should now have 200 * 10% / 2 - 10% commission = 9 tokens reward
        let rewards = stake
            .get_rewards(&store, &block, &delegator, &validator)
            .unwrap()
            .unwrap();
        assert_eq!(rewards.amount.u128(), 9, "should have 9 tokens reward");

        // withdraw rewards
        distr
            .execute(
                &api,
                &mut store,
                &router,
                &block,
                delegator.clone(),
                DistributionMsg::WithdrawDelegatorReward {
                    validator: validator.to_string(),
                },
            )
            .unwrap();

        // should have no rewards left
        let rewards = stake
            .get_rewards(&store, &block, &delegator, &validator)
            .unwrap()
            .unwrap();
        assert_eq!(rewards.amount.u128(), 0);

        // wait another 1/2 year
        block.time = block.time.plus_seconds(60 * 60 * 24 * 365 / 2);
        // should now have 9 tokens again
        let rewards = stake
            .get_rewards(&store, &block, &delegator, &validator)
            .unwrap()
            .unwrap();
        assert_eq!(rewards.amount.u128(), 9);
    }

    #[test]
    fn rewards_work_for_multiple_delegators() {
        let (api, mut store, router, mut block, validator) =
            setup_test(Decimal::percent(10), Decimal::percent(10));
        let stake = &router.staking;
        let distr = &router.distribution;
        let delegator1 = Addr::unchecked("delegator1");
        let delegator2 = Addr::unchecked("delegator2");

        let mut staking_storage = prefixed(&mut store, NAMESPACE_STAKING);

        // add 100 stake to delegator1 and 200 to delegator2
        stake
            .add_stake(
                &mut staking_storage,
                &block,
                &delegator1,
                &validator,
                coin(100, "TOKEN"),
            )
            .unwrap();
        stake
            .add_stake(
                &mut staking_storage,
                &block,
                &delegator2,
                &validator,
                coin(200, "TOKEN"),
            )
            .unwrap();

        // wait 1 year
        block.time = block.time.plus_seconds(60 * 60 * 24 * 365);

        // delegator1 should now have 100 * 10% - 10% commission = 9 tokens
        let rewards = stake
            .get_rewards(&store, &block, &delegator1, &validator)
            .unwrap()
            .unwrap();
        assert_eq!(rewards.amount.u128(), 9);

        // delegator1 should now have 200 * 10% - 10% commission = 18 tokens
        let rewards = stake
            .get_rewards(&store, &block, &delegator2, &validator)
            .unwrap()
            .unwrap();
        assert_eq!(rewards.amount.u128(), 18);

        // delegator1 stakes 100 more
        let mut staking_storage = prefixed(&mut store, NAMESPACE_STAKING);
        stake
            .add_stake(
                &mut staking_storage,
                &block,
                &delegator1,
                &validator,
                coin(100, "TOKEN"),
            )
            .unwrap();

        // wait another year
        block.time = block.time.plus_seconds(60 * 60 * 24 * 365);

        // delegator1 should now have 9 + 200 * 10% - 10% commission = 27 tokens
        let rewards = stake
            .get_rewards(&store, &block, &delegator1, &validator)
            .unwrap()
            .unwrap();
        assert_eq!(rewards.amount.u128(), 27);

        // delegator1 should now have 18 + 200 * 10% - 10% commission = 36 tokens
        let rewards = stake
            .get_rewards(&store, &block, &delegator2, &validator)
            .unwrap()
            .unwrap();
        assert_eq!(rewards.amount.u128(), 36);
    }
}
