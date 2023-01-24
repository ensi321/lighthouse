use beacon_chain::{BeaconChain, BeaconChainTypes};
use eth2::lighthouse::attestation_rewards::{IdealAttestationRewards, TotalAttestationRewards};
use eth2::lighthouse::StandardAttestationRewards;
use participation_cache::ParticipationCache;
use safe_arith::SafeArith;
use slog::{debug, Logger};
use state_processing::{
    common::altair::BaseRewardPerIncrement,
    per_epoch_processing::altair::{participation_cache, rewards_and_penalties::get_flag_weight},
};
use std::{collections::HashMap, sync::Arc};
use types::consts::altair::WEIGHT_DENOMINATOR;
use types::consts::altair::{
    TIMELY_HEAD_FLAG_INDEX, TIMELY_SOURCE_FLAG_INDEX, TIMELY_TARGET_FLAG_INDEX,
};
use types::{Epoch, EthSpec};
use warp_utils::reject::custom_not_found;

use crate::ExecutionOptimistic;

pub fn compute_attestation_rewards<T: BeaconChainTypes>(
    chain: Arc<BeaconChain<T>>,
    epoch: Epoch,
    validators: Vec<usize>,
    log: Logger,
) -> Result<(StandardAttestationRewards, ExecutionOptimistic), warp::Rejection> {
    debug!(log, "computing attestation rewards"; "epoch" => epoch, "validator_count" => validators.len());

    //--- Get state ---//
    let spec = &chain.spec;

    let execution_optimistic = chain
        .is_optimistic_or_invalid_head()
        .map_err(|e| custom_not_found(format!("Unable to get execution_optimistic! {:?}", e)))?;

    let state_slot = (epoch + 1).end_slot(T::EthSpec::slots_per_epoch());

    let state_root = chain
        .state_root_at_slot(state_slot)
        .map_err(warp_utils::reject::beacon_chain_error)?
        .ok_or_else(|| warp_utils::reject::custom_not_found("State root not found".to_owned()))?;

    let state = chain
        .get_state(&state_root, Some(state_slot))
        .map_err(warp_utils::reject::beacon_chain_error)?
        .ok_or_else(|| warp_utils::reject::custom_not_found("State not found".to_owned()))?;

    //--- Calculate ideal_rewards ---//
    let participation_cache = ParticipationCache::new(&state, spec)
        .map_err(|e| custom_not_found(format!("Unable to get participation_cache! {:?}", e)))?;

    let previous_epoch = state.previous_epoch();

    let mut ideal_rewards_hashmap = HashMap::new();

    let flag_index = 0;
    let weight = 0;
    let base_reward = 0;

    for flag_index in [
        TIMELY_SOURCE_FLAG_INDEX,
        TIMELY_TARGET_FLAG_INDEX,
        TIMELY_HEAD_FLAG_INDEX,
    ]
    .iter()
    {
        let weight = get_flag_weight(*flag_index)
            .map_err(|e| custom_not_found(format!("Unable to get weight! {:?}", e)))?;

        let unslashed_participating_indices = participation_cache
            .get_unslashed_participating_indices(*flag_index, previous_epoch)
            .map_err(|e| {
                custom_not_found(format!(
                    "Unable to get unslashed_participating_indices! {:?}",
                    e
                ))
            })?;

        let unslashed_participating_balance = unslashed_participating_indices
            .total_balance()
            .map_err(|e| {
                custom_not_found(format!(
                    "Unable to get unslashed_participating_balance! {:?}",
                    e
                ))
            })?;

        let unslashed_participating_increments = unslashed_participating_balance
            .safe_div(spec.effective_balance_increment)
            .map_err(|e| {
                custom_not_found(format!(
                    "Unable to get unslashed_participating_increments! {:?}",
                    e
                ))
            })?;

        let total_active_balance = participation_cache.current_epoch_total_active_balance();

        let active_increments = total_active_balance
            .safe_div(spec.effective_balance_increment)
            .map_err(|e| custom_not_found(format!("Unable to get active_increments! {:?}", e)))?;

        let base_reward_per_increment = BaseRewardPerIncrement::new(total_active_balance, spec)
            .map_err(|e| {
                custom_not_found(format!("Unable to get base_reward_per_increment! {:?}", e))
            })?;

        for effective_balance_eth in 0..=32 {
            let base_reward = effective_balance_eth.safe_mul(base_reward_per_increment.as_u64());

            let base_reward = base_reward.map_err(|e| {
                warp_utils::reject::custom_not_found(format!("Unable to get base_reward! {:?}", e))
            })?;

            let reward_numerator = base_reward
                .safe_mul(weight)
                .and_then(|reward_numerator| {
                    reward_numerator.safe_mul(unslashed_participating_increments)
                })
                .map_err(|_| {
                    warp_utils::reject::custom_server_error(
                        "Unable to calculate reward numerator".to_owned(),
                    )
                })?;

            let ideal_reward = reward_numerator
                .safe_div(active_increments)
                .and_then(|ideal_reward| ideal_reward.safe_div(WEIGHT_DENOMINATOR))
                .map_err(|_| {
                    warp_utils::reject::custom_server_error(
                        "Unable to calculate ideal_reward".to_owned(),
                    )
                })?;

            if !state.is_in_inactivity_leak(previous_epoch, spec) {
                ideal_rewards_hashmap.insert((*flag_index, effective_balance_eth), ideal_reward);
            } else {
                ideal_rewards_hashmap.insert((*flag_index, effective_balance_eth), 0);
            }
        }
    }

    let ideal_rewards: Vec<IdealAttestationRewards> = ideal_rewards_hashmap
        .iter()
        .fold(
            HashMap::new(),
            |mut acc, ((_flag_index, effective_balance_eth), ideal_reward)| {
                let entry =
                    acc.entry(*effective_balance_eth as u32)
                        .or_insert(IdealAttestationRewards {
                            effective_balance: *effective_balance_eth,
                            head: 0,
                            target: 0,
                            source: 0,
                        });
                match flag_index {
                    TIMELY_SOURCE_FLAG_INDEX => entry.source += *ideal_reward,
                    TIMELY_TARGET_FLAG_INDEX => entry.target += *ideal_reward,
                    TIMELY_HEAD_FLAG_INDEX => entry.head += *ideal_reward,
                    _ => {}
                }
                acc
            },
        )
        .into_values()
        .collect();

    //--- Calculate total rewards ---//
    let mut total_rewards = Vec::new();

    let index;
    if validators.is_empty() {
        index = participation_cache.eligible_validator_indices();
    } else {
        index = &validators;
    }

    for validator_index in index {
        let eligible = state
            .is_eligible_validator(previous_epoch, *validator_index)
            .map_err(|_| {
                warp_utils::reject::custom_server_error("Unable to get eligible".to_owned())
            })?;

        let effective_balance = state.get_effective_balance(*validator_index).unwrap();

        let effective_balance_eth = effective_balance.safe_div(spec.effective_balance_increment);

        let mut head_reward = 0u64;
        let mut target_reward = 0u64;
        let mut source_reward = 0u64;

        for &flag_index in [
            TIMELY_SOURCE_FLAG_INDEX,
            TIMELY_TARGET_FLAG_INDEX,
            TIMELY_HEAD_FLAG_INDEX,
        ]
        .iter()
        {
            if eligible {
                let voted_correctly = participation_cache
                    .get_unslashed_participating_indices(flag_index, previous_epoch)
                    .is_ok();
                if voted_correctly {
                    let _ideal_reward = &ideal_rewards
                        .iter()
                        .find(|reward| {
                            reward.effective_balance == effective_balance_eth.ok().unwrap()
                        })
                        .map(|reward| {
                            head_reward = reward.head;
                            target_reward = reward.target;
                            source_reward = reward.source;
                            reward
                        })
                        .unwrap_or(&IdealAttestationRewards {
                            effective_balance: effective_balance_eth.ok().unwrap_or(0),
                            head: 0,
                            target: 0,
                            source: 0,
                        });
                } else {
                    match flag_index {
                        TIMELY_HEAD_FLAG_INDEX => {}
                        TIMELY_TARGET_FLAG_INDEX => {
                            target_reward = (-(base_reward as i64 as i128) * weight as i128
                                / WEIGHT_DENOMINATOR as i128)
                                as u64
                        }
                        TIMELY_SOURCE_FLAG_INDEX => {
                            source_reward = (-(base_reward as i64 as i128) * weight as i128
                                / WEIGHT_DENOMINATOR as i128)
                                as u64
                        }
                        _ => {}
                    }
                }
            }

            total_rewards.push(TotalAttestationRewards {
                validator_index: *validator_index as u64,
                head: head_reward as i64,
                target: target_reward as i64,
                source: source_reward as i64,
                inclusion_delay: 0,
            });
        }
    }

    Ok((
        StandardAttestationRewards {
            ideal_rewards,
            total_rewards,
        },
        execution_optimistic,
    ))
}
