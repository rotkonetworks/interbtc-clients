use crate::error::Error;
use backoff::{retry, Error as BackoffError, ExponentialBackoff};
use bytes::Bytes;
use codec::Decode;
use futures::{future::BoxFuture, TryFutureExt};
use jsonrpsee::{
    core::client::{Client as WsClient, ClientT},
    rpc_params,
    ws_client::WsClientBuilder,
};
use mockall_double::double;
use reqwest::Url;
use sp_core::{Bytes as SpCoreBytes, H256};
use sp_core_hashing::twox_128;

use std::{
    convert::TryInto,
    fmt::Debug,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::prelude::OpenOptionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    str,
    time::Duration,
};

use futures_util::{FutureExt, StreamExt};
use nix::{
    sys::signal::{self, Signal},
    unistd::Pid,
};
use signal_hook_tokio::SignalsInfo;

use async_trait::async_trait;

pub const PARACHAIN_MODULE: &str = "VaultRegistry";
pub const CURRENT_RELEASE_STORAGE_ITEM: &str = "CurrentClientRelease";
pub const PENDING_RELEASE_STORAGE_ITEM: &str = "PendingClientRelease";
pub const BLOCK_TIME: Duration = Duration::from_secs(6);

// One minute
pub const RETRY_TIMEOUT_MS: u64 = 60_000;

// One second
pub const RETRY_INTERVAL_MS: u64 = 1_000;

// Constant interval retry
pub const RETRY_MULTIPLIER: f64 = 1.0;

// Wrap `WsClient` in a newtype pattern to be able to mock it.
mod ws_client_newtype {
    use crate::runner::WsClient;

    pub struct WebsocketClient(WsClient);

    #[cfg_attr(test, mockall::automock)]
    impl WebsocketClient {
        pub fn new(ws_client: WsClient) -> Self {
            Self(ws_client)
        }

        pub fn inner(&self) -> &WsClient {
            &self.0
        }
    }
}

#[double]
use ws_client_newtype::WebsocketClient;

#[derive(Decode, Default, Eq, PartialEq, Debug, Clone)]
pub struct ClientRelease {
    pub uri: String,
    pub code_hash: H256,
}

#[derive(Default, Eq, PartialEq, Debug, Clone)]
pub struct DownloadedRelease {
    pub release: ClientRelease,
    pub path: PathBuf,
    pub bin_name: String,
}

pub struct Runner {
    parachain_rpc: WebsocketClient,
    vault_args: Vec<String>,
    child_proc: Option<Child>,
    downloaded_release: Option<DownloadedRelease>,
    download_path: PathBuf,
}

impl Runner {
    pub fn new(parachain_rpc: WebsocketClient, vault_args: Vec<String>, download_path: PathBuf) -> Self {
        Self {
            parachain_rpc,
            vault_args,
            child_proc: None,
            downloaded_release: None,
            download_path,
        }
    }

    async fn download_binary(runner: &mut impl RunnerExt, release: ClientRelease) -> Result<DownloadedRelease, Error> {
        let (bin_name, bin_path) = runner.uri_to_bin_path(&release.uri)?;
        log::info!("Downloading {} at: {:?}", bin_name, bin_path);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            // Make the binary executable.
            // The set permissions are: -rwx------
            .mode(0o700)
            .create(true)
            .open(bin_path.clone())?;

        let bytes = runner.get_request_bytes(release.uri.clone()).await?;
        file.write_all(&bytes)?;
        file.sync_all()?;

        let downloaded_release = DownloadedRelease {
            release,
            path: bin_path,
            bin_name: bin_name.to_string(),
        };
        runner.set_downloaded_release(Some(downloaded_release.clone()));
        Ok(downloaded_release)
    }

