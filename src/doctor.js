import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { CONFIG_PATH, DATA_DIR, loadConfig } from './config.js';
import { ZED_DB_PATHS } from './agent/watch-zed.js';
import { VERSION } from './version.js';

const exec = promisify(execFile);

function authHeaders(token) {
  return token ? { authorization: `Bearer ${token}` } : {};
}

export async function diagnose(options = {}) {
  const checks = [];
  const add = (name, status, message, details) => checks.push({ name, status, message, ...(details ? { details } : {}) });
  const configPath = options.configPath || CONFIG_PATH;
  const dataDir = options.dataDir || DATA_DIR;
  const home = options.home || os.homedir();
  const platform = options.platform || process.platform;
  const fetchFn = options.fetchFn || fetch;
  let cfg = options.cfg;

  const nodeMajor = Number(process.versions.node.split('.')[0]);
  add('node', nodeMajor >= 22 ? 'ok' : 'error', `${process.version} (requires >=22)`);
  try {
    await import('node:sqlite');
    add('sqlite', 'ok', 'node:sqlite available');
  } catch (err) {
    add('sqlite', 'error', err.message);
  }

  if (!cfg) {
    if (!fs.existsSync(configPath)) add('config', 'warn', `not found: ${configPath}`);
    try {
      cfg = loadConfig(configPath);
      if (fs.existsSync(configPath)) add('config', 'ok', `valid JSON: ${configPath}`);
    } catch (err) {
      add('config', 'error', `cannot load ${configPath}: ${err.message}`);
    }
  } else {
    add('config', 'ok', 'configuration loaded');
  }
  if (fs.existsSync(configPath) && platform !== 'win32') {
    try {
      const mode = fs.statSync(configPath).mode & 0o777;
      add('config-permissions', mode & 0o077 ? 'warn' : 'ok',
        `${mode.toString(8).padStart(3, '0')} ${configPath}${mode & 0o077 ? ' (recommend 600)' : ''}`);
    } catch (err) { add('config-permissions', 'error', err.message); }
  }

  try {
    let probe = dataDir;
    while (!fs.existsSync(probe) && path.dirname(probe) !== probe) probe = path.dirname(probe);
    fs.accessSync(probe, fs.constants.R_OK | fs.constants.W_OK);
    add('data-dir', 'ok', fs.existsSync(dataDir) ? dataDir : `${dataDir} (will be created)`);
  } catch (err) {
    add('data-dir', 'error', `${dataDir}: ${err.message}`);
  }
  const queuePath = path.join(dataDir, 'queue.jsonl');
  if (fs.existsSync(queuePath)) {
    try {
      const bytes = fs.statSync(queuePath).size;
      add('offline-queue', bytes ? 'warn' : 'ok', `${bytes} bytes waiting in ${queuePath}`);
    } catch (err) { add('offline-queue', 'error', err.message); }
  } else add('offline-queue', 'ok', 'empty');

  if (cfg) {
    if (cfg.server.token || Object.keys(cfg.server.tokens || {}).length || cfg.agent.token) {
      add('token', 'ok', 'configured (value hidden)');
    }
    else add('token', 'warn', 'no ingest token configured');

    const roots = cfg.agent.projectRoots || [];
    if (!roots.length) add('project-roots', 'warn', 'none configured; file watcher cannot run');
    for (const root of roots) {
      try {
        const st = fs.statSync(root);
        if (!st.isDirectory()) throw new Error('not a directory');
        fs.accessSync(root, fs.constants.R_OK);
        add('project-root', 'ok', root);
      } catch (err) { add('project-root', 'error', `${root}: ${err.message}`); }
    }

    const inputs = [
      ['claude-input', cfg.agent.watch.claude, path.join(home, '.claude', 'projects')],
      ['codex-input', cfg.agent.watch.codex, path.join(home, '.codex', 'sessions')],
    ];
    for (const [name, enabled, input] of inputs) {
      if (!enabled) add(name, 'ok', 'disabled');
      else if (!fs.existsSync(input)) add(name, 'warn', `not found: ${input}`);
      else {
        try { fs.accessSync(input, fs.constants.R_OK); add(name, 'ok', input); }
        catch (err) { add(name, 'error', `${input}: ${err.message}`); }
      }
    }
    if (!cfg.agent.watch.zed) add('zed-input', 'ok', 'disabled');
    else {
      const zed = (options.zedDbPaths || ZED_DB_PATHS).find((candidate) => fs.existsSync(candidate));
      add('zed-input', zed ? 'ok' : 'warn', zed || 'threads.db not found');
    }

    if (fs.existsSync(cfg.server.db)) {
      try {
        const { DatabaseSync } = await import('node:sqlite');
        const db = new DatabaseSync(cfg.server.db, { readOnly: true });
        const result = db.prepare('PRAGMA quick_check').get().quick_check;
        db.close();
        add('database', result === 'ok' ? 'ok' : 'error', `${cfg.server.db}: ${result}`);
      } catch (err) { add('database', 'error', `${cfg.server.db}: ${err.message}`); }
    } else add('database', 'warn', `not found: ${cfg.server.db}`);

    const serverUrl = String(cfg.agent.serverUrl || '').replace(/\/$/, '');
    try {
      const res = await fetchFn(`${serverUrl}/api/auth-check`, {
        headers: authHeaders(cfg.agent.token), signal: AbortSignal.timeout(5_000),
      });
      if (res.ok) add('server-auth', 'ok', serverUrl);
      else add('server-auth', 'error', `${serverUrl}: HTTP ${res.status}`);
      if (res.ok) {
        const statuses = await (await fetchFn(`${serverUrl}/api/agent-status`, {
          signal: AbortSignal.timeout(5_000),
        })).json();
        const local = Array.isArray(statuses) && statuses.find((status) => status.machine === cfg.agent.machine);
        if (!local) add('agent-report', 'warn', `no report stored for ${cfg.agent.machine}`);
        else {
          const staleAfter = Math.max(90, (local.intervalSeconds || 20) * 3);
          add('agent-report', local.ageSeconds <= staleAfter ? 'ok' : 'error',
            `${cfg.agent.machine}: ${Math.round(local.ageSeconds)}s old`);
          add('agent-version', local.version === VERSION ? 'ok' : 'warn',
            `agent ${local.version}, doctor ${VERSION}`);
          add('agent-queue', local.queueDepth ? 'warn' : 'ok',
            local.queueDepth ? `${local.queueDepth} heartbeats (${local.queueBytes} bytes) queued` : 'empty');
          add('clock-skew', Math.abs(local.clockSkewSeconds) > 30 ? 'warn' : 'ok',
            `${Math.round(local.clockSkewSeconds)}s`);
          for (const [name, watcher] of Object.entries(local.watchers || {})) {
            if (!watcher.enabled || !watcher.available) continue;
            const watcherAge = Date.now() / 1000 - Number(watcher.lastOk || 0);
            if (watcher.error || watcher.consecutiveErrors > 0) {
              add(`watcher-${name}`, 'error', watcher.error || `${watcher.consecutiveErrors} consecutive errors`);
            } else if (!watcher.lastOk || watcherAge > staleAfter) {
              add(`watcher-${name}`, 'error', `last successful poll ${Math.round(watcherAge)}s ago`);
            } else if (watcher.unmatchedInputRuns >= 5) {
              add(`watcher-${name}`, 'warn', `${watcher.unmatchedInputRuns} input changes produced no heartbeat`);
            } else {
              add(`watcher-${name}`, 'ok', `last poll ${Math.round(Math.max(0, watcherAge))}s ago`);
            }
          }
        }
      }
    } catch (err) { add('server-auth', 'error', `${serverUrl}: ${err.message}`); }

    if (options.checkServices !== false && platform === 'linux') {
      try {
        const { stdout = '' } = await (options.execFn || exec)('systemctl',
          ['--user', 'is-active', 'stackhour-agent', 'stackhour-server']);
        const active = stdout.trim().split('\n').filter((value) => value === 'active').length;
        add('services', active ? 'ok' : 'warn', `${active}/2 Stackhour user services active`);
      } catch (err) {
        const active = String(err.stdout || '').trim().split('\n').filter((value) => value === 'active').length;
        add('services', active ? 'ok' : 'warn', `${active}/2 Stackhour user services active`);
      }
    } else if (options.checkServices !== false && platform === 'darwin') {
      try {
        await (options.execFn || exec)('launchctl',
          ['print', `gui/${process.getuid()}/com.nikita.stackhour-agent`]);
        add('services', 'ok', 'Stackhour launch agent active');
      } catch { add('services', 'warn', 'Stackhour launch agent inactive'); }
    }
  }

  return {
    ok: !checks.some((check) => check.status === 'error'),
    version: VERSION,
    checks,
  };
}

export function printDoctor(report, { json = false, stdout = process.stdout } = {}) {
  if (json) {
    stdout.write(`${JSON.stringify(report, null, 2)}\n`);
    return;
  }
  stdout.write(`Stackhour doctor ${report.version}\n`);
  const icon = { ok: '✓', warn: '!', error: '✗' };
  for (const check of report.checks) {
    stdout.write(`${icon[check.status]} ${check.name}: ${check.message}\n`);
  }
  const warnings = report.checks.filter((check) => check.status === 'warn').length;
  const errors = report.checks.filter((check) => check.status === 'error').length;
  stdout.write(`\n${errors} errors, ${warnings} warnings\n`);
}

export async function runDoctor(options = {}) {
  const report = await diagnose(options);
  printDoctor(report, options);
  return report.ok ? 0 : 1;
}
