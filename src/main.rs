mod cli;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::{collections::HashSet, fmt::Debug};

use bls_signatures::{PrivateKey as BlsPrivateKey, Serialize as BlsSerialize};
use clap::Parser;
use color_eyre::Result;
use color_eyre::{eyre::eyre, Report};
use console::style;
use extend::ext;
use futures::{StreamExt, TryStreamExt};
use jsonrpsee::client_transport::ws::{Receiver, Sender, Uri, WsTransportClientBuilder};
use jsonrpsee::core::client::ClientT;
use jsonrpsee::rpc_params;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sp_core::hashing::{blake2_128, twox_128, twox_64};
use sp_core::Pair as _;
use sp_core::H256;
use subxt::config::WithExtrinsicParams;
use subxt::tx::{BaseExtrinsicParams, PlainTip};
use subxt::{OnlineClient, SubstrateConfig};
use tokio::process::Command;

use crate::cli::StorageFile;

pub type ExtrinsicParams = BaseExtrinsicParams<SubstrateConfig, PlainTip>;

pub type CreditcoinConfig = WithExtrinsicParams<SubstrateConfig, ExtrinsicParams>;

pub type ApiClient<C = CreditcoinConfig> = OnlineClient<C>;

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

#[derive(Deserialize)]
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

#[derive(Deserialize)]
struct GenesisState {
    raw: RawGenesisState,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawGenesisState {
    top: serde_json::Map<String, JsonValue>,
    children_default: JsonValue,
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

const MAX_CONCURRENT_REQUESTS: usize = 2048;
/// Maximum websocket message size; batched value responses can be large.
const MAX_WS_MESSAGE_SIZE: u32 = 64 * 1024 * 1024;
/// `state_getKeysPaged` page size (most public nodes cap this at 1000).
const KEY_PAGE_SIZE: u32 = 1000;
/// Concurrent `state_queryStorageAt` requests in flight.
const VALUE_BATCH_CONCURRENCY: usize = 32;
/// Per-key concurrency within a batch that fell back to single fetches.
const FALLBACK_FETCH_CONCURRENCY: usize = 32;

/// A raw JSON-RPC client for the bulk state fetch: either an HTTP(S) client
/// (preferred — load balancers spread stateless requests across backends,
/// where a websocket session is pinned to one backend's rate limit) or a
/// websocket connection.
#[derive(Clone)]
enum RawClient {
    Http(Arc<jsonrpsee::http_client::HttpClient>),
    Ws(Arc<jsonrpsee::async_client::Client>),
}

impl RawClient {
    async fn request<R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: jsonrpsee::core::params::ArrayParams,
    ) -> Result<R> {
        match self {
            RawClient::Http(c) => c.request(method, params).await.err_into(),
            RawClient::Ws(c) => c.request(method, params).await.err_into(),
        }
    }

    /// `state_getKeysPaged`: all keys (hex) after `start_key`, full keyspace.
    async fn keys_paged(&self, count: u32, start_key: &str, at: &str) -> Result<Vec<String>> {
        self.request(
            "state_getKeysPaged",
            rpc_params!["0x", count, start_key, at],
        )
        .await
    }

    /// `state_getStorage`: a single value (hex) at `at`.
    async fn storage_value(&self, key: &str, at: &str) -> Result<Option<String>> {
        self.request("state_getStorage", rpc_params![key, at]).await
    }

    /// `chain_getBlockHash` of the latest block.
    async fn latest_block_hash(&self) -> Result<String> {
        let hash: Option<String> = self.request("chain_getBlockHash", rpc_params![]).await?;
        hash.ok_or_else(|| eyre!("failed to get latest block hash"))
    }
}

fn new_http_client(url: &str) -> Result<RawClient> {
    let client = jsonrpsee::http_client::HttpClientBuilder::default()
        .max_request_body_size(MAX_WS_MESSAGE_SIZE)
        .request_timeout(std::time::Duration::from_mins(1))
        .build(url)
        .err_into()?;
    Ok(RawClient::Http(Arc::new(client)))
}

/// A pool of RPC clients; requests are spread round-robin by index.
/// HTTP clients pool connections internally, so one client suffices; for
/// websockets each pool entry is a separate connection.
struct NodePool {
    clients: Vec<RawClient>,
}

impl NodePool {
    async fn connect(ws_url: &Uri, http_url: Option<&str>, connections: usize) -> Result<Self> {
        let clients = if let Some(http_url) = http_url {
            println!("fetching state over HTTP at {http_url}");
            vec![new_http_client(http_url)?]
        } else {
            futures::future::try_join_all(
                (0..connections.max(1)).map(|_| new_ws_client(ws_url.clone())),
            )
            .await?
        };
        Ok(Self { clients })
    }

