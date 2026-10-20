import { AbiCoder, Signature, concat, getBytes, keccak256, toBeHex, toUtf8Bytes } from 'ethers';
import { dec, same, units, validAddress, validHash, integer } from './numbers.js';

const abi = AbiCoder.defaultAbiCoder();
const hashText = (text) => keccak256(toUtf8Bytes(text));
const typeHash = hashText('Order(address trader,address relayer,address baseToken,address quoteToken,uint256 baseTokenAmount,uint256 quoteTokenAmount,uint256 gasTokenAmount,bytes32 data)');
const domain = keccak256(abi.encode(['bytes32', 'bytes32'], [hashText('EIP712Domain(string name)'), hashText('NevDEX')]));

export function builtHash(order) {
  const j = order.json;
  if (!j || ![j.trader, j.relayer, j.baseToken, j.quoteToken].every(validAddress) || !/^0x[\da-f]{64}$/i.test(j.data)) {
    throw new Error('Invalid order addresses or signed data.');
  }
  const amounts = [j.baseTokenAmount, j.quoteTokenAmount, j.gasTokenAmount].map((n) => integer(n, 'order amount'));
  if (amounts.some((n) => n >= 2n ** 256n)) throw new Error('Order amount exceeds uint256.');
  const struct = keccak256(abi.encode(
    ['bytes32', 'address', 'address', 'address', 'address', 'uint256', 'uint256', 'uint256', 'bytes32'],
    [typeHash, j.trader.toLowerCase(), j.relayer.toLowerCase(), j.baseToken.toLowerCase(), j.quoteToken.toLowerCase(), ...amounts, j.data],
  ));
  return keccak256(concat(['0x1901', domain, struct]));
}

export function validateBuilt(o, quote, c, m, address, timestamp) {
  try {
    if (!validHash(o.id) || o.marketID !== c.market || o.side !== quote.side || o.type !== 'limit' ||
      !dec(o.price).eq(quote.price) || !dec(o.amount).eq(quote.amount) || !dec(o.gasFeeAmount).isZero()) {
      throw new Error();
    }
    const j = o.json;
    const notional = BigInt(dec(quote.amount).mul(quote.price).mul(dec(10).pow(m.quoteTokenDecimals)).trunc().toFixed(0));
    if (!same(j.trader, address) || !same(j.baseToken, c.baseToken) || !same(j.quoteToken, c.quoteToken) ||
      j.baseTokenAmount !== units(quote.amount, m.baseTokenDecimals).toString() || j.quoteTokenAmount !== notional.toString() || j.gasTokenAmount !== '0') {
      throw new Error();
    }
    const data = getBytes(j.data);
    if (data.length !== 32 || data[0] !== 2 || data[1] !== (quote.side === 'sell' ? 1 : 0) ||
      data[2] !== 0 || data[22] !== 0 || data[12] !== 0 || data[13] !== 0) throw new Error();
    const expiry = BigInt(`0x${Buffer.from(data.slice(3, 8)).toString('hex')}`);
    if (expiry < timestamp + 30n || expiry > timestamp + BigInt(c.orderTTLSeconds) + 10n) throw new Error();
    for (const [i, rate] of [m.makerFeeRate, m.takerFeeRate].entries()) {
      if (BigInt(data[8 + i * 2] * 256 + data[9 + i * 2]) !== units(rate, 5)) throw new Error();
    }
    if (!same(builtHash(o), o.id)) throw new Error();
    return Number(expiry);
  } catch {
    throw new Error('Build response changed the requested order, signed fees, expiry, or hash.');
  }
}

export async function signOrder(wallet, id) {
  if (!validHash(id)) throw new Error('Invalid order ID.');
  const sig = Signature.from(await wallet.signMessage(getBytes(id)));
  // NevDEX uses v at byte zero, 31 zero bytes, followed by r and s (96 bytes).
  return concat([toBeHex(sig.v, 1), new Uint8Array(31), sig.r, sig.s]);
}

export function validateOpenOrders(orders, c, address) {
  if (!Array.isArray(orders) || orders.length > 10000) throw new Error('Invalid account order list.');
  const seen = new Set();
  for (const o of orders) {
    if (!o || o.MarketId !== c.market || !same(o.TraderAddress, address) || !validHash(o.Id) || seen.has(o.Id.toLowerCase()) ||
      !['buy', 'sell'].includes(o.Side) || !['pending', 'settling'].includes(o.Status) || !['limit', 'market'].includes(o.Type)) {
      throw new Error('Account order identity does not match this wallet and market.');
    }
    for (const key of ['Price', 'Amount', 'AvailableAmount', 'PendingAmount']) {
      if (typeof o[key] !== 'string' || o[key].length > 80 || !/^\d+(\.\d+)?$/.test(o[key])) {
        throw new Error(`Account order ${key} is missing or invalid.`);
      }
    }
    if (!dec(o.Price).gt(0) || !dec(o.Amount).gt(0) || dec(o.AvailableAmount).plus(o.PendingAmount).gt(o.Amount)) {
      throw new Error('Invalid account order quantities.');
    }
    seen.add(o.Id.toLowerCase());
  }
  return orders;
}
