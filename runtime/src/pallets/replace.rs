use super::Core;
use crate::ReplaceRequest;
use codec::{Decode, Encode};
use core::marker::PhantomData;
use std::fmt::Debug;
use substrate_subxt_proc_macro::{module, Call, Event, Store};

#[module]
pub trait Replace: Core {}

#[derive(Clone, Debug, PartialEq, Call, Encode)]
pub struct RequestReplaceCall<T: Replace> {
    #[codec(compact)]
    pub btc_amount: T::Issuing,
    #[codec(compact)]
    pub griefing_collateral: T::Backing,
}

#[derive(Clone, Debug, PartialEq, Call, Encode)]
pub struct WithdrawReplaceCall<T: Replace> {
    #[codec(compact)]
    pub amount: T::Issuing,
}

#[derive(Clone, Debug, PartialEq, Call, Encode)]
pub struct AcceptReplaceCall<T: Replace> {
    pub old_vault: T::AccountId,
    #[codec(compact)]
    pub amount_btc: T::Issuing,
    #[codec(compact)]
    pub collateral: T::Backing,
    pub btc_address: T::BtcAddress,
}

#[derive(Clone, Debug, PartialEq, Call, Encode)]
pub struct ExecuteReplaceCall<T: Replace> {
    pub replace_id: T::H256,
    pub merkle_proof: Vec<u8>,
    pub raw_tx: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Call, Encode)]
pub struct CancelReplaceCall<T: Replace> {
    pub replace_id: T::H256,
}

#[derive(Clone, Debug, Eq, PartialEq, Event, Decode)]
pub struct RequestReplaceEvent<T: Replace> {
    pub old_vault_id: T::AccountId,
    pub amount_btc: T::Issuing,
    pub griefing_collateral: T::Backing,
}

#[derive(Clone, Debug, Eq, PartialEq, Event, Decode)]
pub struct WithdrawReplaceEvent<T: Replace> {
    pub old_vault_id: T::AccountId,
    pub withdrawn_tokens: T::Issuing,
    pub withdrawn_griefing_collateral: T::Backing,
}

#[derive(Clone, Debug, Eq, PartialEq, Event, Decode)]
pub struct AcceptReplaceEvent<T: Replace> {
    pub replace_id: T::H256,
    pub old_vault_id: T::AccountId,
    pub new_vault_id: T::AccountId,
    pub amount_btc: T::Issuing,
    pub collateral: T::Backing,
    pub btc_address: T::BtcAddress,
}

#[derive(Clone, Debug, Eq, PartialEq, Event, Decode)]
pub struct ExecuteReplaceEvent<T: Replace> {
    pub replace_id: T::H256,
    pub old_vault_id: T::AccountId,
    pub new_vault_id: T::AccountId,
}

#[derive(Clone, Debug, Eq, PartialEq, Event, Decode)]
pub struct CancelReplaceEvent<T: Replace> {
    pub replace_id: T::H256,
    pub new_vault_id: T::AccountId,
    pub old_vault_id: T::AccountId,
    pub griefing_collateral: T::Backing,
}

#[derive(Clone, Debug, Eq, PartialEq, Store, Encode)]
pub struct ReplacePeriodStore<T: Replace> {
    #[store(returns = T::BlockNumber)]
    pub _runtime: PhantomData<T>,
}

#[derive(Clone, Debug, Eq, PartialEq, Store, Encode)]
pub struct ReplaceBtcDustValueStore<T: Replace> {
    #[store(returns = T::Issuing)]
    pub _runtime: PhantomData<T>,
}

#[derive(Clone, Debug, Eq, PartialEq, Store, Encode)]
pub struct ReplaceRequestsStore<T: Replace> {
    #[store(returns = ReplaceRequest<T::AccountId, T::BlockNumber, T::Issuing, T::Backing>)]
    pub _runtime: PhantomData<T>,
    pub replace_id: T::H256,
}

#[derive(Clone, Debug, PartialEq, Call, Encode)]
pub struct SetReplacePeriodCall<T: Replace> {
    pub period: T::BlockNumber,
    pub _runtime: PhantomData<T>,
}