    fn get(&self, index: usize) -> &RawClient {
        &self.clients[index % self.clients.len()]
    }
}

/// Width (in bytes) at which key ranges are split. Map keys distinguish
/// themselves by a uniformly distributed hash that starts after the 32-byte
/// pallet+item prefix, so splits need resolution well past that.
const RANGE_BYTES: usize = 64;

/// A density-based split lands `2^SPLIT_LOOKAHEAD_SHIFT` pages ahead of the
/// scan cursor: the splitting worker keeps roughly that many pages and each
/// spawned chunk covers the same. Larger chunks waste less (each range death
/// discards up to one partial page) but spread work more slowly.
const SPLIT_LOOKAHEAD_SHIFT: u32 = 4;

type RangePos = [u8; RANGE_BYTES];

fn hex_to_pos(s: &str, fill: u8) -> RangePos {
    let hex = s.strip_prefix("0x").unwrap_or(s);
    let mut out = [fill; RANGE_BYTES];
    for (i, slot) in out.iter_mut().enumerate() {
        match hex
            .get(2 * i..2 * i + 2)
            .and_then(|c| u8::from_str_radix(c, 16).ok())
        {
            Some(b) => *slot = b,
            None => break,
        }
    }
    out
}

fn pos_sub(a: &RangePos, b: &RangePos) -> Option<RangePos> {
    let mut out = [0u8; RANGE_BYTES];
    let mut borrow = 0i16;
    for i in (0..RANGE_BYTES).rev() {
        let d = i16::from(a[i]) - i16::from(b[i]) - borrow;
        out[i] = d.to_le_bytes()[0];
        borrow = i16::from(d < 0);
    }
    (borrow == 0).then_some(out)
}

fn pos_add(a: &RangePos, b: &RangePos) -> Option<RangePos> {
    let mut out = [0u8; RANGE_BYTES];
    let mut carry = 0u16;
    for i in (0..RANGE_BYTES).rev() {
        let s = u16::from(a[i]) + u16::from(b[i]) + carry;
        out[i] = (s & 0xff) as u8;
        carry = s >> 8;
    }
    (carry == 0).then_some(out)
}

fn pos_shl(a: &RangePos, bits: u32) -> Option<RangePos> {
    let mut out = *a;
    for _ in 0..bits {
        let doubled = pos_add(&out, &out)?;
        out = doubled;
    }
    Some(out)
}

/// Carve up to `max_chunks` ascending split points inside a range using
/// observed key density: each lands `2^SPLIT_LOOKAHEAD_SHIFT` pages further
/// along, extrapolated from the span of the page just fetched. A keyspace
/// midpoint would not work here — dense maps occupy a vanishing sliver of
/// their range, so the midpoint lands in empty space and the spawned range
/// finds nothing.
fn density_split_points(
    page_first: &str,
    page_last: &str,
    end: &str,
    max_chunks: usize,
) -> Vec<String> {
    let first = hex_to_pos(page_first, 0);
    let last = hex_to_pos(page_last, 0);
    let end_pos = hex_to_pos(end, 0xff);

    let mut points = Vec::new();
    let Some(page_span) = pos_sub(&last, &first) else {
        return points;
    };
    let Some(jump) = pos_shl(&page_span, SPLIT_LOOKAHEAD_SHIFT) else {
        return points;
    };

    let mut cur = last;
    for _ in 0..max_chunks {
        match pos_add(&cur, &jump) {
            Some(next) if next > cur && next < end_pos => {
                points.push(next.to_hex());
                cur = next;
            }
            _ => break,
        }
    }
    points
}

/// Fetch every storage key at `at` by scanning keyspace ranges concurrently.
///
/// Keys cluster under a handful of 32-byte pallet/item prefixes, so static
/// partitioning starves: nearly all keys end up in a few partitions that page
/// sequentially. Instead, ranges split dynamically — whenever a range yields a
/// full page, its unscanned remainder is halved and queued for another worker,
/// so dense regions keep splitting until every worker is busy.
#[allow(clippy::too_many_lines)]
async fn fetch_all_keys(
    pool: &NodePool,
    at: &str,
    key_scan_concurrency: usize,
) -> Result<Vec<String>> {
    let spinner = Mutex::new(cli::ProgressBarManager::new_spinner(
        "Fetching storage keys",
    )?);

    // Ranges are (start_exclusive, end_inclusive), 0x-hex. Seed one range per
    // first byte for a fast warmup.
    let queue: Mutex<Vec<(String, String)>> = Mutex::new(
        (0u16..=255)
            .map(|b| {
                let end_byte = "ff".repeat(RANGE_BYTES - 1);
                (format!("0x{b:02x}"), format!("0x{b:02x}{end_byte}"))
            })
            .collect(),
    );
    let in_progress = std::sync::atomic::AtomicUsize::new(0);
    let work_available = tokio::sync::Notify::new();
    let all_keys: Mutex<Vec<String>> = Mutex::new(Vec::new());

    let workers = key_scan_concurrency.max(1);
    let scan = futures::stream::iter(0..workers)
        .map(|worker| {
            let client = pool.get(worker).clone();
            let queue = &queue;
            let in_progress = &in_progress;
            let work_available = &work_available;
            let all_keys = &all_keys;
            let spinner = &spinner;
            async move {
                use std::sync::atomic::Ordering;
                loop {
                    // Pop a range, or finish when the queue is drained and no
                    // worker is mid-range (one could still split and refill).
                    let (mut start, mut end) = {
                        loop {
                            let notified = work_available.notified();
                            if let Some(range) = queue.lock().unwrap().pop() {
                                in_progress.fetch_add(1, Ordering::SeqCst);
                                break range;
                            }
                            if in_progress.load(Ordering::SeqCst) == 0 {
                                return Ok::<_, Report>(());
                            }
                            notified.await;
                        }
                    };

                    let result = async {
                        loop {
                            let page = client.keys_paged(KEY_PAGE_SIZE, &start, at).await?;
                            let full_page = page.len() == KEY_PAGE_SIZE as usize;
                            let mut kept = 0u64;
                            let mut past_end = false;
                            let mut first_kept: Option<String> = None;
                            let mut last_kept: Option<String> = None;
                            {
                                let mut all = all_keys.lock().unwrap();
                                for key in page {
                                    if key.as_str() > end.as_str() {
                                        past_end = true;
                                        break;
                                    }
                                    if first_kept.is_none() {
                                        first_kept = Some(key.clone());
                                    }
                                    last_kept = Some(key.clone());
                                    all.push(key);
                                    kept += 1;
                                }
                            }
                            spinner.lock().unwrap().inc(kept);
                            if past_end || !full_page {
                                return Ok::<_, Report>(());
                            }
                            let page_first =
                                first_kept.expect("a full page within range was just kept");
                            start = last_kept.expect("a full page within range was just kept");
                            // The remainder is non-empty. If any worker is
                            // starved, hand it everything past a few pages
                            // ahead of the cursor; splitting unconditionally
                            // would shrink ranges below one page and waste
                            // most of each response.
                            let deficit = {
                                let queued = queue.lock().unwrap().len();
                                workers.saturating_sub(queued + in_progress.load(Ordering::SeqCst))
                            };
                            if deficit > 0 {
                                // One chunk per starved worker; each spawned
                                // range covers ~2^SPLIT_LOOKAHEAD_SHIFT pages,
                                // the final one takes the remainder to `end`.
                                let splits =
                                    density_split_points(&page_first, &start, &end, deficit);
                                if let Some(first_split) = splits.first().cloned() {
                                    {
                                        let mut q = queue.lock().unwrap();
                                        for pair in splits.windows(2) {
                                            q.push((pair[0].clone(), pair[1].clone()));
                                        }
                                        q.push((
                                            splits.last().expect("non-empty").clone(),
                                            end.clone(),
                                        ));
                                    }
                                    work_available.notify_waiters();
                                    end = first_split;
                                }
                            }
                        }
                    }
                    .await;

                    in_progress.fetch_sub(1, Ordering::SeqCst);
                    work_available.notify_waiters();
                    result?;
                }
            }
        })
        .buffer_unordered(workers)
        .try_collect::<Vec<()>>();
    scan.await?;

    spinner.into_inner().unwrap().finish_with_message("Done");

    Ok(all_keys.into_inner().unwrap())
}

/// One entry of the `state_queryStorageAt` response.
#[derive(Deserialize)]
struct StorageChangeSet {
    #[allow(dead_code)]
    block: H256,
    changes: Vec<(String, Option<String>)>,
}

async fn query_storage_batch(
    client: &RawClient,
    keys_hex: &[String],
    at: &str,
) -> Result<Vec<(String, String)>> {
    let sets: Vec<StorageChangeSet> = client
        .request("state_queryStorageAt", rpc_params![keys_hex, at])
        .await?;
    let mut pairs = Vec::with_capacity(keys_hex.len());
    for set in sets {
        for (key, value) in set.changes {
            let value = value.ok_or_else(|| eyre!("missing storage value for key {key}"))?;
            pairs.push((key, value));
        }
    }
    Ok(pairs)
}

static BATCH_FALLBACK_WARNED: OnceLock<()> = OnceLock::new();

fn warn_once_batch_fallback(err: &Report) {
    BATCH_FALLBACK_WARNED.get_or_init(|| {
        eprintln!("note: state_queryStorageAt failed ({err}); falling back to per-key fetches");
    });
}

/// Fetch the values for a batch of keys, preferring one `state_queryStorageAt`
/// request and falling back to per-key fetches if the node rejects the batch.
async fn fetch_batch(
    client: RawClient,
    batch: Vec<String>,
    at: Arc<str>,
) -> Result<Vec<(String, String)>> {
    match query_storage_batch(&client, &batch, &at).await {
        Ok(pairs) => Ok(pairs),
        Err(err) => {
            warn_once_batch_fallback(&err);
            futures::stream::iter(batch.into_iter().map(|key| {
                let client = client.clone();
                let at = at.clone();
                async move {
                    let value = client
                        .storage_value(&key, &at)
                        .await?
                        .ok_or_else(|| eyre!("missing storage value for key {key}"))?;
                    Ok::<_, Report>((key, value))
                }
            }))
            .buffer_unordered(FALLBACK_FETCH_CONCURRENCY)
            .try_collect()
            .await
        }
    }
}

/// Fetch all storage pairs at `at` and stream them to `path` as a JSON object,
/// never holding the values in memory (only the key list). The file is written
/// to a temporary path and renamed on success so an interrupted fetch never
/// leaves a half-written cache behind.
async fn fetch_storage_to_file(
    pool: &NodePool,
    at: &str,
    path: &Path,
    value_batch_size: usize,
    key_scan_concurrency: usize,
) -> Result<()> {
    let keys = fetch_all_keys(pool, at, key_scan_concurrency).await?;
    let at: Arc<str> = Arc::from(at);

    let mut bar = cli::ProgressBarManager::new_bar(
        keys.len().try_into().unwrap(),
        "Fetching storage values",
    )?;

    let tmp_path = path.with_extension("json.tmp");
    let file = File::create(&tmp_path)?;
    let mut writer = BufWriter::new(file);
    {
        use serde::Serializer as _;
        let mut ser = serde_json::Serializer::new(&mut writer);
        let mut map = (&mut ser).serialize_map(None).err_into()?;

        let mut batches = futures::stream::iter(
            keys.chunks(value_batch_size.max(1))
                .enumerate()
                .map(|(i, batch)| fetch_batch(pool.get(i).clone(), batch.to_vec(), at.clone())),
        )
        .buffer_unordered(VALUE_BATCH_CONCURRENCY);

        while let Some(batch) = batches.next().await {
            for (key, value) in batch? {
                map.serialize_entry(&key, &value).err_into()?;
                bar.inc(1);
            }
        }
        SerializeMap::end(map).err_into()?;
    }
    writer.flush()?;
    drop(writer);
    std::fs::rename(&tmp_path, path)?;

    bar.finish_with_message("Done");

    Ok(())
}

/// Stream-deserialize the storage file, retaining only the `wanted` keys.
struct SelectedKeys<'a> {
    wanted: &'a HashSet<String>,
}