    fn uri_to_bin_path(runner: &impl RunnerExt, uri: &str) -> Result<(String, PathBuf), Error> {
        // Remove any trailing slashes from the release URI
        let parsed_uri = Url::parse(uri.trim_end_matches('/'))?;
        let bin_name = parsed_uri
            .path_segments()
            .and_then(|segments| segments.last())
            .and_then(|name| if name.is_empty() { None } else { Some(name) })
            .ok_or(Error::ClientNameDerivationError)?;
        let bin_path = runner.download_path().join(bin_name);
        Ok((bin_name.to_string(), bin_path))
    }

    fn delete_downloaded_release(runner: &mut impl RunnerExt) -> Result<(), Error> {
        let release = runner.downloaded_release().as_ref().ok_or(Error::NoDownloadedRelease)?;
        log::info!("Removing old release, with path {:?}", release.path);

        retry_with_log(
            || Ok(fs::remove_file(&release.path)?),
            "Failed to remove old release".to_string(),
        )?;

        runner.set_downloaded_release(None);
        Ok(())
    }

    fn terminate_proc_and_wait(runner: &mut impl RunnerExt) -> Result<u32, Error> {
        let child_proc = runner.child_proc().as_mut().ok_or(Error::NoChildProcess)?;

        let _ = retry_with_log(
            || {
                Ok(signal::kill(
                    Pid::from_raw(child_proc.id().try_into().map_err(|_| Error::IntegerConversionError)?),
                    Signal::SIGTERM,
                ))
            },
            "Failed to kill child process".to_string(),
        )?;

        match child_proc.wait() {
            Ok(exit_status) => log::info!(
                "Terminated vault process (pid: {}) with exit status {}",
                child_proc.id(),
                exit_status
            ),
            Err(error) => log::warn!("Vault process termination error: {}", error),
        };
        let pid = child_proc.id();
        runner.set_child_proc(None);
        Ok(pid)
    }

    async fn try_get_release<T: RunnerExt + StorageReader>(
        runner: &T,
        pending: bool,
    ) -> Result<Option<ClientRelease>, Error> {
        let storage_item = if pending {
            PENDING_RELEASE_STORAGE_ITEM
        } else {
            CURRENT_RELEASE_STORAGE_ITEM
        };

        let storage_key = runner.compute_storage_key(PARACHAIN_MODULE.to_string(), storage_item.to_string());

        retry_with_log_async(
            || {
                let key = &storage_key;
                runner
                    .read_chain_storage::<ClientRelease>(runner.parachain_rpc(), Some(key.to_string()))
                    .into_future()
                    .boxed()
            },
            "Error fetching executable".to_string(),
        )
        .await
    }

