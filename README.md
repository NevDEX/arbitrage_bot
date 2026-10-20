# NevDEX × Uniswap V3 Arbitrage Examples

Standalone examples for CASHCAT/USDG on Robinhood Chain (4663), with read-only monitoring by default and an explicit live-trading mode.

| Implementation | Runtime | Entry point |
| --- | --- | --- |
| [Rust](rust-arbitrage/README.md) | Rust 1.91+ | Client library, replaceable `Strategy` interface, and CLI |
| [Go](go-arbitrage/README.md) | Go 1.23+ | Standalone bot |
| [Node.js](node-arbitrage/README.md) | Node.js 22+ | JavaScript modules and CLI |

Each project includes its own configuration template and setup instructions. The execution flow places a NevDEX order first, tracks confirmed on-chain fills, and hedges the actual filled amount on Uniswap V3. Submission acknowledgements are separate from fills.

Use a dedicated wallet for one running implementation. The implementations have separate recovery formats and do not share process locks. Review the selected project's configuration, inventory requirements, and recovery instructions before enabling live trading.