impl<'de> DeserializeSeed<'de> for SelectedKeys<'_> {
    type Value = HashMap<String, String>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for SelectedKeys<'_> {
    type Value = HashMap<String, String>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "a map of hex storage key-value pairs")
    }

    fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut selected = HashMap::new();
        while let Some(key) = access.next_key::<String>()? {
            if self.wanted.contains(&key) {
                selected.insert(key, access.next_value::<String>()?);
            } else {
                access.next_value::<IgnoredAny>()?;
            }
        }
        Ok(selected)
    }
}

fn read_selected_keys(path: &Path, wanted: &HashSet<String>) -> Result<HashMap<String, String>> {
    let file = File::open(path)?;
    let mut de = serde_json::Deserializer::from_reader(BufReader::new(file));
    SelectedKeys { wanted }.deserialize(&mut de).err_into()
}

/// Filters applied while streaming state into the fork's chain-spec.
struct TopFilter {
    /// Storage entries must match one of these prefixes to be kept.
    include_prefixes: Vec<String>,
    /// Entries (from storage or the base spec) matching any of these prefixes are dropped.
    exclude_prefixes: Vec<String>,
    /// Exact keys dropped from storage and the base spec.
    remove_exact: HashSet<String>,
}

impl TopFilter {
    fn keeps_base_key(&self, key: &str) -> bool {
        !self.exclude_prefixes.iter().any(|p| key.starts_with(p))
            && !self.remove_exact.contains(key)
    }

