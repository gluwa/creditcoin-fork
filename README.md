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
accessible at `ws://localhost:9944`, make sure to pass the RPC URL of the
live creditcoin node with the `--rpc` flag.

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
