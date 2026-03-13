mod cli;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::{collections::HashSet, fmt::Debug};

use clap::Parser;
use color_eyre::Result;
use color_eyre::{eyre::eyre, Report};
use console::style;
use extend::ext;
use futures::{TryStream, TryStreamExt};
use fxhash::FxHashMap;
use jsonrpsee::client_transport::ws::{Receiver, Sender, Uri, WsTransportClientBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sp_core::hashing::twox_128;
use sp_core::Pair as _;
use sp_core::H256;
use subxt::config::WithExtrinsicParams;
use subxt::storage::StorageKey;
use subxt::tx::{BaseExtrinsicParams, PlainTip};
use subxt::{OnlineClient, SubstrateConfig};
use tokio::process::Command;
use tokio::sync::Semaphore;

use crate::cli::StorageFile;

pub type ExtrinsicParams = BaseExtrinsicParams<SubstrateConfig, PlainTip>;

pub type CreditcoinConfig = WithExtrinsicParams<SubstrateConfig, ExtrinsicParams>;

pub type ApiClient<C = CreditcoinConfig> = OnlineClient<C>;

type StoragePairs = FxHashMap<String, String>;

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
    #[serde(flatten)]
    extensions: Option<JsonValue>,
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
    fn remove_keys_with_prefix(&mut self, prefix: &str) {
        self.genesis.raw.top.retain(|k, _| !k.starts_with(prefix));
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

fn key_stream<'a>(
    api: &'a ApiClient,
    at: &'a H256,
    sema: Arc<Semaphore>,
) -> impl TryStream<Ok = StorageKey, Error = Report> + 'a {
    Box::pin(async_stream::try_stream! {
        let mut start: Option<StorageKey> = None;
        let mut spinner = cli::ProgressBarManager::new_spinner("Fetching storage keys")?;
        loop {
            let keys = {
                let _permit = sema.acquire().await?;
                api.rpc().storage_keys_paged(&[], 512, start.map(|k| k.0).as_deref(), Some(*at)).await.dbg_err()?
            };
            start = keys.last().cloned();
            if keys.is_empty() {
                spinner.finish_with_message("Done");
                break;
            }
            spinner.inc(u64::try_from(keys.len()).unwrap());
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
    let keys: Vec<_> = keys.try_collect().await?;

    let mut bar = cli::ProgressBarManager::new_bar(
        keys.len().try_into().unwrap(),
        "Fetching storage values",
    )?;

    let mut futs = Vec::new();

    for key in keys {
        let api = api.clone();
        let at = *at;
        let sema = sema.clone();

        futs.push(tokio::spawn(async move {
            let _permit = sema.acquire().await?;
            let value = api
                .rpc()
                .storage(&key.0, Some(at))
                .await
                .dbg_err()?
                .unwrap();

            Ok::<_, Report>((key.to_hex(), value.0.to_hex()))
        }));
    }

    let mut pairs = FxHashMap::default();

    for fut in futs {
        let (k, v) = fut.await??;
        bar.inc(1);
        pairs.insert(k, v);
    }

    bar.finish_with_message("Done");

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

// Well-known dev account seed (32-byte hex). Account ID is derived via sr25519.
const ALICE_SEED_HEX: &str = "0xe5be9a5092b81bca64be81d212e7f2f9eba183bb7a90954f7b76361f6edb5c0a";

/// Derive sr25519 account ID (32-byte public key) from a 0x-prefixed 64-char hex seed.
fn account_id_from_seed_hex(hex: &str) -> Result<[u8; 32], Report> {
    let s = hex.trim().strip_prefix("0x").unwrap_or(hex).trim();
    if s.len() != 64 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(eyre!("seed must be 0x + 64 hex chars, got {}", hex));
    }
    let mut seed = [0u8; 32];
    hex::decode_to_slice(s, &mut seed).map_err(|e| eyre!("invalid hex seed: {e}"))?;
    let pair = sp_core::sr25519::Pair::from_seed(&seed);
    Ok(pair.public().0)
}

fn blake2_128(data: &[u8]) -> [u8; 16] {
    use blake2::digest::{Update, VariableOutput};
    let mut hasher = blake2::Blake2bVar::new(16).unwrap();
    hasher.update(data);
    let mut hash = [0u8; 16];
    hasher.finalize_variable(&mut hash).unwrap();
    hash
}

/// `TargetSampleSize(chain_key)` storage key.
fn target_sample_size_key(chain_key: u64) -> String {
    let mut key = Vec::with_capacity(32 + 16 + 8);
    key.extend_from_slice(&twox_128(b"Attestation"));
    key.extend_from_slice(&twox_128(b"TargetSampleSize"));
    let ck = chain_key.to_le_bytes();
    key.extend_from_slice(&blake2_128(&ck));
    key.extend_from_slice(&ck);
    key.to_hex()
}

/// `System.Account` storage key (`Blake2_128Concat` hasher).
fn system_account_key(account_id: &[u8; 32]) -> String {
    let mut key = Vec::with_capacity(32 + 16 + 32);
    key.extend_from_slice(&twox_128(b"System"));
    key.extend_from_slice(&twox_128(b"Account"));
    key.extend_from_slice(&blake2_128(account_id));
    key.extend_from_slice(account_id);
    key.to_hex()
}

const CTC: u128 = 1_000_000_000_000_000_000;

/// SCALE-encode an `AccountInfo` with the given free balance.
/// Format: nonce(u32) + consumers(u32) + providers(u32) + sufficients(u32)
///       + free(u128) + reserved(u128) + frozen(u128) + flags(u128)
fn account_info_value(free: u128) -> String {
    let mut v = Vec::with_capacity(80);
    v.extend_from_slice(&0u32.to_le_bytes()); // nonce
    v.extend_from_slice(&0u32.to_le_bytes()); // consumers
    v.extend_from_slice(&1u32.to_le_bytes()); // providers (>0 to keep alive)
    v.extend_from_slice(&0u32.to_le_bytes()); // sufficients
    v.extend_from_slice(&free.to_le_bytes()); // free balance
    v.extend_from_slice(&0u128.to_le_bytes()); // reserved
    v.extend_from_slice(&0u128.to_le_bytes()); // frozen
    v.extend_from_slice(&(1u128 << 127).to_le_bytes()); // flags (new_logic)
    v.to_hex()
}

#[derive(Clone, Debug, PartialEq)]
pub enum Chain {
    Dev,
    Other(String),
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
    wasm_hex.push_str(hex::encode(wasm).trim());

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

    fetch_storage_pairs(api, &at).await
}

/// Ensures the RPC URL has an explicit port so Uri parsing succeeds (it does not use default ports).
/// `wss://host` -> `wss://host:443`, `ws://host` -> `ws://host:80`. Path and query are preserved.
fn normalize_rpc_url(s: &str) -> String {
    let s = s.trim();
    let (scheme, default_port) = if s.starts_with("wss://") {
        ("wss://", ":443")
    } else if s.starts_with("ws://") {
        ("ws://", ":80")
    } else {
        return s.to_owned();
    };
    let after_scheme = &s[scheme.len()..];
    let (authority, rest) = if let Some(i) = after_scheme.find('/') {
        (&after_scheme[..i], &after_scheme[i..])
    } else {
        let (auth, q) = match after_scheme.find('?') {
            Some(i) => (&after_scheme[..i], &after_scheme[i..]),
            None => (after_scheme, ""),
        };
        if auth.contains(':') {
            return s.to_owned();
        }
        return format!("{scheme}{auth}{default_port}{q}");
    };
    if authority.contains(':') {
        return s.to_owned();
    }
    format!("{scheme}{authority}{default_port}{rest}")
}

fn parse_rpc_uri(s: &str) -> Result<Uri> {
    let normalized = normalize_rpc_url(s);
    normalized.parse().err_into()
}

async fn ws_transport(url: Uri) -> Result<(Sender, Receiver)> {
    const CONNECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    WsTransportClientBuilder::default()
        .connection_timeout(CONNECTION_TIMEOUT)
        .build(url)
        .await
        .err_into()
}

async fn new_client(url: Uri) -> Result<ApiClient> {
    let (sender, receiver) = ws_transport(url).await?;
    let client = Arc::new(
        jsonrpsee::async_client::ClientBuilder::default()
            .max_concurrent_requests(MAX_CONCURRENT_REQUESTS)
            .build_with_tokio(sender, receiver),
    );
    let api = ApiClient::from_rpc_client(client).await?;
    Ok(api)
}

fn apply_usc_genesis(spec: &mut ChainSpec) -> Result<()> {
    let alice = account_id_from_seed_hex(ALICE_SEED_HEX)?;
    spec.set_state(storage_prefix("Sudo", "Key"), alice.to_hex());

    spec.set_state(
        system_account_key(&alice),
        account_info_value(1_000_000 * CTC),
    );

    spec.remove_keys_with_prefix(&storage_prefix("Attestation", "Attestors"));
    spec.remove_keys_with_prefix(&storage_prefix("Attestation", "ActiveAttestors"));
    spec.remove_keys_with_prefix(&storage_prefix("Attestation", "TargetSampleSize"));
    spec.remove_keys_with_prefix(&module_prefix("Randomness"));

    spec.set_state(target_sample_size_key(1), "0x03000000");
    Ok(())
}

async fn inject_dev_validators(spec: &mut ChainSpec, binary: &Path) -> Result<()> {
    let validator_pallets = [
        "Babe",
        "Grandpa",
        "Session",
        "Staking",
        "ImOnline",
        "VoterList",
    ];
    for pallet in &validator_pallets {
        spec.remove_keys_with_prefix(&module_prefix(pallet));
    }
    let dev_spec = build_spec(binary, Chain::Dev).await?;
    let prefixes: Vec<_> = validator_pallets.iter().map(|p| module_prefix(p)).collect();
    for (k, v) in &dev_spec.genesis.raw.top {
        if prefixes.iter().any(|p| k.starts_with(p.as_str())) {
            spec.set_state(k, v.clone());
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = cli::Cli::parse();

    let rpc_url = parse_rpc_uri(&cli.rpc)?;

    let storage = if let Some(path) = cli.storage {
        match path {
            StorageFile::None => HashMap::default(),
            StorageFile::Path(path) => {
                if let Ok(storage) = tokio::fs::read(&path).await {
                    println!("using existing storage");
                    serde_json::from_slice(&storage)?
                } else {
                    let api = new_client(rpc_url.clone()).await?;
                    let storage = fetch_storage_at(&api, cli.at).await?;
                    let storage_bytes = serde_json::to_vec(&storage)?;
                    tokio::fs::write(&path, storage_bytes).await?;
                    storage
                }
            }
        }
    } else {
        let api = new_client(rpc_url.clone()).await?;
        fetch_storage_at(&api, cli.at).await?
    };

    let orig_spec = build_spec(&cli.binary, cli.original_chain).await?;
    let mut spec = build_spec(&cli.binary, cli.base_chain).await?;

    spec.name = cli
        .name
        .unwrap_or_else(|| orig_spec.name.joined_with("-fork"));
    spec.id = cli.id.unwrap_or_else(|| orig_spec.id.joined_with("-fork"));
    spec.protocol_id.clone_from(&orig_spec.protocol_id);

    let mut excludes: HashSet<&str> = if cli.no_default_excludes {
        HashSet::default()
    } else {
        [
            "System",
            "Authorship",
            "Difficulty",
            "Rewards",
            "Staking",
            "Session",
            "Grandpa",
            "Babe",
        ]
        .into_iter()
        .collect()
    };

    if let Some(extra_excludes) = &cli.exclude_pallets {
        excludes.extend(extra_excludes.iter().map(String::as_str));
    }

    let mut prefixes = vec![
        storage_prefix("System", "Account"), // System.Account
    ];
    if let Some(pallets) = cli.pallets {
        prefixes.extend(pallets.iter().map(|n| module_prefix(n)));
    } else {
        let api = ApiClient::<CreditcoinConfig>::from_url(rpc_url.to_string()).await?;
        let meta = api.rpc().metadata().await?;
        for pallet in &meta.runtime_metadata().pallets {
            let n = &pallet.name;
            if pallet.storage.is_some() && !excludes.contains(n.as_str()) {
                let hashed = module_prefix(n);
                prefixes.push(hashed);
            }
        }
    }

    for (key, value) in &storage {
        if prefixes.iter().any(|p| key.starts_with(p)) {
            spec.set_state(key, value.clone());
        }
    }

    let wasm_hex = if let Some(runtime_path) = &cli.runtime {
        println!("Reading from runtime wasm file: {}", runtime_path.display());
        read_wasm_hex(runtime_path).await?
    } else {
        storage
            .get(&*b":code".to_hex())
            .expect("storage should include the runtime code")
            .clone()
    };

    // make sure to remove System.LastRuntimeUpgrade to trigger a migration
    spec.remove_state(storage_prefix("System", "LastRuntimeUpgrade"));

    // Overwrite the on-chain wasm blob
    spec.set_state(b":code".to_hex(), wasm_hex);

    // Make sure that the genesis state is different
    spec.set_state("0xdeadbeef", "0x1");

    if cli.usc {
        apply_usc_genesis(&mut spec)?;
    }

    spec.boot_nodes = vec![];
    inject_dev_validators(&mut spec, &cli.binary).await?;

    println!("{}", style("Writing chain specification for fork").green());

    tokio::fs::write(cli.out, serde_json::to_vec_pretty(&spec)?).await?;

    println!("{}", style("Done!").green());

    Ok(())
}