    fn keeps_storage_key(&self, key: &str) -> bool {
        self.include_prefixes.iter().any(|p| key.starts_with(p)) && self.keeps_base_key(key)
    }
}

/// The fork's `genesis.raw.top` map, assembled at serialization time by
/// streaming the storage file from disk and merging the (small) base-spec and
/// override maps, so the bulk state is never held in memory.
///
/// Precedence matches the old in-memory merge: storage entries shadow base-spec
/// entries, and overrides win over everything.
struct StreamedTop<'a> {
    storage_path: Option<&'a Path>,
    base_top: &'a serde_json::Map<String, JsonValue>,
    overrides: &'a serde_json::Map<String, JsonValue>,
    filter: &'a TopFilter,
}

impl Serialize for StreamedTop<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::Error;

        let mut map = serializer.serialize_map(None)?;
        let mut shadowed: HashSet<String> = HashSet::new();

        if let Some(path) = self.storage_path {
            let file = File::open(path).map_err(S::Error::custom)?;
            let mut de = serde_json::Deserializer::from_reader(BufReader::new(file));
            StreamTopSeed {
                top: self,
                map: &mut map,
                shadowed: &mut shadowed,
            }
            .deserialize(&mut de)
            .map_err(S::Error::custom)?;
        }

        for (key, value) in self.base_top {
            if shadowed.contains(key)
                || self.overrides.contains_key(key)
                || !self.filter.keeps_base_key(key)
            {
                continue;
            }
            map.serialize_entry(key, value)?;
        }

        for (key, value) in self.overrides {
            map.serialize_entry(key, value)?;
        }

        map.end()
    }
}

/// Drives the storage-file deserializer, forwarding each kept entry straight
/// into the output map serializer.
struct StreamTopSeed<'a, 'b, M> {
    top: &'a StreamedTop<'a>,
    map: &'b mut M,
    shadowed: &'b mut HashSet<String>,
}

impl<'de, M: SerializeMap> DeserializeSeed<'de> for StreamTopSeed<'_, '_, M> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(self)
    }
}

impl<'de, M: SerializeMap> Visitor<'de> for StreamTopSeed<'_, '_, M> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "a map of hex storage key-value pairs")
    }

    fn visit_map<A>(self, mut access: A) -> Result<(), A::Error>
    where
        A: MapAccess<'de>,
    {
        use serde::de::Error;

        while let Some(key) = access.next_key::<String>()? {
            if self.top.filter.keeps_storage_key(&key) && !self.top.overrides.contains_key(&key) {
                let value = access.next_value::<String>()?;
                self.map
                    .serialize_entry(&key, &value)
                    .map_err(A::Error::custom)?;
                if self.top.base_top.contains_key(&key) {
                    self.shadowed.insert(key);
                }
            } else {
                access.next_value::<IgnoredAny>()?;
            }
        }
        Ok(())
    }
}

