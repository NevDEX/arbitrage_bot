# NevDEX × Uniswap V3 Arbitrage — Rust

A standalone Rust client library and runnable arbitrage example. Built with Alloy, Tokio, Reqwest, and Tokio Tungstenite. The strategy interface is separate from order signing, settlement tracking, and hedge execution so bot developers can replace the signal without rebuilding the execution flow.

The example market is CASHCAT/USDG on Robinhood Chain (4663). Update both token addresses and the direct V3 pool when changing markets. Example addresses and settings do not guarantee current liquidity or a profitable trade.

## Quick start

Requires Rust 1.91 or later. Live trading requires macOS or Linux for private-file ownership and permission checks.

```bash
cd rust-arbitrage
cp arbitrage.example.json arbitrage.json
cargo build --locked --release

# Check market, chain, token decimals, pool, factory, Router, and Quoter.
./target/release/arbitrage --config arbitrage.json --check

# Preview one fresh book and executable quote pair, then exit.
./target/release/arbitrage --config arbitrage.json --once

# Monitor continuously in read-only mode; press Ctrl+C to stop.
./target/release/arbitrage --config arbitrage.json
```

`--check`, `--once`, and `--live` are mutually exclusive. Both read-only inspection modes have a 30-second deadline and exit with a nonzero status on failure. Read-only modes do not read private keys, create recovery directories, sign orders, submit cancellations, or broadcast transactions.

```bash
cargo run --locked -- --help
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo fmt --check
```

Tests use local mock HTTP/WebSocket services and public deterministic signing fixtures. They cover numeric precision, the sibling JavaScript implementation's order hash and signature, Router ABI encoding, order validation, planning, journal recovery, confirmed fill scanning, and execution behavior. They do not submit transactions to a public chain.

## Use your own strategy

The crate exports reusable modules:

| Module | Responsibility |
| --- | --- |
| `transport` | NevDEX API, serialized place/cancel requests, bounded JSON-RPC transport |
| `feeds` | Market and account WebSockets, REST bootstrap, reconnects, freshness |
| `orders` | Built-order validation, local order hash, NevDEX EIP-191 signature |
| `planner` | `Strategy` interface, exact arithmetic, default cross-venue signal |
| `uniswap` | Deployment checks, executable quotes, confirmed fills, swaps, receipts |
| `state` | Exclusive wallet journal lock and durable order/hedge intents |
| `engine` | One unfinished cycle per wallet, risk checks and recovery |

Implement `planner::Strategy` and pass it to `Engine::with_strategy(config, strategy)`. `StrategyInput` supplies the validated market, depth, executable buy/sell quotes, and quote start time. Return an `Opportunity`, or `None` to skip the trade. The engine requires the configured order size and enforces per-trade and cumulative commitment caps independently of the strategy. Changes to the signal still require reviewing its profit and execution assumptions.

See [examples/custom_strategy.rs](examples/custom_strategy.rs) for a strategy that adds a minimum net-profit filter:

```bash
cargo run --locked --example custom_strategy -- arbitrage.json
```

This example runs one read-only preview. To consume this crate from another local project:

```toml
[dependencies]
nevdex-arbitrage = { path = "../rust-arbitrage" }
```

## Live trading

Use a dedicated wallet exclusively for one bot. The Rust OS lock covers one wallet within one Rust state directory. It does not coordinate with Go, Node.js, another state directory, or manual wallet transactions. Recovery files are specific to this implementation; do not copy another version's journal into this directory.

Set these fields in the complete `arbitrage.json` configuration:

```json
{
  "vaultAddress": "REVIEWED_VAULT_ADDRESS",
  "exchangeAddress": "EXCHANGE_ADDRESS_BOUND_TO_THIS_VAULT",
  "account": "DEDICATED_EOA_ADDRESS",
  "privateKeyFile": "keys/arbitrage.key"
}
```

These are placeholders. The example configuration deliberately leaves live credentials and contract bindings empty. Relative file paths resolve from the configuration file's directory.

1. Prefund base and quote inventory in the reviewed NevDEX Vault for the first leg.
2. Separately fund the wallet with base, quote, and native gas tokens for hedges. Manually grant the official SwapRouter02 a bounded token allowance.
3. Put a 64-character hexadecimal private key, optionally prefixed with `0x`, in a private file owned by your user. Set mode `600`; keep it outside version control.
4. Start with no existing wallet orders or pending settlements. Use the same state directory on subsequent starts.

```bash
chmod 600 keys/arbitrage.key
./target/release/arbitrage --config arbitrage.json --live
```

Only `--live` enables real orders, cancellations, and swaps. Startup checks both `Exchange.vault()` and `Vault.exchange()` bindings. These checks do not replace a contract audit or a required Vault fix and migration. The bot does not approve tokens, deposit, withdraw, or move inventory between venues.

