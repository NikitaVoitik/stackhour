import { loadConfig, CONFIG_PATH } from './config.js';

const cmd = process.argv[2];

if (cmd === 'doctor') {
  const { runDoctor } = await import('./doctor.js');
  process.exitCode = await runDoctor({ json: process.argv.includes('--json') });
} else if (cmd === 'init') {
  const { runInit } = await import('./setup.js');
  try { runInit(process.argv.slice(3)); }
  catch (err) { console.error(`stackhour init: ${err.message}`); process.exitCode = 1; }
} else if (cmd === 'token') {
  const { runToken } = await import('./tokens.js');
  try { runToken(process.argv.slice(3)); }
  catch (err) { console.error(`stackhour token: ${err.message}`); process.exitCode = 1; }
} else if (cmd === 'data') {
  const { runData } = await import('./data.js');
  try { runData(process.argv.slice(3)); }
  catch (err) { console.error(`stackhour data: ${err.message}`); process.exitCode = 1; }
} else if (cmd === 'backup') {
  const { runBackup } = await import('./backup.js');
  try { await runBackup(process.argv.slice(3)); }
  catch (err) { console.error(`stackhour backup: ${err.message}`); process.exitCode = 1; }
} else if (cmd === 'install') {
  const { runInstall } = await import('./install.js');
  try { runInstall(process.argv.slice(3)); }
  catch (err) { console.error(`stackhour install: ${err.message}`); process.exitCode = 1; }
} else if (cmd === 'bridge') {
  const { runBridgeCli } = await import('./bridge/cli.mjs');
  await runBridgeCli(process.argv.slice(3));
} else {
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
    console.log(`stackhour — self-hosted coding time tracker

usage: stackhour <command>

  serve             run the server (ingest API + dashboard) on this machine
  agent [--once]    run the watcher agent (files, claude, codex, mac apps)
  import-wakatime [--days=365]   backfill history from wakatime.com
  status            print today's totals from the server
  doctor [--json]   check config, inputs, database, server, and services
  init server [--public-url=URL] [--project-root=PATH ...] [--install]
                    configure the server and its local agent
  init agent --enrollment=CODE [--project-root=PATH ...] [--install]
                    enroll and optionally install an agent service
  token create MACHINE [--force] [--raw] [--server-url=URL]
                    enroll a machine and print its copy-paste command
  token list                       list enrolled machines (never secrets)
  token revoke MACHINE             revoke a machine token
  data stats [--json]              inspect local database size and coverage
  data export --output=FILE [--from=TIME] [--to=TIME] [--force]
                                   atomically export JSONL
  data prune --before=TIME [--confirm]
                                   preview or confirm retention pruning
  backup create [--output=FILE] [--force]
                                   create and verify a consistent snapshot
  backup verify FILE               integrity-check a backup
  backup restore FILE [--confirm]  preview or restore, preserving old DB
  install <server|agent>           install and start user service(s)
  bridge install <coordinator|worker> [--reconfigure] [--no-start]
                                   set up the Telegram Claude/Codex bridge
  bridge doctor|status|restart <coordinator|worker>
                                   operate the bridge service

config: ${CONFIG_PATH}`);
  }
}