/// Mirror of [`ChainSpec`] for output, with `top` streamed from disk.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChainSpecOut<'a> {
    name: &'a str,
    id: &'a str,
    chain_type: &'a str,
    boot_nodes: &'a [String],
    telemetry_endpoints: &'a Option<Vec<String>>,
    protocol_id: &'a Option<String>,
    properties: &'a Option<JsonValue>,
    code_substitutes: &'a JsonValue,
    genesis: GenesisOut<'a>,
    #[serde(flatten)]
    extensions: &'a Option<JsonValue>,
}

#[derive(Serialize)]
struct GenesisOut<'a> {
    raw: RawGenesisOut<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RawGenesisOut<'a> {
    top: StreamedTop<'a>,
    children_default: &'a JsonValue,
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

fn append_twox_64_concat_u64(key: &mut Vec<u8>, chain_key: u64) {
    let le = chain_key.to_le_bytes();
    key.extend_from_slice(&twox_64(&le));
    key.extend_from_slice(&le);
}

/// Substrate `Blake2_128Concat`: `blake2_128(data) || data`.
fn append_blake2_128_concat_bytes(key: &mut Vec<u8>, data: &[u8]) {
    key.extend_from_slice(&blake2_128(data));
    key.extend_from_slice(data);
}

/// `Blake2_128Concat` for SCALE-encoded `u64` chain key (maps that use `Blake2_128Concat, ChainKey`).
fn append_blake2_128_concat_u64(key: &mut Vec<u8>, chain_key: u64) {
    let encoded = chain_key.to_le_bytes();
    append_blake2_128_concat_bytes(key, &encoded);
}

/// Prefix shared by every `Attestation::Attestors(chain_key, _)` key for a fixed `chain_key`.
fn attestors_storage_key_prefix(chain_key: u64) -> String {
    let mut key = Vec::with_capacity(48);
    key.extend_from_slice(&twox_128(b"Attestation"));
    key.extend_from_slice(&twox_128(b"Attestors"));
    append_twox_64_concat_u64(&mut key, chain_key);
    key.to_hex()
}

/// Attestation pallet: `Attestors(chain_key, account_id)`.
/// Storage key = `twox_128("Attestation") + twox_128("Attestors") + twox_64_concat(chain_key) + blake2_128_concat(account_id)`.
fn attestors_storage_key(account_id: &[u8; 32], chain_key: u64) -> String {
    use blake2::digest::{Update, VariableOutput};
    let mut key = Vec::with_capacity(32 + 8 + 8 + 16 + 32);
    key.extend_from_slice(&twox_128(b"Attestation"));
    key.extend_from_slice(&twox_128(b"Attestors"));
    append_twox_64_concat_u64(&mut key, chain_key);
    let mut hasher = blake2::Blake2bVar::new(16).unwrap();
    hasher.update(account_id);
    let mut hash = [0u8; 16];
    hasher.finalize_variable(&mut hash).unwrap();
    key.extend_from_slice(&hash);
    key.extend_from_slice(account_id);
    key.to_hex()
}

/// BLS public key (48 bytes) from the same derivation as `attestor`: `PrivateKey::new` over the
/// UTF-8 bytes of the secret URI string (`0x` + 64 hex for raw seeds).
fn bls_public_key_from_hex_seed_uri(seed_hex_uri: &str) -> Result<[u8; 48], Report> {
    let pk = BlsPrivateKey::new(seed_hex_uri.as_bytes()).public_key();
    let bytes = BlsSerialize::as_bytes(&pk);
    bytes
        .try_into()
        .map_err(|_| eyre!("BLS public key must be 48 bytes"))
}

/// `Attestor` SCALE value: `Some(bls)`, `AttestorStatus::Active`, stash.
fn attestor_value_with_bls(bls_public_key: &[u8; 48], stash: &[u8; 32]) -> String {
    let mut v = Vec::with_capacity(1 + 48 + 1 + 32);
    v.push(0x01u8); // Option::Some
    v.extend_from_slice(bls_public_key);
    v.push(0x00u8); // AttestorStatus::Active (first variant)
    v.extend_from_slice(stash);
    v.to_hex()
}

/// `ActiveAttestors(chain_key)`: `twox_128` + `twox_128` + `blake2_128_concat`(SCALE `u64`).
fn active_attestors_storage_key(chain_key: u64) -> String {
    let mut key = Vec::with_capacity(16 + 16 + 16 + 8);
    key.extend_from_slice(&twox_128(b"Attestation"));
    key.extend_from_slice(&twox_128(b"ActiveAttestors"));
    append_blake2_128_concat_u64(&mut key, chain_key);
    key.to_hex()
}

