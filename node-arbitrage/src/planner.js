import { dec, units, fee, slip, max } from './numbers.js';

function limitPrice(levels, quantity, m) {
  let remaining = quantity;
  for (const [price, amount] of levels) {
    if (units(price, m.priceDecimals) <= 0n) throw new Error('Invalid book price precision.');
    const size = units(amount, m.baseTokenDecimals);
    if (size <= 0n) throw new Error('Invalid book quantity precision.');
    remaining -= size > remaining ? remaining : size;
    if (remaining === 0n) return dec(price);
  }
  return null;
}

export function plan(c, m, depth, uniSell, uniBuy, createdAt = Date.now()) {
  const base = units(c.orderAmount, m.baseTokenDecimals);
  const gas = units(c.gasCostQuote, m.quoteTokenDecimals);
  const minimum = units(c.minProfitQuote, m.quoteTokenDecimals);
  const cap = units(c.maxQuotePerTrade, m.quoteTokenDecimals);
  let best = null;
  for (const side of ['sell', 'buy']) {
    const levels = side === 'sell' ? depth.bids : depth.asks;
    const limit = limitPrice(levels, base, m);
    if (!limit || limit.mul(c.orderAmount).lt(m.minOrderSize)) continue;
    // Every unit may fill at the worst permitted limit price.
    const gross = BigInt(limit.mul(c.orderAmount).mul(dec(10).pow(m.quoteTokenDecimals)).trunc().toFixed(0));
    if (gross <= 0n) continue;
    const orderCap = gross + fee(gross, m);
    let cost = side === 'sell' ? slip(uniBuy, c.slippageBPS, true) : orderCap;
    const proceeds = side === 'sell' ? gross - fee(gross, m) : slip(uniSell, c.slippageBPS, false);
    cost += BigInt(levels.length);
    const net = proceeds - cost - gas;
    const commitment = max(orderCap, cost);
    if (commitment > cap || proceeds > cap || net < minimum || dec(net.toString()).lt(dec(cost.toString()).mul(c.minProfitPct))) continue;
    const candidate = {
      route: side === 'sell' ? 'BUY_UNI_SELL_MYOB' : 'BUY_MYOB_SELL_UNI',
      quote: { side, price: limit.toFixed(), amount: c.orderAmount },
      cost, proceeds, net, commitment, createdAt,
    };
    if (!best || candidate.net > best.net) best = candidate;
  }
  return best;
}