## Execution and recovery

1. Load the REST book and subscribe to market depth. Require fresh account snapshots before a new order.
2. Quote both Uniswap directions for the configured amount at the same block.
3. Walk visible depth and calculate profit assuming the entire NevDEX order fills at its worst permitted limit price. Deduct the higher maker/taker fee, slippage, gas budget, and integer rounding reserve.
4. Check available Vault inventory after locked balances, wallet hedge inventory, allowance, and gas reserve. Validate all built-order fields, signed fees and expiry, then recompute the order ID locally.
5. Persist the order ID and committed budget before submitting a crossing limit order with `postOnly:false`. A successful HTTP response acknowledges queue submission; it is not a fill.
6. Scan confirmed Exchange `Match` events, deduplicated by `transactionHash:logIndex`. Hedge each actual partial fill. Account snapshots and disappearing orders do not trigger hedges.
7. Requote and simulate each Router swap. Persist its hash and nonce before broadcast. Clear a fill only after confirmations and verification of the actual ERC-20 transfer deltas.

| Combined route | NevDEX first leg | Uniswap hedge after confirmed fill |
| --- | --- | --- |
| `BUY_UNI_SELL_MYOB` | Sell base with a crossing limit order | Buy the actual filled base using `exactOutputSingle` |
| `BUY_MYOB_SELL_UNI` | Buy base with a crossing limit order | Sell the actual filled base using `exactInputSingle` |

Stale or disconnected market/account feeds pause new orders and trigger cancellation attempts. Confirmed fill scanning and hedging continue independently of market connectivity. An order remains tracked until its signed expiry, an empty fresh account snapshot, a caught-up confirmed scan, and completion of all hedges. Cancellation acknowledgements do not clear the order.

State is stored under `stateDir/<wallet>/state.json` with a deployment identity. Files are mode `600`, directories mode `700`; updates use a temporary file, fsync, atomic rename, and directory fsync. The OS releases the journal lock when the process exits, including after a crash. Keep `rust.lock` in place; its existence alone does not indicate a running process.

An uncertain NevDEX placement, unresolved hedge hash/nonce, failed hedge receipt, unexpected wallet fill, or detected confirmed-block reorganization stops execution and retains the journal. Reconcile the original order/transaction before restarting or replacing it. The implementation does not automatically replace transactions or repair reorganized history. Do not delete state or switch wallets/deployments to bypass unresolved trades.

Execution across venues is not atomic. Price movement and separate gas charges for partial fills can reduce realized profit. Estimated PnL deducts the configured quote-denominated gas budget for each hedge; it is not a conversion of actual native-token gas costs.

## Configuration

Use [arbitrage.example.json](arbitrage.example.json) as the complete template. All fields are required and unknown fields are rejected. Monetary settings are decimal strings. Amounts and intermediate price calculations use arbitrary-precision integers and exact fractions; token amounts must fit `uint256`. API numeric literals retain their original precision.

| Fields | Meaning |
| --- | --- |
| `endpoint`, `rpcURL`, `chainID` | NevDEX API, EVM RPC, expected chain (4663) |
| `market`, `baseToken`, `quoteToken`, `uniswapV3Pool` | Matching NevDEX market and direct V3 pair |
| `vaultAddress`, `exchangeAddress`, `account`, `privateKeyFile` | Live deployment and wallet settings |
| `orderAmount` | Maximum base amount per cycle |
| `minProfitPct`, `minProfitQuote` | Minimum net profit relative to cost and in quote tokens |
| `gasCostQuote`, `slippageBPS` | Estimated gas deduction per hedge and adverse-price bound |
| `maxQuotePerTrade` | Quote cap for the first leg and each hedge |
| `maxTotalQuote` | Persistent cumulative committed budget; cancellations and unfilled orders do not refund it |
| `maxLossQuote` | Cumulative estimated losses; profits do not offset losses |
| `maxFeeRate` | Maximum permitted NevDEX fee |
| `maxGasPriceGwei`, `maxHedgeGasNative` | Hedge gas-price and total native gas limits |
| `confirmations` | Subsequent blocks required for settlement and hedge confirmation |
| `pollMilliseconds` | Processing interval (default 1000 ms) |
| `cancelAfterMilliseconds` | Delay before cancelling remaining order quantity |
| `orderTTLSeconds` | Signed order lifetime, 60–120 seconds |
| `timeoutSeconds` | Per-request network timeout |
| `maxDataAgeSeconds` | Maximum age of chain, market, and account data; default 30 seconds accommodates the server's 20-second pings |
| `stateDir` | Private persistent recovery directory |

Other standalone implementations: [Go](../go-arbitrage/README.md) · [Node.js](../node-arbitrage/README.md).
