use crate::{
    abi::balancesCall,
    config::Config,
    feeds::Feeds,
    market::Market,
    numbers::*,
    orders::{BuiltOrder, sign_order, validate_built},
    planner::{CrossVenue, Opportunity, Quote, Side, Strategy, StrategyInput},
    state::{Intent, Journal, private_read},
    transport::{Api, Block, Rpc},
    uniswap::Uniswap,
};
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result, ensure};
use num_bigint::BigInt;
use num_traits::Zero;
use serde_json::json;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Default)]
pub struct Options {
    pub check: bool,
    pub once: bool,
    pub live: bool,
}
pub struct Engine<S = CrossVenue> {
    pub config: Config,
    pub api: Api,
    pub rpc: Rpc,
    pub feeds: Feeds,
    pub strategy: S,
    market: Option<Market>,
    uni: Option<Uniswap>,
    wallet: Option<PrivateKeySigner>,
    journal: Option<Journal>,
    workers: Vec<JoinHandle<()>>,
    feed_stop: CancellationToken,
}
impl Engine<CrossVenue> {
    pub fn new(c: Config) -> Result<Self> {
        Self::with_strategy(c, CrossVenue)
    }
}
impl<S: Strategy> Engine<S> {
    pub fn with_strategy(config: Config, strategy: S) -> Result<Self> {
        Ok(Self {
            api: Api::new(&config)?,
            rpc: Rpc::new(&config)?,
            feeds: Feeds::new(&config),
            config,
            strategy,
            market: None,
            uni: None,
            wallet: None,
            journal: None,
            workers: Vec::new(),
            feed_stop: CancellationToken::new(),
        })
    }
    pub async fn run(&mut self, options: Options, stop: CancellationToken) -> Result<()> {
        ensure!(
            [options.check, options.once, options.live]
                .into_iter()
                .filter(|v| *v)
                .count()
                <= 1,
            "Modes are mutually exclusive"
        );
        self.config.validate(options.live)?;
        let deadline = async {
            if options.check || options.once {
                tokio::time::sleep(Duration::from_secs(30)).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        let result = tokio::select! {
            result = self.run_inner(options) => result,
            _ = stop.cancelled() => Ok(()),
            _ = deadline => Err(anyhow::anyhow!("Read-only inspection timed out after 30 seconds")),
        };
        self.feed_stop.cancel();
        for worker in self.workers.drain(..) {
            let _ = worker.await;
        }
        if let Some(active) = self.journal.as_ref().and_then(|j| j.state.active.as_ref()) {
            let owner = self.wallet.as_ref().unwrap().address().to_string();
            if !matches!(
                tokio::time::timeout(
                    Duration::from_secs(10),
                    self.api.cancel(&self.config.market, &owner, &active.id)
                )
                .await,
                Ok(Ok(()))
            ) {
                log(
                    "Shutdown cancellation uncertain",
                    json!({"orderID":active.id}),
                );
            }
            log(
                "Stopped; recovery state retained. Settlement may still be pending",
                json!({"orderID":active.id}),
            );
        }
        self.journal = None;
        self.wallet = None;
        result
    }
    async fn run_inner(&mut self, options: Options) -> Result<()> {
        let market: Market = self
            .api
            .get("getMarket", json!({"marketID":self.config.market}))
            .await?;
        market.validate(&self.config)?;
        let head = self.rpc.head().await?;
        self.uni = Some(
            Uniswap::checked(
                self.config.clone(),
                market.clone(),
                self.rpc.clone(),
                &head,
                options.live,
            )
            .await?,
        );
        self.market = Some(market);
        log(
            "Deployment checked",
            json!({"market":self.config.market,"chainID":self.config.chain_id,"live":options.live}),
        );
        if options.check {
            return Ok(());
        }
        if options.live {
            let bytes = private_read(&self.config.private_key_file, 4096)?;
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| anyhow::anyhow!("Invalid private-key file"))?
                .trim();
            let text = text.strip_prefix("0x").unwrap_or(text);
            ensure!(
                text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()),
                "Invalid private-key file"
            );
            let signer: PrivateKeySigner = text
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid private-key file"))?;
            let owner = signer.address().to_string();
            ensure!(
                self.config.account.is_empty() || same(&self.config.account, &owner),
                "Configured account differs from private-key wallet"
            );
            self.journal = Some(Journal::open(
                &self.config,
                self.market.as_ref().unwrap(),
                &owner,
            )?);
            self.wallet = Some(signer);
            self.spawn_feed(Some(owner));
        }
        self.spawn_feed(None);
        loop {
            ensure!(
                self.workers.iter().all(|w| !w.is_finished()),
                "A stream worker stopped unexpectedly"
            );
            if self.step(options.live).await? && options.once {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(self.config.poll_milliseconds)).await;
        }
    }
    fn spawn_feed(&mut self, owner: Option<String>) {
        let (feeds, c, api, stop) = (
            self.feeds.clone(),
            self.config.clone(),
            self.api.clone(),
            self.feed_stop.clone(),
        );
        self.workers.push(tokio::spawn(async move {
            feeds.watch(c, api, owner, stop).await
        }));
    }
    async fn cancel_intent(&mut self) -> Result<()> {
        let j = self.journal.as_mut().unwrap();
        let Some(o) = j.state.active.as_mut() else {
            return Ok(());
        };
        if now_ms().saturating_sub(o.last_cancel_at) < 5000 {
            return Ok(());
        }
        o.last_cancel_at = now_ms();
        let id = o.id.clone();
        j.save()?;
        let owner = self.wallet.as_ref().unwrap().address().to_string();
        if self
            .api
            .cancel(&self.config.market, &owner, &id)
            .await
            .is_ok()
        {
            log(
                "Cancellation queued; retain intent until expiry and settlement scan",
                json!({"orderID":id}),
            );
        } else {
            log(
                "Cancellation uncertain; settlement scanning will continue",
                json!({"orderID":id}),
            );
        }
        Ok(())
    }
    async fn reconcile(&mut self, head: &Block) -> Result<bool> {
        let snapshot = self.feeds.account();
        if self.journal.as_ref().unwrap().state.cursor_hash.is_empty() {
            let Some(snapshot) = &snapshot else {
                return Ok(false);
            };
            ensure!(
                snapshot.orders.is_empty(),
                "First startup requires no wallet orders or pending settlements"
            );
            let j = self.journal.as_mut().unwrap();
            j.state.cursor = head.number;
            j.state.cursor_hash = head.hash.clone();
            j.save()?;
        }
        let mut cancel = snapshot.is_none() || self.feeds.book().is_none();
        if let Some(snapshot) = &snapshot {
            for row in &snapshot.orders {
                let j = self.journal.as_mut().unwrap();
                let o = j
                    .state
                    .active
                    .as_mut()
                    .context("Untracked wallet order; use a dedicated wallet")?;
                ensure!(
                    same(&row.id, &o.id),
                    "Untracked wallet order; use a dedicated wallet"
                );
                let changed = !o.seen;
                o.seen = true;
                cancel |= decimal(&row.available)? > num_rational::BigRational::zero()
                    && now_ms().saturating_sub(o.created_at)
                        >= self.config.cancel_after_milliseconds;
                if changed {
                    j.save()?;
                }
            }
        }
        if cancel {
            self.cancel_intent().await?;
        }
        let owner = self.wallet.as_ref().unwrap().address();
        let uni = self.uni.as_ref().unwrap();
        let j = self.journal.as_mut().unwrap();
        let (caught_up, confirmed) = uni.scan(j, owner, head).await?;
        if let Some(tx) = j.state.hedge_tx.clone() {
            if uni.settle(j, owner, head).await? {
                log(
                    "Hedge confirmed",
                    json!({"hash":tx.hash,"estimatedPnLRaw":j.state.estimated_pnl_raw,"cumulativeLossRaw":j.state.cumulative_loss_raw}),
                );
            }
            return Ok(false);
        }
        if !j.state.pending.is_empty() {
            uni.submit(j, self.wallet.as_ref().unwrap(), head).await?;
            log(
                "Hedge submitted",
                json!({"fillID":j.state.pending[0].id,"hash":j.state.hedge_tx.as_ref().unwrap().hash}),
            );
            return Ok(false);
        }
        if let Some(o) = &j.state.active {
            ensure!(
                o.seen || now_ms().saturating_sub(o.created_at) <= 30000,
                "Unconfirmed NevDEX submission retained: {}. Inspect original order before restarting",
                o.id
            );
            if !caught_up
                || snapshot.as_ref().is_none_or(|s| !s.orders.is_empty())
                || !o.seen
                || confirmed.timestamp <= o.expires
            {
                return Ok(false);
            }
            log(
                "Arbitrage cycle complete",
                json!({"orderID":o.id,"filledBaseRaw":o.filled_base_raw}),
            );
            j.state.active = None;
            j.save()?;
        }
        if !caught_up || snapshot.as_ref().is_none_or(|s| s.block > confirmed.number) {
            return Ok(false);
        }
        ensure!(
            snapshot.unwrap().orders.is_empty(),
            "Untracked orders prevent a new arbitrage cycle"
        );
        ensure!(
            unsigned(&j.state.cumulative_loss_raw)?
                < units(
                    &self.config.max_loss_quote,
                    self.market.as_ref().unwrap().quote_token_decimals
                )?,
            "Cumulative estimated loss cap reached"
        );
        Ok(true)
    }
    async fn step(&mut self, live: bool) -> Result<bool> {
        let head = self.rpc.head().await?;
        if live && !self.reconcile(&head).await? {
            return Ok(false);
        }
        let Some(book) = self.feeds.book() else {
            return Ok(false);
        };
        let c = &self.config;
        let m = self.market.as_ref().unwrap();
        let uni = self.uni.as_ref().unwrap();
        let base = units(&c.order_amount, m.base_token_decimals)?;
        let started = now_ms();
        uni.liquidity(&head).await?;
        let (sell, buy) = tokio::try_join!(
            uni.quote(Side::Sell, &base, &head),
            uni.quote(Side::Buy, &base, &head)
        )?;
        if self
            .feeds
            .book()
            .is_none_or(|b| b.revision != book.revision)
        {
            return Ok(false);
        }
        let opportunity = self.strategy.evaluate(StrategyInput {
            config: c,
            market: m,
            depth: &book.depth,
            uni_sell: &sell,
            uni_buy: &buy,
            created_at: started,
        })?;
        log(
            "Arbitrage preview",
            json!({"market":c.market,"base":c.order_amount,"uniswapSellQuote":display(&sell,m.quote_token_decimals),"uniswapBuyQuote":display(&buy,m.quote_token_decimals),"opportunity":opportunity.is_some()}),
        );
        if let Some(o) = opportunity {
            log(
                "Opportunity found",
                json!({"route":o.route,"limitPrice":o.quote.price,"estimatedNetQuote":display(&o.net,m.quote_token_decimals)}),
            );
            if live {
                self.place(o, &head, &base, &buy, book.revision).await?;
            }
        }
        Ok(true)
    }
    async fn vault_funds(&self, q: &Quote, head: &Block) -> Result<()> {
        let c = &self.config;
        let m = self.market.as_ref().unwrap();
        let owner = self.wallet.as_ref().unwrap().address();
        let locks = self
            .api
            .call("lockedBalance", json!({"address":owner}), true)
            .await?;
        let latest = self.rpc.quantity("eth_blockNumber", json!([])).await?;
        ensure!(latest >= head.number, "RPC head behind settlement");
        let symbols: Vec<_> = c.market.split('-').collect();
        let mut free = Vec::new();
        for (i, token) in [&c.base_token, &c.quote_token].iter().enumerate() {
            let total = bigint(
                self.rpc
                    .read(
                        address(&c.vault_address)?,
                        balancesCall {
                            token: address(token)?,
                            owner,
                        },
                        &format!("0x{latest:x}"),
                    )
                    .await?,
            );
            let locked = unsigned(
                locks[symbols[i]]
                    .as_str()
                    .context("Missing integer locked balance")?,
            )?;
            free.push((total - locked).max(BigInt::zero()));
        }
        let gross = notional(&q.price, &q.amount, m.quote_token_decimals)?;
        ensure!(
            if q.side.is_buy() {
                free[1] >= &gross + m.fee(&gross)?
            } else {
                free[0] >= units(&q.amount, m.base_token_decimals)?
            },
            "Insufficient available Vault inventory"
        );
        Ok(())
    }
    async fn place(
        &mut self,
        o: Opportunity,
        head: &Block,
        base: &BigInt,
        uni_buy: &BigInt,
        revision: u64,
    ) -> Result<()> {
        let c = &self.config;
        let m = self.market.as_ref().unwrap();
        let owner = self.wallet.as_ref().unwrap().address();
        let gross = notional(&o.quote.price, &o.quote.amount, m.quote_token_decimals)?;
        let order_cap = &gross + m.fee(&gross)?;
        let required = if o.quote.side == Side::Sell {
            order_cap.max(slip(uni_buy, c.slippage_bps, true))
        } else {
            order_cap
        };
        let cap = units(&c.max_quote_per_trade, m.quote_token_decimals)?;
        ensure!(
            gross > BigInt::zero()
                && o.commitment >= required
                && o.commitment <= cap
                && o.proceeds <= cap,
            "Strategy opportunity violates per-trade risk limits"
        );
        let commitment =
            unsigned(&self.journal.as_ref().unwrap().state.committed_quote_raw)? + &o.commitment;
        ensure!(
            commitment <= units(&c.max_total_quote, m.quote_token_decimals)?,
            "Persistent total quote budget exhausted"
        );
        ensure!(
            decimal(&o.quote.amount)? == decimal(&c.order_amount)?,
            "Strategy order size differs from configured risk size"
        );
        self.uni
            .as_ref()
            .unwrap()
            .funds(
                owner,
                o.quote.side.opposite(),
                base,
                &slip(uni_buy, c.slippage_bps, true),
                head,
            )
            .await?;
        self.vault_funds(&o.quote, head).await?;
        let built:BuiltOrder = self.api.get("order/build",json!({"marketID":c.market,"address":owner,"side":o.quote.side,"orderType":"limit","postOnly":false,"price":o.quote.price,"amount":o.quote.amount,"expires":c.order_ttl_seconds})).await?;
        let expires = validate_built(&built, &o.quote, c, m, &owner.to_string(), head.timestamp)?;
        if self.feeds.book().is_none_or(|b| b.revision != revision)
            || now_ms().saturating_sub(o.created_at) > c.max_data_age_seconds * 1000
            || now_ms() / 1000 > head.timestamp + c.max_data_age_seconds
            || self
                .feeds
                .account()
                .is_none_or(|s| !s.orders.is_empty() || s.block > head.number)
        {
            log(
                "Opportunity or account changed during build; skipping",
                json!({}),
            );
            return Ok(());
        }
        let signature = sign_order(self.wallet.as_ref().unwrap(), &built.id)?;
        let j = self.journal.as_mut().unwrap();
        ensure!(
            j.state.active.is_none() && j.state.pending.is_empty() && j.state.hedge_tx.is_none(),
            "Previous cycle is unfinished"
        );
        j.state.active = Some(Intent {
            id: built.id.clone(),
            relayer: built.json.relayer,
            quote: o.quote,
            expires,
            created_at: now_ms(),
            seen: false,
            last_cancel_at: 0,
            filled_base_raw: "0".into(),
        });
        j.state.committed_quote_raw = commitment.to_string();
        j.state.validate(m)?;
        j.save()?;
        ensure!(
            self.api
                .call(
                    "order/place",
                    json!({"address":owner,"orderID":built.id,"signature":signature}),
                    false
                )
                .await
                .is_ok(),
            "NevDEX placement uncertain: {}. Retain original ID and inspect before restarting",
            built.id
        );
        log("Order queued", json!({"orderID":built.id}));
        Ok(())
    }
}
