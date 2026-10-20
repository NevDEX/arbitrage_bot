import WebSocket from 'ws';
import { setTimeout as sleep } from 'node:timers/promises';
import { dec, integer, same } from './numbers.js';
import { parseWire } from './transport.js';
import { validateOpenOrders } from './orders.js';

export function validateDepth(raw) {
  if (!raw || !Array.isArray(raw.bids) || !Array.isArray(raw.asks)) throw new Error('Missing order book.');
  const sequence = integer(raw.sequence, 'book sequence');
  for (const [i, levels] of [raw.bids, raw.asks].entries()) {
    if (levels.length > 10000) throw new Error('Order book exceeds level limit.');
    let previous;
    for (const row of levels) {
      if (!Array.isArray(row) || row.length !== 2 || row.some((v) => typeof v !== 'string' || v.length > 80 || !/^\d+(\.\d+)?$/.test(v))) {
        throw new Error('Invalid order book level.');
      }
      const price = dec(row[0]);
      if (!price.gt(0) || !dec(row[1]).gt(0) || previous && (i === 0 ? price.gte(previous) : price.lte(previous))) {
        throw new Error('Invalid or unsorted order book.');
      }
      previous = price;
    }
  }
  if (raw.bids.length && raw.asks.length && dec(raw.bids[0][0]).gte(raw.asks[0][0])) throw new Error('Crossed order book.');
  return { ...raw, sequence };
}

async function watch(c, path, params, hooks, signal) {
  const url = new URL(`${c.endpoint.replace(/\/+$/, '')}/${path}`);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  for (const [key, value] of Object.entries(params)) url.searchParams.set(key, value);
  let delay = 1000;
  while (!signal.aborted) {
    await new Promise((resolve) => {
      const socket = new WebSocket(url, { handshakeTimeout: c.timeoutSeconds * 1000, maxPayload: 4 << 20, followRedirects: false });
      let contact = Date.now();
      const abort = () => socket.terminate();
      const heartbeat = setInterval(() => { if (Date.now() - contact > 45000) socket.terminate(); }, 1000);
      signal.addEventListener('abort', abort, { once: true });
      socket.on('error', () => hooks.close?.());
      socket.on('open', () => hooks.open?.());
      socket.on('ping', () => {
        contact = Date.now();
        try { hooks.ping?.(); } catch { hooks.close?.(); socket.terminate(); }
      });
      socket.on('message', (data, binary) => {
        contact = Date.now();
        try {
          if (binary) throw new Error('Binary market frames are unsupported.');
          hooks.message(parseWire(data.toString()));
          delay = 1000;
        } catch {
          hooks.close?.();
          socket.terminate();
        }
      });
      socket.once('close', () => {
        clearInterval(heartbeat);
        signal.removeEventListener('abort', abort);
        hooks.close?.();
        resolve();
      });
      if (signal.aborted) abort();
    });
    try { await sleep(delay, undefined, { signal }); } catch { break; }
    delay = Math.min(delay * 2, 8000);
  }
}

export class MarketFeed {
  constructor(c, api) { this.c = c; this.api = api; this.ready = false; this.contact = 0; this.generation = 0; this.revision = 0; }
  fresh() { return this.ready && Date.now() - this.contact <= this.c.maxDataAgeSeconds * 1000; }
  update(raw) {
    const next = validateDepth(raw);
    if (this.ready && next.sequence < this.depth.sequence) return;
    this.depth = next;
    this.revision++;
    this.ready = true;
    this.contact = Date.now();
  }
  async start(signal) {
    return watch(this.c, 'v1/ws', { marketID: this.c.market }, {
      open: () => {
        this.ready = false;
        const generation = ++this.generation;
        this.api.call('order/orderbook', { marketID: this.c.market }, signal)
          .then((depth) => { if (!signal.aborted && this.generation === generation) this.update(depth); })
          .catch(() => {});
      },
      ping: () => { if (this.ready) this.contact = Date.now(); },
      message: (event) => {
        if (event.marketID !== this.c.market) return;
        if (event.type === 'depth_update') this.update(event.data);
      },
      close: () => { this.ready = false; this.generation++; },
    }, signal);
  }
}

export class AccountFeed {
  constructor(c, address) { this.c = c; this.address = address; this.block = 0n; this.invalidate(); }
  invalidate() { this.ready = false; this.version = 0n; this.orders = []; }
  snapshot() {
    if (!this.ready || Date.now() - this.contact > this.c.maxDataAgeSeconds * 1000) return null;
    return { orders: this.orders, block: this.block };
  }
  apply(data) {
    if (!data || !same(data.address, this.address)) throw new Error('Invalid account snapshot identity.');
    const version = integer(data.version, 'account version');
    const block = integer(data.blockNumber, 'account block');
    if (version <= this.version) throw new Error('Out-of-order account snapshot.');
    this.orders = validateOpenOrders(data.openOrder, this.c, this.address);
    this.version = version;
    if (block > this.block) this.block = block;
    this.ready = true;
    this.contact = Date.now();
  }
  async start(signal) {
    return watch(this.c, 'v1/orders/ws', { marketID: this.c.market, address: this.address, openOnly: '1' }, {
      open: () => this.invalidate(),
      ping: () => {
        if (this.ready && !this.snapshot()) { this.invalidate(); throw new Error('Account heartbeat gap.'); }
        this.contact = Date.now();
      },
      message: (event) => {
        if (event.marketID !== this.c.market || event.type !== 'orders_snapshot') throw new Error('Unexpected account event.');
        this.apply(event.data);
      },
      close: () => this.invalidate(),
    }, signal);
  }
}
