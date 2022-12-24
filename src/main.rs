use std::ffi::OsStr;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::{collections::HashSet, fmt::Debug, path::PathBuf, str::FromStr};

use clap::Parser;
use color_eyre::Result;
use color_eyre::{eyre::eyre, Report};
use extend::ext;
use futures::{pin_mut, TryStream, TryStreamExt};
use jsonrpsee::client_transport::ws::{Receiver, Sender, Uri, WsTransportClientBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sp_core::{hashing::twox_128, Encode, H256};
use subxt::config::WithExtrinsicParams;
use subxt::storage::StorageKey;
use subxt::tx::{BaseExtrinsicParams, PlainTip};
use subxt::{OnlineClient, SubstrateConfig};
use tokio::process::Command;
use tokio::sync::Semaphore;

pub type ExtrinsicParams = BaseExtrinsicParams<SubstrateConfig, PlainTip>;

pub type CreditcoinConfig = WithExtrinsicParams<SubstrateConfig, ExtrinsicParams>;

pub type ApiClient = OnlineClient<CreditcoinConfig>;

#[derive(serde::Deserialize, serde::Serialize, Debug)]
struct StoragePair(String, String);
type StoragePairs = Vec<StoragePair>;

#[ext]
impl<T, E> Result<T, E>
where
    E: Debug,
{
    fn dbg_err(self) -> Result<T, Report> {
        self.map_err(|err| eyre!("{err:?}"))
    }
}

#[ext(name = ErrorInto)]
impl<T, E> Result<T, E>
where
    E: Into<Report>,
{
    fn err_into(self) -> Result<T, Report> {
        self.map_err(Into::into)
    }
}

#[derive(PartialEq, Eq, Debug)]
struct SortPriority<T>(usize, T);

impl<T> PartialOrd for SortPriority<T>
where
    T: PartialEq,
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.0.partial_cmp(&other.0)
    }
}
impl<T: Eq> Ord for SortPriority<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChainSpec {
    name: String,
    id: String,
    chain_type: String,
    boot_nodes: Vec<String>,
    telemetry_endpoints: Option<Vec<String>>,
    protocol_id: Option<String>,
    properties: Option<JsonValue>,
    code_substitutes: JsonValue,
    genesis: GenesisState,
}

#[derive(Deserialize, Serialize)]
struct GenesisState {
    raw: RawGenesisState,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RawGenesisState {
    top: serde_json::Map<String, JsonValue>,
    children_default: JsonValue,
}

impl ChainSpec {
    fn set_state(&mut self, key: impl AsRef<str>, value: impl Into<JsonValue>) {
        self.genesis
            .raw
            .top
            .insert(key.as_ref().to_owned(), value.into());
    }
    fn remove_state(&mut self, key: impl AsRef<str>) {
        self.genesis.raw.top.remove(key.as_ref());
    }
}

#[ext]
impl<S> S
where
    S: AsRef<str>,
{
    fn joined_with(&self, other: impl AsRef<str>) -> String {
        let self_str = self.as_ref();
        let other = other.as_ref();
        let mut s = String::with_capacity(self_str.len() + other.len());
        s.push_str(self_str);
        s.push_str(other);
        s
    }
}

#[ext]
impl<Slice: AsRef<[u8]>> Slice {
    fn to_hex(&self) -> String {
        let mut s = String::from("0x");
        s.push_str(&hex::encode(self));
        s
    }
}

fn key_stream<'a, 'h>(
    api: &'a ApiClient,
    at: &'a H256,
    sema: Arc<Semaphore>,
) -> impl TryStream<Ok = StorageKey, Error = Report> + 'a {
    Box::pin(async_stream::try_stream! {
        let mut start: Option<StorageKey> = None;
        loop {
            let keys = {
                let _permit = sema.acquire().await?;
                api.rpc().storage_keys_paged(&[], 512, start.map(|k| k.0).as_deref(), Some(at.clone())).await.dbg_err()?
            };
            start = keys.last().map(|k| k.clone());
            if keys.is_empty() {
                break;
            }
            for key in keys {
                yield key;
            }
        }
    })
}

const MAX_CONCURRENT_REQUESTS: usize = 2048;

async fn fetch_storage_pairs(api: &ApiClient, at: &H256) -> Result<StoragePairs> {
    let sema = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));

    let keys = key_stream(api, at, sema.clone());
    pin_mut!(keys);

    let mut futs = Vec::new();

    while let Some(key) = keys.try_next().await? {
        let api = api.clone();
        let at = at.clone();
        let sema = sema.clone();
        futs.push(tokio::spawn(async move {
            let _permit = sema.acquire().await?;
            let value = api
                .rpc()
                .storage(&key.0, Some(at.clone()))
                .await
                .dbg_err()?
                .unwrap();

            Ok::<_, Report>(StoragePair(key.to_hex(), value.0.to_hex()))
        }));
    }

    let mut pairs = Vec::new();
    for fut in futs {
        let pair = fut.await??;
        pairs.push(pair);
    }

    Ok(pairs)
}

fn storage_prefix(module: &str, name: &str) -> String {
    let mut key = [0u8; 32];
    key[..16].copy_from_slice(&twox_128(module.as_bytes()));
    key[16..].copy_from_slice(&twox_128(name.as_bytes()));
    key.to_hex()
}

fn module_prefix(module: &str) -> String {
    twox_128(module.as_bytes()).to_hex()
}

