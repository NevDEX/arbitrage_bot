import { Wallet } from 'ethers';
import { setTimeout as sleep } from 'node:timers/promises';
import { API, RPC } from './transport.js';
import { validateMarket } from './config.js';
import { MarketFeed, AccountFeed } from './feeds.js';
import { Journal, privateRead } from './state.js';
import { Uniswap } from './uniswap.js';
import { plan } from './planner.js';
import { validateBuilt, signOrder } from './orders.js';
import { dec, display, integer, log, same, units, slip, fee } from './numbers.js';

export class Engine {
  constructor(config, options) {
    this.c = config;
    this.options = options;
    this.api = new API(config);
    this.rpc = new RPC(config);
    this.book = new MarketFeed(config, this.api);
  }

  async openWallet() {
    const raw = await privateRead(this.c.privateKeyFile, 4096);
    try {
      const key = raw.toString('utf8').trim().replace(/^0x/, '');
      if (!/^[\da-f]{64}$/i.test(key)) throw new Error();
      this.wallet = new Wallet(`0x${key}`);
    } catch { throw new Error('Invalid private-key file.'); }
    finally { raw.fill(0); }
    this.address = this.wallet.address;
    if (this.c.account && !same(this.c.account, this.address)) throw new Error('Configured account differs from the private-key wallet.');
    this.journal = await Journal.open(this.c, this.address);
    this.account = new AccountFeed(this.c, this.address);
  }

  async run(parentSignal) {
    const control = new AbortController();
    const signal = AbortSignal.any([parentSignal, control.signal]);
    const workers = [];
    let workerError;
    const background = (promise) => workers.push(promise.catch(() => {
      if (!signal.aborted) { workerError = new Error('A market stream stopped unexpectedly.'); control.abort(workerError); }
    }));
    try {
      this.market = validateMarket(await this.api.call('getMarket', { marketID: this.c.market }, signal), this.c);
      const head = await this.rpc.head(signal);
      this.uni = new Uniswap(this.c, this.market, this.rpc);
      await this.uni.check(head, this.options.live, signal);
      log('Deployment checked.', { market: this.c.market, chainID: this.c.chainID, live: this.options.live });
      if (this.options.check) return;
      if (this.options.live) await this.openWallet();
      background(this.book.start(signal));
      if (this.options.live) background(this.account.start(signal));
      while (!signal.aborted) {
        const previewed = await this.step(signal);
        if (this.options.once && previewed) return;
        await sleep(this.c.pollMilliseconds, undefined, { signal });
      }
      signal.throwIfAborted();
    } catch (error) {
      if (workerError) throw workerError;
      throw error;
    } finally {
      control.abort();
      await Promise.allSettled(workers);
      try { await this.shutdown(); }
      finally { await this.journal?.close(); }
      this.wallet = null;
    }
  }

  async cancelIntent(signal) {
    const o = this.journal.state.active;
    if (!o || Date.now() - o.lastCancelAt < 5000) return;
    o.lastCancelAt = Date.now();
    await this.journal.save();
    try {
      await this.api.cancel(this.address, o.id, signal);
      log('Cancellation queued; intent retained until expiry and confirmed settlement scan.', { orderID: o.id });
    } catch { log('Cancellation uncertain; settlement scanning will continue.', { orderID: o.id }); }
  }

  async reconcile(head, signal) {
    const j = this.journal;
    const snapshot = this.account.snapshot();
    if (!j.state.cursorHash) {
      if (!snapshot) return false;
      if (snapshot.orders.length) throw new Error('First startup requires no existing wallet orders or pending settlements.');
      j.state.cursor = head.number.toString();
      j.state.cursorHash = head.hash;
      await j.save();
    }
    if (snapshot) {
      for (const row of snapshot.orders) {
        const o = j.state.active;
        if (!o || !same(row.Id, o.id)) throw new Error('Untracked wallet order; use a dedicated wallet.');
        if (!o.seen) { o.seen = true; await j.save(); }
        if (dec(row.AvailableAmount).gt(0) && (!this.book.fresh() || Date.now() - o.createdAt >= this.c.cancelAfterMilliseconds)) await this.cancelIntent(signal);
      }
    } else {
      await this.cancelIntent(signal);
    }
    const { caughtUp, confirmed } = await this.uni.scan(j, this.address, head, signal);
    if (j.state.hedgeTx) {
      const hash = j.state.hedgeTx.hash;
      await this.uni.settle(j, this.address, head, signal);
      if (!j.state.hedgeTx) log('Hedge confirmed.', { hash, estimatedPnLRaw: j.state.estimatedPnLRaw, cumulativeLossRaw: j.state.cumulativeLossRaw });
      return false;
    }
    if (j.state.pending.length) {
      await this.uni.submit(j, this.wallet, head, signal);
      log('Hedge submitted.', { fillID: j.state.pending[0].id, hash: j.state.hedgeTx.hash });
      return false;
    }
    const o = j.state.active;
    if (o) {
      if (!o.seen && Date.now() - o.createdAt > 30000) throw new Error(`Unconfirmed NevDEX submission retained: ${o.id}. Inspect the original order before restarting.`);
      if (!caughtUp || !snapshot || snapshot.orders.length || !o.seen || confirmed.timestamp <= BigInt(o.expires)) return false;
      log('Arbitrage cycle complete.', { orderID: o.id, filledBaseRaw: o.filledBaseRaw });
      j.state.active = null;
      await j.save();
    }
    if (!snapshot || !caughtUp || snapshot.block > confirmed.number) return false;
    if (snapshot.orders.length) throw new Error('Untracked wallet orders prevent a new arbitrage cycle.');
    if (integer(j.state.cumulativeLossRaw) >= units(this.c.maxLossQuote, this.market.quoteTokenDecimals)) throw new Error('Cumulative estimated loss limit reached; no new orders.');
    return true;
  }

