use log::error;
use parity_scale_codec::Decode;
use runtime::pallet_balances_dot::*;
use runtime::pallet_btc_relay::*;
use runtime::pallet_exchange_rate_oracle::*;
use runtime::pallet_security::*;
use runtime::pallet_staked_relayers::*;
use runtime::pallet_timestamp::*;
use runtime::pallet_vault_registry::*;
use runtime::PolkaBTC;
use sp_core::crypto::{AccountId32, Pair};
use sp_core::sr25519::Pair as KeyPair;
use std::convert::TryInto;
use std::sync::Arc;
use substrate_subxt::{
    balances::AccountData, system::System, Client, EventSubscription, EventsDecoder, PairSigner,
};
use tokio::sync::Mutex;

mod error;
mod oracle;

pub use oracle::OracleChecker;

#[cfg(test)]
pub mod mock;

pub use error::Error;

pub type PolkaBTCVault = Vault<
    <PolkaBTC as System>::AccountId,
    <PolkaBTC as System>::BlockNumber,
    <PolkaBTC as VaultRegistry>::PolkaBTC,
>;

#[derive(Clone)]
pub struct Provider {
    client: Client<PolkaBTC>,
    signer: Arc<Mutex<PairSigner<PolkaBTC, KeyPair>>>,
}

fn bytes_to_address(id: &[u8]) -> Result<[u8; 32], std::array::TryFromSliceError> {
    id.try_into()
}

impl Provider {
    pub fn new(
        client: Client<PolkaBTC>,
        signer: Arc<Mutex<PairSigner<PolkaBTC, KeyPair>>>,
    ) -> Self {
        Self { client, signer }
    }

    pub async fn get_address(&self) -> AccountId32 {
        self.signer.lock().await.signer().public().into()
    }

    pub async fn get_time_now(&self) -> Result<u64, Error> {
        Ok(self.client.now(None).await?)
    }

    pub async fn get_best_block(&self) -> Result<H256Le, Error> {
        Ok(self.client.best_block(None).await?)
    }

    pub async fn get_best_block_height(&self) -> Result<u32, Error> {
        Ok(self.client.best_block_height(None).await?)
    }

    pub async fn get_block_hash(&self, height: u32) -> Result<H256Le, Error> {
        // TODO: adjust chain index
        Ok(self.client.chains_hashes(0, height, None).await?)
    }

    pub async fn get_block_header(&self, hash: H256Le) -> Result<RichBlockHeader, Error> {
        Ok(self.client.block_headers(hash, None).await?)
    }

    pub async fn get_account_data(
        &self,
        id: <PolkaBTC as System>::AccountId,
    ) -> Result<AccountData<<PolkaBTC as DOT>::Balance>, Error> {
        Ok(self.client.account(id, None).await?)
    }

    pub async fn get_parachain_status(&self) -> Result<StatusCode, Error> {
        Ok(self.client.parachain_status(None).await?)
    }

    pub async fn get_status_update(
        &self,
        id: u64,
    ) -> Result<
        StatusUpdate<
            <PolkaBTC as System>::AccountId,
            <PolkaBTC as System>::BlockNumber,
            <PolkaBTC as StakedRelayers>::DOT,
        >,
        Error,
    > {
        Ok(self.client.status_updates(id.into(), None).await?)
    }

    pub async fn get_vault(&self, id: Vec<u8>) -> Result<PolkaBTCVault, Error> {
        Ok(self
            .client
            .vaults(bytes_to_address(&id)?.into(), None)
            .await?)
    }

    pub async fn get_all_vaults(&self) -> Result<Vec<PolkaBTCVault>, Error> {
        let mut vaults = Vec::new();
        let mut iter = self.client.vaults_iter(None).await?;
        while let Some((_, account)) = iter.next().await? {
            vaults.push(account);
        }
        Ok(vaults)
    }

    pub async fn get_liquidation_threshold(&self) -> Result<u128, Error> {
        Ok(self.client.liquidation_collateral_threshold(None).await?)
    }

    pub async fn get_exchange_rate_info(&self) -> Result<(u64, u64, u64), Error> {
        let get_rate = self.client.exchange_rate(None);
        let get_time = self.client.last_exchange_rate_time(None);
        let get_delay = self.client.max_delay(None);

        match tokio::try_join!(get_rate, get_time, get_delay) {
            Ok((rate, time, delay)) => Ok((rate.try_into()?, time.into(), delay.into())),
            Err(_) => Err(Error::ExchangeRateInfo),
        }
    }

    pub async fn initialize_btc_relay(
        &self,
        header: RawBlockHeader,
        height: BitcoinBlockHeight,
    ) -> Result<(), Error> {
        self.client
            .initialize_and_watch(&*self.signer.lock().await, header, height)
            .await?;
        Ok(())
    }

    pub async fn store_block_header(&self, header: RawBlockHeader) -> Result<(), Error> {
        self.client
            .store_block_header_and_watch(&*self.signer.lock().await, header)
            .await?;
        Ok(())
    }

    pub async fn register_vault(&self, collateral: u128, btc_address: H160) -> Result<(), Error> {
        self.client
            .register_vault_and_watch(&*self.signer.lock().await, collateral, btc_address)
            .await?;
        Ok(())
    }

    pub async fn register_staked_relayer(&self, stake: u128) -> Result<(), Error> {
        self.client
            .register_staked_relayer_and_watch(&*self.signer.lock().await, stake)
            .await?;
        Ok(())
    }

    pub async fn deregister_staked_relayer(&self) -> Result<(), Error> {
        self.client
            .deregister_staked_relayer_and_watch(&*self.signer.lock().await)
            .await?;
        Ok(())
    }

    pub async fn suggest_status_update(
        &self,
        deposit: u128,
        status_code: StatusCode,
        add_error: Option<ErrorCode>,
        remove_error: Option<ErrorCode>,
    ) -> Result<(), Error> {
        self.client
            .suggest_status_update_and_watch(
                &*self.signer.lock().await,
                deposit,
                status_code,
                add_error,
                remove_error,
                None,
            )
            .await?;
        Ok(())
    }

    pub async fn on_register<F>(&self, mut cb: F) -> Result<(), Error>
    where
        F: FnMut(
            Vault<
                <PolkaBTC as System>::AccountId,
                <PolkaBTC as System>::BlockNumber,
                <PolkaBTC as VaultRegistry>::PolkaBTC,
            >,
        ),
    {
        let sub = self.client.subscribe_events().await?;
        let mut decoder = EventsDecoder::<PolkaBTC>::new(self.client.metadata().clone());
        decoder.register_type_size::<u128>("Balance");
        decoder.register_type_size::<u128>("DOT");

        let mut sub = EventSubscription::<PolkaBTC>::new(sub, decoder);
        sub.filter_event::<RegisterVaultEvent<_>>();
        while let Some(result) = sub.next().await {
            match result {
                Ok(raw_event) => {
                    let event = RegisterVaultEvent::<PolkaBTC>::decode(&mut &raw_event.data[..])?;
                    let account = self.client.vaults(event.account_id, None).await?;
                    cb(account);
                }
                Err(err) => {
                    error!("{}", err);
                }
            };
        }

        Ok(())
    }
}