    async fn read_chain_storage<T: 'static + Decode + Debug, V: RunnerExt + StorageReader>(
        runner: &V,
        parachain_rpc: &WebsocketClient,
        maybe_storage_key: Option<String>,
    ) -> Result<Option<T>, Error> {
        let enc_res = runner
            .query_storage(parachain_rpc, maybe_storage_key, "state_getStorage".to_string())
            .await;
        enc_res
            .map(|r| {
                let v = r.to_vec();
                T::decode(&mut &v[..])
            })
            .transpose()
            .map_err(Into::into)
    }

    pub async fn run(mut runner: Box<dyn RunnerExt + Send>, mut shutdown_signals: SignalsInfo) -> Result<(), Error> {
        tokio::select! {
            _ = shutdown_signals.next() => {
                log::info!("Terminating vault...");
                runner.terminate_proc_and_wait()?;
            },
            _ = runner.as_mut().auto_update() => {
                log::error!("Auto-updater unexpectedly terminated.");
                return Err(Error::AutoUpdaterTerminated);
            }
        };
        Ok(())
    }

    async fn auto_update(runner: &mut impl RunnerExt) -> Result<(), Error> {
        // Create all directories for the `download_path` if they don't already exist.
        fs::create_dir_all(&runner.download_path())?;
        let release = runner.try_get_release(false).await?.expect("No current release");
        // WARNING: This will overwrite any pre-existing binary with the same name
        // TODO: Check if a release with the same version is already at the `download_path`
        runner.download_binary(release).await?;

        runner.run_binary()?;

        loop {
            if let Some(new_release) = runner.try_get_release(false).await? {
                let maybe_downloaded_release = runner.downloaded_release();
                let downloaded_release = maybe_downloaded_release.as_ref().ok_or(Error::NoDownloadedRelease)?;
                if new_release.uri != downloaded_release.release.uri {
                    // Wait for child process to finish completely.
                    // To ensure there can't be two vault processes using the same Bitcoin wallet.
                    runner.terminate_proc_and_wait()?;

                    // Delete old release
                    runner.delete_downloaded_release()?;

                    // Download new release
                    runner.download_binary(new_release).await?;

                    // Run the downloaded release
                    runner.run_binary()?;
                }
            }
            tokio::time::sleep(BLOCK_TIME).await;
        }
    }

    fn run_binary(runner: &mut impl RunnerExt, stdout_mode: impl Into<Stdio>) -> Result<Child, Error> {
        // Ensure there is no other child running
        if runner.child_proc().is_some() {
            return Err(Error::ChildProcessExists);
        }
        let downloaded_release = runner.downloaded_release().as_ref().ok_or(Error::NoDownloadedRelease)?;
        let mut command = Command::new(downloaded_release.path.as_os_str());
        command.args(runner.vault_args().clone()).stdout(stdout_mode);
        let child = retry::<_, _, Child, _>(custom_retry_config(), || {
            command
                .spawn()
                // The `Transient` error type means the closure will be retried
                .map_err(BackoffError::Transient)
        })?;
        log::info!("Vault started, with pid {}", child.id());
        Ok(child)
    }
}

#[async_trait]
pub trait RunnerExt {
    fn parachain_rpc(&self) -> &WebsocketClient;
    fn vault_args(&self) -> &Vec<String>;
    fn child_proc(&mut self) -> &mut Option<Child>;
    fn set_child_proc(&mut self, child_proc: Option<Child>);
    fn downloaded_release(&self) -> &Option<DownloadedRelease>;
    fn set_downloaded_release(&mut self, downloaded_release: Option<DownloadedRelease>);
    fn download_path(&self) -> &PathBuf;
    fn set_download_path(&mut self, download_path: PathBuf);
    async fn try_get_release(&self, pending: bool) -> Result<Option<ClientRelease>, Error>;
    async fn download_binary(&mut self, release: ClientRelease) -> Result<(), Error>;
    fn uri_to_bin_path(&self, uri: &str) -> Result<(String, PathBuf), Error>;
    fn delete_downloaded_release(&mut self) -> Result<(), Error>;
    fn run_binary(&mut self) -> Result<(), Error>;
    fn terminate_proc_and_wait(&mut self) -> Result<(), Error>;
    async fn get_request_bytes(&self, url: String) -> Result<Bytes, Error>;
    fn auto_update(&mut self) -> BoxFuture<'_, Result<(), Error>>;
}

#[async_trait]
impl RunnerExt for Runner {
    fn parachain_rpc(&self) -> &WebsocketClient {
        &self.parachain_rpc
    }

    fn vault_args(&self) -> &Vec<String> {
        &self.vault_args
    }

    fn child_proc(&mut self) -> &mut Option<Child> {
        &mut self.child_proc
    }

    fn set_child_proc(&mut self, child_proc: Option<Child>) {
        self.child_proc = child_proc;
    }

    fn downloaded_release(&self) -> &Option<DownloadedRelease> {
        &self.downloaded_release
    }

    fn set_downloaded_release(&mut self, downloaded_release: Option<DownloadedRelease>) {
        self.downloaded_release = downloaded_release;
    }

    fn download_path(&self) -> &PathBuf {
        &self.download_path
    }

    fn set_download_path(&mut self, download_path: PathBuf) {
        self.download_path = download_path;
    }

    async fn try_get_release(&self, pending: bool) -> Result<Option<ClientRelease>, Error> {
        Runner::try_get_release(self, pending).await
    }

