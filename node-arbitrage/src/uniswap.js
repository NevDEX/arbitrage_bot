import { Interface, keccak256 } from 'ethers';
import { hex, integer, same, units, slip, validHash } from './numbers.js';

export const FACTORY = '0x1f7d7550b1b028f7571e69a784071f0205fd2efa';
export const ROUTER = '0xcaf681a66d020601342297493863e78c959e5cb2';
export const QUOTER = '0x33e885ed0ec9bf04ecfb19341582aadcb4c8a9e7';

const v3 = new Interface([
  'function quoteExactInputSingle((address tokenIn,address tokenOut,uint256 amountIn,uint24 fee,uint160 sqrtPriceLimitX96) params) returns (uint256 amountOut,uint160 sqrtPriceX96After,uint32 initializedTicksCrossed,uint256 gasEstimate)',
  'function quoteExactOutputSingle((address tokenIn,address tokenOut,uint256 amount,uint24 fee,uint160 sqrtPriceLimitX96) params) returns (uint256 amountIn,uint160 sqrtPriceX96After,uint32 initializedTicksCrossed,uint256 gasEstimate)',
  'function exactInputSingle((address tokenIn,address tokenOut,uint24 fee,address recipient,uint256 amountIn,uint256 amountOutMinimum,uint160 sqrtPriceLimitX96) params) returns (uint256 amountOut)',
  'function exactOutputSingle((address tokenIn,address tokenOut,uint24 fee,address recipient,uint256 amountOut,uint256 amountInMaximum,uint160 sqrtPriceLimitX96) params) returns (uint256 amountIn)',
  'function multicall(uint256 deadline,bytes[] data) returns (bytes[])',
]);
const exchange = new Interface([
  'event Match((address baseToken,address quoteToken,address relayer) addressSet,(address maker,address taker,address buyer,uint256 makerFee,uint256 makerRebate,uint256 takerFee,uint256 makerGasFee,uint256 takerGasFee,uint256 baseTokenFilledAmount,uint256 quoteTokenFilledAmount) result)',
]);
const erc20 = new Interface(['event Transfer(address indexed from,address indexed to,uint256 value)']);

export class Uniswap {
  constructor(c, m, rpc) { this.c = c; this.m = m; this.rpc = rpc; }
  async read(address, signature, args, block, signal) {
    return (await this.rpc.read(address, signature, args, block, signal))[0];
  }

  async check(head, live, signal) {
    const c = this.c;
    if (BigInt(await this.rpc.call('eth_chainId', [], signal)) !== BigInt(c.chainID)) throw new Error('RPC chain mismatch.');
    const pool = c.uniswapV3Pool;
    const t0 = await this.read(pool, 'token0() view returns (address)', [], head.tag, signal);
    const t1 = await this.read(pool, 'token1() view returns (address)', [], head.tag, signal);
    if (!(same(t0, c.baseToken) && same(t1, c.quoteToken) || same(t1, c.baseToken) && same(t0, c.quoteToken))) throw new Error('Pool tokens do not match the market.');
    const factory = await this.read(pool, 'factory() view returns (address)', [], head.tag, signal);
    if (!same(factory, FACTORY)) throw new Error('Pool factory does not match the official deployment.');
    this.fee = await this.read(pool, 'fee() view returns (uint24)', [], head.tag, signal);
    const registered = await this.read(FACTORY, 'getPool(address,address,uint24) view returns (address)', [t0, t1, this.fee], head.tag, signal);
    if (!same(registered, pool)) throw new Error('Pool is not registered in the official factory.');
    for (const [token, decimals] of [[c.baseToken, this.m.baseTokenDecimals], [c.quoteToken, this.m.quoteTokenDecimals]]) {
      if (await this.read(token, 'decimals() view returns (uint8)', [], head.tag, signal) !== BigInt(decimals)) throw new Error('On-chain token decimals differ from the market.');
    }
    for (const address of [ROUTER, QUOTER]) {
      if (!same(await this.read(address, 'factory() view returns (address)', [], head.tag, signal), FACTORY)) throw new Error('Router or Quoter factory mismatch.');
    }
    await this.liquidity(head, signal);
    if (!live) return;
    const code = await this.rpc.call('eth_getCode', [c.vaultAddress, head.tag], signal);
    if (typeof code !== 'string' || !/^0x(?:[\da-f]{2})+$/i.test(code)) throw new Error('Vault has no runtime code.');
    if (!same(await this.read(c.exchangeAddress, 'vault() view returns (address)', [], head.tag, signal), c.vaultAddress) ||
      !same(await this.read(c.vaultAddress, 'exchange() view returns (address)', [], head.tag, signal), c.exchangeAddress)) {
      throw new Error('Exchange and Vault bindings do not match.');
    }
  }

