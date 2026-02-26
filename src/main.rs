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
use sp_core::hashing::{twox_128, twox_64};
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

// Well-known dev account seeds (32-byte hex). Account IDs are derived via sr25519.
const ALICE_SEED_HEX: &str = "0xe5be9a5092b81bca64be81d212e7f2f9eba183bb7a90954f7b76361f6edb5c0a";
const BOB_SEED_HEX: &str = "0x398f0c28f98885e046333d4a41c19cee4c37368a9832c6502f6cfd182e2aef89";

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

/// Attestation pallet: Attestors(chain_key, account_id) for chain key 3.
/// Storage key = twox_128("Attestation") + twox_128("Attestors") + twox_64_concat(3) + blake2_128_concat(account_id).
fn attestors_storage_key(account_id: &[u8; 32]) -> String {
    use blake2::digest::{Update, VariableOutput};
    let mut key = Vec::with_capacity(32 + 8 + 8 + 16 + 32);
    key.extend_from_slice(&twox_128(b"Attestation"));
    key.extend_from_slice(&twox_128(b"Attestors"));
    let chain_key_3 = 3u64.to_le_bytes();
    key.extend_from_slice(&twox_64(&chain_key_3));
    key.extend_from_slice(&chain_key_3);
    let mut hasher = blake2::Blake2bVar::new(16).unwrap();
    hasher.update(account_id);
    let mut hash = [0u8; 16];
    hasher.finalize_variable(&mut hash).unwrap();
    key.extend_from_slice(&hash);
    key.extend_from_slice(account_id);
    key.to_hex()
}

/// Attestor value: Option<BlsPublicKey>=None (0x00), AttestorStatus=Idle (0x01), stash=AccountId (32 bytes).
fn attestor_value_placeholder(account_id: &[u8; 32]) -> String {
    let mut v = Vec::with_capacity(1 + 1 + 32);
    v.push(0x00u8); // None for bls_public_key
    v.push(0x01u8); // AttestorStatus::Idle
    v.extend_from_slice(account_id);
    v.to_hex()
}

/// ActiveAttestors(chain_key) for chain key 3. Key from user.
const ACTIVE_ATTESTORS_CHAIN_3_KEY: &str =
    "0x6310fed47319b658f9b8b2504e0d72ec605e795422de90908f14285054a6764b8e79fdf1428e95842eaa9af0b22414be0300000000000000";

/// TargetSampleSize(chain_key) for chain key 3 (Attestation pallet). Key from user.
const TARGET_SAMPLE_SIZE_CHAIN_3_KEY: &str =
    "0x6310fed47319b658f9b8b2504e0d72ec77c34a0cad03a52cd52d9faa5a75ec8f8e79fdf1428e95842eaa9af0b22414be0300000000000000";

/// SCALE: Vec<AccountId> with [alice, bob] = compact(2) + 32 + 32.
fn active_attestors_value(alice: &[u8; 32], bob: &[u8; 32]) -> String {
    let mut v = Vec::with_capacity(1 + 32 + 32);
    v.push(0x08u8); // compact encoding of 2
    v.extend_from_slice(alice);
    v.extend_from_slice(bob);
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
/// wss://host -> wss://host:443, ws://host -> ws://host:80. Path and query are preserved.
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
    let (authority, rest) = match after_scheme.find('/') {
        Some(i) => (&after_scheme[..i], &after_scheme[i..]),
        None => {
            let (auth, q) = match after_scheme.find('?') {
                Some(i) => (&after_scheme[..i], &after_scheme[i..]),
                None => (after_scheme, ""),
            };
            if auth.contains(':') {
                return s.to_owned();
            }
            return format!("{scheme}{auth}{default_port}{q}");
        }
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
        // USC component: derive Alice and Bob from hex seeds; set Sudo key and Attestation pallet genesis
        let alice = account_id_from_seed_hex(ALICE_SEED_HEX)?;
        let bob = account_id_from_seed_hex(BOB_SEED_HEX)?;

        spec.set_state(storage_prefix("Sudo", "Key"), alice.to_hex());

        spec.set_state(
            attestors_storage_key(&alice),
            attestor_value_placeholder(&alice),
        );
        spec.set_state(
            attestors_storage_key(&bob),
            attestor_value_placeholder(&bob),
        );
        spec.set_state(ACTIVE_ATTESTORS_CHAIN_3_KEY, active_attestors_value(&alice, &bob));
        spec.set_state(TARGET_SAMPLE_SIZE_CHAIN_3_KEY, "0x02000000");
    }

    spec.boot_nodes = vec![];

    // Force validator set to dev chain (Alice only) so --alice produces blocks regardless of --base.
    // Remove any existing consensus/session genesis from the fork, then inject dev's.
    let validator_pallet_prefixes = [
        module_prefix("Babe"),
        module_prefix("Grandpa"),
        module_prefix("Session"),
        module_prefix("Staking"),
        module_prefix("ImOnline"),
    ];
    let to_remove: Vec<String> = spec
        .genesis
        .raw
        .top
        .keys()
        .filter(|k| {
            validator_pallet_prefixes
                .iter()
                .any(|p| k.starts_with(p.as_str()))
        })
        .cloned()
        .collect();
    for k in &to_remove {
        spec.remove_state(k);
    }
    let dev_spec = build_spec(&cli.binary, Chain::Dev).await?;
    for (k, v) in &dev_spec.genesis.raw.top {
        if validator_pallet_prefixes
            .iter()
            .any(|p| k.starts_with(p.as_str()))
        {
            spec.set_state(k, v.clone());
        }
    }

    println!("{}", style("Writing chain specification for fork").green());

    tokio::fs::write(cli.out, serde_json::to_vec_pretty(&spec)?).await?;

    println!("{}", style("Done!").green());

    Ok(())
}
