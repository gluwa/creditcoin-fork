# creditcoin-fork

This is a tool to fork a creditcoin network and create a new, distinct
chain with mostly the same on-chain state as the original. This is especially
useful for testing runtime upgrades and migrations, as you can simulate updating
mainnet while safely separating from the actual mainnet.

## Building

1. Install the [rust toolchain](https://rustup.rs)
2. Clone the repo

   ```bash
   git clone https://github.com/gluwa/creditcoin-fork
   cd creditcoin-fork
   ```

3. Build it! The resulting binary will be located at `target/release/creditcoin-fork` by default

   ```bash
   cargo build --release
   ```

## Running

First, make sure you're able to build the repo. You can then take a look at the options
by running the `creditcoin-fork` binary with the `--help` flag:

```bash
./target/release/creditcoin-fork --help
```

### Pre-requisites

1. RPC access to a live creditcoin node
2. Working `creditcoin-fork` binary

### Instructions

Run the creditcoin-fork binary. If you're not running a local creditcoin-node
accessible at `ws://localhost:9944`, pass the RPC URL with the `--rpc` flag.
For **public RPC endpoints** you can omit the port—e.g.
`wss://rpc.usc-devnet.creditcoin.network`—the tool uses 443 for `wss://` and 80 for `ws://` when no port is given.

Minimal example, assuming a live testnet node running on localhost and a `creditcoin-node`
binary is in your `PATH`:

```bash
./target/release/creditcoin-fork --bin creditcoin-node --orig test --base dev -o fork.json
```

This should run successfully and the fork's chain spec will be located at `fork.json`.
You can then run a node on the fork by passing the chain spec path as the `--chain`, for example:

```bash
creditcoin-node --chain ./fork.json --validator --mining-key 5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY
```

### Single-node fork (producing blocks with `--alice`)

The fork **always** injects the dev chain’s validator genesis (Babe, Grandpa, Session, Staking) so that **Alice** is the sole authority. You can use any `--base` (e.g. `dev` or `devnet`); the fork will overwrite consensus state with the dev chain’s, so running with `--alice` will produce blocks.

Create the fork (example with devnet as source):

```bash
./target/release/creditcoin-fork --bin creditcoin3-node --orig devnet --base dev --name Development -o fork.json --rpc wss://rpc.usc-devnet.creditcoin.network
```

Then start the node:

```bash
creditcoin3-node --chain ./fork.json --validator --alice --pruning archive --base-path ./fork
```

### Custom runtime (`--runtime`)

By default the fork uses the runtime WASM blob fetched from the live chain. If you want to use a custom runtime, for example one with shorter epoch/era durations for faster testing—build your runtime and pass it with `--runtime`:

```bash
# Build the runtime (from the creditcoin3-next repo)
cargo build --release -p creditcoin3-runtime

# Create the fork with the custom runtime
./target/release/creditcoin-fork \
  --bin creditcoin3-node \
  --orig testnet --base testnet --name Testnet \
  -o fork.json \
  --rpc wss://rpc.usc-testnet2.creditcoin.network \
  --usc \
  --runtime /path/to/creditcoin3-next/target/release/wbuild/creditcoin3-runtime/creditcoin3_runtime.compact.compressed.wasm
```

To shorten epoch/era durations, edit `runtime/src/lib.rs` in the creditcoin3-next repo before building:

```rust
// Change epoch from 12 hours to 15 minutes:
pub const EPOCH_DURATION_IN_BLOCKS: u32 = prod_or_fast!(15 * MINUTES, BLOCKS_FOR_FASTER_EPOCH);
//                                                       ^^^^^^^^^^^^
//                                            was: 12 * HOURS (2,880 blocks / 12 hrs)
//                                            now: 15 * MINUTES (60 blocks / 15 min)
```

`SessionsPerEra` is 2 by default, so era duration = 2 × epoch. With the change above, eras go from 24 hours to 30 minutes.