  async liquidity(head, signal) {
    const state = await this.rpc.read(this.c.uniswapV3Pool,
      'slot0() view returns (uint160,int24,uint16,uint16,uint16,uint8,bool)', [], head.tag, signal);
    if (state[0] <= 0n || state[6] !== true || await this.read(this.c.uniswapV3Pool, 'liquidity() view returns (uint128)', [], head.tag, signal) <= 0n) {
      throw new Error('Pool is locked or has no active liquidity.');
    }
  }

  async quote(side, base, head, signal) {
    const sell = side === 'sell';
    const method = sell ? 'quoteExactInputSingle' : 'quoteExactOutputSingle';
    const data = v3.encodeFunctionData(method, [[
      (sell ? this.c.baseToken : this.c.quoteToken).toLowerCase(),
      (sell ? this.c.quoteToken : this.c.baseToken).toLowerCase(), base, this.fee, 0n,
    ]]);
    const raw = await this.rpc.call('eth_call', [{ to: QUOTER, data }, head.tag], signal);
    const [amount, sqrt] = v3.decodeFunctionResult(method, raw);
    if (amount <= 0n || sqrt <= 4295128740n || sqrt >= 1461446703485210103287273052203988822378723970341n) {
      throw new Error('Uniswap quote has no full-size liquidity.');
    }
    return amount;
  }

  async funds(address, side, base, quote, head, signal) {
    const token = side === 'buy' ? this.c.quoteToken : this.c.baseToken;
    const amount = side === 'buy' ? quote : base;
    const owner = address.toLowerCase();
    if (await this.read(token, 'balanceOf(address) view returns (uint256)', [owner], head.tag, signal) < amount ||
      await this.read(token, 'allowance(address,address) view returns (uint256)', [owner, ROUTER], head.tag, signal) < amount) {
      throw new Error('Insufficient hedge wallet balance or Router allowance.');
    }
    if (BigInt(await this.rpc.call('eth_getBalance', [owner, head.tag], signal)) < units(this.c.maxHedgeGasNative, 18)) throw new Error('Insufficient native gas reserve.');
  }

  decodeFill(raw, address, active) {
    const event = exchange.parseLog(raw);
    const a = event.args.addressSet;
    const r = event.args.result;
    if (!same(a.baseToken, this.c.baseToken) || !same(a.quoteToken, this.c.quoteToken) || !same(r.maker, address) && !same(r.taker, address)) return null;
    if (!active || !same(a.relayer, active.relayer) || same(r.maker, r.taker) || !same(r.buyer, r.maker) && !same(r.buyer, r.taker)) throw new Error('Unexpected wallet fill; this wallet must be exclusive to one bot.');
    const buy = same(r.buyer, address);
    if (buy !== (active.quote.side === 'buy') || r.baseTokenFilledAmount <= 0n || r.quoteTokenFilledAmount <= 0n) throw new Error('Settled fill differs from the active order direction.');
    const fee = same(r.maker, address) ? r.makerFee + r.makerGasFee - r.makerRebate : r.takerFee + r.takerGasFee;
    return {
      id: `${raw.transactionHash.toLowerCase()}:${BigInt(raw.logIndex)}`, hedgeSide: buy ? 'sell' : 'buy',
      baseRaw: r.baseTokenFilledAmount.toString(),
      quoteDeltaRaw: (buy ? -(r.quoteTokenFilledAmount + fee) : r.quoteTokenFilledAmount - fee).toString(),
      block: BigInt(raw.blockNumber).toString(), blockHash: raw.blockHash,
    };
  }

