import { constants } from 'node:fs';
import { open, mkdir, lstat, rename, unlink, rmdir } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { hostname } from 'node:os';
import { randomUUID } from 'node:crypto';
import { integer, units, validAddress, validHash, safeInteger } from './numbers.js';

export async function privateRead(file, limit) {
  let handle;
  try {
    handle = await open(file, constants.O_RDONLY | constants.O_NOFOLLOW);
    const stat = await handle.stat();
    if (!stat.isFile() || stat.mode & 0o077 || typeof process.getuid !== 'function' || stat.uid !== process.getuid()) {
      throw new Error();
    }
    const buffer = Buffer.alloc(limit + 1);
    let size = 0;
    while (size < buffer.length) {
      const { bytesRead } = await handle.read(buffer, size, buffer.length - size, null);
      if (!bytesRead) break;
      size += bytesRead;
    }
    if (size > limit) throw new Error();
    return buffer.subarray(0, size);
  } catch {
    throw new Error('Cannot read private file: require a regular file owned by this user, mode 600 or stricter, within the size limit.');
  } finally { await handle?.close(); }
}

async function privateDir(path) {
  await mkdir(path, { recursive: true, mode: 0o700 });
  const stat = await lstat(path);
  if (!stat.isDirectory() || stat.mode & 0o077 || typeof process.getuid !== 'function' || stat.uid !== process.getuid()) {
    throw new Error('State directories must be owned by this user with mode 700.');
  }
}

export class Journal {
  static async open(c, address) {
    await privateDir(c.stateDir);
    const dir = join(c.stateDir, address.toLowerCase());
    await privateDir(dir);
    const j = new Journal();
    j.file = join(dir, 'state.json');
    j.lock = join(dir, 'node.lock');
    try { await mkdir(j.lock, { mode: 0o700 }); }
    catch { throw new Error('Wallet state is locked. If the previous process crashed, inspect node.lock/owner.json and remove only that lock after confirming it is stopped.'); }
    try {
      const owner = await open(join(j.lock, 'owner.json'), 'wx', 0o600);
      try { await owner.writeFile(JSON.stringify({ pid: process.pid, host: hostname(), startedAt: new Date().toISOString() })); }
      finally { await owner.close(); }
      const identity = JSON.stringify(['node-arbitrage-v1', c.chainID, c.endpoint, c.rpcURL, c.market,
        address.toLowerCase(), ...['vaultAddress', 'exchangeAddress', 'baseToken', 'quoteToken', 'uniswapV3Pool'].map((key) => c[key].toLowerCase()), c.confirmations]);
      j.state = { identity, cursor: '0', cursorHash: '', active: null, pending: [], hedgeTx: null,
        committedQuoteRaw: '0', cumulativeLossRaw: '0', estimatedPnLRaw: '0' };
      try { await lstat(j.file); }
      catch (error) { if (error.code === 'ENOENT') return j; throw error; }
      let saved;
      try { saved = JSON.parse((await privateRead(j.file, 1 << 20)).toString('utf8')); }
      catch { throw new Error('Cannot load recovery state; retain the original state file.'); }
      if (saved.identity !== identity) throw new Error('Recovery identity mismatch. Do not switch wallets, deployments, or state directories to bypass unfinished trades.');
      j.state = saved;
      j.validate();
      return j;
    } catch (error) { await j.close(); throw error; }
  }

  validate() {
    const s = this.state;
    const cursor = integer(s.cursor, 'scan cursor');
    integer(s.committedQuoteRaw, 'committed budget');
    integer(s.cumulativeLossRaw, 'cumulative loss');
    integer(s.estimatedPnLRaw, 'estimated PnL', true);
    if (!validHash(s.cursorHash) || !Array.isArray(s.pending) || s.pending.length > 100) throw new Error('Invalid recovery cursor or fill queue.');
    if (s.active) {
      const o = s.active;
      if (!validHash(o.id) || !validAddress(o.relayer) || !['buy', 'sell'].includes(o.quote?.side) ||
        units(o.quote.amount, 18) <= 0n || units(o.quote.price, 18) <= 0n ||
        safeInteger(o.expires, 'order expiry') <= 0 || safeInteger(o.createdAt, 'order creation time') <= 0 || typeof o.seen !== 'boolean') {
        throw new Error('Invalid persisted order intent.');
      }
      safeInteger(o.lastCancelAt, 'last cancellation time');
      integer(o.filledBaseRaw, 'filled base amount');
    }
    const seen = new Set();
    let total = 0n;
    for (const f of s.pending) {
      const [hash, index, extra] = String(f.id).split(':');
      if (!validHash(hash) || extra !== undefined || index === undefined || seen.has(f.id.toLowerCase()) ||
        !['buy', 'sell'].includes(f.hedgeSide) || !validHash(f.blockHash) || !s.active) throw new Error('Invalid persisted fill.');
      integer(index, 'fill log index');
      const amount = integer(f.baseRaw, 'fill amount');
      const block = integer(f.block, 'fill block');
      integer(f.quoteDeltaRaw, 'fill quote delta', true);
      if (amount <= 0n || block <= 0n || block > cursor || f.hedgeSide === s.active.quote.side) throw new Error('Invalid persisted fill amount or direction.');
      seen.add(f.id.toLowerCase());
      total += amount;
    }
    if (s.active && total > integer(s.active.filledBaseRaw)) throw new Error('Pending hedge amount exceeds settled fills.');
    if (s.hedgeTx) {
      const t = s.hedgeTx;
      if (!validHash(t.hash) || t.fillID !== s.pending[0]?.id || integer(t.boundRaw) <= 0n || integer(t.deadline) <= 0n) {
        throw new Error('Invalid persisted hedge intent.');
      }
      integer(t.nonce, 'hedge nonce');
      integer(t.gasBudgetRaw, 'gas budget');
    }
  }

  async save() {
    const temporary = join(dirname(this.file), `.state-${randomUUID()}`);
    const handle = await open(temporary, 'wx', 0o600);
    try {
      try { await handle.writeFile(JSON.stringify(this.state)); await handle.sync(); }
      finally { await handle.close(); }
      await rename(temporary, this.file);
      const directory = await open(dirname(this.file), 'r');
      try { await directory.sync(); } finally { await directory.close(); }
    } finally { await unlink(temporary).catch((e) => { if (e.code !== 'ENOENT') throw e; }); }
  }

  async close() {
    if (!this.lock) return;
    await unlink(join(this.lock, 'owner.json')).catch((e) => { if (e.code !== 'ENOENT') throw e; });
    await rmdir(this.lock);
    this.lock = null;
  }
}
