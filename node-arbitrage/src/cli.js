#!/usr/bin/env node
import { parseArgs } from 'node:util';
import { loadConfig } from './config.js';
import { Engine } from './engine.js';

const help = `NevDEX x Uniswap V3 Arbitrage Bot

Usage: npm start -- [options]

  --config <file>  JSON configuration (default: arbitrage.example.json)
  --check          Validate the market and deployment, then exit (read-only)
  --once           Show one fresh book and executable quote pair (read-only)
  --live           Enable real orders, cancellations, and hedge transactions
  --help           Show this help

With no mode flag, continuously monitor in read-only mode.
--check and --once have a 30-second timeout. Modes are mutually exclusive.
Private keys are loaded only with --live. Press Ctrl+C to stop.
`;

const control = new AbortController();
const stop = () => control.abort(new Error('Shutdown requested.'));
process.once('SIGINT', stop);
process.once('SIGTERM', stop);
let deadline;
try {
  let values;
  try {
    ({ values } = parseArgs({ options: {
      config: { type: 'string', default: 'arbitrage.example.json' },
      check: { type: 'boolean', default: false }, once: { type: 'boolean', default: false },
      live: { type: 'boolean', default: false }, help: { type: 'boolean', default: false },
    }, strict: true, allowPositionals: false }));
  } catch { throw new Error('Invalid arguments. Use --help for usage.'); }
  if (values.help) console.log(help);
  else {
    if ([values.check, values.once, values.live].filter(Boolean).length > 1) throw new Error('Choose only one of --check, --once, or --live.');
    const c = await loadConfig(values.config, values.live);
    const signal = values.check || values.once
      ? AbortSignal.any([control.signal, deadline = AbortSignal.timeout(30000)]) : control.signal;
    await new Engine(c, values).run(signal);
  }
} catch (error) {
  if (!control.signal.aborted) {
    console.error(deadline?.aborted ? 'Read-only inspection timed out after 30 seconds.' : error.message);
    process.exitCode = 1;
  }
} finally {
  process.removeListener('SIGINT', stop);
  process.removeListener('SIGTERM', stop);
}