  async scan(journal, address, head, signal) {
    const s = structuredClone(journal.state);
    const cursor = integer(s.cursor);
    const previous = await this.rpc.block(hex(cursor), signal);
    if (!same(previous.hash, s.cursorHash)) throw new Error('Settlement cursor reorganization; reconcile before restarting.');
    const confirmations = BigInt(this.c.confirmations);
    if (head.number < confirmations || head.number - confirmations <= cursor) return { caughtUp: true, confirmed: previous };
    let end = head.number - confirmations;
    const caughtUp = end - cursor <= 500n;
    if (!caughtUp) end = cursor + 500n;
    const last = await this.rpc.block(hex(end), signal);
    const topic = exchange.getEvent('Match').topicHash;
    const logs = await this.rpc.call('eth_getLogs', [{ address: this.c.exchangeAddress, fromBlock: hex(cursor + 1n), toBlock: hex(end), topics: [topic] }], signal);
    if (!Array.isArray(logs)) throw new Error('Invalid settlement logs.');
    logs.sort((a, b) => BigInt(a.blockNumber) < BigInt(b.blockNumber) ? -1 : BigInt(a.blockNumber) > BigInt(b.blockNumber) ? 1 : BigInt(a.logIndex) < BigInt(b.logIndex) ? -1 : 1);
    const seen = new Set(s.pending.map((f) => f.id.toLowerCase()));
    for (const raw of logs) {
      if (raw.removed || !same(raw.address, this.c.exchangeAddress) || BigInt(raw.blockNumber) <= cursor || BigInt(raw.blockNumber) > end ||
        !validHash(raw.transactionHash) || !validHash(raw.blockHash) || !Array.isArray(raw.topics) || raw.topics.length !== 1 || !same(raw.topics[0], topic)) throw new Error('Invalid settlement log identity.');
      const fill = this.decodeFill(raw, address, s.active);
      if (!fill || seen.has(fill.id)) continue;
      const block = await this.rpc.block(raw.blockNumber, signal);
      if (!same(block.hash, raw.blockHash)) throw new Error('Settlement event reorganization.');
      const filled = integer(s.active.filledBaseRaw) + integer(fill.baseRaw);
      if (filled > units(s.active.quote.amount, this.m.baseTokenDecimals)) throw new Error('Settled fills exceed the persisted order amount.');
      s.active.filledBaseRaw = filled.toString();
      s.active.seen = true;
      s.pending.push(fill);
      seen.add(fill.id);
    }
    if (s.pending.length > 100) throw new Error('Unhedged fill queue exceeds the limit.');
    if (!same((await this.rpc.block(hex(end), signal)).hash, last.hash)) throw new Error('Scan block changed during read.');
    s.cursor = end.toString();
    s.cursorHash = last.hash;
    journal.state = s;
    await journal.save();
    return { caughtUp, confirmed: last };
  }

  async submit(j, wallet, head, signal) {
    const c = this.c;
    const f = j.state.pending[0];
    const base = integer(f.baseRaw);
    const price = await this.quote(f.hedgeSide, base, head, signal);
    const buy = f.hedgeSide === 'buy';
    const bound = slip(price, c.slippageBPS, buy);
    const cap = units(c.maxQuotePerTrade, this.m.quoteTokenDecimals);
    if (price > cap || bound > cap || bound <= 0n) throw new Error('Hedge exceeds the per-trade quote cap.');
    await this.funds(wallet.address, f.hedgeSide, base, bound, head, signal);
    const swap = v3.encodeFunctionData(buy ? 'exactOutputSingle' : 'exactInputSingle', [[
      (buy ? c.quoteToken : c.baseToken).toLowerCase(), (buy ? c.baseToken : c.quoteToken).toLowerCase(),
      this.fee, wallet.address, base, bound, 0n,
    ]]);
    const deadline = head.timestamp + 60n;
    const data = v3.encodeFunctionData('multicall', [deadline, [swap]]);
    const call = { from: wallet.address, to: ROUTER, data, value: '0x0' };
    await this.rpc.call('eth_call', [call, head.tag], signal);
    const estimate = BigInt(await this.rpc.call('eth_estimateGas', [call], signal));
    if (estimate <= 0n || estimate > 833333n) throw new Error('Hedge gas estimate exceeds the hard cap.');
    const gasLimit = estimate * 120n / 100n + 1n;
    const gasPrice = BigInt(await this.rpc.call('eth_gasPrice', [], signal));
    if (gasPrice <= 0n || gasPrice > units(c.maxGasPriceGwei, 9) || gasLimit * gasPrice > units(c.maxHedgeGasNative, 18)) throw new Error('Hedge gas exceeds configured caps.');
    const nonce = BigInt(await this.rpc.call('eth_getTransactionCount', [wallet.address, 'pending'], signal));
    if (nonce > BigInt(Number.MAX_SAFE_INTEGER)) throw new Error('Transaction nonce exceeds the supported range.');
    if (BigInt(Math.floor(Date.now() / 1000)) - head.timestamp > BigInt(c.maxDataAgeSeconds)) throw new Error('Hedge quote became stale.');
    signal.throwIfAborted();
    const raw = await wallet.signTransaction({ type: 0, chainId: c.chainID, nonce: Number(nonce), to: ROUTER, value: 0n, gasLimit, gasPrice, data });
    const hash = keccak256(raw);
    j.state.hedgeTx = { hash, nonce: nonce.toString(), deadline: deadline.toString(), fillID: f.id,
      boundRaw: bound.toString(), gasBudgetRaw: units(c.gasCostQuote, this.m.quoteTokenDecimals).toString() };
    await j.save();
    try {
      const response = await this.rpc.call('eth_sendRawTransaction', [raw], signal);
      if (!same(response, hash)) throw new Error();
    } catch { throw new Error(`Hedge broadcast uncertain: ${hash}. Reconcile the original hash and nonce before replacement.`); }
  }

