import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { openDb, insertHeartbeats, listAgentStatus, upsertAgentStatus } from './db.js';
import { computeCredits, totalsBy, dayBuckets, buildSegments } from './summarize.js';
import { VERSION } from './version.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const GROUP_FIELDS = ['project', 'source', 'machine', 'category', 'language', 'entity', 'actor', 'branch'];

function readBody(req, limit = 5 * 1024 * 1024) {
  return new Promise((resolve, reject) => {
    let size = 0;
    let tooLarge = false;
    const chunks = [];
    req.on('data', (c) => {
      if (tooLarge) return;
      size += c.length;
      if (size > limit) {
        tooLarge = true;
        chunks.length = 0;
        const err = new Error('body too large');
        err.statusCode = 413;
        reject(err);
        return;
      }
      chunks.push(c);
    });
    req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    req.on('error', reject);
  });
}

async function readJson(req) {
  const body = await readBody(req);
  try { return JSON.parse(body); }
  catch {
    const err = new Error('invalid JSON');
    err.statusCode = 400;
    throw err;
  }
}

function numberParam(url, name, fallback, { min = -Infinity, max = Infinity, integer = false } = {}) {
  const raw = url.searchParams.get(name);
  let value = raw === null ? fallback : Number(raw);
  if (!Number.isFinite(value)) value = fallback;
  value = Math.min(max, Math.max(min, value));
  return integer ? Math.floor(value) : value;
}

function json(res, status, obj) {
  const body = JSON.stringify(obj);
  res.writeHead(status, { 'content-type': 'application/json' });
  res.end(body);
}

function safeEqual(left, right) {
  const a = Buffer.from(String(left));
  const b = Buffer.from(String(right));
  return a.length === b.length && crypto.timingSafeEqual(a, b);
}

function requestToken(req, url) {
  const header = req.headers.authorization || '';
  if (header.startsWith('Bearer ')) return header.slice(7);
  // wakatime plugins send: Basic base64(api_key)
  if (header.startsWith('Basic ')) {
    try {
      const decoded = Buffer.from(header.slice(6), 'base64').toString('utf8');
      return decoded.endsWith(':') ? decoded.slice(0, -1) : decoded;
    } catch { /* fall through */ }
  }
  return url.searchParams.get('api_key') || '';
}

export function authenticate(req, url, serverConfig) {
  const tokens = serverConfig.tokens && typeof serverConfig.tokens === 'object' && !Array.isArray(serverConfig.tokens)
    ? serverConfig.tokens : {};
  const legacy = serverConfig.token || '';
  if (!legacy && Object.keys(tokens).length === 0) return { kind: 'open' };
  const supplied = requestToken(req, url);
  if (legacy && safeEqual(supplied, legacy)) return { kind: 'global' };
  for (const [machine, token] of Object.entries(tokens)) {
    if (token && safeEqual(supplied, token)) return { kind: 'machine', machine };
  }
  return null;
}

function allowsMachine(principal, machine) {
  return principal?.kind !== 'machine' || String(machine || 'unknown') === principal.machine;
}

// Map a wakatime-protocol heartbeat (from official editor plugins) to our schema.
function fromWakatime(h, userAgent, machineHeader) {
  const plugin = String(h.plugin || userAgent || 'wakatime-plugin');
  // plugin strings look like "webstorm/2024.1 webstorm-wakatime/15.0.2"
  const source = (plugin.split(' ')[0] || 'wakatime-plugin').split('/')[0].toLowerCase();
  return {
    time: Number(h.time),
    machine: machineHeader || 'unknown',
    source,
    project: h.project || h.alternate_project || 'unknown',
    entity: h.entity || 'unknown',
    entity_type: h.type === 'app' ? 'app' : 'file',
    category: h.category || 'coding',
    language: h.language || null,
    branch: h.branch || null,
    is_write: h.is_write ? 1 : 0,
    // editor plugins are keystroke-driven (human), unless the plugin itself
    // reports AI activity (e.g. wakatime-cli --sync-ai-activity)
    actor: /\bai\b/i.test(String(h.category || '')) ? 'agent' : 'human',
  };
}

