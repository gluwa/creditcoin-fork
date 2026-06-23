# Stream state to disk (fix OOM) — plan: ~/.claude/plans/wiggly-questing-robin.md

- [x] Back up HEAD-built binary for equivalence diff (/tmp/creditcoin-fork-old)
- [x] src/main.rs: fetch streams to storage file (batched values + SerializeMap writer)
- [x] src/main.rs: partitioned parallel key scan (256 keyspace partitions, 32 concurrent)
- [x] src/main.rs: read_selected_keys streaming pre-pass (:code, TotalIssuance, Alice/Bob accounts)
- [x] src/main.rs: include/exclude/remove/overrides small maps (semantics preserved)
- [x] src/main.rs: StreamedTop Serialize impl + streamed output write
- [x] src/cli.rs + README: doc updates for --storage default
- [x] cargo build/test/clippy (2 tests pass, clippy clean)
- [x] Equivalence diff old vs new binary on synthetic storage file (jq -S): IDENTICAL (usc + plain)
- [x] Fetch-path equivalence on local dev node at pinned block: fetched state + outputs IDENTICAL
- [x] Memory sanity: /usr/bin/time -v 29.5 MB (new) vs 62 MB (old) on 14 MB input

## Review

**What changed** (commit-ready on `feat_usc`, not yet committed):
- `src/main.rs`: state is never held in memory. Fetch streams pairs to the storage
  file via an incremental `SerializeMap`; output streams the file back through
  `StreamedTop` (custom `Serialize` driving a `DeserializeSeed`) merged with small
  base-spec/override maps. The handful of values `main()` reads (`:code`,
  `TotalIssuance`, Alice/Bob accounts) come from a streaming pre-pass.
- Fetch speedups: 256-way partitioned key scan (keys start with uniformly
  distributed `twox_128`, partitioned by first byte, 32 concurrent) and batched
  `state_queryStorageAt` value fetches (256 keys/req, 32 in flight, per-key fallback).
- `--storage` omitted now caches at `<out>.storage.json` and reuses it; interrupted
  fetches write to a `.tmp` file renamed only on success.
- Removed now-unused deps `async-stream`, `fxhash`; removed `inject_dev_validators`,
  `fetch_storage_pairs`, `key_stream` (replaced).

**Verification**: old (HEAD) vs new binary produce byte-identical sorted output for
usc + plain configs on a 22k-entry synthetic storage file, and identical fetched
state + output against a live dev node at a pinned block. Tests + clippy clean.

**Why it OOM'd before**: 4 concurrent copies of state (fetch HashMap, cache
serialization buffer, cloned spec map, output serialization buffer) ≈ 48 GB RSS for
mainnet on a 62 GB box.
