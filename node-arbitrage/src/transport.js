import { parse, isLosslessNumber } from 'lossless-json';
import { setTimeout as sleep } from 'node:timers/promises';
import { Interface } from 'ethers';
import { hex, validHash } from './numbers.js';

// API numeric literals become strings before JavaScript can round them.
export const parseWire = (text) => parse(text, (_, value) => isLosslessNumber(value) ? value.toString() : value);

async function post(url, payload, signal, seconds, limit, label) {
  const timeout = AbortSignal.timeout(seconds * 1000);
  try {
    const response = await fetch(url, {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload), signal: AbortSignal.any([signal, timeout]), redirect: 'manual',
    });
    try {
      if (response.status !== 200) throw new Error(`${label} HTTP status=${response.status}.`);
      const chunks = [];
      let size = 0;
      for await (const chunk of response.body) {
        size += chunk.length;
        if (size > limit) throw new Error(`${label} response exceeds size limit.`);
        chunks.push(chunk);
      }
      return parseWire(Buffer.concat(chunks).toString('utf8'));
    } finally {
      if (response.body && !response.body.locked) await response.body.cancel().catch(() => {});
    }
  } catch (error) {
    if (signal.aborted) signal.throwIfAborted();
    // Never echo arbitrary backend bodies, signed payloads, or fetch error URLs.
    if (error.message.startsWith(`${label} HTTP`) || error.message.startsWith(`${label} response exceeds`)) throw error;
    throw new Error(`${label} unavailable or returned invalid JSON.`);
  }
}

export class API {
  #lastMutation = 0;
  #queue = Promise.resolve();
  constructor(config) { this.c = config; }

  async call(path, payload, signal, dataRequired = true) {
    const request = async () => {
      const result = await post(`${this.c.endpoint.replace(/\/+$/, '')}/${path}`, payload,
        signal, this.c.timeoutSeconds, 4 << 20, `API ${path}`);
      if (String(result?.code) !== '20000') throw new Error(`API ${path} rejected the request.`);
      if (dataRequired && result.data == null) throw new Error(`API ${path} returned no data.`);
      return result.data;
    };
    if (!['order/place', 'order/cancel'].includes(path)) return request();
    const queued = this.#queue.then(async () => {
      const wait = this.#lastMutation + 200 + Math.floor(Math.random() * 301) - Date.now();
      if (wait > 0) await sleep(wait, undefined, { signal });
      signal.throwIfAborted();
      try { return await request(); } finally { this.#lastMutation = Date.now(); }
    });
    this.#queue = queued.catch(() => {});
    return queued;
  }

  cancel(address, id, signal) {
    return this.call('order/cancel', { marketID: this.c.market, address, orderID: id }, signal, false);
  }
}

export class RPC {
  #id = 0;
  constructor(config) { this.c = config; }

  async call(method, params, signal) {
    const id = ++this.#id;
    const result = await post(this.c.rpcURL, { jsonrpc: '2.0', id, method, params },
      signal, this.c.timeoutSeconds, 4 << 20, `RPC ${method}`);
    if (result?.jsonrpc !== '2.0' || String(result.id) !== String(id) || result.error != null || !Object.hasOwn(result, 'result')) {
      throw new Error(`RPC ${method} failed.`);
    }
    return result.result;
  }

  async read(address, fragment, args, block, signal) {
    const abi = new Interface([`function ${fragment}`]);
    const method = abi.fragments[0].name;
    const data = abi.encodeFunctionData(method, args);
    const raw = await this.call('eth_call', [{ to: address.toLowerCase(), data }, block], signal);
    try { return abi.decodeFunctionResult(method, raw); }
    catch { throw new Error(`Invalid contract result for ${method}.`); }
  }

  async block(tag, signal) {
    const b = await this.call('eth_getBlockByNumber', [tag, false], signal);
    if (!b || !validHash(b.hash) || !/^0x[\da-f]+$/i.test(b.number) || !/^0x[\da-f]+$/i.test(b.timestamp)) {
      throw new Error('Invalid chain block.');
    }
    return { number: BigInt(b.number), hash: b.hash, timestamp: BigInt(b.timestamp), tag: b.number };
  }

  async head(signal) {
    const b = await this.block('latest', signal);
    const now = BigInt(Math.floor(Date.now() / 1000));
    if (b.timestamp <= 0n || b.timestamp > now + 5n || now - b.timestamp > BigInt(this.c.maxDataAgeSeconds)) {
      throw new Error('RPC head is stale or the local clock is incorrect.');
    }
    return b;
  }
}

export { hex };