#[derive(clap::Parser)]
struct Cli {
    #[clap(long = "bin")]
    binary: PathBuf,
    #[clap(long)]
    runtime: PathBuf,
    #[clap(short, long)]
    out: Option<PathBuf>,
    #[clap(long = "orig")]
    original_chain: Chain,
    #[clap(long = "new", default_value_t = Chain::Dev)]
    new_chain: Chain,
    #[clap(long)]
    storage: Option<PathBuf>,
    #[clap(long)]
    at: Option<H256>,
    #[clap(long)]
    pallets: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq)]
enum Chain {
    Dev,
    Other(String),
}

impl fmt::Display for Chain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Chain::Dev => write!(f, "dev"),
            Chain::Other(c) => write!(f, "{c}"),
        }
    }
}

impl FromStr for Chain {
    type Err = Report;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "dev" => Chain::Dev,
            _ => Chain::Other(s.to_owned()),
        })
    }
}

impl Chain {
    fn to_args(&self) -> impl IntoIterator<Item = impl AsRef<OsStr> + '_> {
        match self {
            Chain::Dev => vec!["--dev"],
            Chain::Other(s) => vec!["--chain", s.as_str()],
        }
    }
}

async fn build_spec(binary: &Path, chain: Chain) -> Result<ChainSpec> {
    let out = Command::new(binary)
        .args(["build-spec"])
        .args(chain.to_args())
        .arg("--raw")
        .output()
        .await?;

    serde_json::from_slice(&out.stdout).err_into()
}

async fn read_wasm_hex(wasm_path: &Path) -> Result<String> {
    let wasm = tokio::fs::read(wasm_path).await?;
    let mut wasm_hex = "0x".to_owned();
    wasm_hex.push_str(&hex::encode(&wasm).trim());

    Ok(wasm_hex)
}

async fn fetch_storage_at(api: &ApiClient, at: Option<H256>) -> Result<StoragePairs> {
    let at = if let Some(at) = at {
        at
    } else {
        api.rpc()
            .block_hash(None)
            .await?
            .ok_or_else(|| eyre!("failed to get latest block hash"))?
    };

    fetch_storage_pairs(&api, &at).await
}

async fn ws_transport(url: &str) -> Result<(Sender, Receiver)> {
    let url: Uri = url.parse().err_into()?;
    WsTransportClientBuilder::default()
        .build(url)
        .await
        .err_into()
}

async fn new_client() -> Result<ApiClient> {
    let (sender, receiver) = ws_transport("ws://127.0.0.1:9944").await?;
    let client = Arc::new(
        jsonrpsee::async_client::ClientBuilder::default()
            .max_concurrent_requests(MAX_CONCURRENT_REQUESTS)
            .build_with_tokio(sender, receiver),
    );
    let api = ApiClient::from_rpc_client(client).await?;
    Ok(api)
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();

    let storage = match cli.storage {
        Some(path) => {
            if let Ok(storage) = tokio::fs::read(&path).await {
                println!("using existing storage");
                serde_json::from_slice(&storage)?
            } else {
                let api = new_client().await?;
                // let api = ApiClient::new().await?;
                let storage = fetch_storage_at(&api, cli.at).await?;
                let storage_bytes = serde_json::to_vec(&storage)?;
                tokio::fs::write(&path, storage_bytes).await?;
                storage
            }
        }
        None => {
            let api = new_client().await?;
            fetch_storage_at(&api, cli.at).await?
        }
    };

    let orig_spec = build_spec(&cli.binary, cli.original_chain).await?;
    let mut spec = build_spec(&cli.binary, cli.new_chain).await?;

    spec.name = orig_spec.name.joined_with("-fork");
    spec.id = orig_spec.id.joined_with("-fork");
    spec.protocol_id = orig_spec.protocol_id.clone();

    let exclude: HashSet<&str> = [
        "System",
        "Session",
        "Babe",
        "Grandpa",
        "GrandpaFinality",
        "FinalityTracker",
        "Authorship",
        "Difficulty",
        "Rewards",
    ]
    .into_iter()
    .collect();

    let mut prefixes = vec![
        storage_prefix("System", "Account"), // System.Account
    ];
    if let Some(pallets) = cli.pallets {
        prefixes.extend(pallets.iter().map(|n| module_prefix(n)))
    } else {
        let api = ApiClient::new().await?;
        let meta = api.rpc().metadata().await?;
        for pallet in &meta.runtime_metadata().pallets {
            let n = &pallet.name;
            if pallet.storage.is_some() && !exclude.contains(n.as_str()) {
                let hashed = module_prefix(&n);
                prefixes.push(hashed);
            }
        }
    }

    for StoragePair(key, value) in &storage {
        if prefixes.iter().any(|p| key.starts_with(p)) {
            spec.set_state(key, value.to_owned());
        }
    }

    let wasm_hex = read_wasm_hex(&cli.runtime).await?;

    // remove System.LastRuntimeUpgrade to trigger a migration (why is this desirable??)
    spec.remove_state(storage_prefix("System", "LastRuntimeUpgrade"));

    // Overwrite the on-chain wasm blob
    spec.set_state(&b":code".to_hex(), wasm_hex);

    // set the sudo key to Alice
    spec.set_state(
        &storage_prefix("Sudo", "Key"),
        "0xd43593c715fdd31c61141abd04a99fd6822c8558854ccde39a5684e7a56da27d".to_owned(),
    );
    spec.boot_nodes = vec![];

    spec.set_state(
        storage_prefix("Difficulty", "TargetBlockTime"),
        6000u64.encode().to_hex(),
    );

    tokio::fs::write(
        cli.out.unwrap_or("fork.json".into()),
        serde_json::to_vec_pretty(&spec)?,
    )
    .await?;

    println!("Done!");

    Ok(())
}