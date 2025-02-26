use thiserror::Error;

use cosmwasm_std::StdError;
use cw_controllers::AdminError;

#[derive(Error, Debug, PartialEq)]
pub enum StableArbError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error("{0}")]
    Admin(#[from] AdminError),

    #[error("Semver parsing error: {0}")]
    SemVer(String),

    #[error("Unauthorized")]
    Unauthorized {},

    #[error("Call is not a callback!")]
    NotCallback {},

    #[error("Not enough funds to perform arb-trade")]
    Broke {},

    #[error("The name of the proposed pool can not have length 0.")]
    EmptyPoolName {},

    #[error("The pool list has reached its limit, can't store more contracts.")]
    PoolLimitReached {},
}

impl From<semver::Error> for StableArbError {
    fn from(err: semver::Error) -> Self {
        Self::SemVer(err.to_string())
    }
}
