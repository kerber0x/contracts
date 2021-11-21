use cosmwasm_std::{coin, coins, from_binary};
use cosmwasm_std::{Decimal, Uint128};
use cosmwasm_std::testing::{mock_env, mock_info};
use terraswap::asset::{Asset, AssetInfo};

use white_whale::fee::*;
use white_whale::ust_vault::msg::*;
use white_whale::ust_vault::msg::VaultQueryMsg as QueryMsg;

use crate::contract::{execute, query};
use crate::tests::common::{TEST_CREATOR};
use crate::tests::instantiate::mock_instantiate;
use crate::tests::mock_querier::mock_dependencies;

/*#[test]
pub fn test_config_query() {
    let mut deps = mock_dependencies(&[]);
    mock_instantiate(deps.as_mut());
    let env = mock_env();

    let msg = ExecuteMsg::ProvideLiquidity {
        asset: Asset {
            info: AssetInfo::NativeToken {
                denom: String::from("uusd")
            },
            amount: Uint128::new(1000),
        }
    };
    let info = mock_info(TEST_CREATOR, &coins(1000, "uusd"));
    execute(deps.as_mut(), env.clone(), info, msg).unwrap();


    let q_res: PoolInfo = from_binary(&query(deps.as_ref(), env, QueryMsg::Config {}).unwrap()).unwrap();
    assert_eq!(
        q_res.stable_cap,
        Uint128::from(100_000_000u64)
    )
}*/

#[test]
pub fn test_state_query() {
    let mut deps = mock_dependencies(&[]);
    mock_instantiate(deps.as_mut());
    let env = mock_env();
    let msg = ExecuteMsg::ProvideLiquidity {
        asset: Asset {
            info: AssetInfo::NativeToken {
                denom: String::from("uusd")
            },
            amount: Uint128::new(1000),
        }
    };


    let info = mock_info(TEST_CREATOR, &coins(1000, "uusd"));
    let execute_res = execute(deps.as_mut(), env.clone(), info, msg);


    let q_res: StateResponse =
        from_binary(&query(deps.as_ref(), env, QueryMsg::State {}).unwrap()).unwrap();
    assert_eq!(
        q_res.allow_non_whitelisted,
        false
    )
}

//#[test]
pub fn test_pool_query() {
    let mut deps = mock_dependencies(&coins(1000, "uusd"));
    mock_instantiate(deps.as_mut());
    let env = mock_env();

    let info = mock_info(TEST_CREATOR, &[]);

    let q_res: PoolResponse =
        from_binary(&query(deps.as_ref(), env, QueryMsg::Pool {}).unwrap()).unwrap();
    assert_eq!(
        q_res.assets,
        [
            Asset {
                amount: Uint128::from(10000u128),
                info: AssetInfo::NativeToken {
                    denom: "uwhale".to_string(),
                },
            },
            Asset {
                amount: Uint128::from(10000u128),
                info: AssetInfo::NativeToken {
                    denom: "uusd".to_string(),
                },
            },
        ]
    );
    assert_eq!(
        q_res.total_share,
        Uint128::from(1000u128)
    )
}

#[test]
pub fn test_fees_query() {
    let mut deps = mock_dependencies(&[]);
    mock_instantiate(deps.as_mut());
    let env = mock_env();


    let q_res: FeeResponse =
        from_binary(&query(deps.as_ref(), env, QueryMsg::Fees {}).unwrap()).unwrap();
    assert_eq!(
        q_res.fees.warchest_fee.share,
        Decimal::percent(10u64)
    );
    assert_eq!(
        q_res.fees.flash_loan_fee.share,
        Decimal::permille(5u64)
    );
}

#[test]
pub fn test_vault_value_query() {
    let mut deps = mock_dependencies(&[coin(1000, "uusd")]);
    mock_instantiate(deps.as_mut());
    let env = mock_env();

    let q_res: ValueResponse =
        from_binary(&query(deps.as_ref(), env, QueryMsg::VaultValue {}).unwrap()).unwrap();
    assert_eq!(
        q_res.total_ust_value,
        Uint128::new(1000)
    )
}

//#[test]
pub fn test_vault_estimate_fee_query() {
    let mut deps = mock_dependencies(&[coin(1000, "uusd")]);
    mock_instantiate(deps.as_mut());
    let env = mock_env();

    let q_res: EstimateWithdrawFeeResponse =
        from_binary(&query(deps.as_ref(), env, QueryMsg::EstimateWithdrawFee { amount: Uint128::new(1000) }).unwrap()).unwrap();
    assert_ne!(
        q_res.fee,
        vec![]
    )
}