    fn run_binary(&mut self) -> Result<(), Error> {
        let child = Runner::run_binary(self, Stdio::inherit())?;
        self.child_proc = Some(child);
        Ok(())
    }

    async fn download_binary(&mut self, release: ClientRelease) -> Result<(), Error> {
        let _downloaded_release = Runner::download_binary(self, release).await?;
        Ok(())
    }

    fn uri_to_bin_path(&self, uri: &str) -> Result<(String, PathBuf), Error> {
        Runner::uri_to_bin_path(self, uri)
    }

    fn delete_downloaded_release(&mut self) -> Result<(), Error> {
        Runner::delete_downloaded_release(self)?;
        Ok(())
    }

    fn terminate_proc_and_wait(&mut self) -> Result<(), Error> {
        Runner::terminate_proc_and_wait(self)?;
        Ok(())
    }

    // Declaring as a static method would highly complicate mocking
    async fn get_request_bytes(&self, url: String) -> Result<Bytes, Error> {
        log::info!("Fetching executable from {}", url);
        Ok(retry_with_log_async(
            || reqwest::get(url.clone()).into_future().boxed(),
            "Error fetching executable".to_string(),
        )
        .await?
        .bytes()
        .await?)
    }

    fn auto_update(&mut self) -> BoxFuture<'_, Result<(), Error>> {
        Runner::auto_update(self).into_future().boxed()
    }
}

#[async_trait]
pub trait StorageReader {
    fn compute_storage_key(&self, module: String, key: String) -> String;
    async fn query_storage(
        &self,
        parachain_rpc: &WebsocketClient,
        maybe_storage_key: Option<String>,
        method: String,
    ) -> Option<SpCoreBytes>;
    async fn read_chain_storage<T: 'static + Decode + Debug>(
        &self,
        parachain_rpc: &WebsocketClient,
        maybe_storage_key: Option<String>,
    ) -> Result<Option<T>, Error>;
}

#[async_trait]
impl StorageReader for Runner {
    fn compute_storage_key(&self, module: String, key: String) -> String {
        let module = twox_128(module.as_bytes());
        let item = twox_128(key.as_bytes());
        let key = hex::encode([module, item].concat());
        format!("0x{}", key)
    }

    async fn query_storage(
        &self,
        parachain_rpc: &WebsocketClient,
        maybe_storage_key: Option<String>,
        method: String,
    ) -> Option<SpCoreBytes> {
        let params = maybe_storage_key.map_or(rpc_params![], |key| rpc_params![key]);
        parachain_rpc.inner().request(method.as_str(), params).await.ok()
    }

    async fn read_chain_storage<T: 'static + Decode + Debug>(
        &self,
        parachain_rpc: &WebsocketClient,
        maybe_storage_key: Option<String>,
    ) -> Result<Option<T>, Error> {
        Runner::read_chain_storage(self, parachain_rpc, maybe_storage_key).await
    }
}

pub async fn ws_client(url: &str) -> Result<WebsocketClient, Error> {
    let ws_client = WsClientBuilder::default().build(url).await?;
    Ok(WebsocketClient::new(ws_client))
}

pub fn custom_retry_config() -> ExponentialBackoff {
    ExponentialBackoff {
        initial_interval: Duration::from_millis(RETRY_INTERVAL_MS),
        max_elapsed_time: Some(Duration::from_millis(RETRY_TIMEOUT_MS)),
        multiplier: RETRY_MULTIPLIER,
        ..ExponentialBackoff::default()
    }
}

pub fn retry_with_log<T, F>(f: F, log_msg: String) -> Result<T, Error>
where
    F: Fn() -> Result<T, Error>,
{
    Ok(retry(custom_retry_config(), || {
        f().map_err(|e| {
            log::info!("{}: {}. Retrying...", log_msg, e.to_string());
            BackoffError::Transient(Error::ProcessTerminationFailure)
        })
    })?)
}