/// `TargetSampleSize(chain_key)`: `twox_128` + `twox_128` + `blake2_128_concat`(SCALE `u64`).
fn target_sample_size_storage_key(chain_key: u64) -> String {
    let mut key = Vec::with_capacity(16 + 16 + 16 + 8);
    key.extend_from_slice(&twox_128(b"Attestation"));
    key.extend_from_slice(&twox_128(b"TargetSampleSize"));
    append_blake2_128_concat_u64(&mut key, chain_key);
    key.to_hex()
}

/// SCALE: Vec<AccountId> with [alice, bob] = compact(2) + 32 + 32.
fn active_attestors_value(alice: &[u8; 32], bob: &[u8; 32]) -> String {
    let mut v = Vec::with_capacity(1 + 32 + 32);
    v.push(0x08u8); // compact encoding of 2
    v.extend_from_slice(alice);
    v.extend_from_slice(bob);
    v.to_hex()
}

/// `System.Account(AccountId)`: `twox_128` + `twox_128` + `blake2_128_concat`(`AccountId` bytes).
fn system_account_storage_key(account_id: &[u8; 32]) -> String {
    let mut key = Vec::with_capacity(16 + 16 + 16 + 32);
    key.extend_from_slice(&twox_128(b"System"));
    key.extend_from_slice(&twox_128(b"Account"));
    append_blake2_128_concat_bytes(&mut key, account_id.as_slice());
    key.to_hex()
}

/// 10 CTC in planck (1 CTC = 1e18 planck; matches node `chain_spec` `UNITS`).
const TEN_CTC_PLANCK: u128 = 10 * 1_000_000_000_000_000_000;

/// `pallet_balances::ExtraFlags::default()` — new balance ref-counting is active.
const BALANCE_EXTRA_FLAGS: u128 = 0x8000_0000_0000_0000_0000_0000_0000_0000u128;

/// `frame_system::AccountInfo` + `pallet_balances::AccountData` SCALE encoding (nonce, refcounts, free/reserved/frozen/flags).
fn system_account_info_with_free_balance(free: u128) -> String {
    let mut v = Vec::with_capacity(80);
    v.extend_from_slice(&0u32.to_le_bytes()); // nonce
    v.extend_from_slice(&0u32.to_le_bytes()); // consumers
    v.extend_from_slice(&1u32.to_le_bytes()); // providers (balances pallet)
    v.extend_from_slice(&0u32.to_le_bytes()); // sufficients
    v.extend_from_slice(&free.to_le_bytes());
    v.extend_from_slice(&0u128.to_le_bytes()); // reserved
    v.extend_from_slice(&0u128.to_le_bytes()); // frozen
    v.extend_from_slice(&BALANCE_EXTRA_FLAGS.to_le_bytes());
    v.to_hex()
}

fn json_hex_bytes(v: &JsonValue) -> Option<Vec<u8>> {
    let s = v.as_str()?;
    let s = s.strip_prefix("0x").unwrap_or(s);
    hex::decode(s).ok()
}

fn u128_le_from_first_16(bytes: &[u8]) -> Option<u128> {
    if bytes.len() < 16 {
        return None;
    }
    let mut arr = [0u8; 16];
    arr.copy_from_slice(&bytes[..16]);
    Some(u128::from_le_bytes(arr))
}

/// `free` field inside `AccountInfo` (offset 16: after nonce + 3× `RefCount`).
fn free_balance_from_account_storage(bytes: &[u8]) -> Option<u128> {
    if bytes.len() < 32 {
        return None;
    }
    let mut arr = [0u8; 16];
    arr.copy_from_slice(&bytes[16..32]);
    Some(u128::from_le_bytes(arr))
}

fn free_balance_from_account_json(v: &JsonValue) -> Option<u128> {
    let b = json_hex_bytes(v)?;
    free_balance_from_account_storage(&b)
}

