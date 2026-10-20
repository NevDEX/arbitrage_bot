# NevDEX x Uniswap V3 Arbitrage Bot — Node.js

Other standalone implementations: [Go](../go-arbitrage/README.md) · [Rust](../rust-arbitrage/README.md).

A standalone JavaScript implementation of the NevDEX and Uniswap V3 arbitrage flow. It uses native ES modules, `ethers`, `decimal.js`, `lossless-json`, and `ws`. No compilation step is required.

The default market is CASHCAT/USDG on Robinhood Chain (4663). The example configuration does not guarantee current liquidity or an arbitrage opportunity.

## Quick start

Requires Node.js 22 or later. Live trading requires macOS or Linux for private-file ownership and permission checks.

```bash
cd node-arbitrage
npm ci
cp arbitrage.example.json arbitrage.json

# Validate the market, chain, pool, factory, Router, and Quoter.
npm run inspect -- --config arbitrage.json

# Show one fresh book and executable quote pair, then exit.
npm run preview -- --config arbitrage.json

# Monitor continuously in read-only mode. Press Ctrl+C to stop.
npm start -- --config arbitrage.json
```

Read-only modes do not read private keys, create recovery directories, sign messages, place or cancel orders, or broadcast transactions. `--check` and `--once` have a 30-second timeout and return a nonzero exit code on failure. Modes are mutually exclusive.

```bash
npm start -- --help
npm run check
```

`npm run check` checks JavaScript syntax. It does not connect to either venue or validate trading behavior.

## Live trading

Use a dedicated wallet exclusively for this bot. Do not run the Go version, another Node.js process, or manual trades with the same wallet at the same time. The Node.js lock protects one Node.js state directory; it does not coordinate with Go or another state directory.

Configure these fields in `arbitrage.json`:

```json
{
  "vaultAddress": "REVIEWED_VAULT_ADDRESS",
  "exchangeAddress": "EXCHANGE_ADDRESS_BOUND_TO_THIS_VAULT",
  "account": "DEDICATED_EOA_ADDRESS",
  "privateKeyFile": "keys/arbitrage.key"
}
```

This is a field example to incorporate into the complete configuration, not usable deployment addresses. Relative file paths resolve from the configuration file's directory.

1. Prefund base and quote inventory in the reviewed NevDEX Vault for the first leg.
2. Separately prefund the wallet with base, quote, and native gas tokens for Uniswap hedges. Grant the official SwapRouter02 a bounded token allowance manually.
3. Store the private key in a file owned by the current user, containing 64 hexadecimal characters with an optional `0x` prefix. Set its permissions to `600`. Keep it outside version control.
4. Confirm there are no existing orders or pending settlements for this wallet and market before the first startup.

```bash
chmod 600 keys/arbitrage.key
npm start -- --config arbitrage.json --live
```

Only `--live` enables real orders, cancellations, and swaps. Startup verifies both `Exchange.vault()` and `Vault.exchange()` bindings. These checks do not replace a contract audit or a required Vault fix and migration.

The bot does not approve tokens, deposit, withdraw, or move inventory between the Vault and the wallet.

## Execution flow

1. Subscribe to market depth and load a REST book snapshot on connection.
2. Request executable Uniswap buy and sell quotes at one chain block for the configured size.
3. Walk the NevDEX book and calculate profit at the worst allowed limit price, after fees, slippage, a rounding reserve, and the configured gas budget.
4. Check inventory, account freshness, and risk limits; validate the complete built order and recompute its hash locally before signing.
5. Save the original order intent before submitting a crossing limit order to NevDEX. An HTTP acknowledgement is not a fill.
6. Scan confirmed Exchange `Match` events. Deduplicate each fill by transaction hash and log index, and hedge actual partial fills separately.
7. Requote, simulate the hedge, and save its hash and nonce before broadcasting. Confirm the receipt and actual token transfer amounts before clearing the fill.

| Combined route | NevDEX first leg | Uniswap hedge after confirmed settlement |
| --- | --- | --- |
| `BUY_UNI_SELL_MYOB` | Sell base with a crossing limit order | Buy back base using `exactOutputSingle` |
| `BUY_MYOB_SELL_UNI` | Buy base with a crossing limit order | Sell base using `exactInputSingle` |

Only one unfinished cycle is allowed per wallet. Disconnected or stale data pauses new orders and triggers cancellation attempts. Confirmed fill scanning and hedge recovery continue while market data is disconnected.

Execution across the two venues is not atomic. Prices can move before a hedge, and each partial fill may incur another gas charge. A quoted full-order profit does not guarantee a realized profit.

## Configuration

Use [arbitrage.example.json](arbitrage.example.json) as the full template. All monetary amounts must be decimal **strings**. API numeric literals are parsed without JavaScript floating-point rounding; token arithmetic uses `BigInt` and high-precision decimals.