pub async fn retry_with_log_async<'a, T, F, E>(f: F, log_msg: String) -> Result<T, Error>
where
    F: Fn() -> BoxFuture<'a, Result<T, E>>,
    E: Into<Error> + Sized,
{
    backoff::future::retry(custom_retry_config(), || async {
        f().await.map_err(|e| {
            log::info!("{}: {}. Retrying...", log_msg, e.into().to_string());
            BackoffError::Transient(Error::ProcessTerminationFailure)
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use bytes::Bytes;
    use codec::Decode;

    use futures::future::BoxFuture;
    use sp_core::{Bytes as SpCoreBytes, H256};
    use tempdir::TempDir;

    use std::{
        convert::TryInto,
        fmt::Debug,
        fs::{self, File},
        io::Write,
        os::unix::prelude::PermissionsExt,
        path::PathBuf,
        process::{Child, Command, Stdio},
        str::FromStr,
        thread,
    };

    use signal_hook::consts::*;
    use signal_hook_tokio::Signals;

    use crate::error::Error;

    use super::*;

    use sysinfo::{Pid, System, SystemExt};

    mockall::mock! {
        Runner {}

        #[async_trait]
        pub trait RunnerExt {
            fn parachain_rpc(&self) -> &WebsocketClient;
            fn vault_args(&self) -> &Vec<String>;
            fn child_proc(&mut self) -> &mut Option<Child>;
            fn set_child_proc(&mut self, child_proc: Option<Child>);
            fn downloaded_release(&self) -> &Option<DownloadedRelease>;
            fn set_downloaded_release(&mut self, downloaded_release: Option<DownloadedRelease>);
            fn download_path(&self) -> &PathBuf;
            fn set_download_path(&mut self, download_path: PathBuf);
            async fn try_get_release(&self, pending: bool) -> Result<Option<ClientRelease>, Error>;
            async fn download_binary(&mut self, release: ClientRelease) -> Result<(), Error>;
            fn uri_to_bin_path(&self, uri: &str) -> Result<(String, PathBuf), Error>;
            fn delete_downloaded_release(&mut self) -> Result<(), Error>;
            fn run_binary(&mut self) -> Result<(), Error>;
            fn terminate_proc_and_wait(&mut self) -> Result<(), Error>;
            async fn get_request_bytes(&self, url: String) -> Result<Bytes, Error>;
            fn auto_update(&mut self) ->  BoxFuture<'static, Result<(), Error>>;
        }

        #[async_trait]
        pub trait StorageReader {
            fn compute_storage_key(&self, module: String, key: String) -> String;
            async fn query_storage(
                &self,
                parachain_rpc: &WebsocketClient,
                maybe_storage_key: Option<String>,
                method: String,
            ) -> Option<SpCoreBytes>;
            async fn read_chain_storage<T: 'static + Decode + Debug>(
                &self,
                parachain_rpc: &WebsocketClient,
                maybe_storage_key: Option<String>,
            ) -> Result<Option<T>, Error>;
        }
    }

    #[tokio::test]
    async fn test_runner_download_binary() {
        let mut runner = MockRunner::default();
        let tmp = TempDir::new("runner-tests").expect("failed to create tempdir");
        let mock_path = tmp.path().clone().join("vault-standalone-metadata");
        let moved_mock_path = tmp.path().clone().join("vault-standalone-metadata");
        let mock_bin_name = "vault-standalone-metadata".to_string();

        let client_release = ClientRelease {
            uri: "https://github.com/interlay/interbtc-clients/releases/download/1.15.0/vault-standalone-metadata"
                .to_string(),
            code_hash: H256::default(),
        };

        runner
            .expect_uri_to_bin_path()
            .returning(move |_| Ok(("vault-standalone-metadata".to_string(), moved_mock_path.clone())));
        runner
            .expect_get_request_bytes()
            .returning(|_| Ok(Bytes::from_static(&[1, 2, 3, 4])));
        runner.expect_set_downloaded_release().return_const(());

        let downloaded_release = Runner::download_binary(&mut runner, client_release.clone())
            .await
            .unwrap();
        assert_eq!(
            downloaded_release,
            DownloadedRelease {
                release: client_release,
                path: mock_path.clone(),
                bin_name: mock_bin_name
            }
        );

        let meta = std::fs::metadata(mock_path.clone()).unwrap();
        // The POSIX mode returned by `Permissions::mode()` contains two kinds of
        // information: the file type code, and the access permission bits.
        // Since the executable is a regular file, its file type code fits the
        // `S_IFREG` bit mask (`0o0100000`).
        // Sources:
        // - https://www.gnu.org/software/libc/manual/html_node/Testing-File-Type.html
        // - https://en.wikibooks.org/wiki/C_Programming/POSIX_Reference/sys/stat.h
        assert_eq!(
            meta.permissions(),
            // Expect the mode to include both the file type (`0100000`) and file permissions (`700`).
            fs::Permissions::from_mode(0o0100700)
        );

        let file_content = fs::read(mock_path.clone()).unwrap();
        assert_eq!(file_content, vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn test_runner_uri_to_bin_path() {
        let mut runner = MockRunner::default();
        runner
            .expect_download_path()
            .return_const(PathBuf::from_str("./mock_download_dir").unwrap());
        let uri = "https://github.com/interlay/interbtc-clients/releases/download/1.15.0/vault-standalone-metadata"
            .to_string();
        let (bin_name, bin_path) = Runner::uri_to_bin_path(&runner, &uri).unwrap();
        assert_eq!(bin_name, "vault-standalone-metadata".to_string());
        assert_eq!(
            bin_path,
            PathBuf::from_str("./mock_download_dir/vault-standalone-metadata").unwrap()
        );
    }

    #[tokio::test]
    async fn test_runner_delete_downloaded_release() {
        let tmp = TempDir::new("runner-tests").expect("failed to create tempdir");
        // Create dummy file
        let mock_path = tmp.path().join("mock_file");
        File::create(mock_path.clone()).unwrap();

        let mut runner = MockRunner::default();
        let downloaded_release = DownloadedRelease {
            release: ClientRelease {
                uri: String::default(),
                code_hash: H256::default(),
            },
            path: mock_path.clone(),
            bin_name: String::default(),
        };
        runner
            .expect_downloaded_release()
            .return_const(Some(downloaded_release));
        runner.expect_set_downloaded_release().return_const(());

        Runner::delete_downloaded_release(&mut runner).unwrap();
        assert_eq!(mock_path.exists(), false);
    }

    #[tokio::test]
    async fn test_runner_terminate_proc_and_wait() {
        // spawn long-running child process
        let mut runner = MockRunner::default();
        runner
            .expect_child_proc()
            .returning(|| Some(Command::new("sleep").arg("100").spawn().unwrap()));
        runner.expect_set_child_proc().return_const(());
        let pid = Runner::terminate_proc_and_wait(&mut runner).unwrap();
        let pid_i32: i32 = pid.try_into().unwrap();
        let s = System::new_all();
        // Get all running processes
        let processes = s.processes();
        // Get the child process based on its pid
        let child_process = processes.get(&Pid::from(pid_i32));

        assert_eq!(child_process.is_none(), true);
    }

    #[tokio::test]
    async fn test_runner_try_get_release() {
        let mut runner = MockRunner::default();
        let expected_storage_key = "0x8402aaa79721798ff725d48776181a4428c2fc0938165431e2fa3fc50f072550".to_string();
        let mock_storage_value = vec![
            125, 1, 104, 116, 116, 112, 115, 58, 47, 47, 103, 105, 116, 104, 117, 98, 46, 99, 111, 109, 47, 105, 110,
            116, 101, 114, 108, 97, 121, 47, 105, 110, 116, 101, 114, 98, 116, 99, 45, 99, 108, 105, 101, 110, 116,
            115, 47, 114, 101, 108, 101, 97, 115, 101, 115, 47, 100, 111, 119, 110, 108, 111, 97, 100, 47, 49, 46, 49,
            53, 46, 48, 47, 118, 97, 117, 108, 116, 45, 115, 116, 97, 110, 100, 97, 108, 111, 110, 101, 45, 109, 101,
            116, 97, 100, 97, 116, 97, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 18, 48, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0,
        ];
        let mock_ws_client = WebsocketClient::default();
        runner
            .expect_query_storage()
            .return_const(Some(SpCoreBytes::from(mock_storage_value)));

        let release = Runner::read_chain_storage::<ClientRelease, MockRunner>(
            &runner,
            &mock_ws_client,
            Some(expected_storage_key),
        )
        .await
        .unwrap()
        .unwrap();
        let expected_uri =
            "https://github.com/interlay/interbtc-clients/releases/download/1.15.0/vault-standalone-metadata"
                .to_string();
        assert_eq!(
            release,
            ClientRelease {
                uri: expected_uri,
                code_hash: H256::from_str("0x0000000000000000000000000000000000000000123000000000000000000000")
                    .unwrap()
            }
        );
    }

    #[tokio::test]
    async fn test_runner_run_binary_with_retry() {
        let tmp = TempDir::new("runner-tests").expect("failed to create tempdir");

        let mock_executable_path = tmp.path().join("print_cli_input");
        {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .mode(0o700)
                .create(true)
                .open(mock_executable_path.clone())
                .unwrap();

            // Script that prints CLI input to stdout
            file.write_all(b"#!/bin/bash\necho $@").unwrap();

            file.sync_all().unwrap();
            // drop `file` here to close it and avoid `ExecutableFileBusy` errors
        }

        let mut runner = MockRunner::default();
        let mock_vault_args: Vec<String> = vec![
            "--bitcoin-rpc-url",
            "http://localhost:18443",
            "--bitcoin-rpc-user",
            "rpcuser",
            "--bitcoin-rpc-pass",
            "rpcpassword",
            "--keyfile",
            "keyfile.json",
            "--keyname",
            "0xa81f76187f1e5d2059f67439c4242a92a5cd66a409579db73f156c6e2aae5102",
            "--faucet-url",
            "http://localhost:3033",
            "--auto-register=KSM=faucet",
            "--btc-parachain-url",
            "ws://localhost:9944",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let mock_downloaded_release = DownloadedRelease {
            release: ClientRelease::default(),
            path: mock_executable_path.clone(),
            bin_name: String::default(),
        };
        runner.expect_child_proc().return_var(None);
        runner
            .expect_downloaded_release()
            .return_const(Some(mock_downloaded_release));
        runner.expect_vault_args().return_const(mock_vault_args.clone());
        runner.expect_set_child_proc().return_const(());
        let child = Runner::run_binary(&mut runner, Stdio::piped()).unwrap();

        let output = child.wait_with_output().unwrap();

        let mut expected_output = mock_vault_args.join(" ");
        expected_output.push('\n');
        assert_eq!(output.stdout, expected_output.as_bytes());
    }

    #[tokio::test]
    async fn test_runner_terminate_child_proc() {
        let mut runner = MockRunner::default();
        runner.expect_terminate_proc_and_wait().once().returning(|| Ok(()));
        runner.expect_auto_update().returning(|| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(100_000)).await;
                Ok(())
            })
        });
        let shutdown_signals = Signals::new(&[SIGHUP, SIGTERM, SIGINT, SIGQUIT]).unwrap();
        let task = tokio::spawn(Runner::run(Box::new(runner), shutdown_signals));
        // Wait for the signals iterator to be polled
        // This `sleep` is based on the test case in `signal-hook-tokio` itself:
        // https://github.com/vorner/signal-hook/blob/a9e5ca5e46c9c8e6de89ff1b3ce63c5ff89cd708/signal-hook-tokio/tests/tests.rs#L50
        thread::sleep(Duration::from_millis(100));
        signal_hook::low_level::raise(SIGTERM).unwrap();
        task.await.unwrap().unwrap();
    }
}
