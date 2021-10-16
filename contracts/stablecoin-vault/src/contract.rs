use cosmwasm_std::{ entry_point, CanonicalAddr,
    from_binary, to_binary, BankMsg, Binary, Coin, CosmosMsg, Deps, DepsMut, Env, Fraction, MessageInfo, Response, StdError,
    StdResult, WasmMsg, Uint128, Decimal, SubMsg, Reply, ReplyOn
};
use terra_cosmwasm::{create_swap_msg, TerraMsgWrapper};
use terraswap::asset::{Asset, AssetInfo};
use terraswap::pair::Cw20HookMsg;
use terraswap::querier::{query_balance, query_token_balance, query_supply};
use terraswap::token::InstantiateMsg as TokenInstantiateMsg;
use protobuf::Message;

use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg, MinterResponse};

use white_whale::community_fund::msg::ExecuteMsg as CommunityFundMsg;
use white_whale::denom::LUNA_DENOM;
use white_whale::deposit_info::DepositInfo;
use white_whale::fee::{Fee, CappedFee, VaultFee};
use white_whale::msg::{create_terraswap_msg, VaultQueryMsg as QueryMsg, AnchorMsg, EstimateDepositFeeResponse, EstimateWithdrawFeeResponse, FeeResponse};
use white_whale::query::terraswap::simulate_swap as simulate_terraswap_swap;
use white_whale::query::anchor::query_aust_exchange_rate;
use white_whale::profit_check::msg::{HandleMsg as ProfitCheckMsg};
use white_whale::anchor::try_deposit_to_anchor as try_deposit;

use crate::error::StableVaultError;
use crate::msg::{ExecuteMsg, InitMsg, PoolResponse};
use crate::state::{State, ADMIN, STATE, POOL_INFO, DEPOSIT_INFO, FEE};
use crate::pool_info::{PoolInfo, PoolInfoRaw};
use crate::querier::{query_market_price};
use crate::response::MsgInstantiateContractResponse;

const INSTANTIATE_REPLY_ID: u8 = 1u8;
const CAP_UST_REPLY_ID: u8 = 2u8;

type VaultResult = Result<Response<TerraMsgWrapper>, StableVaultError>;


