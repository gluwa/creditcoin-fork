# Lessons

## Benchmark what production does, not what's easy to generate (2026-06-03)

While optimizing the state fetch, early benchmarks used uniformly random start
keys for `state_getKeysPaged`. Those mostly land in empty keyspace and return
the same ~30 hot (server-cached) pallet-boundary pages — showing 36-46k keys/s
when the real sequential scan could only ever get ~10k. Hours of tuning chased
a client-side bottleneck that didn't exist.

**Rule**: when a benchmark disagrees with production behavior, first make the
benchmark reproduce production's *access pattern* (sequential chains, cold
pages, same `at` pinning) before trusting either number. The decisive tests
were: (1) replicating the binary's exact pattern in an independent client
(python), and (2) bisecting the binary with env-var toggles.

## Static keyspace partitioning starves on substrate state (2026-06-03)

Storage keys cluster under a handful of 32-byte pallet/item prefixes.
Partitioning the keyspace by first byte (or any fixed grid) puts nearly all
keys into a few partitions that page sequentially. Also, a keyspace *midpoint*
split lands in empty space (maps occupy a vanishing sliver of their range).
Working design: dynamic range splitting with **density-based** split points
(jump N pages ahead using the span of the page just fetched), spawning one
chunk per starved worker — each range must spawn multiple children or the
worker population never grows.

## Public RPC endpoint behavior (mainnet3.creditcoin.network, 2026-06-03)

- Websocket sessions are pinned to one backend: ~4 req/s ceiling per session,
  unaffected by in-flight concurrency; extra ws connections from one IP do not
  help (2-3k keys/s total).
- HTTPS requests load-balance per-request: ~38-45 pages/s sustained on clean
  regions from one client.
- The hard floor is the backend's trie iteration over the big maps (EVM
  `0x1da5...`, `0xcec5...`): ~10 pages/s there for every client/transport.
  Faster requires an internal/local archive node.
