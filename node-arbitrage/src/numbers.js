import Decimal from 'decimal.js';

// Enough precision for uint256 values, decimal prices, and intermediate products.
Decimal.set({ precision: 240, rounding: Decimal.ROUND_DOWN, toExpNeg: -240, toExpPos: 240 });
export { Decimal };
export const dec = (value) => new Decimal(value);
export const max = (a, b) => a > b ? a : b;
export const ceilDiv = (n, d) => (n + d - 1n) / d;
export const hex = (value) => `0x${BigInt(value).toString(16)}`;
export const same = (a, b) => typeof a === 'string' && typeof b === 'string' && a.toLowerCase() === b.toLowerCase();
export const validAddress = (value) => typeof value === 'string' && /^0x[\da-f]{40}$/i.test(value) && !/^0x0{40}$/i.test(value);
export const validHash = (value) => typeof value === 'string' && /^0x[\da-f]{64}$/i.test(value) && !/^0x0{64}$/i.test(value);

export function integer(value, name = 'integer', signed = false) {
  if (typeof value !== 'string' || !(signed ? /^-?\d+$/ : /^\d+$/).test(value) || value.length > 160) {
    throw new Error(`Invalid ${name}.`);
  }
  return BigInt(value);
}

export function safeInteger(value, name) {
  const text = String(value);
  if (!/^\d+$/.test(text)) throw new Error(`Invalid ${name}.`);
  const n = Number(text);
  if (!Number.isSafeInteger(n)) throw new Error(`Invalid ${name}.`);
  return n;
}

export function units(value, decimals) {
  const n = dec(value).mul(dec(10).pow(decimals));
  if (!n.isFinite() || n.isNegative() || !n.isInteger() || n.gt(dec(2).pow(256).minus(1))) {
    throw new Error('Amount cannot be represented at token precision.');
  }
  return BigInt(n.toFixed(0));
}

export const display = (value, decimals) => dec(value.toString()).div(dec(10).pow(decimals)).toFixed();
export const fee = (amount, market) => ceilDiv(amount * units(Decimal.max(market.makerFeeRate, market.takerFeeRate).toFixed(5), 5), 100000n);
export const slip = (amount, bps, buy) => buy
  ? ceilDiv(amount * BigInt(10000 + bps), 10000n)
  : amount * BigInt(10000 - bps) / 10000n;

export function log(message, fields = {}) {
  console.log(JSON.stringify({ time: new Date().toISOString(), message, ...fields }, (_, v) => typeof v === 'bigint' ? v.toString() : v));
}