#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InitMsg,
) -> VaultResult {
    let state = State {
        trader: deps.api.addr_canonicalize(info.sender.as_str())?,
        pool_address: deps.api.addr_canonicalize(&msg.pool_address)?,
        anchor_money_market_address: deps.api.addr_canonicalize(&msg.anchor_money_market_address)?,
        aust_address: deps.api.addr_canonicalize(&msg.aust_address)?,
        seignorage_address: deps.api.addr_canonicalize(&msg.seignorage_address)?,
        profit_check_address: deps.api.addr_canonicalize(&msg.profit_check_address)?,
    };

    // Store the initial config
    STATE.save(deps.storage, &state)?;
    DEPOSIT_INFO.save(deps.storage, &DepositInfo{
        asset_info: msg.asset_info.clone()
    })?;
    // Setup the fees system with a fee and other contract addresses
    FEE.save(deps.storage, &VaultFee{
        community_fund_fee: CappedFee{
            fee: Fee{ share: msg.community_fund_fee },
            max_fee: msg.max_community_fund_fee
        },
        warchest_fee: Fee{ share: msg.warchest_fee },
        community_fund_addr: deps.api.addr_canonicalize(&msg.community_fund_addr)?,
        warchest_addr: deps.api.addr_canonicalize(&msg.warchest_addr)?
    })?;

    // Setup and save the relevant pools info in state. The saved pool will be the one used by the vault.
    let pool_info: &PoolInfoRaw = &PoolInfoRaw {
        contract_addr: env.contract.address.clone(),
        liquidity_token: CanonicalAddr::from(vec![]),
        stable_cap: msg.stable_cap,
        asset_infos: [
            msg.asset_info.to_raw(deps.api)?,
            AssetInfo::NativeToken{ denom: LUNA_DENOM.to_string()}.to_raw(deps.api)?,
            AssetInfo::Token{ contract_addr: msg.aust_address }.to_raw(deps.api)?
        ],
    };
    POOL_INFO.save(deps.storage, pool_info)?;
    // Setup the admin as the creator of the contract
    ADMIN.set(deps, Some(info.sender))?;
    // TODO: Update the creation of LP token, use non default name and symbol.
    Ok(Response::new().add_submessage(SubMsg {
        // Create LP token
        msg: WasmMsg::Instantiate {
            admin: None,
            code_id: msg.token_code_id,
            msg: to_binary(&TokenInstantiateMsg {
                name: "test liquidity token".to_string(),
                symbol: "tLP".to_string(),
                decimals: 6,
                initial_balances: vec![
                    // Cw20Coin{address: env.contract.address.to_string(),
                    // amount: Uint128::from(1u8)},
                ],
                mint: Some(MinterResponse {
                    minter: env.contract.address.to_string(),
                    cap: None,
                }),
            })?,
            funds: vec![],
            label: "".to_string(),
        }
        .into(),
        gas_limit: None,
        id: u64::from(INSTANTIATE_REPLY_ID),
        reply_on: ReplyOn::Success,
    }))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> VaultResult {
    match msg {
        ExecuteMsg::Receive(msg) => receive_cw20(deps, env, info, msg),
        ExecuteMsg::BelowPeg { amount, slippage, belief_price } => try_arb_below_peg(deps, env, info, amount, slippage, belief_price),
        ExecuteMsg::AbovePeg { amount, slippage, belief_price } => try_arb_above_peg(deps, env, info, amount, slippage, belief_price),
        ExecuteMsg::ProvideLiquidity{ asset } => try_provide_liquidity(deps, info, asset),
        ExecuteMsg::AnchorDeposit{ amount } => try_deposit_to_anchor(deps, info, amount),
        ExecuteMsg::SetStableCap{ stable_cap } => set_stable_cap(deps, info, stable_cap),
        ExecuteMsg::SetAdmin{ admin } => {
            let admin_addr = deps.api.addr_validate(&admin)?;
            let previous_admin = ADMIN.get(deps.as_ref())?.unwrap();
            ADMIN.execute_update_admin(deps, info, Some(admin_addr))?;
            Ok(Response::default().add_attribute("previous admin", previous_admin).add_attribute("admin", admin))
        },
        ExecuteMsg::SetTrader{ trader } => set_trader(deps, info, trader),
        ExecuteMsg::SetFee{ community_fund_fee, warchest_fee } => set_fee(deps, info, community_fund_fee, warchest_fee),
    }
}

pub fn try_provide_liquidity(
    deps: DepsMut,
    msg_info: MessageInfo,
    mut asset: Asset
) -> VaultResult {
    let deposit_info = DEPOSIT_INFO.load(deps.storage)?;
    let state = STATE.load(deps.storage)?;
    deposit_info.assert(&asset.info)?;
    asset.assert_sent_native_token_balance(&msg_info)?;
    asset.amount = asset.amount - asset.compute_tax(&deps.querier)?;

    let deposit_fee = compute_transaction_fee(deps.as_ref(), asset.amount)?;
    let deposit: Uint128 = asset.amount - deposit_fee;
    
    let info: PoolInfoRaw = POOL_INFO.load(deps.storage)?;
    let (total_deposits_in_ust, stables_in_contract,_) = compute_total_value(deps.as_ref(), &info)?;

    let total_share = query_supply(&deps.querier, deps.api.addr_humanize(&info.liquidity_token)?)?;
    let share = if total_share == Uint128::zero() {
        // Initial share = collateral amount
        deposit
    } else {
        // WARNING: This could causes issues if total_deposits_in_ust - asset.amount is really small 
        deposit.multiply_ratio(total_share, total_deposits_in_ust - asset.amount)
    };

    // mint LP token to sender
    let msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: deps.api.addr_humanize(&info.liquidity_token)?.to_string(),
        msg: to_binary(&Cw20ExecuteMsg::Mint {
            recipient: msg_info.sender.to_string(),
            amount: share,
        })?,
        funds: vec![],
    });
    // send fees to community fund
    let denom = deposit_info.get_denom()?;
    let fee_config = FEE.load(deps.storage)?;
    let community_fund_asset = Asset{
        info: AssetInfo::NativeToken{denom: denom.clone()},
        amount: deposit_fee
    };
    let community_fund_fee_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: deps.api.addr_humanize(&fee_config.community_fund_addr)?.to_string(),
        funds: vec![community_fund_asset.deduct_tax(&deps.querier)?],
        msg: to_binary(&CommunityFundMsg::Deposit{})?
    });

    let attrs = vec![
    ("Sent minus tax:", asset.amount.to_string()),
    ("Deposited into the vault:", deposit.to_string()),
    ("Community fee", deposit_fee.to_string()),
    ];

    let response = Response::new().add_attributes(attrs)
    .add_message(msg)
    .add_message(community_fund_fee_msg);

    // If contract holds more then ANCHOR_DEPOSIT_THRESHOLD [UST] then try deposit to anchor and leave UST_CAP [UST] in contract.
    if stables_in_contract > Uint128::from(info.stable_cap * Decimal::percent(150)) {
        let deposit_amount = stables_in_contract - Uint128::from(info.stable_cap);
        let anchor_deposit_asset = Asset{
            info: AssetInfo::NativeToken{denom: denom},
            amount: deposit_amount
        };
        let anchor_money_market_address = deps.api.addr_humanize(&state.anchor_money_market_address)?.to_string();

        let msg = CosmosMsg::Wasm(WasmMsg::Execute{
            contract_addr: anchor_money_market_address,
            msg: to_binary(&AnchorMsg::DepositStable{})?,
            funds: vec![anchor_deposit_asset.deduct_tax(&deps.querier)?]
        });

        // Implicitly borrows as mut
        return Ok(response.add_message(msg));
    };
    
    Ok(response)
}

