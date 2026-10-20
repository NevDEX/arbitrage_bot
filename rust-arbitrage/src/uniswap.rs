use crate::{
    abi::*,
    config::Config,
    market::Market,
    numbers::*,
    planner::Side,
    state::{Fill, HedgeTx, Intent, Journal},
    transport::{Block, Rpc},
};
use alloy::{
    consensus::{SignableTransaction, TxLegacy},
    eips::eip2718::Encodable2718,
    primitives::{
        Address, B256, Bytes, TxKind, U256,
        aliases::{U24, U160},
        keccak256,
    },
    signers::{SignerSync, local::PrivateKeySigner},
    sol_types::{SolCall, SolEvent},
};
use anyhow::{Context, Result, ensure};
use num_bigint::BigInt;
use num_traits::{ToPrimitive, Zero};
use serde::Deserialize;
use serde_json::{Value, json};

pub const FACTORY: &str = "0x1f7d7550b1b028f7571e69a784071f0205fd2efa";
pub const ROUTER: &str = "0xcaf681a66d020601342297493863e78c959e5cb2";
pub const QUOTER: &str = "0x33e885ed0ec9bf04ecfb19341582aadcb4c8a9e7";

#[derive(Clone)]
pub struct Uniswap {
    pub config: Config,
    pub market: Market,
    pub rpc: Rpc,
    pub pool_fee: U24,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainLog {
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Bytes,
    pub transaction_hash: String,
    pub log_index: String,
    pub block_number: String,
    pub block_hash: String,
    #[serde(default)]
    pub removed: bool,
}
impl Uniswap {
    pub async fn checked(c: Config, m: Market, rpc: Rpc, head: &Block, live: bool) -> Result<Self> {
        ensure!(
            rpc.quantity("eth_chainId", json!([])).await? == c.chain_id,
            "RPC chain mismatch"
        );
        let pool = address(&c.uniswap_v3_pool)?;
        let t0 = rpc.read(pool, token0Call {}, &head.tag).await?;
        let t1 = rpc.read(pool, token1Call {}, &head.tag).await?;
        let base = address(&c.base_token)?;
        let quote = address(&c.quote_token)?;
        ensure!(
            (t0 == base && t1 == quote) || (t0 == quote && t1 == base),
            "Pool tokens differ from market"
        );
        let factory = address(FACTORY)?;
        ensure!(
            rpc.read(pool, factoryCall {}, &head.tag).await? == factory,
            "Pool factory mismatch"
        );
        let pool_fee = rpc.read(pool, feeCall {}, &head.tag).await?;
        ensure!(
            rpc.read(
                factory,
                getPoolCall {
                    tokenA: t0,
                    tokenB: t1,
                    fee: pool_fee
                },
                &head.tag
            )
            .await?
                == pool,
            "Pool not registered in official factory"
        );
        for (token, expected) in [
            (base, m.base_token_decimals),
            (quote, m.quote_token_decimals),
        ] {
            ensure!(
                rpc.read(token, decimalsCall {}, &head.tag).await? as u32 == expected,
                "Token decimals mismatch"
            );
        }
        for contract in [ROUTER, QUOTER] {
            ensure!(
                rpc.read(address(contract)?, factoryCall {}, &head.tag)
                    .await?
                    == factory,
                "Router or Quoter factory mismatch"
            );
        }
        if live {
            let vault = address(&c.vault_address)?;
            let exchange = address(&c.exchange_address)?;
            let code = rpc.call("eth_getCode", json!([vault, head.tag])).await?;
            let code: Bytes = serde_json::from_value(code).context("Invalid Vault runtime code")?;
            ensure!(!code.is_empty(), "Vault has no runtime code");
            ensure!(
                rpc.read(exchange, vaultCall {}, &head.tag).await? == vault
                    && rpc.read(vault, exchangeCall {}, &head.tag).await? == exchange,
                "Vault and Exchange bindings mismatch"
            );
        }
        let uni = Self {
            config: c,
            market: m,
            rpc,
            pool_fee,
        };
        uni.liquidity(head).await?;
        Ok(uni)
    }
    pub async fn liquidity(&self, head: &Block) -> Result<()> {
        let pool = address(&self.config.uniswap_v3_pool)?;
        let state = self.rpc.read(pool, slot0Call {}, &head.tag).await?;
        ensure!(
            state.sqrtPriceX96 > U160::ZERO
                && state.unlocked
                && self.rpc.read(pool, liquidityCall {}, &head.tag).await? > 0,
            "Pool locked or no active liquidity"
        );
        Ok(())
    }
    pub async fn quote(&self, side: Side, base: &BigInt, head: &Block) -> Result<BigInt> {
        let c = &self.config;
        let params = QuoteParams {
            tokenIn: address(if side.is_buy() {
                &c.quote_token
            } else {
                &c.base_token
            })?,
            tokenOut: address(if side.is_buy() {
                &c.base_token
            } else {
                &c.quote_token
            })?,
            amount: uint(base)?,
            fee: self.pool_fee,
            sqrtPriceLimitX96: U160::ZERO,
        };
        let (amount, sqrt) = if side.is_buy() {
            let r = self
                .rpc
                .read(
                    address(QUOTER)?,
                    quoteExactOutputSingleCall { params },
                    &head.tag,
                )
                .await?;
            (r.amountIn, U256::from(r.sqrtPriceX96After))
        } else {
            let r = self
                .rpc
                .read(
                    address(QUOTER)?,
                    quoteExactInputSingleCall { params },
                    &head.tag,
                )
                .await?;
            (r.amountOut, U256::from(r.sqrtPriceX96After))
        };
        ensure!(
            amount > U256::ZERO
                && sqrt > U256::from(4295128740u64)
                && sqrt < "1461446703485210103287273052203988822378723970341".parse::<U256>()?,
            "No full-size quote liquidity"
        );
        Ok(bigint(amount))
    }
    pub async fn funds(
        &self,
        owner: Address,
        side: Side,
        base: &BigInt,
        quote: &BigInt,
        head: &Block,
    ) -> Result<()> {
        let token = address(if side.is_buy() {
            &self.config.quote_token
        } else {
            &self.config.base_token
        })?;
        let required = uint(if side.is_buy() { quote } else { base })?;
        ensure!(
            self.rpc
                .read(token, balanceOfCall { owner }, &head.tag)
                .await?
                >= required
                && self
                    .rpc
                    .read(
                        token,
                        allowanceCall {
                            owner,
                            spender: address(ROUTER)?
                        },
                        &head.tag
                    )
                    .await?
                    >= required,
            "Insufficient hedge balance or Router allowance"
        );
        ensure!(
            self.rpc
                .amount("eth_getBalance", json!([owner, head.tag]))
                .await?
                >= units(&self.config.max_hedge_gas_native, 18)?,
            "Insufficient native gas reserve"
        );
        Ok(())
    }
    pub fn decode_fill(
        &self,
        log: &ChainLog,
        owner: Address,
        active: Option<&Intent>,
    ) -> Result<Option<Fill>> {
        let event = Match::decode_raw_log_validate(log.topics.iter().copied(), &log.data)
            .context("Invalid Match event")?;
        let a = event.addressSet;
        let r = event.result;
        if a.baseToken != address(&self.config.base_token)?
            || a.quoteToken != address(&self.config.quote_token)?
            || (r.maker != owner && r.taker != owner)
        {
            return Ok(None);
        }
        let o = active.context("Unexpected wallet fill without active order")?;
        ensure!(
            a.relayer == address(&o.relayer)?
                && r.maker != r.taker
                && (r.buyer == r.maker || r.buyer == r.taker),
            "Unexpected wallet fill identity; use an exclusive wallet"
        );
        let buy = r.buyer == owner;
        ensure!(
            buy == o.quote.side.is_buy()
                && r.baseTokenFilledAmount > U256::ZERO
                && r.quoteTokenFilledAmount > U256::ZERO,
            "Fill differs from active order direction"
        );
        let fee = if r.maker == owner {
            bigint(r.makerFee) + bigint(r.makerGasFee) - bigint(r.makerRebate)
        } else {
            bigint(r.takerFee) + bigint(r.takerGasFee)
        };
        let quote = if buy {
            -(bigint(r.quoteTokenFilledAmount) + fee)
        } else {
            bigint(r.quoteTokenFilledAmount) - fee
        };
        ensure!(
            if buy {
                quote < 0.into()
            } else {
                quote > 0.into()
            },
            "Invalid settlement quote delta"
        );
        Ok(Some(Fill {
            id: format!(
                "{}:{}",
                log.transaction_hash.to_lowercase(),
                hex_u64(&log.log_index)?
            ),
            hedge_side: if buy { Side::Sell } else { Side::Buy },
            base_raw: bigint(r.baseTokenFilledAmount).to_string(),
            quote_delta_raw: quote.to_string(),
            block: hex_u64(&log.block_number)?,
            block_hash: log.block_hash.clone(),
        }))
    }
    pub async fn scan(
        &self,
        j: &mut Journal,
        owner: Address,
        head: &Block,
    ) -> Result<(bool, Block)> {
        let cursor = j.state.cursor;
        let previous = self.rpc.block(&format!("0x{cursor:x}")).await?;
        ensure!(
            same(&previous.hash, &j.state.cursor_hash),
            "Settlement cursor reorganization; reconcile before restarting"
        );
        ensure!(
            head.number >= cursor,
            "RPC head regressed behind settlement cursor"
        );
        let target = head.number.saturating_sub(self.config.confirmations);
        if target <= cursor {
            return Ok((true, previous));
        }
        let end = target.min(cursor.saturating_add(500));
        let last = self.rpc.block(&format!("0x{end:x}")).await?;
        let logs = self.rpc.call("eth_getLogs",json!([{"address":self.config.exchange_address,"fromBlock":format!("0x{:x}",cursor+1),"toBlock":last.tag,"topics":[Match::SIGNATURE_HASH]}])).await?;
        let mut logs: Vec<ChainLog> =
            serde_json::from_value(logs).context("Invalid settlement log list")?;
        ensure!(logs.len() <= 100000, "Too many settlement logs");
        for l in &logs {
            let block = hex_u64(&l.block_number)?;
            hex_u64(&l.log_index)?;
            ensure!(
                !l.removed
                    && l.address == address(&self.config.exchange_address)?
                    && block > cursor
                    && block <= end
                    && l.topics == [Match::SIGNATURE_HASH],
                "Invalid settlement log identity"
            );
            hash(&l.transaction_hash)?;
            hash(&l.block_hash)?;
        }
        logs.sort_by_key(|l| {
            (
                hex_u64(&l.block_number).unwrap(),
                hex_u64(&l.log_index).unwrap(),
            )
        });
        let mut next = j.state.clone();
        let mut seen: std::collections::HashSet<_> =
            next.pending.iter().map(|f| f.id.clone()).collect();
        for log in &logs {
            let Some(fill) = self.decode_fill(log, owner, next.active.as_ref())? else {
                continue;
            };
            if !seen.insert(fill.id.clone()) {
                continue;
            }
            ensure!(
                same(
                    &self.rpc.block(&log.block_number).await?.hash,
                    &log.block_hash
                ),
                "Settlement event reorganization"
            );
            let active = next.active.as_mut().unwrap();
            let filled = unsigned(&active.filled_base_raw)? + unsigned(&fill.base_raw)?;
            ensure!(
                filled <= units(&active.quote.amount, self.market.base_token_decimals)?,
                "Settled fills exceed persisted order amount"
            );
            active.filled_base_raw = filled.to_string();
            active.seen = true;
            next.pending.push(fill);
            ensure!(
                next.pending.len() <= 100,
                "Unhedged fill queue exceeds limit"
            );
        }
        ensure!(
            same(&self.rpc.block(&last.tag).await?.hash, &last.hash),
            "Scan block changed during read"
        );
        next.cursor = end;
        next.cursor_hash = last.hash.clone();
        next.validate(&self.market)?;
        j.state = next;
        j.save()?;
        Ok((end == target, last))
    }
    pub async fn submit(
        &self,
        j: &mut Journal,
        signer: &PrivateKeySigner,
        head: &Block,
    ) -> Result<()> {
        ensure!(
            j.state.hedge_tx.is_none(),
            "Existing hedge requires reconciliation"
        );
        let f = j.state.pending.first().context("No fill to hedge")?;
        let base = unsigned(&f.base_raw)?;
        let price = self.quote(f.hedge_side, &base, head).await?;
        let bound = slip(&price, self.config.slippage_bps, f.hedge_side.is_buy());
        let cap = units(
            &self.config.max_quote_per_trade,
            self.market.quote_token_decimals,
        )?;
        ensure!(
            price <= cap && bound <= cap && bound > BigInt::zero(),
            "Hedge quote exceeds per-trade cap"
        );
        self.funds(signer.address(), f.hedge_side, &base, &bound, head)
            .await?;
        let params = SwapParams {
            tokenIn: address(if f.hedge_side.is_buy() {
                &self.config.quote_token
            } else {
                &self.config.base_token
            })?,
            tokenOut: address(if f.hedge_side.is_buy() {
                &self.config.base_token
            } else {
                &self.config.quote_token
            })?,
            fee: self.pool_fee,
            recipient: signer.address(),
            amount: uint(&base)?,
            bound: uint(&bound)?,
            sqrtPriceLimitX96: U160::ZERO,
        };
        let swap = if f.hedge_side.is_buy() {
            exactOutputSingleCall { params }.abi_encode()
        } else {
            exactInputSingleCall { params }.abi_encode()
        };
        let deadline = head.timestamp + 60;
        let data = Bytes::from(
            multicallCall {
                deadline: U256::from(deadline),
                data: vec![Bytes::from(swap)],
            }
            .abi_encode(),
        );
        let call = json!({"from":signer.address(),"to":ROUTER,"data":data,"value":"0x0"});
        self.rpc.call("eth_call", json!([call, head.tag])).await?;
        let estimate = self.rpc.quantity("eth_estimateGas", json!([call])).await?;
        ensure!(
            (1..=833333).contains(&estimate),
            "Hedge gas estimate exceeds hard cap"
        );
        let gas_limit = estimate * 120 / 100 + 1;
        let gas_price = self.rpc.amount("eth_gasPrice", json!([])).await?;
        ensure!(
            gas_price > BigInt::zero()
                && gas_price <= units(&self.config.max_gas_price_gwei, 9)?
                && &gas_price * gas_limit <= units(&self.config.max_hedge_gas_native, 18)?,
            "Hedge gas exceeds configured caps"
        );
        let nonce = self
            .rpc
            .quantity(
                "eth_getTransactionCount",
                json!([signer.address(), "pending"]),
            )
            .await?;
        ensure!(
            now_ms() / 1000 <= head.timestamp + self.config.max_data_age_seconds,
            "Hedge quote became stale"
        );
        let tx = TxLegacy {
            chain_id: Some(self.config.chain_id),
            nonce,
            gas_price: gas_price.to_u128().context("Gas price exceeds uint128")?,
            gas_limit,
            to: TxKind::Call(address(ROUTER)?),
            value: U256::ZERO,
            input: data,
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash())?;
        let signed = tx.into_signed(signature);
        let bytes = Bytes::from(signed.encoded_2718());
        let tx_hash = keccak256(&bytes).to_string();
        j.state.hedge_tx = Some(HedgeTx {
            hash: tx_hash.clone(),
            nonce,
            deadline,
            fill_id: f.id.clone(),
            bound_raw: bound.to_string(),
            gas_budget_raw: units(
                &self.config.gas_cost_quote,
                self.market.quote_token_decimals,
            )?
            .to_string(),
        });
        j.save()?;
        let result = self
            .rpc
            .call("eth_sendRawTransaction", json!([bytes]))
            .await;
        ensure!(
            result
                .as_ref()
                .ok()
                .and_then(Value::as_str)
                .is_some_and(|h| same(h, &tx_hash)),
            "Hedge broadcast uncertain: {tx_hash}. Reconcile original hash and nonce before replacement"
        );
        Ok(())
    }
    pub fn transfer_delta(receipt: &Value, token: Address, owner: Address) -> Result<BigInt> {
        let mut delta = BigInt::zero();
        for log in receipt["logs"]
            .as_array()
            .context("Missing receipt transfer logs")?
        {
            if !log["address"]
                .as_str()
                .is_some_and(|s| same(s, &token.to_string()))
                || !log["topics"][0]
                    .as_str()
                    .is_some_and(|s| same(s, &Transfer::SIGNATURE_HASH.to_string()))
            {
                continue;
            }
            let topics: Vec<B256> =
                serde_json::from_value(log["topics"].clone()).context("Invalid Transfer topics")?;
            let data: Bytes =
                serde_json::from_value(log["data"].clone()).context("Invalid Transfer data")?;
            ensure!(
                topics.len() == 3 && data.len() == 32,
                "Malformed Transfer event"
            );
            let event = Transfer::decode_raw_log_validate(topics, &data)?;
            if event.to == owner {
                delta += bigint(event.value);
            }
            if event.from == owner {
                delta -= bigint(event.value);
            }
        }
        Ok(delta)
    }
    pub async fn settle(&self, j: &mut Journal, owner: Address, head: &Block) -> Result<bool> {
        let t = j.state.hedge_tx.as_ref().context("Missing hedge intent")?;
        let receipt = self
            .rpc
            .call("eth_getTransactionReceipt", json!([t.hash]))
            .await?;
        if receipt.is_null() {
            ensure!(
                head.timestamp <= t.deadline + 60,
                "Hedge remains unresolved: {}. Inspect original nonce {}",
                t.hash,
                t.nonce
            );
            return Ok(false);
        }
        ensure!(
            receipt["transactionHash"]
                .as_str()
                .is_some_and(|h| same(h, &t.hash))
                && receipt["status"] == "0x1",
            "Hedge failed or receipt mismatched; fill retained"
        );
        let block = self
            .rpc
            .block(
                receipt["blockNumber"]
                    .as_str()
                    .context("Missing receipt block")?,
            )
            .await?;
        ensure!(
            receipt["blockHash"]
                .as_str()
                .is_some_and(|h| same(h, &block.hash)),
            "Hedge receipt reorganization"
        );
        if head.number < block.number + self.config.confirmations {
            return Ok(false);
        }
        let f = j
            .state
            .pending
            .first()
            .context("Missing pending hedge fill")?;
        ensure!(t.fill_id == f.id, "Hedge fill identity mismatch");
        let base = Self::transfer_delta(&receipt, address(&self.config.base_token)?, owner)?;
        let quote = Self::transfer_delta(&receipt, address(&self.config.quote_token)?, owner)?;
        let amount = unsigned(&f.base_raw)?;
        ensure!(
            base == if f.hedge_side == Side::Sell {
                -amount
            } else {
                amount
            },
            "Hedge base transfers differ from intent; reconcile residual exposure"
        );
        let bound = unsigned(&t.bound_raw)?;
        ensure!(
            if f.hedge_side == Side::Sell {
                quote >= bound
            } else {
                quote < BigInt::zero() && -&quote <= bound
            },
            "Hedge quote transfers violate bound"
        );
        let pnl = raw(&f.quote_delta_raw)? + quote - unsigned(&t.gas_budget_raw)?;
        let mut next = j.state.clone();
        next.estimated_pnl_raw = (raw(&next.estimated_pnl_raw)? + &pnl).to_string();
        if pnl < BigInt::zero() {
            next.cumulative_loss_raw = (unsigned(&next.cumulative_loss_raw)? - pnl).to_string();
        }
        next.pending.remove(0);
        next.hedge_tx = None;
        next.validate(&self.market)?;
        j.state = next;
        j.save()?;
        Ok(true)
    }
}