fn scale_u128_storage_hex(n: u128) -> String {
    n.to_le_bytes().to_hex()
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

async fn resolve_block_hash(client: &RawClient, at: Option<H256>) -> Result<String> {
    if let Some(at) = at {
        Ok(at.0.to_hex())
    } else {
        client.latest_block_hash().await
    }
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
        .max_request_body_size(MAX_WS_MESSAGE_SIZE)
        .build(url)
        .await
        .err_into()
}

async fn new_ws_client(url: Uri) -> Result<RawClient> {
    let (sender, receiver) = ws_transport(url).await?;
    let raw = Arc::new(
        jsonrpsee::async_client::ClientBuilder::default()
            .max_concurrent_requests(MAX_CONCURRENT_REQUESTS)
            .build_with_tokio(sender, receiver),
    );
    Ok(RawClient::Ws(raw))
}

/// The HTTP(S) endpoint used for the bulk state fetch: an explicit
/// `--http-rpc` URL, `None` for `--http-rpc none`, or (by default) the
/// `--rpc` URL with `wss://` swapped for `https://` — public endpoints serve
/// both, and stateless HTTP requests load-balance across backends where a
/// websocket session is pinned to one. Plain `ws://` (typically a local node)
/// keeps using the websocket.
fn fetch_http_url(http_rpc: Option<&str>, ws_url: &str) -> Option<String> {
    match http_rpc {
        Some(s) if s.eq_ignore_ascii_case("none") => None,
        Some(url) => Some(url.to_owned()),
        None => {
            let normalized = normalize_rpc_url(ws_url);
            normalized
                .starts_with("wss://")
                .then(|| normalized.replacen("wss://", "https://", 1))
        }
    }
}

/// The fork always replaces these pallets' state with the dev chain's so that
/// Alice is the sole validator.
const VALIDATOR_PALLETS: [&str; 6] = [
    "Babe",
    "Grandpa",
    "Session",
    "Staking",
    "ImOnline",
    "VoterList",
];

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = cli::Cli::parse();

    let rpc_url = parse_rpc_uri(&cli.rpc)?;

    // The bulk chain state only ever lives in the storage file on disk; it is
    // streamed back out when writing the fork's chain-spec.
    let storage_path = match cli.storage {
        Some(StorageFile::None) => None,
        Some(StorageFile::Path(path)) => Some(path),
        None => {
            let mut path = cli.out.clone().into_os_string();
            path.push(".storage.json");
            Some(PathBuf::from(path))
        }
    };

    if let Some(path) = &storage_path {
        if path.exists() {
            println!("using existing storage at {}", path.display());
        } else {
            let http_url = fetch_http_url(cli.http_rpc.as_deref(), &cli.rpc);
            let pool =
                NodePool::connect(&rpc_url, http_url.as_deref(), cli.rpc_connections).await?;
            let at = resolve_block_hash(pool.get(0), cli.at).await?;
            fetch_storage_to_file(
                &pool,
                &at,
                path,
                cli.value_batch_size,
                cli.key_scan_concurrency,
            )
            .await?;
            println!(
                "cached fetched state at {} (reused on the next run; delete it to refetch)",
                path.display()
            );
        }
    }

    let orig_spec = build_spec(&cli.binary, cli.original_chain).await?;
    let mut spec = build_spec(&cli.binary, cli.base_chain).await?;

    spec.name = cli
        .name
        .unwrap_or_else(|| orig_spec.name.joined_with("-fork"));
    spec.id = cli.id.unwrap_or_else(|| orig_spec.id.joined_with("-fork"));
    spec.protocol_id.clone_from(&orig_spec.protocol_id);
    spec.boot_nodes = vec![];

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

    let mut include_prefixes = vec![
        storage_prefix("System", "Account"), // System.Account
    ];
    if let Some(pallets) = cli.pallets {
        include_prefixes.extend(pallets.iter().map(|n| module_prefix(n)));
    } else {
        let api = ApiClient::<CreditcoinConfig>::from_url(rpc_url.to_string()).await?;
        let meta = api.rpc().metadata().await?;
        for pallet in &meta.runtime_metadata().pallets {
            let n = &pallet.name;
            if pallet.storage.is_some() && !excludes.contains(n.as_str()) {
                let hashed = module_prefix(n);
                include_prefixes.push(hashed);
            }
        }
    }

    // The fork always injects the dev chain's validator genesis, so drop the
    // original chain's validator state (from storage and the base spec alike).
    let mut exclude_prefixes: Vec<String> =
        VALIDATOR_PALLETS.iter().map(|p| module_prefix(p)).collect();
    if cli.usc {
        exclude_prefixes.push(storage_prefix("Attestation", "ActiveAttestors"));
        exclude_prefixes.push(storage_prefix("Attestation", "TargetSampleSize"));
        exclude_prefixes.push(module_prefix("Randomness"));
        // Merged RPC state includes every on-chain `Attestors` entry; drop them so only Alice/Bob remain.
        exclude_prefixes.push(attestors_storage_key_prefix(cli.usc_chain_key));
    }

    // make sure to remove System.LastRuntimeUpgrade to trigger a migration
    let remove_exact = HashSet::from([storage_prefix("System", "LastRuntimeUpgrade")]);

    let filter = TopFilter {
        include_prefixes,
        exclude_prefixes,
        remove_exact,
    };

    // The few values main() needs to read are pulled from the storage file in a
    // single streaming pass instead of holding the whole state in memory.
    let code_key = b":code".to_hex();
    let issuance_key = storage_prefix("Balances", "TotalIssuance");
    let alice = account_id_from_seed_hex(ALICE_SEED_HEX)?;
    let bob = account_id_from_seed_hex(BOB_SEED_HEX)?;
    let alice_acct_key = system_account_storage_key(&alice);
    let bob_acct_key = system_account_storage_key(&bob);

    let mut wanted = HashSet::from([code_key.clone()]);
    if cli.usc {
        wanted.extend([
            issuance_key.clone(),
            alice_acct_key.clone(),
            bob_acct_key.clone(),
        ]);
    }
    let selected = match &storage_path {
        Some(path) => read_selected_keys(path, &wanted)?,
        None => HashMap::default(),
    };

    let wasm_hex = if let Some(runtime_path) = &cli.runtime {
        println!("Reading from runtime wasm file: {}", runtime_path.display());
        read_wasm_hex(runtime_path).await?
    } else {
        selected
            .get(&code_key)
            .expect("storage should include the runtime code")
            .clone()
    };

    // Entries that win over both storage and the base spec.
    let mut overrides = serde_json::Map::new();

    // Overwrite the on-chain wasm blob
    overrides.insert(code_key, wasm_hex.into());

    // Make sure that the genesis state is different
    overrides.insert("0xdeadbeef".to_owned(), "0x1".into());

    // Always set the sudo key to Alice, replacing the original chain's key.
    // creditcoin3's runtime-upgrade CI forks testnet/mainnet without `--usc`
    // and submits sudo calls signed by //Alice against the fork.
    overrides.insert(storage_prefix("Sudo", "Key"), alice.to_hex().into());

    if cli.usc {
        // USC component: Alice and Bob from hex seeds; set Attestation pallet genesis
        let ck = cli.usc_chain_key;

        // Reads a merged-state value the way the old in-memory merge saw it:
        // filtered storage first, then the base spec.
        let merged_value = |key: &str| -> Option<JsonValue> {
            if filter.keeps_storage_key(key) {
                if let Some(v) = selected.get(key) {
                    return Some(JsonValue::String(v.clone()));
                }
            }
            spec.genesis.raw.top.get(key).cloned()
        };

        let alice_bls = bls_public_key_from_hex_seed_uri(ALICE_SEED_HEX)?;
        let bob_bls = bls_public_key_from_hex_seed_uri(BOB_SEED_HEX)?;

        overrides.insert(
            attestors_storage_key(&alice, ck),
            attestor_value_with_bls(&alice_bls, &alice).into(),
        );
        overrides.insert(
            attestors_storage_key(&bob, ck),
            attestor_value_with_bls(&bob_bls, &bob).into(),
        );
        overrides.insert(
            active_attestors_storage_key(ck),
            active_attestors_value(&alice, &bob).into(),
        );
        overrides.insert(target_sample_size_storage_key(ck), "0x02000000".into());

        // 10 CTC each on `System.Account`; keep `Balances::TotalIssuance` consistent with prior state.
        let old_issuance = merged_value(&issuance_key)
            .as_ref()
            .and_then(json_hex_bytes)
            .and_then(|b| u128_le_from_first_16(&b))
            .unwrap_or(0);

        let old_alice_free = merged_value(&alice_acct_key)
            .as_ref()
            .and_then(free_balance_from_account_json)
            .unwrap_or(0);
        let old_bob_free = merged_value(&bob_acct_key)
            .as_ref()
            .and_then(free_balance_from_account_json)
            .unwrap_or(0);

        let new_issuance = old_issuance
            .saturating_sub(old_alice_free)
            .saturating_sub(old_bob_free)
            .saturating_add(TEN_CTC_PLANCK)
            .saturating_add(TEN_CTC_PLANCK);

        overrides.insert(
            alice_acct_key,
            system_account_info_with_free_balance(TEN_CTC_PLANCK).into(),
        );
        overrides.insert(
            bob_acct_key,
            system_account_info_with_free_balance(TEN_CTC_PLANCK).into(),
        );
        overrides.insert(issuance_key, scale_u128_storage_hex(new_issuance).into());
    }

    // Inject the dev chain's validator genesis so Alice is the sole authority.
    let dev_spec = build_spec(&cli.binary, Chain::Dev).await?;
    let validator_prefixes: Vec<_> = VALIDATOR_PALLETS.iter().map(|p| module_prefix(p)).collect();
    for (k, v) in &dev_spec.genesis.raw.top {
        if validator_prefixes.iter().any(|p| k.starts_with(p.as_str())) {
            overrides.insert(k.clone(), v.clone());
        }
    }

    println!("{}", style("Writing chain specification for fork").green());

    let out = ChainSpecOut {
        name: &spec.name,
        id: &spec.id,
        chain_type: &spec.chain_type,
        boot_nodes: &spec.boot_nodes,
        telemetry_endpoints: &spec.telemetry_endpoints,
        protocol_id: &spec.protocol_id,
        properties: &spec.properties,
        code_substitutes: &spec.code_substitutes,
        genesis: GenesisOut {
            raw: RawGenesisOut {
                top: StreamedTop {
                    storage_path: storage_path.as_deref(),
                    base_top: &spec.genesis.raw.top,
                    overrides: &overrides,
                    filter: &filter,
                },
                children_default: &spec.genesis.raw.children_default,
            },
        },
        extensions: &spec.extensions,
    };

    let file = File::create(&cli.out)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, &out)?;
    writer.flush()?;

    println!("{}", style("Done!").green());

    Ok(())
}

#[cfg(test)]
mod usc_storage_key_tests {
    use super::*;

    #[test]
    fn chain_3_keys_match_prior_hardcoded_constants() {
        assert_eq!(
            active_attestors_storage_key(3),
            "0x6310fed47319b658f9b8b2504e0d72ec605e795422de90908f14285054a6764b8e79fdf1428e95842eaa9af0b22414be0300000000000000"
        );
        assert_eq!(
            target_sample_size_storage_key(3),
            "0x6310fed47319b658f9b8b2504e0d72ec77c34a0cad03a52cd52d9faa5a75ec8f8e79fdf1428e95842eaa9af0b22414be0300000000000000"
        );
    }

    #[test]
    fn attestors_storage_key_starts_with_chain_prefix() {
        let alice = account_id_from_seed_hex(ALICE_SEED_HEX).unwrap();
        let ck = 3u64;
        let prefix = attestors_storage_key_prefix(ck);
        let key = attestors_storage_key(&alice, ck);
        assert!(key.starts_with(&prefix), "{key} should start with {prefix}");
    }
}