/// attempt to withdraw deposits. Fees are calculated and deducted and the net refund is sent 
/// a withdrawal from Anchor Money Market may be performed as a part of the withdrawal process.
/// Luna holdings are not eligible for withdrawal 
fn try_withdraw_liquidity(
    deps: DepsMut,
    env: Env,
    sender: String,
    amount: Uint128,
) -> VaultResult {
    let info: PoolInfoRaw = POOL_INFO.load(deps.storage)?;

    let lp_addr = deps.api.addr_humanize(&info.liquidity_token)?;
    let total_share: Uint128 = query_supply(&deps.querier, lp_addr)?;
    let (total_value,_,uaust_value_in_contract) = compute_total_value(deps.as_ref(), &info)?;

    let share_ratio: Decimal = Decimal::from_ratio(amount, total_share);
    let mut refund_amount: Uint128 = total_value * share_ratio;

    let mut response = Response::new();
    // // withdraw from anchor if necessary
    // // TODO: Improve
    let state = STATE.load(deps.storage)?;
    let denom = DEPOSIT_INFO.load(deps.storage)?.get_denom()?;

    let max_aust_amount = query_token_balance(&deps.querier, deps.api.addr_humanize(&state.aust_address)?, env.contract.address)?;
    
    let mut withdrawn_ust = Asset{
        info: AssetInfo::NativeToken{ denom: denom.clone() },
        amount: Uint128::zero()
    };
    
    if max_aust_amount > Uint128::zero() {
        let ust_uaust_rate = Decimal::from_ratio(uaust_value_in_contract, max_aust_amount);
        
        if uaust_value_in_contract < refund_amount {
            // Withdraw all aUST left
            
            response = response.add_message(CosmosMsg::Wasm(WasmMsg::Execute{
                contract_addr: deps.api.addr_humanize(&state.aust_address)?.to_string(),
                msg: to_binary(
                    &Cw20ExecuteMsg::Send{
                        contract: deps.api.addr_humanize(&state.anchor_money_market_address)?.to_string(),
                        amount: max_aust_amount,
                        msg: to_binary(&AnchorMsg::RedeemStable{})?
                    }
                )?,
                funds: vec![]
            })).add_attribute("Max anchor withdrawal", max_aust_amount.to_string()).add_attribute("ust_aust_rate", ust_uaust_rate.to_string());
            withdrawn_ust.amount = uaust_value_in_contract;
            // TODO: check if you pay 2x tax on this tx. One on withdraw and one on pay-out
        } else {
            let withdraw_amount = refund_amount * ust_uaust_rate.inv().unwrap();
            response = response.add_message(CosmosMsg::Wasm(WasmMsg::Execute{
                contract_addr: deps.api.addr_humanize(&state.aust_address)?.to_string(),
                msg: to_binary(
                    &Cw20ExecuteMsg::Send{
                        contract: deps.api.addr_humanize(&state.anchor_money_market_address)?.to_string(),
                        amount: withdraw_amount,
                        msg: to_binary(&AnchorMsg::RedeemStable{})?
                    }
                )?,
                funds: vec![]
            })).add_attribute("Anchor withdrawal", withdraw_amount.to_string()).add_attribute("ust_aust_rate", ust_uaust_rate.to_string());
            withdrawn_ust.amount = refund_amount;
        };
        let withdrawtx_tax = withdrawn_ust.compute_tax(&deps.querier)?;
        refund_amount -= withdrawtx_tax;
    };
    
    let community_fund_fee = compute_transaction_fee(deps.as_ref(), refund_amount)?;
    let warchest_fee = compute_warchest_fee(deps.as_ref(), refund_amount)?;
    let net_refund_amount = refund_amount - community_fund_fee - warchest_fee;

    let refund_asset = Asset{
        info: AssetInfo::NativeToken{ denom: denom.clone() },
        amount: net_refund_amount
    };

    // Prepare refund message 
    let refund_msg = CosmosMsg::Bank(BankMsg::Send {
        to_address: sender,
        amount: vec![refund_asset.deduct_tax(&deps.querier)?],
    });

    let burn_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: deps.api.addr_humanize(&info.liquidity_token)?.to_string(),
        msg: to_binary(&Cw20ExecuteMsg::Burn { amount })?,
        funds: vec![],
    });

    let community_fund_asset = Asset{
        info: AssetInfo::NativeToken{ denom: denom.clone() },
        amount: community_fund_fee
    };

    let warchest_asset = Asset{
        info: AssetInfo::NativeToken{ denom },
        amount: warchest_fee
    };

    // Prepare deposit messages for warchest and community fund.
    let fee_config = FEE.load(deps.storage)?;
    let community_fund_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: deps.api.addr_humanize(&fee_config.community_fund_addr)?.to_string(),
        funds: vec![community_fund_asset.deduct_tax(&deps.querier)?],
        msg: to_binary(&CommunityFundMsg::Deposit{})?
    });
    let warchest_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: deps.api.addr_humanize(&fee_config.warchest_addr)?.to_string(),
        funds: vec![warchest_asset.deduct_tax(&deps.querier)?],
        msg: to_binary(&CommunityFundMsg::Deposit{})?
    });

    Ok(response.add_message(refund_msg).add_message(burn_msg).add_message(community_fund_msg).add_message(warchest_msg)
        .add_attribute("action", "withdraw_liquidity")
        .add_attribute("withdrawn_amount", refund_amount.to_string())
        .add_attribute("refund amount", net_refund_amount.to_string())
        .add_attribute("community fund fee", community_fund_fee.to_string())
        .add_attribute("warchest fee", warchest_fee.to_string())
    )
}