  async step(signal) {
    const head = await this.rpc.head(signal);
    if (this.options.live && !await this.reconcile(head, signal)) return false;
    if (!this.book.fresh()) return false;
    const revision = this.book.revision;
    const depth = this.book.depth;
    const base = units(this.c.orderAmount, this.market.baseTokenDecimals);
    const quoteStarted = Date.now();
    await this.uni.liquidity(head, signal);
    // Both executable quotes use the same chain block.
    const uniSell = await this.uni.quote('sell', base, head, signal);
    const uniBuy = await this.uni.quote('buy', base, head, signal);
    if (!this.book.fresh() || revision !== this.book.revision) return false;
    const opportunity = plan(this.c, this.market, depth, uniSell, uniBuy, quoteStarted);
    log('Arbitrage preview.', {
      market: this.c.market, base: this.c.orderAmount,
      uniswapSellQuote: display(uniSell, this.market.quoteTokenDecimals),
      uniswapBuyQuote: display(uniBuy, this.market.quoteTokenDecimals), opportunity: Boolean(opportunity),
    });
    if (opportunity) log('Opportunity found.', { route: opportunity.route, limitPrice: opportunity.quote.price, estimatedNetQuote: display(opportunity.net, this.market.quoteTokenDecimals) });
    if (this.options.live && opportunity) await this.place(opportunity, head, base, uniBuy, revision, signal);
    return true;
  }

  async vaultFunds(quote, head, signal) {
    const c = this.c;
    const locks = await this.api.call('lockedBalance', { address: this.address }, signal);
    const latest = BigInt(await this.rpc.call('eth_blockNumber', [], signal));
    if (latest < head.number) throw new Error('RPC head is behind settlement.');
    const free = [];
    const symbols = c.market.split('-');
    for (const [i, token] of [c.baseToken, c.quoteToken].entries()) {
      const [total] = await this.rpc.read(c.vaultAddress, 'balances(address,address) view returns (uint256)',
        [token.toLowerCase(), this.address], hex(latest), signal);
      const locked = integer(locks?.[symbols[i]], 'locked token balance');
      free.push(total > locked ? total - locked : 0n);
    }
    const base = units(quote.amount, this.market.baseTokenDecimals);
    const gross = BigInt(dec(quote.amount).mul(quote.price).mul(dec(10).pow(this.market.quoteTokenDecimals)).trunc().toFixed(0));
    if (quote.side === 'buy' ? free[1] < gross + fee(gross, this.market) : free[0] < base) throw new Error('Insufficient available Vault inventory.');
  }

  async place(o, head, base, uniBuy, revision, signal) {
    const c = this.c;
    const j = this.journal;
    const commitment = integer(j.state.committedQuoteRaw) + o.commitment;
    if (commitment > units(c.maxTotalQuote, this.market.quoteTokenDecimals)) throw new Error('Persistent total quote budget exhausted.');
    await this.uni.funds(this.address, o.quote.side === 'sell' ? 'buy' : 'sell', base, slip(uniBuy, c.slippageBPS, true), head, signal);
    await this.vaultFunds(o.quote, head, signal);
    const built = await this.api.call('order/build', {
      marketID: c.market, address: this.address, side: o.quote.side, orderType: 'limit', postOnly: false,
      price: o.quote.price, amount: o.quote.amount, expires: c.orderTTLSeconds,
    }, signal);
    const expires = validateBuilt(built, o.quote, c, this.market, this.address, head.timestamp);
    const snapshot = this.account.snapshot();
    if (!this.book.fresh() || revision !== this.book.revision || Date.now() - o.createdAt > c.maxDataAgeSeconds * 1000 ||
      BigInt(Math.floor(Date.now() / 1000)) - head.timestamp > BigInt(c.maxDataAgeSeconds) ||
      !snapshot || snapshot.orders.length || snapshot.block > head.number) {
      log('Opportunity or account changed during order build; skipping submission.');
      return;
    }
    signal.throwIfAborted();
    const signature = await signOrder(this.wallet, built.id);
    j.state.active = { id: built.id, relayer: built.json.relayer, quote: o.quote, expires,
      createdAt: Date.now(), seen: false, lastCancelAt: 0, filledBaseRaw: '0' };
    j.state.committedQuoteRaw = commitment.toString();
    await j.save();
    try { await this.api.call('order/place', { address: this.address, orderID: built.id, signature }, signal, false); }
    catch { throw new Error(`NevDEX placement uncertain: ${built.id}. Retain the original ID and inspect before restarting.`); }
    log('Order queued.', { orderID: built.id, ...o.quote });
  }

  async shutdown() {
    const active = this.journal?.state.active;
    if (!active) return;
    try { await this.api.cancel(this.address, active.id, AbortSignal.timeout(10000)); }
    catch { log('Shutdown cancellation uncertain.', { orderID: active.id }); }
    log('Stopped; order and hedge recovery state retained. Settlement may still be pending.');
  }
}

function hex(value) { return `0x${value.toString(16)}`; }
