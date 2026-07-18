import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { openDb, insertHeartbeats } from './db.js';
import { computeCredits, totalsBy, dayBuckets, buildSegments } from './summarize.js';

const __dirname = path.dirname(fileURLToPath(import.meta.url));

function readBody(req, limit = 5 * 1024 * 1024) {
  return new Promise((resolve, reject) => {
    let size = 0;
    const chunks = [];
    req.on('data', (c) => {
      size += c.length;
      if (size > limit) { reject(new Error('body too large')); req.destroy(); return; }
      chunks.push(c);
    });
    req.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
    req.on('error', reject);
  });
}

function json(res, status, obj) {
  const body = JSON.stringify(obj);
  res.writeHead(status, { 'content-type': 'application/json' });
  res.end(body);
}

function authOk(req, url, token) {
  if (!token) return true; // no token configured -> open (use on trusted networks only)
  const header = req.headers.authorization || '';
  if (header === `Bearer ${token}`) return true;
  // wakatime plugins send: Basic base64(api_key)
  if (header.startsWith('Basic ')) {
    try {
      const decoded = Buffer.from(header.slice(6), 'base64').toString('utf8');
      if (decoded === token || decoded === `${token}:`) return true;
    } catch { /* fall through */ }
  }
  if (url.searchParams.get('api_key') === token) return true;
  return false;
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
    actor: String(h.category || '').includes('ai') ? 'agent' : 'human',
  };
}

// A file save observed by the generic file watcher may actually be an agent's
// edit (Claude/Codex writing files triggers mtime changes too). If an agent
// reported touching the same file within the window, hand the save to it.
function reattributeFileSaves(rows, windowSeconds = 120) {
  const agentEdits = rows.filter((r) => r.actor === 'agent' && r.entity_type === 'file');
  if (!agentEdits.length) return rows;
  return rows.map((r) => {
    if (r.source !== 'editor-files') return r;
    const match = agentEdits.find((a) => a.entity === r.entity
      && Math.abs(a.time - r.time) <= windowSeconds);
    return match ? { ...r, actor: 'agent', source: match.source } : r;
  });
}

export function startServer(cfg) {
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
        json(res, 200, { ok: true });
        return;
      }

      // ---- ingest (tempo agents) ----
      if (req.method === 'POST' && p === '/api/ingest') {
        if (!authOk(req, url, cfg.server.token)) return json(res, 401, { error: 'unauthorized' });
        const rows = JSON.parse(await readBody(req));
        if (!Array.isArray(rows)) return json(res, 400, { error: 'expected array' });
        const inserted = insertHeartbeats(db, rows);
        return json(res, 200, { inserted, received: rows.length });
      }

      // ---- wakatime-protocol ingest (official editor plugins pointed at api_url) ----
      if (req.method === 'POST' && (p === '/api/v1/users/current/heartbeats'
        || p === '/api/v1/users/current/heartbeats.bulk'
        || p === '/users/current/heartbeats'
        || p === '/users/current/heartbeats.bulk')) {
        if (!authOk(req, url, cfg.server.token)) return json(res, 401, { error: 'unauthorized' });
        const body = JSON.parse(await readBody(req));
        const items = Array.isArray(body) ? body : [body];
        const machine = req.headers['x-machine-name'] || 'unknown';
        const rows = items.map((h) => fromWakatime(h, req.headers['user-agent'], machine));
        insertHeartbeats(db, rows);
        // official API returns 201/202 with a responses array for bulk
        return json(res, 202, {
          responses: items.map((h) => [{ data: { id: null, entity: h.entity, time: h.time } }, 201]),
        });
      }

      // ---- queries ----
      if (req.method === 'GET' && p === '/api/summary') {
        const days = Math.min(Number(url.searchParams.get('days') || 1), 366);
        const to = Number(url.searchParams.get('to') || Date.now() / 1000);
        const from = Number(url.searchParams.get('from') || to - days * 86400);
        const groupBy = (url.searchParams.get('groupBy') || 'project')
          .split(',').filter((k) => ['project', 'source', 'machine', 'category', 'language', 'entity', 'actor'].includes(k));
        const tz = Number(url.searchParams.get('tz') || 0);
        const raw = db.prepare('SELECT * FROM heartbeats WHERE time >= ? AND time <= ?').all(from, to);
        const rows = reattributeFileSaves(raw, cfg.summary.reattributeWindowSeconds);
        const credited = computeCredits(rows, cfg.summary);
        const total = Math.round(credited.reduce((a, r) => a + r.credit, 0));
        const humanTotal = Math.round(credited.filter((r) => r.actor !== 'agent').reduce((a, r) => a + r.credit, 0));
        return json(res, 200, {
          from, to, total, humanTotal, agentTotal: total - humanTotal,
          totals: totalsBy(credited, groupBy.length ? groupBy : ['project']),
          days: dayBuckets(credited, groupBy.length ? groupBy : ['project'], tz),
        });
      }

      if (req.method === 'GET' && p === '/api/timeline') {
        const hours = Math.min(Number(url.searchParams.get('hours') || 24), 24 * 14);
        const to = Number(url.searchParams.get('to') || Date.now() / 1000);
        const from = to - hours * 3600;
        const raw = db.prepare('SELECT * FROM heartbeats WHERE time >= ? AND time <= ?').all(from, to);
        const rows = reattributeFileSaves(raw, cfg.summary.reattributeWindowSeconds);
        const credited = computeCredits(rows, cfg.summary);
        return json(res, 200, { from, to, segments: buildSegments(credited, cfg.summary) });
      }

      if (req.method === 'GET' && p === '/api/recent') {
        const limit = Math.min(Number(url.searchParams.get('limit') || 50), 500);
        const rows = db.prepare('SELECT * FROM heartbeats ORDER BY time DESC LIMIT ?').all(limit);
        return json(res, 200, rows);
      }

      if (req.method === 'GET' && p === '/api/wakatime-days') {
        const rows = db.prepare('SELECT * FROM wakatime_days ORDER BY date').all();
        return json(res, 200, rows);
      }

      json(res, 404, { error: 'not found' });
    } catch (err) {
      json(res, 500, { error: String(err.message || err) });
    }
  });

  server.listen(cfg.server.port, cfg.server.host, () => {
    console.log(`[tempo] server listening on http://${cfg.server.host}:${cfg.server.port} (db: ${cfg.server.db})`);
    if (!cfg.server.token) console.log('[tempo] WARNING: no server.token configured — ingest is open to anyone who can reach this port');
  });
  return server;
}