/// handler function invoked when the stablecoin-vault contract receives
/// a transaction. This is akin to a payable function in Solidity
fn receive_cw20(
    deps: DepsMut,
    env: Env,
    msg_info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> VaultResult {
        match from_binary(&cw20_msg.msg)? {
            Cw20HookMsg::Swap {
                ..
            } => {
                Err(StableVaultError::NoSwapAvailabe{})
            }
            Cw20HookMsg::WithdrawLiquidity {} => {
                let info: PoolInfoRaw = POOL_INFO.load(deps.storage)?;
                if deps.api.addr_canonicalize(&msg_info.sender.to_string())? != info.liquidity_token {
                    return Err(StableVaultError::Unauthorized{});
                }

                try_withdraw_liquidity(deps, env, cw20_msg.sender, cw20_msg.amount)
            }
        }
}

/// helper method which takes two msgs assumed to be Terraswap trades
/// and then composes a response with a ProfitCheck BeforeTrade and AfterTrade
/// the result is an OK'd response with a series of msgs in this order
/// Profit Check before trade - first_msg - second_msg - Profit Check after trade
fn add_profit_check(
    deps: Deps,
    response: Response<TerraMsgWrapper>,
    first_msg: CosmosMsg<TerraMsgWrapper>,
    second_msg: CosmosMsg<TerraMsgWrapper>
) -> VaultResult {
    let state = STATE.load(deps.storage)?;

    Ok(response.add_message(CosmosMsg::Wasm(WasmMsg::Execute{
        contract_addr: deps.api.addr_humanize(&state.profit_check_address)?.to_string(),
        msg: to_binary(
            &ProfitCheckMsg::BeforeTrade{}
        )?,
        funds: vec![]
    }))
    .add_message(first_msg)
    .add_message(second_msg)
    .add_message(CosmosMsg::Wasm(WasmMsg::Execute{
        contract_addr: deps.api.addr_humanize(&state.profit_check_address)?.to_string(),
        msg: to_binary(
            &ProfitCheckMsg::AfterTrade{}
        )?,
        funds: vec![]
    })))
}

fn try_arb_below_peg(
    deps: DepsMut,
    env: Env,
    msg_info: MessageInfo,
    amount: Coin,
    belief_price: Decimal,
    slippage: Decimal,
) -> VaultResult {

    let state = STATE.load(deps.storage)?;
    let info: PoolInfoRaw = POOL_INFO.load(deps.storage)?;
    // Ensure the caller is a named Trader
    if deps.api.addr_canonicalize(&msg_info.sender.to_string())? != state.trader {
        return Err(StableVaultError::Unauthorized{});
    }
    let (total_value,stables_availabe,_) = compute_total_value(deps.as_ref(), &info)?;

    if total_value < amount.amount + Uint128::from(10_000_000u64) {
        return Err(StableVaultError::Broke{});
    }

    let ask_denom = LUNA_DENOM.to_string();
    let expected_luna_received = query_market_price(deps.as_ref(), amount.clone(), LUNA_DENOM.to_string())?;
    let residual_luna = query_balance(&deps.querier, env.contract.address, LUNA_DENOM.to_string())?;
    let offer_coin = Coin{ denom: ask_denom.clone(), amount: residual_luna + expected_luna_received};
    let mut response = Response::new();

    // 10 UST as buffer for fees
    if (amount.amount + Uint128::from(10_000_000u64)) > stables_availabe {
        // Attempt to remove some money from anchor 
        let to_withdraw = (amount.amount + Uint128::from(10_000_000u64)) - stables_availabe;
        let epoch_state_response = query_aust_exchange_rate(deps.as_ref(), deps.api.addr_humanize(&state.anchor_money_market_address)?.to_string())?;
        let aust_exchange_rate = Decimal::from(epoch_state_response.exchange_rate);

        response = response.add_message(CosmosMsg::Wasm(WasmMsg::Execute{
            contract_addr: deps.api.addr_humanize(&state.aust_address)?.to_string(),
            msg: to_binary(
                &Cw20ExecuteMsg::Send{
                    contract: deps.api.addr_humanize(&state.anchor_money_market_address)?.to_string(),
                    amount: to_withdraw * aust_exchange_rate.inv().unwrap(),
                    msg: to_binary(&AnchorMsg::RedeemStable{})?
                }
            )?,
            funds: vec![]
        }));
    }

    // Market swap msg, swap STABLE -> LUNA
    let swap_msg = create_swap_msg(
        amount.clone(),
        ask_denom.clone(),
    );

    // Terraswap msg, swap LUNA -> STABLE
    let terraswap_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: deps.api.addr_humanize(&state.pool_address)?.to_string(),
        funds: vec![offer_coin.clone()],
        msg: to_binary(&create_terraswap_msg(offer_coin, belief_price, Some(slippage)))?,
    });

    // Finish off all the above by wrapping the swap and terraswap messages in between the 2 profit check queries
    add_profit_check(deps.as_ref(), response, swap_msg, terraswap_msg)
}

