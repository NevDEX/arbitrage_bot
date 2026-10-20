import { readFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { Decimal, dec, same, units, validAddress, safeInteger } from './numbers.js';

const defaults = {
  endpoint: 'https://nevdex.xyz/api/',
  rpcURL: 'https://rpc.mainnet.chain.robinhood.com',
  chainID: 4663,
  market: 'CASHCAT-USDG',
  baseToken: '0x020bfC650A365f8BB26819deAAbF3E21291018b4',
  quoteToken: '0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168',
  uniswapV3Pool: '0x4b0c312ffbb068f6a0bea128759e35d94b94d0e1',
  vaultAddress: '', exchangeAddress: '', account: '', privateKeyFile: '',
  stateDir: '.node-arbitrage-state',
  orderAmount: '0.1000', minProfitPct: '0.005', minProfitQuote: '0.0001',
  gasCostQuote: '0.001', maxQuotePerTrade: '1', maxTotalQuote: '10', maxLossQuote: '0.1',
  maxFeeRate: '0.01', maxGasPriceGwei: '100', maxHedgeGasNative: '0.001',
  slippageBPS: 50, confirmations: 3, pollMilliseconds: 1000,
  cancelAfterMilliseconds: 1500, orderTTLSeconds: 60, timeoutSeconds: 10, maxDataAgeSeconds: 30,
};
const amounts = ['orderAmount', 'minProfitPct', 'minProfitQuote', 'gasCostQuote', 'maxQuotePerTrade',
  'maxTotalQuote', 'maxLossQuote', 'maxFeeRate', 'maxGasPriceGwei', 'maxHedgeGasNative'];

export async function loadConfig(file, live) {
  const raw = await readFile(file);
  if (raw.length > 65536) throw new Error('Configuration exceeds 64 KiB.');
  let input;
  try { input = JSON.parse(raw.toString('utf8')); }
  catch { throw new Error('Configuration must contain one valid JSON object.'); }
  if (!input || Array.isArray(input) || typeof input !== 'object') throw new Error('Configuration must be an object.');
  for (const key of Object.keys(input)) {
    if (!Object.hasOwn(defaults, key)) throw new Error(`Unknown configuration field: ${key}.`);
  }
  const c = { ...defaults, ...input };
  for (const [key, value] of Object.entries(c)) {
    if (typeof value !== typeof defaults[key]) throw new Error(`Invalid type for ${key}.`);
  }
  for (const key of ['endpoint', 'rpcURL']) {
    let url;
    try { url = new URL(c[key]); } catch { throw new Error(`Invalid ${key}.`); }
    if (url.username || url.password || url.search || url.hash ||
      !(url.protocol === 'https:' || url.protocol === 'http:' && ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname))) {
      throw new Error('Use HTTPS or loopback HTTP endpoints without credentials or query parameters.');
    }
  }
  if (c.chainID !== 4663 || !/^[A-Za-z0-9]+-[A-Za-z0-9]+$/.test(c.market) ||
    !validAddress(c.baseToken) || !validAddress(c.quoteToken) || same(c.baseToken, c.quoteToken) || !validAddress(c.uniswapV3Pool)) {
    throw new Error('Configure a direct ERC-20 V3 market on chain 4663.');
  }
  for (const key of amounts) {
    if (c[key].length > 80 || !/^\d+(\.\d+)?$/.test(c[key]) || !dec(c[key]).gt(0)) {
      throw new Error(`${key} must be a positive decimal string.`);
    }
  }
  if (dec(c.minProfitPct).gte(1) || dec(c.maxFeeRate).gte(1) || dec(c.maxQuotePerTrade).gt(c.maxTotalQuote)) {
    throw new Error('Invalid profit, fee, or total quote limit.');
  }
  for (const [key, low, high] of [
    ['slippageBPS', 1, 1000], ['confirmations', 1, 100], ['pollMilliseconds', 250, 60000],
    ['cancelAfterMilliseconds', 250, 10000], ['orderTTLSeconds', 60, 120],
    ['timeoutSeconds', 1, 30], ['maxDataAgeSeconds', 2, 60],
  ]) {
    if (!Number.isSafeInteger(c[key]) || c[key] < low || c[key] > high) throw new Error(`Invalid ${key}.`);
  }
  if (!c.stateDir || c.account && !validAddress(c.account)) throw new Error('Invalid stateDir or account.');
  if (live && (!validAddress(c.vaultAddress) || !validAddress(c.exchangeAddress) || !c.privateKeyFile)) {
    throw new Error('Live mode requires vaultAddress, exchangeAddress, and privateKeyFile.');
  }
  for (const key of ['stateDir', 'privateKeyFile']) if (c[key]) c[key] = resolve(dirname(resolve(file)), c[key]);
  return c;
}

export function validateMarket(m, c) {
  if (!m || m.id !== c.market || m.isPublished !== true || !same(m.baseTokenAddress, c.baseToken) || !same(m.quoteTokenAddress, c.quoteToken)) {
    throw new Error('Market and token deployment do not match configuration.');
  }
  for (const key of ['baseTokenDecimals', 'quoteTokenDecimals', 'priceDecimals', 'amountDecimals']) {
    m[key] = safeInteger(m[key], key);
    if (m[key] > 18) throw new Error('Market precision exceeds 18 decimals.');
  }
  if (m.amountDecimals > m.baseTokenDecimals) throw new Error('Order precision exceeds base token precision.');
  for (const key of ['minOrderSize', 'makerFeeRate', 'takerFeeRate']) {
    if (typeof m[key] !== 'string' || m[key].length > 80 || !/^\d+(\.\d+)?$/.test(m[key])) throw new Error(`Invalid market ${key}.`);
  }
  for (const key of ['makerFeeRate', 'takerFeeRate']) {
    // Fees are encoded in five decimal places in the signed order.
    if (units(m[key], 5) > 65535n) throw new Error('Invalid signed fee precision or range.');
  }
  if (Decimal.max(m.makerFeeRate, m.takerFeeRate).gt(c.maxFeeRate)) throw new Error('Market fee exceeds maxFeeRate.');
  units(c.orderAmount, m.baseTokenDecimals);
  units(c.orderAmount, m.amountDecimals);
  for (const key of ['gasCostQuote', 'minProfitQuote', 'maxQuotePerTrade', 'maxTotalQuote', 'maxLossQuote']) units(c[key], m.quoteTokenDecimals);
  units(c.maxGasPriceGwei, 9);
  units(c.maxHedgeGasNative, 18);
  return m;
}