| Field | Meaning |
| --- | --- |
| `endpoint`, `rpcURL`, `chainID` | NevDEX API and RPC endpoints; chain 4663 is supported |
| `market`, `baseToken`, `quoteToken`, `uniswapV3Pool` | One published market and its direct V3 pool; change all corresponding fields together |
| `vaultAddress`, `exchangeAddress`, `account`, `privateKeyFile` | Live deployment and dedicated wallet settings |
| `orderAmount` | Maximum base-token quantity per cycle |
| `minProfitPct` | Minimum net profit divided by buy cost; `"0.005"` means 0.5% |
| `minProfitQuote` | Minimum absolute net profit in quote tokens |
| `gasCostQuote` | Manually maintained quote-token gas budget per hedge |
| `maxQuotePerTrade` | Conservative quote commitment cap for each first leg and hedge |
| `maxTotalQuote` | Persistent cumulative commitment budget; cancellations and unfilled orders do not refund it |
| `maxLossQuote` | Cumulative absolute estimated loss cap; profits do not offset losses |
| `maxFeeRate` | Maximum permitted market fee rate |
| `maxGasPriceGwei`, `maxHedgeGasNative` | Gas-price and per-hedge native gas-cost caps |
| `slippageBPS` | Quote bounds; `50` means 0.5% |
| `confirmations` | Subsequent blocks required for fill and hedge confirmations |
| `pollMilliseconds` | Delay between processing steps |
| `cancelAfterMilliseconds` | Delay before requesting cancellation of a remaining first-leg order |
| `orderTTLSeconds` | Signed order lifetime, from 60 to 120 seconds |
| `timeoutSeconds` | Timeout for each HTTP or RPC request |
| `maxDataAgeSeconds` | Maximum age of market/account contact and RPC blocks |
| `stateDir` | Node.js recovery directory, defaulting to `.node-arbitrage-state` |

Gas accounting uses `gasCostQuote`, not an automatically fetched native-token exchange rate. PnL values are estimates in raw quote-token units. Budget and loss caps stop new cycles; already confirmed fills still require hedging.

## Recovery and shutdown

Recovery state is written to `stateDir/<wallet>/state.json` with a temporary file, file synchronization, atomic rename, and directory synchronization. State files use mode `600`, and state directories use mode `700`. No private keys, signatures, or signed raw transactions are stored in recovery state.

- Always restart with the same configuration identity and state directory. The Node.js state format is separate from the Go format. Do not migrate unfinished Go cycles by copying their state files.
- A lost order response preserves the original order ID. A lost hedge broadcast response preserves the original transaction hash and nonce. Recovery does not create a replacement transaction automatically.
- Failed receipts, mismatched actual transfers, unresolved transactions, or chain reorganizations stop execution while retaining exposure records. Reconcile the original orders and transactions before restarting.
- Cancellation is asynchronous. The bot retains the active intent until its signed expiry, account reconciliation, confirmed event scan, and hedging are complete.
- Ctrl+C or SIGTERM stops the streams and attempts cancellation for up to 10 seconds before releasing the lock. Matching and settlement can still occur after shutdown.
- An atomic `node.lock` directory prevents concurrent Node.js use of the same wallet state. A crash leaves this lock in place intentionally. Inspect `node.lock/owner.json` and confirm the recorded process is stopped before removing only `owner.json` and the empty `node.lock` directory. Never delete `state.json` to clear an error, reset budgets, or bypass recovery.

## Project layout

| File | Responsibility |
| --- | --- |
| `src/cli.js` | Arguments, read-only/live modes, timeouts, and signals |
| `src/config.js` | Configuration and market validation |
| `src/engine.js` | Sequential trading loop, inventory checks, order submission, and cycle recovery |
| `src/feeds.js` | Market and account WebSockets, snapshots, freshness, and reconnects |
| `src/planner.js` | Book depth, conservative profit calculations, and per-trade limits |
| `src/orders.js` | Built-order validation, local hashes, and NevDEX's 96-byte signatures |
| `src/uniswap.js` | Deployment checks, quotes, confirmed fills, hedge swaps, and receipts |
| `src/state.js` | Private files, exclusive locks, atomic persistence, and recovery validation |
| `src/transport.js` | Limited HTTP responses, exact JSON parsing, JSON-RPC, and mutation pacing |
| `src/numbers.js` | Integer token units, decimal arithmetic, and structured logs |

Supported scope: standard ERC-20 tokens, one dedicated wallet, one market, and a direct Uniswap V3 pool. Multihop routes, V4 hooks, cross-chain execution, fee-on-transfer/rebasing tokens, automatic inventory rebalancing, transaction replacement, and automatic reorganization compensation are not implemented.

Interface references: the existing [Go implementation](../go-arbitrage/README.md), [official Uniswap deployments for chain 4663](https://github.com/Uniswap/contracts/blob/main/deployments/4663.md), [QuoterV2](https://github.com/Uniswap/v3-periphery/blob/main/contracts/interfaces/IQuoterV2.sol), [SwapRouter02](https://github.com/Uniswap/swap-router-contracts/blob/main/contracts/interfaces/IV3SwapRouter.sol), [ethers documentation](https://docs.ethers.org/v6/), and [Node.js globals](https://nodejs.org/api/globals.html).