fn try_arb_above_peg(
    deps: DepsMut,
    env: Env,
    msg_info: MessageInfo,
    amount: Coin, 
    belief_price: Decimal,
    slippage: Decimal,
) -> VaultResult {

    let state = STATE.load(deps.storage)?;
    let info: PoolInfoRaw = POOL_INFO.load(deps.storage)?;
    // Check the caller is a named Trader
    if deps.api.addr_canonicalize(&msg_info.sender.to_string())? != state.trader {
        return Err(StableVaultError::Unauthorized{});
    }
    let (total_value,stables_availabe,_) = compute_total_value(deps.as_ref(), &info)?;

    if total_value < amount.amount + Uint128::from(10_000_000u64) {
        return Err(StableVaultError::Broke{});
    }

    let ask_denom = LUNA_DENOM.to_string();
    let expected_luna_received = simulate_terraswap_swap(deps.as_ref(), deps.api.addr_humanize(&state.pool_address)?, amount.clone())?;
    let residual_luna = query_balance(&deps.querier, env.contract.address, LUNA_DENOM.to_string())?;
    let offer_coin = Coin{ denom: ask_denom, amount: residual_luna + expected_luna_received};
    let mut response = Response::new();

    // 10 UST as buffer for fees
    if (amount.amount + Uint128::from(10_000_000u64)) > stables_availabe {
        // Attempt to remove some money from anchor 
        let to_withdraw = (amount.amount + Uint128::from(10_000_000u64)) - stables_availabe;
        let epoch_state_response = query_aust_exchange_rate(deps.as_ref(), deps.api.addr_humanize(&state.anchor_money_market_address)?.to_string())?;
        let aust_exchange_rate = Decimal::from(epoch_state_response.exchange_rate);

        response = response.add_message(CosmosMsg::Wasm(WasmMsg::Execute{
            contract_addr: deps.api.addr_humanize(&state.aust_address)?.to_string(),
            msg: to_binary(
                &Cw20ExecuteMsg::Send{
                    contract: deps.api.addr_humanize(&state.anchor_money_market_address)?.to_string(),
                    amount: to_withdraw * aust_exchange_rate.inv().unwrap(),
                    msg: to_binary(&AnchorMsg::RedeemStable{})?
                }
            )?,
            funds: vec![]
        }));
    }

    // Terraswap msg, swap STABLE -> LUNA
    let terraswap_msg = CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: deps.api.addr_humanize(&state.pool_address)?.to_string(),
        funds: vec![amount.clone()],
        msg: to_binary(&create_terraswap_msg(amount.clone(), belief_price, Some(slippage)))?,
    });

    // Market swap msg, swap LUNA -> STABLE
    let swap_msg = create_swap_msg(
        offer_coin,
        amount.denom,
    );

    // Finish off all the above by wrapping the swap and terraswap messages in between the 2 profit check queries
    add_profit_check(deps.as_ref(), response, terraswap_msg, swap_msg)
}


/// This just stores the result for future query
#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(deps: DepsMut, _env: Env, msg: Reply) -> StdResult<Response> {
    if msg.id == u64::from(INSTANTIATE_REPLY_ID) {
        let data = msg.result.unwrap().data.unwrap();
        let res: MsgInstantiateContractResponse =
            Message::parse_from_bytes(data.as_slice()).map_err(|_| {
                StdError::parse_err("MsgInstantiateContractResponse", "failed to parse data")
            })?;
        let liquidity_token = res.get_contract_address();

        let api = deps.api;
        POOL_INFO.update(deps.storage, |mut meta| -> StdResult<_> {
            meta.liquidity_token = api.addr_canonicalize(liquidity_token)?;
            Ok(meta)
        })?;

        return Ok(Response::new().add_attribute("liquidity_token_addr", liquidity_token));
    }

    if msg.id == u64::from(CAP_UST_REPLY_ID) {
    
    }

    Ok(Response::default())
}

// const COMMISSION_RATE: &str = "0.003";
// fn compute_swap(
//     offer_pool: Uint128,
//     ask_pool: Uint128,
//     offer_amount: Uint128,
// ) -> StdResult<(Uint128, Uint128, Uint128)> {
//     // offer => ask
//     // ask_amount = (ask_pool - cp / (offer_pool + offer_amount)) * (1 - commission_rate)
//     let cp = Uint128(offer_pool.u128() * ask_pool.u128());
//     let return_amount = (ask_pool - cp.multiply_ratio(1u128, offer_pool + offer_amount))?;

//     // calculate spread & commission
//     let spread_amount: Uint128 = (offer_amount * Decimal::from_ratio(ask_pool, offer_pool)
//         - return_amount)
//         .unwrap_or_else(|_| Uint128::zero());
//     let commission_amount: Uint128 = return_amount * Decimal::from_str(&COMMISSION_RATE).unwrap();

//     // commission will be absorbed to pool
//     let return_amount: Uint128 = (return_amount - commission_amount).unwrap();

//     Ok((return_amount, spread_amount, commission_amount))
// }

