import { loadConfig, CONFIG_PATH } from './config.js';

const cmd = process.argv[2];
const cfg = loadConfig();

switch (cmd) {
  case 'serve': {
    const { startServer } = await import('./server.js');
    startServer(cfg);
    break;
  }
  case 'agent': {
    const { runAgent } = await import('./agent/index.js');
    await runAgent(cfg, { once: process.argv.includes('--once') });
    break;
  }
  case 'import-wakatime': {
    const { importWakatime } = await import('./import-wakatime.js');
    const daysArg = process.argv.find((a) => a.startsWith('--days='));
    await importWakatime(cfg, { days: daysArg ? Number(daysArg.split('=')[1]) : 365 });
    break;
  }
  case 'status': {
    const res = await fetch(`${cfg.agent.serverUrl}/api/summary?days=1&groupBy=project,source`).catch((e) => e);
    if (res instanceof Error) { console.error(`server unreachable at ${cfg.agent.serverUrl}: ${res.message}`); process.exit(1); }
    const s = await res.json();
    const h = (sec) => `${Math.floor(sec / 3600)}h ${Math.round((sec % 3600) / 60)}m`;
    console.log(`today: ${h(s.total)} total`);
    for (const t of s.totals.slice(0, 15)) console.log(`  ${h(t.seconds).padEnd(9)} ${t.project} (${t.source})`);
    break;
  }
  default:
    console.log(`tempo — self-hosted coding time tracker

usage: tempo <command>

  serve             run the server (ingest API + dashboard) on this machine
  agent [--once]    run the watcher agent (files, claude, codex, mac apps)
  import-wakatime [--days=365]   backfill history from wakatime.com
  status            print today's totals from the server

config: ${CONFIG_PATH}`);
}
