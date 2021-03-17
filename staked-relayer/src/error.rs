use crate::{core::Error as CoreError, relay::Error as RelayError};
use backoff::ExponentialBackoff;
use bitcoin::{BitcoinError as BitcoinCoreError, Error as BitcoinError};
use jsonrpc_core_client::RpcError;
use parity_scale_codec::Error as CodecError;
use runtime::{substrate_subxt::Error as XtError, Error as RuntimeError};
use std::time::Duration;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Could not verify that the oracle is offline")]
    CheckOracleOffline,
    #[error("Suggested status update does not contain block hash")]
    EventNoBlockHash,
    #[error("Error fetching transaction")]
    TransactionFetchingError,
    #[error("Mathematical operation caused an overflow")]
    ArithmeticOverflow,
    #[error("Mathematical operation caused an underflow")]
    ArithmeticUnderflow,

    #[error("RuntimeError: {0}")]
    RuntimeError(#[from] RuntimeError),
    #[error("RelayError: {0}")]
    RelayError(#[from] RelayError),
    #[error("SubXtError: {0}")]
    SubXtError(#[from] XtError),
    #[error("CoreError: {0}")]
    CoreError(#[from] CoreError<RelayError>),
    #[error("CodecError: {0}")]
    CodecError(#[from] CodecError),
    #[error("BitcoinError: {0}")]
    BitcoinError(#[from] BitcoinError),
    #[error("BitcoinCoreError: {0}")]
    BitcoinCoreError(#[from] BitcoinCoreError),
    #[error("RPC error: {0}")]
    RpcError(#[from] RpcError),
}

/// Gets the default retrying policy
pub fn get_retry_policy() -> ExponentialBackoff {
    ExponentialBackoff {
        max_elapsed_time: Some(Duration::from_secs(24 * 60 * 60)),
        max_interval: Duration::from_secs(10 * 60), // wait at 10 minutes before retrying
        initial_interval: Duration::from_secs(1),
        current_interval: Duration::from_secs(1),
        multiplier: 2.0,            // delay doubles every time
        randomization_factor: 0.25, // random value between 25% below and 25% above the ideal delay
        ..Default::default()
    }
}