pub fn compute_total_value(
    deps: Deps,
    info: &PoolInfoRaw
) -> StdResult<(Uint128,Uint128,Uint128)> {
    let state = STATE.load(deps.storage)?;
    let stable_info = info.asset_infos[0].to_normal(deps.api)?;
    let stable_denom = match stable_info {
        AssetInfo::Token{..} => String::default(),
        AssetInfo::NativeToken{denom} => denom
    };
    let stable_amount = query_balance(&deps.querier, info.contract_addr.clone(), stable_denom)?;

    // let luna_info = info.asset_infos[1].to_normal(deps.api)?;
    // let luna_amount = match luna_info {
    //     AssetInfo::Token{..} => Uint128::zero(),
    //     AssetInfo::NativeToken{denom} => query_balance(&deps.querier, info.contract_addr.clone(), denom)?,
    // };
    //let luna_price = from_micro(query_market_price(deps, Coin{ denom: LUNA_DENOM.to_string(), amount: Uint128::from(1000000u64)}, stable_denom)?);
    // Get on-chain luna/ust price

    // TEST: exclude luna from LP size calculation.
    // reason: If someone were to deposit a large amount of luna into this wallet, it would f*ck things up. 
    // let luna_price = pool_ratio(deps, deps.api.addr_humanize(&state.pool_address)?)?;
    // let luna_value_in_stable = luna_amount * luna_price;

    let aust_info = info.asset_infos[2].to_normal(deps.api)?;
    let aust_amount = match aust_info {
        AssetInfo::Token{contract_addr} => query_token_balance(&deps.querier, deps.api.addr_validate(&contract_addr)?, info.contract_addr.clone())?,
        AssetInfo::NativeToken{..} => Uint128::zero()
    };

    
    let epoch_state_response = query_aust_exchange_rate(deps, deps.api.addr_humanize(&state.anchor_money_market_address)?.to_string())?;
    let aust_exchange_rate = Decimal::from(epoch_state_response.exchange_rate);
    let aust_value_in_ust = aust_exchange_rate*aust_amount;

    let total_deposits_in_ust = stable_amount + aust_value_in_ust;
    Ok((total_deposits_in_ust, stable_amount, aust_value_in_ust))
}

pub fn compute_transaction_fee(deps: Deps, amount: Uint128) -> StdResult<Uint128> {
    let fee_config = FEE.load(deps.storage)?;
    let fee = fee_config.community_fund_fee.compute(amount);
    Ok(fee)
}

pub fn compute_warchest_fee(deps: Deps, amount: Uint128) -> StdResult<Uint128> {
    let fee_config = FEE.load(deps.storage)?;
    let fee = fee_config.warchest_fee.compute(amount);
    Ok(fee)
}

pub fn compute_withdraw_fee(deps: Deps, amount: Uint128) -> StdResult<Uint128> {
    let community_fund_fee = compute_transaction_fee(deps, amount)?;
    let warchest_fee = compute_warchest_fee(deps, amount)?;
    Ok(community_fund_fee + warchest_fee)
}

pub fn try_deposit_to_anchor(
    deps: DepsMut,
    msg_info: MessageInfo,
    amount: Coin
) -> VaultResult {
    ADMIN.assert_admin(deps.as_ref(), &msg_info.sender)?;

    let state = STATE.load(deps.storage)?;
    Ok(try_deposit(deps.api.addr_humanize(&state.anchor_money_market_address)?.to_string(), amount)?)
}

/// Setters for contract parameters/config values

pub fn set_stable_cap(
    deps: DepsMut,
    msg_info: MessageInfo,
    stable_cap: Uint128,
) -> VaultResult {
    // Only the admin should be able to call this
    ADMIN.assert_admin(deps.as_ref(), &msg_info.sender)?;

    let mut info: PoolInfoRaw = POOL_INFO.load(deps.storage)?;
    let previous_cap = info.stable_cap;
    info.stable_cap = stable_cap;
    POOL_INFO.save(deps.storage, &info)?;
    Ok(Response::new().add_attribute("slippage", stable_cap.to_string()).add_attribute("previous slippage", previous_cap.to_string()))
}

pub fn set_trader(
    deps: DepsMut,
    msg_info: MessageInfo,
    trader: String
) -> VaultResult {
    // Only the admin should be able to call this
    ADMIN.assert_admin(deps.as_ref(), &msg_info.sender)?;

    let mut state = STATE.load(deps.storage)?;
    // Get the old trader 
    let previous_trader = deps.api.addr_humanize(&state.trader)?.to_string();
    // Store the new trader, validating it is indeed an address along the way
    state.trader = deps.api.addr_canonicalize(&trader)?;
    STATE.save(deps.storage, &state)?;
    // Respond and note the previous traders address
    Ok(Response::new().add_attribute("trader", trader).add_attribute("previous trader", previous_trader))
}

pub fn set_fee(
    deps: DepsMut,
    msg_info: MessageInfo,
    community_fund_fee: Option<CappedFee>,
    warchest_fee: Option<Fee>,
) -> VaultResult {
    // Only the admin should be able to call this
    ADMIN.assert_admin(deps.as_ref(), &msg_info.sender)?;
    // TODO: Evaluate this.
    let mut fee_config = FEE.load(deps.storage)?;
    if let Some(fee) = community_fund_fee {
        fee_config.community_fund_fee = fee;
    }
    if let Some(fee) = warchest_fee {
        fee_config.warchest_fee = fee;
    }
    FEE.save(deps.storage, &fee_config)?;
    Ok(Response::default())
}

/// Query Handler and query functions 

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(
    deps: Deps,
    _env: Env,
    msg: QueryMsg,
) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config{} => to_binary(&try_query_config(deps)?),
        QueryMsg::Pool{} => to_binary(&try_query_pool(deps)?),
        QueryMsg::Fees{} => to_binary(&query_fees(deps)?),
        QueryMsg::EstimateDepositFee{ amount } => to_binary(&estimate_deposit_fee(deps, amount)?),
        QueryMsg::EstimateWithdrawFee{ amount } => to_binary(&estimate_withdraw_fee(deps, amount)?),
    }
}