// A file save observed by the generic file watcher may actually be an agent's
// edit (Claude/Codex writing files triggers mtime changes too). If an agent
// reported touching the same file within the window, hand the save to it.
export function reattributeFileSaves(rows, windowSeconds = 120) {
  const agentEdits = rows.filter((r) => r.actor === 'agent'
    && r.entity_type === 'file' && r.is_write);
  if (!agentEdits.length) return rows;
  const editsByEntity = new Map();
  for (const edit of agentEdits) {
    const key = JSON.stringify([edit.machine, edit.entity]);
    let edits = editsByEntity.get(key);
    if (!edits) editsByEntity.set(key, (edits = []));
    edits.push(edit);
  }
  for (const edits of editsByEntity.values()) edits.sort((a, b) => a.time - b.time);

  return rows.map((r) => {
    if (r.source !== 'editor-files') return r;
    const edits = editsByEntity.get(JSON.stringify([r.machine, r.entity]));
    if (!edits) return r;
    let lo = 0;
    let hi = edits.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (edits[mid].time < r.time) lo = mid + 1;
      else hi = mid;
    }
    let match = null;
    let matchDistance = Infinity;
    for (const index of [lo - 1, lo]) {
      const a = edits[index];
      if (!a) continue;
      const distance = Math.abs(a.time - r.time);
      if (distance <= windowSeconds && distance < matchDistance) {
        match = a;
        matchDistance = distance;
      }
    }
    return match ? { ...r, actor: 'agent', source: match.source } : r;
  });
}

function reattributedRange(db, from, to, windowSeconds) {
  const raw = db.prepare('SELECT * FROM heartbeats WHERE time >= ? AND time <= ?')
    .all(from - windowSeconds, to + windowSeconds);
  return reattributeFileSaves(raw, windowSeconds)
    .filter((r) => r.time >= from && r.time <= to);
}

