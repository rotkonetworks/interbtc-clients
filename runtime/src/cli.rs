use crate::{
    error::{Error, KeyLoadingError},
    InterBtcParachain, InterBtcSigner, ShutdownSender,
};
use clap::Parser;
use sp_core::{sr25519, Pair};
use sp_core::crypto::SecretStringError::InvalidFormat;
use sp_keyring::AccountKeyring;
use std::{collections::HashMap, num::ParseIntError, str::FromStr, time::Duration};
use hex;

#[derive(Parser, Debug, Clone)]
pub struct ProviderUserOpts {
    /// Keyring to use, mutually exclusive with keyname.
    #[clap(long, conflicts_with_all = ["keyfile","keyuri"], value_parser = parse_account_keyring)]
    pub keyring: Option<AccountKeyring>,

    /// Path to the json file containing key pairs in a map.
    /// Valid content of this file is e.g.
    /// `{ "MyUser1": "<Polkadot Account Mnemonic or Hex Secret Seed>", "MyUser2": "<Polkadot Account Mnemonic or Hex Secret Seed>" }`.
    #[clap(long, conflicts_with_all = ["keyring"], requires = "keyname", required_unless_present_any = ["keyring","keyuri"])]
    pub keyfile: Option<String>,

    /// The name of the account from the keyfile to use.
    #[clap(long, conflicts_with = "keyring", required_unless_present = "keyring")]
    pub keyname: Option<String>,

    /// The secret seed or mnemonic to use directly.
    #[clap(long, conflicts_with_all = ["keyring"], requires = "keyname", required_unless_present_any = ["keyring","keyfile"])]
    pub keyuri: Option<String>,
}

impl ProviderUserOpts {
    pub fn get_key_pair(&self) -> Result<(sr25519::Pair, String), Error> {
        match (
            self.keyfile.as_ref(),
            self.keyname.as_ref(),
            &self.keyring,
            self.keyuri.as_ref(),
        ) {
            (Some(file_path), Some(keyname), None, None) => {
                Ok((get_credentials_from_file(file_path, keyname)?, keyname.to_string()))
            }
            (None, Some(keyname), None, Some(keyuri)) => {
                Ok((get_pair_from_uri(keyuri)?, keyname.to_string()))
            }
            (Some(_), Some(keyname), None, Some(keyuri)) => {
                Ok((get_pair_from_uri(keyuri)?, keyname.to_string()))
            }
            (None, None, Some(keyring), None) => Ok((keyring.pair(), keyring.to_string())),
            _ => Err(Error::KeyringArgumentError),
        }
    }
}

/// Creates a key pair from URI (supports both mnemonic and hex seed)
fn get_pair_from_uri(uri: &str) -> Result<sr25519::Pair, KeyLoadingError> {
    // Try parsing as hex seed first if it looks like a hex string
    if (uri.len() == 64 && uri.chars().all(|c| c.is_ascii_hexdigit())) || 
       (uri.starts_with("0x") && uri.len() == 66 && uri[2..].chars().all(|c| c.is_ascii_hexdigit())) {
        return get_pair_from_hex_seed(uri);
    }

    // Fall back to mnemonic parsing
    sr25519::Pair::from_string(uri, None).map_err(KeyLoadingError::SecretStringError)
}

/// Creates a key pair from hex seed
fn get_pair_from_hex_seed(hex_seed: &str) -> Result<sr25519::Pair, KeyLoadingError> {
    let clean_hex = hex_seed.trim_start_matches("0x");
    
    // Parse hex to bytes
    let seed_bytes = hex::decode(clean_hex)
        .map_err(|_| KeyLoadingError::SecretStringError(InvalidFormat))?;
    
    if seed_bytes.len() != 32 {
        return Err(KeyLoadingError::SecretStringError(InvalidFormat));
    }

    // Create pair from seed bytes
    let pair = sr25519::Pair::from_seed_slice(&seed_bytes)
        .map_err(|_| KeyLoadingError::SecretStringError(InvalidFormat))?;
    
    Ok(pair)
}

/// Loads the credentials from file (supports both mnemonic and hex seed)
///
/// # Arguments
///
/// * `file_path` - path to the json file containing the credentials
/// * `keyname` - name of the key to get
fn get_credentials_from_file(file_path: &str, keyname: &str) -> Result<sr25519::Pair, KeyLoadingError> {
    let file = std::fs::File::open(file_path)?;
    let reader = std::io::BufReader::new(file);
    let map: HashMap<String, String> = serde_json::from_reader(reader)?;
    let key_str = map.get(keyname).ok_or(KeyLoadingError::KeyNotFound)?;
    
    // Try hex first if it starts with 0x
    if key_str.starts_with("0x") {
        return get_pair_from_hex_seed(key_str);
    }
    
    // Otherwise try as mnemonic
    sr25519::Pair::from_string(key_str, None).map_err(KeyLoadingError::SecretStringError)
}

pub fn parse_account_keyring(src: &str) -> Result<AccountKeyring, Error> {
    AccountKeyring::from_str(src).map_err(|_| Error::KeyringAccountParsingError)
}

pub fn parse_duration_ms(src: &str) -> Result<Duration, ParseIntError> {
    Ok(Duration::from_millis(src.parse::<u64>()?))
}

pub fn parse_duration_minutes(src: &str) -> Result<Duration, ParseIntError> {
    Ok(Duration::from_secs(src.parse::<u64>()? * 60))
}

#[derive(Parser, Debug, Clone)]
pub struct ConnectionOpts {
    /// Parachain websocket URL.
    #[cfg_attr(
        feature = "parachain-metadata-kintsugi",
        clap(long, default_value = "wss://api-kusama.interlay.io:443/parachain")
    )]
    #[cfg_attr(
        feature = "parachain-metadata-interlay",
        clap(long, default_value = "wss://api.interlay.io:443/parachain")
    )]
    pub btc_parachain_url: String,

    /// Timeout in milliseconds to wait for connection to btc-parachain.
    #[clap(long, value_parser = parse_duration_ms, default_value = "60000")]
    pub btc_parachain_connection_timeout_ms: Duration,

    /// Maximum number of concurrent requests
    #[clap(long)]
    pub max_concurrent_requests: Option<usize>,

    /// Maximum notification capacity for each subscription
    #[clap(long)]
    pub max_notifs_per_subscription: Option<usize>,
}

impl ConnectionOpts {
    pub async fn try_connect(
        &self,
        signer: InterBtcSigner,
        shutdown_tx: ShutdownSender,
    ) -> Result<InterBtcParachain, Error> {
        InterBtcParachain::from_url_and_config_with_retry(
            &self.btc_parachain_url,
            signer,
            self.max_concurrent_requests,
            self.max_notifs_per_subscription,
            self.btc_parachain_connection_timeout_ms,
            shutdown_tx,
        )
        .await
    }
}