pub fn query_fees(deps: Deps) -> StdResult<FeeResponse> {
    Ok(FeeResponse{
        fees: FEE.load(deps.storage)?
    })
}

// Fees not including tax. 
pub fn estimate_deposit_fee(deps: Deps, amount: Uint128) -> StdResult<EstimateDepositFeeResponse> {
    let fee = compute_transaction_fee(deps, amount)?;
    Ok(EstimateDepositFeeResponse{
        fee: vec![Coin{ denom: DEPOSIT_INFO.load(deps.storage)?.get_denom()?, amount: fee }]
    })
}

pub fn estimate_withdraw_fee(deps: Deps, amount: Uint128) -> StdResult<EstimateWithdrawFeeResponse> {
    let fee = compute_withdraw_fee(deps, amount)?;
    Ok(EstimateWithdrawFeeResponse{
        fee: vec![Coin{ denom: DEPOSIT_INFO.load(deps.storage)?.get_denom()?, amount: fee }]
    })
}

pub fn try_query_config(
    deps: Deps
) -> StdResult<PoolInfo> {
    let info: PoolInfoRaw = POOL_INFO.load(deps.storage)?;
    info.to_normal(deps)
}

pub fn try_query_pool(
    deps: Deps
) -> StdResult<PoolResponse> {
    let info: PoolInfoRaw = POOL_INFO.load(deps.storage)?;
    let assets: [Asset; 3] = info.query_pools(deps, info.contract_addr.clone())?;
    let total_share: Uint128 =
        query_supply(&deps.querier, deps.api.addr_humanize(&info.liquidity_token)?)?;

    let (total_value_in_ust,_,_) = compute_total_value(deps, &info)?;

    Ok(PoolResponse { assets, total_value_in_ust, total_share })
}

// Tests to add
// - set trader

#[cfg(test)]
mod tests {
    use super::*;
    use cosmwasm_std::testing::{mock_env};
    use crate::testing::{mock_dependencies};
    use cosmwasm_std::{Uint128, Api};
    use terra_cosmwasm::TerraRoute;
    use terraswap::asset::AssetInfo;

    fn get_test_init_msg() -> InitMsg {
        InitMsg {
            pool_address: "test_pool".to_string(),
            anchor_money_market_address: "test_mm".to_string(),
            aust_address: "test_aust".to_string(),
            seignorage_address: "test_seignorage".to_string(),
            profit_check_address: "test_profit_check".to_string(),
            community_fund_addr: "community_fund".to_string(),
            warchest_addr: "warchest".to_string(),
            asset_info: AssetInfo::NativeToken{ denom: "uusd".to_string() },
            token_code_id: 0u64,
            warchest_fee: Decimal::percent(10u64),
            community_fund_fee: Decimal::permille(5u64),
            max_community_fund_fee: Uint128::from(1000000u64),
            stable_cap: Uint128::from(100_000_000u64),
        }
    }

    #[test]
    fn test_initialization() {
        let mut deps = mock_dependencies(&[]);

        let msg = get_test_init_msg();
        let env = mock_env();
        let info = MessageInfo{sender: deps.api.addr_validate("creator").unwrap(), funds: vec![]};

        let res = instantiate(deps.as_mut(), env, info, msg).unwrap();
        assert_eq!(1, res.messages.len());
    }

    #[test]
    fn test_set_stable_cap() {
        let mut deps = mock_dependencies(&[]);

        let msg = get_test_init_msg();
        let env = mock_env();
        let msg_info = MessageInfo{sender: deps.api.addr_validate("creator").unwrap(), funds: vec![]};

        let res = instantiate(deps.as_mut(), env.clone(), msg_info.clone(), msg).unwrap();
        assert_eq!(1, res.messages.len());

        let info: PoolInfoRaw = POOL_INFO.load(&deps.storage).unwrap();
        assert_eq!(info.stable_cap, Uint128::from(100_000_000u64));

        let msg = ExecuteMsg::SetStableCap {
            stable_cap: Uint128::from(100_000u64)
        };
        let _res = execute(deps.as_mut(), env, msg_info, msg).unwrap();
        let info: PoolInfoRaw = POOL_INFO.load(&deps.storage).unwrap();
        assert_eq!(info.stable_cap, Uint128::from(100_000u64));
    }

    #[test]
    fn test_set_warchest_fee() {
        let mut deps = mock_dependencies(&[]);

        let msg = get_test_init_msg();
        let env = mock_env();
        let msg_info = MessageInfo{sender: deps.api.addr_validate("creator").unwrap(), funds: vec![]};

        let res = instantiate(deps.as_mut(), env.clone(), msg_info.clone(), msg).unwrap();
        assert_eq!(1, res.messages.len());

        let info: PoolInfoRaw = POOL_INFO.load(&deps.storage).unwrap();
        assert_eq!(info.stable_cap, Uint128::from(100_000_000u64));

        let warchest_fee = FEE.load(&deps.storage).unwrap().warchest_fee.share;
        let new_fee =  Decimal::permille(1u64);
        assert_ne!(warchest_fee, new_fee);
        let msg = ExecuteMsg::SetFee {
            community_fund_fee: None,
            warchest_fee: Some(Fee { share: new_fee })
        };
        let _res = execute(deps.as_mut(), env, msg_info, msg).unwrap();
        let warchest_fee = FEE.load(&deps.storage).unwrap().warchest_fee.share;
        assert_eq!(warchest_fee, new_fee);
    }