export function startServer(cfg) {
  if (fs.existsSync(`${cfg.server.db}.maintenance.lock`)) {
    throw new Error(`database maintenance is in progress: ${cfg.server.db}`);
  }
  const db = openDb(cfg.server.db);
  const dashboardPath = path.join(__dirname, 'dashboard.html');

  const server = http.createServer(async (req, res) => {
    const url = new URL(req.url, 'http://localhost');
    const p = url.pathname;
    try {
      if (req.method === 'GET' && (p === '/' || p === '/index.html')) {
        res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
        res.end(fs.readFileSync(dashboardPath));
        return;
      }
      if (req.method === 'GET' && p === '/api/health') {
        json(res, 200, { ok: true, version: VERSION });
        return;
      }
      if (req.method === 'GET' && p === '/api/auth-check') {
        const principal = authenticate(req, url, cfg.server);
        if (!principal) return json(res, 401, { error: 'unauthorized' });
        return json(res, 200, { ok: true, version: VERSION, machine: principal.machine || null });
      }

      // ---- ingest (Stackhour agents) ----
      if (req.method === 'POST' && p === '/api/ingest') {
        const principal = authenticate(req, url, cfg.server);
        if (!principal) return json(res, 401, { error: 'unauthorized' });
        const rows = await readJson(req);
        if (!Array.isArray(rows)) return json(res, 400, { error: 'expected array' });
        if (rows.some((row) => !allowsMachine(principal, row?.machine))) {
          return json(res, 403, { error: `token is restricted to machine ${principal.machine}` });
        }
        const inserted = insertHeartbeats(db, rows);
        return json(res, 200, { inserted, received: rows.length });
      }

      if (req.method === 'POST' && p === '/api/agent-status') {
        const principal = authenticate(req, url, cfg.server);
        if (!principal) return json(res, 401, { error: 'unauthorized' });
        const status = await readJson(req);
        if (!status || typeof status !== 'object' || Array.isArray(status)) {
          return json(res, 400, { error: 'expected object' });
        }
        if (!allowsMachine(principal, status.machine)) {
          return json(res, 403, { error: `token is restricted to machine ${principal.machine}` });
        }
        try { return json(res, 200, upsertAgentStatus(db, status)); }
        catch (err) { return json(res, 400, { error: err.message }); }
      }

      // ---- wakatime-protocol ingest (official editor plugins pointed at api_url) ----
      if (req.method === 'POST' && (p === '/api/v1/users/current/heartbeats'
        || p === '/api/v1/users/current/heartbeats.bulk'
        || p === '/users/current/heartbeats'
        || p === '/users/current/heartbeats.bulk')) {
        const principal = authenticate(req, url, cfg.server);
        if (!principal) return json(res, 401, { error: 'unauthorized' });
        const body = await readJson(req);
        const items = Array.isArray(body) ? body : [body];
        const machine = req.headers['x-machine-name'] || 'unknown';
        if (!allowsMachine(principal, machine)) {
          return json(res, 403, { error: `token is restricted to machine ${principal.machine}` });
        }
        const rows = items.map((h) => fromWakatime(h, req.headers['user-agent'], machine));
        insertHeartbeats(db, rows);
        // official API returns 201/202 with a responses array for bulk
        return json(res, 202, {
          responses: items.map((h) => [{ data: { id: null, entity: h.entity, time: h.time } }, 201]),
        });
      }

      // ---- queries ----
      if (req.method === 'GET' && p === '/api/agent-status') {
        return json(res, 200, listAgentStatus(db));
      }

      if (req.method === 'GET' && p === '/api/summary') {
        const days = numberParam(url, 'days', 1, { min: 1, max: 366 });
        const to = numberParam(url, 'to', Date.now() / 1000);
        const from = numberParam(url, 'from', to - days * 86400);
        const groupBy = (url.searchParams.get('groupBy') || 'project')
          .split(',').filter((k) => GROUP_FIELDS.includes(k));
        const tz = numberParam(url, 'tz', 0, { min: -1440, max: 1440 });
        const rows = reattributedRange(db, from, to, cfg.summary.reattributeWindowSeconds);
        const credited = computeCredits(rows, cfg.summary);
        const total = Math.round(credited.reduce((a, r) => a + r.credit, 0));
        const humanTotal = Math.round(credited.filter((r) => r.actor !== 'agent').reduce((a, r) => a + r.credit, 0));
        const totalCost = Math.round(rows.reduce((a, r) => a + (r.cost || 0), 0) * 100) / 100;
        const totalTokens = rows.reduce((a, r) => a + (r.tokens_in || 0) + (r.tokens_out || 0), 0);
        return json(res, 200, {
          from, to, total, humanTotal, agentTotal: total - humanTotal,
          totalCost, totalTokens,
          totals: totalsBy(credited, groupBy.length ? groupBy : ['project']),
          days: dayBuckets(credited, groupBy.length ? groupBy : ['project'], tz),
        });
      }

      if (req.method === 'GET' && p === '/api/detail') {
        const days = numberParam(url, 'days', 7, { min: 1, max: 366 });
        const to = numberParam(url, 'to', Date.now() / 1000);
        const from = numberParam(url, 'from', to - days * 86400);
        const dimension = url.searchParams.get('dimension') || '';
        const value = url.searchParams.get('value');
        if (!GROUP_FIELDS.includes(dimension) || value === null) {
          return json(res, 400, { error: 'dimension and value are required' });
        }
        const rows = reattributedRange(db, from, to, cfg.summary.reattributeWindowSeconds);
        const credited = computeCredits(rows, cfg.summary);
        // Empty is the explicit API representation for SQL null. The visible
        // "unknown" label selects that same bucket when clicked in the UI.
        const matches = (row) => value === ''
          ? row[dimension] == null
          : String(row[dimension] ?? 'unknown') === value;
        const selectedRows = rows.filter(matches);
        const selectedCredits = credited.filter(matches);
        const total = Math.round(selectedCredits.reduce((sum, row) => sum + row.credit, 0));
        const humanTotal = Math.round(selectedCredits.filter((row) => row.actor !== 'agent')
          .reduce((sum, row) => sum + row.credit, 0));
        const breakdowns = {};
        for (const field of GROUP_FIELDS) {
          if (field !== dimension) breakdowns[field] = totalsBy(selectedCredits, [field]).slice(0, 20);
        }
        return json(res, 200, {
          from, to, dimension, value, total, humanTotal, agentTotal: total - humanTotal,
          totalCost: Math.round(selectedRows.reduce((sum, row) => sum + (row.cost || 0), 0) * 100) / 100,
          totalTokens: selectedRows.reduce((sum, row) => sum + (row.tokens_in || 0) + (row.tokens_out || 0), 0),
          breakdowns,
          segments: buildSegments(selectedCredits, cfg.summary),
          recent: selectedRows.sort((a, b) => b.time - a.time).slice(0, 50),
        });
      }

      if (req.method === 'GET' && p === '/api/now') {
        // what's active right now: distinct (actor, project, source, machine)
        // seen in the last ~2 minutes
        const windowS = numberParam(url, 'window', 150, { min: 1, max: 86400 });
        const to = Date.now() / 1000;
        const rows = reattributedRange(db, to - windowS, to, cfg.summary.reattributeWindowSeconds);
        const seen = new Map();
        for (const r of rows) {
          const key = JSON.stringify([r.actor, r.project, r.source, r.machine]);
          const prev = seen.get(key);
          if (!prev || r.time > prev.time) {
            seen.set(key, { actor: r.actor, project: r.project, source: r.source, machine: r.machine, time: r.time, entity: r.entity });
          }
        }
        return json(res, 200, [...seen.values()].sort((a, b) => b.time - a.time));
      }

      if (req.method === 'GET' && p === '/api/timeline') {
        const hours = numberParam(url, 'hours', 24, { min: 1 / 60, max: 24 * 14 });
        const to = numberParam(url, 'to', Date.now() / 1000);
        const from = to - hours * 3600;
        const rows = reattributedRange(db, from, to, cfg.summary.reattributeWindowSeconds);
        const credited = computeCredits(rows, cfg.summary);
        return json(res, 200, { from, to, segments: buildSegments(credited, cfg.summary) });
      }

      if (req.method === 'GET' && p === '/api/recent') {
        const limit = numberParam(url, 'limit', 50, { min: 1, max: 500, integer: true });
        const selected = db.prepare('SELECT * FROM heartbeats ORDER BY time DESC LIMIT ?').all(limit);
        if (!selected.length) return json(res, 200, selected);
        const window = cfg.summary.reattributeWindowSeconds;
        const minTime = Math.min(...selected.map((r) => r.time));
        const maxTime = Math.max(...selected.map((r) => r.time));
        const context = db.prepare('SELECT * FROM heartbeats WHERE time >= ? AND time <= ?')
          .all(minTime - window, maxTime + window);
        const byId = new Map(reattributeFileSaves(context, window).map((r) => [r.id, r]));
        return json(res, 200, selected.map((r) => byId.get(r.id) || r));
      }

      if (req.method === 'GET' && p === '/api/wakatime-days') {
        const rows = db.prepare('SELECT * FROM wakatime_days ORDER BY date').all();
        return json(res, 200, rows);
      }

      json(res, 404, { error: 'not found' });
    } catch (err) {
      if (!res.headersSent) json(res, err.statusCode || 500, { error: String(err.message || err) });
      else res.destroy();
    }
  });

  server.on('close', () => {
    try { db.close(); } catch { /* already closed */ }
  });

  server.listen(cfg.server.port, cfg.server.host, () => {
    console.log(`[stackhour] server listening on http://${cfg.server.host}:${cfg.server.port} (db: ${cfg.server.db})`);
    if (!cfg.server.token && Object.keys(cfg.server.tokens || {}).length === 0) {
      console.log('[stackhour] WARNING: no server tokens configured — ingest is open to anyone who can reach this port');
    }
  });
  return server;
}