  transferDelta(receipt, token, address) {
    let delta = 0n;
    if (!Array.isArray(receipt.logs)) throw new Error('Receipt is missing token transfer logs.');
    for (const raw of receipt.logs) {
      if (!same(raw.address, token) || !same(raw.topics?.[0], erc20.getEvent('Transfer').topicHash)) continue;
      if (raw.topics.length !== 3 || !/^0x[\da-f]{64}$/i.test(raw.data)) throw new Error('Malformed token transfer event.');
      const event = erc20.parseLog(raw);
      if (same(event.args.to, address)) delta += event.args.value;
      if (same(event.args.from, address)) delta -= event.args.value;
    }
    return delta;
  }

  async settle(j, address, head, signal) {
    const t = j.state.hedgeTx;
    const receipt = await this.rpc.call('eth_getTransactionReceipt', [t.hash], signal);
    if (!receipt) {
      if (head.timestamp > integer(t.deadline) + 60n) throw new Error(`Hedge remains unresolved: ${t.hash}. Inspect the original nonce.`);
      return;
    }
    if (!same(receipt.transactionHash, t.hash) || BigInt(receipt.status) !== 1n || !receipt.blockNumber) throw new Error('Hedge failed or receipt mismatched; fill retained.');
    const block = await this.rpc.block(receipt.blockNumber, signal);
    if (!same(block.hash, receipt.blockHash)) throw new Error('Hedge receipt reorganization.');
    if (head.number < block.number + BigInt(this.c.confirmations)) return;
    const f = j.state.pending[0];
    if (!f || f.id !== t.fillID) throw new Error('Hedge fill identity mismatch.');
    const baseDelta = this.transferDelta(receipt, this.c.baseToken, address);
    const quoteDelta = this.transferDelta(receipt, this.c.quoteToken, address);
    const sell = f.hedgeSide === 'sell';
    if (baseDelta !== integer(f.baseRaw) * (sell ? -1n : 1n) ||
      (sell ? quoteDelta < integer(t.boundRaw) : quoteDelta >= 0n || -quoteDelta > integer(t.boundRaw))) {
      throw new Error('Hedge actual transfers differ from the intended amounts; reconcile residual exposure.');
    }
    const pnl = integer(f.quoteDeltaRaw, 'fill quote delta', true) + quoteDelta - integer(t.gasBudgetRaw);
    j.state.estimatedPnLRaw = (integer(j.state.estimatedPnLRaw, 'estimated PnL', true) + pnl).toString();
    if (pnl < 0n) j.state.cumulativeLossRaw = (integer(j.state.cumulativeLossRaw) - pnl).toString();
    j.state.pending.shift();
    j.state.hedgeTx = null;
    await j.save();
  }
}