    #[test]
    fn test_set_community_fund_fee() {
        let mut deps = mock_dependencies(&[]);

        let msg = get_test_init_msg();
        let env = mock_env();
        let msg_info = MessageInfo{sender: deps.api.addr_validate("creator").unwrap(), funds: vec![]};

        let res = instantiate(deps.as_mut(), env.clone(), msg_info.clone(), msg).unwrap();
        assert_eq!(1, res.messages.len());

        let info: PoolInfoRaw = POOL_INFO.load(&deps.storage).unwrap();
        assert_eq!(info.stable_cap, Uint128::from(100_000u64));

        let community_fund_fee = FEE.load(&deps.storage).unwrap().community_fund_fee.fee.share;
        let new_fee =  Decimal::permille(1u64);
        let new_max_fee = Uint128::from(42u64);
        assert_ne!(community_fund_fee, new_fee);
        let msg = ExecuteMsg::SetFee {
            community_fund_fee:  Some(CappedFee { fee: Fee{ share: new_fee }, max_fee: new_max_fee }),
            warchest_fee: None
        };
        let _res = execute(deps.as_mut(), env, msg_info, msg).unwrap();
        let community_fund_fee = FEE.load(&deps.storage).unwrap().community_fund_fee.fee.share;
        let community_fund_max_fee = FEE.load(&deps.storage).unwrap().community_fund_fee.max_fee;
        assert_eq!(community_fund_fee, new_fee);
        assert_eq!(community_fund_max_fee, new_max_fee);
    }

    #[test]
    fn when_given_a_below_peg_msg_then_handle_returns_first_a_mint_then_a_terraswap_msg() {
        let mut deps = mock_dependencies(&[]);

        let msg = get_test_init_msg();
        let env = mock_env();
        let msg_info = MessageInfo{sender: deps.api.addr_validate("creator").unwrap(), funds: vec![]};

        let _res = instantiate(deps.as_mut(), env.clone(), msg_info.clone(), msg).unwrap();

        let msg = ExecuteMsg::BelowPeg {
            amount: Coin{denom: "uusd".to_string(), amount: Uint128::from(1000000u64)},
            slippage: Decimal::percent(1u64),
            belief_price: Decimal::from_ratio(Uint128::new(320),Uint128::new(10)), 
        };

        let res = execute(deps.as_mut(), env, msg_info, msg).unwrap();
        assert_eq!(4, res.messages.len());
        let second_msg = res.messages[1].msg.clone();
        match second_msg {
            CosmosMsg::Bank(_bank_msg) => panic!("unexpected"),
            CosmosMsg::Custom(t) => assert_eq!(TerraRoute::Market, t.route),
            CosmosMsg::Wasm(_wasm_msg) => panic!("unexpected"),
            _ => panic!("unexpected"),
        }
        let second_msg = res.messages[2].msg.clone();
        match second_msg {
            CosmosMsg::Bank(_bank_msg) => panic!("unexpected"),
            CosmosMsg::Custom(_t) => panic!("unexpected"),
            CosmosMsg::Wasm(_wasm_msg) => {},
            _ => panic!("unexpected"),
        }
    }

    #[test]
    fn when_given_an_above_peg_msg_then_handle_returns_first_a_terraswap_then_a_mint_msg() {
        let mut deps = mock_dependencies(&[]);

        let msg = get_test_init_msg();
        let env = mock_env();
        let msg_info = MessageInfo{sender: deps.api.addr_validate("creator").unwrap(), funds: vec![]};

        let _res = instantiate(deps.as_mut(), env.clone(), msg_info.clone(), msg).unwrap();

        let msg = ExecuteMsg::AbovePeg {
            amount: Coin{denom: "uusd".to_string(), amount: Uint128::from(1000000u64)},
            slippage: Decimal::percent(1u64),
            belief_price: Decimal::from_ratio(Uint128::new(320),Uint128::new(10)), 
        };

        let res = execute(deps.as_mut(), env, msg_info, msg).unwrap();
        assert_eq!(4, res.messages.len());
        let second_msg = res.messages[1].msg.clone();
        match second_msg {
            CosmosMsg::Bank(_bank_msg) => panic!("unexpected"),
            CosmosMsg::Custom(_t) => panic!("unexpected"),
            CosmosMsg::Wasm(_wasm_msg) => {},
            _ => panic!("unexpected"),
        }
        let third_msg = res.messages[2].msg.clone();
        match third_msg {
            CosmosMsg::Bank(_bank_msg) => panic!("unexpected"),
            CosmosMsg::Custom(t) => assert_eq!(TerraRoute::Market, t.route),
            CosmosMsg::Wasm(_wasm_msg) => panic!("unexpected"),
            _ => panic!("unexpected"),
        }
    }
}

// TODO: 
// - Deposit when 0 in pool -> fix by requiring one UST one init
// - Add config for deposit amounts 
