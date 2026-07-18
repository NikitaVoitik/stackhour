import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import vm from 'node:vm';
import { once } from 'node:events';
import { test } from 'node:test';

import { startServer } from '../src/server.js';

const SUMMARY = {
  capSeconds: 120,
  lastEventCreditSeconds: 60,
  reattributeWindowSeconds: 120,
  joinGapSeconds: 300,
};

async function withServer(run) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'stackhour-detail-'));
  const cfg = {
    server: {
      db: path.join(dir, 'stackhour.db'),
      host: '127.0.0.1',
      port: 0,
      token: 'detail-test-token',
    },
    summary: SUMMARY,
  };
  const server = startServer(cfg);
  await once(server, 'listening');
  const base = `http://127.0.0.1:${server.address().port}`;
  const ingest = async (rows) => {
    const response = await fetch(`${base}/api/ingest`, {
      method: 'POST',
      headers: {
        authorization: 'Bearer detail-test-token',
        'content-type': 'application/json',
      },
      body: JSON.stringify(rows),
    });
    assert.equal(response.status, 200);
    assert.equal((await response.json()).inserted, rows.length);
  };
  const detail = async (params) => {
    const query = new URLSearchParams(params);
    const response = await fetch(`${base}/api/detail?${query}`);
    return { response, body: await response.json() };
  };

  try {
    await run({ base, detail, ingest });
  } finally {
    server.close();
    await once(server, 'close');
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

function heartbeat(overrides = {}) {
  return {
    time: 100,
    machine: 'laptop',
    source: 'editor-files',
    project: 'alpha',
    entity: '/work/alpha/main.js',
    entity_type: 'file',
    category: 'coding',
    language: 'JavaScript',
    branch: 'main',
    is_write: 1,
    actor: 'human',
    ...overrides,
  };
}

test('/api/detail validates its selector while accepting an explicitly empty value', async () => {
  await withServer(async ({ base }) => {
    for (const suffix of [
      'value=alpha',
      'dimension=project',
      'dimension=&value=alpha',
      'dimension=not-a-field&value=alpha',
      'dimension=Project&value=alpha',
    ]) {
      const response = await fetch(`${base}/api/detail?${suffix}`);
      assert.equal(response.status, 400, suffix);
      assert.deepEqual(await response.json(), { error: 'dimension and value are required' });
    }

    const empty = await fetch(`${base}/api/detail?dimension=branch&value=&from=0&to=1000`);
    assert.equal(empty.status, 200);
    assert.equal((await empty.json()).value, '');
  });
});

test('/api/detail applies exact filters for every allowed dimension, including branch and null fields', async () => {
  await withServer(async ({ detail, ingest }) => {
    const selected = heartbeat({
      machine: 'machine<&"\'',
      source: 'codex-desktop',
      project: 'alpha',
      entity: '/work/alpha/<script>.js',
      category: 'debugging',
      language: 'TypeScript',
      branch: 'feature/detail',
      actor: 'agent',
      tokens_in: 7,
      tokens_out: 3,
      cost: 0.12,
    });
    await ingest([
      selected,
      heartbeat({
        time: 200,
        machine: 'other-machine',
        source: 'editor-files',
        project: 'alpha-longer',
        entity: '/work/other/main.js',
        category: 'coding',
        language: null,
        branch: null,
      }),
    ]);

    const selectors = {
      project: selected.project,
      source: selected.source,
      machine: selected.machine,
      category: selected.category,
      language: selected.language,
      entity: selected.entity,
      actor: selected.actor,
      branch: selected.branch,
    };
    for (const [dimension, value] of Object.entries(selectors)) {
      const { response, body } = await detail({ dimension, value, from: 0, to: 300 });
      assert.equal(response.status, 200, dimension);
      assert.equal(body.dimension, dimension);
      assert.equal(body.value, value);
      assert.equal(body.recent.length, 1, dimension);
      assert.equal(body.recent[0][dimension], value, dimension);
    }

    const project = (await detail({ dimension: 'project', value: 'alpha', from: 0, to: 300 })).body;
    assert.equal(project.recent.length, 1, 'project selection must not use prefix/substring matching');
    assert.equal(project.recent[0].project, 'alpha');

    for (const dimension of ['language', 'branch']) {
      const missing = (await detail({ dimension, value: '', from: 0, to: 300 })).body;
      assert.equal(missing.recent.length, 1, `${dimension}=empty selects the SQL null value`);
      assert.equal(missing.recent[0][dimension], null);

      const displayLabel = (await detail({ dimension, value: 'unknown', from: 0, to: 300 })).body;
      assert.equal(displayLabel.recent.length, 1, 'dashboard unknown label selects the same null bucket');
      assert.equal(displayLabel.recent[0][dimension], null);
    }
  });
});

test('/api/detail filters after global credit calculation and returns complete breakdown, segment, token, and cost data', async () => {
  await withServer(async ({ detail, ingest }) => {
    await ingest([
      heartbeat({ time: 100, project: 'alpha', entity: '/work/alpha/a.js', tokens_in: 1, tokens_out: 2, cost: 0.014 }),
      heartbeat({ time: 110, project: 'beta', entity: '/work/beta/b.js', tokens_in: 50, tokens_out: 50, cost: 9 }),
      heartbeat({ time: 120, project: 'alpha', entity: '/work/alpha/c.js', tokens_in: 3, tokens_out: 4, cost: 0.016 }),
      heartbeat({
        time: 130,
        machine: 'worker',
        source: 'codex-cli',
        project: 'alpha',
        entity: '/work/alpha/agent.js',
        actor: 'agent',
        branch: 'agent/topic',
        tokens_in: 100,
        tokens_out: 20,
        cost: 0.456,
      }),
    ]);

    const { body } = await detail({ dimension: 'project', value: 'alpha', from: 90, to: 200 });
    // The intervening beta heartbeat ends alpha's first human credit at 10s.
    // Filtering first would incorrectly produce 140s (20 + 60 + 60).
    assert.equal(body.total, 130);
    assert.equal(body.humanTotal, 70);
    assert.equal(body.agentTotal, 60);
    assert.equal(body.totalTokens, 130);
    assert.equal(body.totalCost, 0.49);
    assert.deepEqual(Object.keys(body.breakdowns).sort(),
      ['actor', 'branch', 'category', 'entity', 'language', 'machine', 'source'].sort());
    assert.equal('project' in body.breakdowns, false);

    assert.deepEqual(body.breakdowns.actor.map(({ actor, seconds, tokens, cost }) => ({ actor, seconds, tokens, cost })), [
      { actor: 'human', seconds: 70, tokens: 10, cost: 0.03 },
      { actor: 'agent', seconds: 60, tokens: 120, cost: 0.46 },
    ]);
    assert.deepEqual(body.breakdowns.branch.map(({ branch, seconds }) => ({ branch, seconds })), [
      { branch: 'main', seconds: 70 },
      { branch: 'agent/topic', seconds: 60 },
    ]);
    assert.deepEqual(body.segments.map(({ project, actor, start, end, seconds }) =>
      ({ project, actor, start, end, seconds })), [
      { project: 'alpha', actor: 'human', start: 100, end: 120, seconds: 70 },
      { project: 'alpha', actor: 'agent', start: 130, end: 130, seconds: 60 },
    ]);
  });
});

test('/api/detail recent rows are newest-first and capped at 50', async () => {
  await withServer(async ({ detail, ingest }) => {
    const rows = Array.from({ length: 55 }, (_, index) => heartbeat({
      time: 1000 + index,
      entity: `/work/alpha/${index}.js`,
    }));
    await ingest(rows);

    const { body } = await detail({ dimension: 'project', value: 'alpha', from: 900, to: 1100 });
    assert.equal(body.recent.length, 50);
    assert.deepEqual(body.recent.map((row) => row.time),
      Array.from({ length: 50 }, (_, index) => 1054 - index));
    assert.ok(body.recent.every((row) => row.project === 'alpha'));
  });
});

test('/api/detail uses the same lookaround reattribution for filtering, totals, breakdowns, and recent rows', async () => {
  await withServer(async ({ detail, ingest }) => {
    const entity = '/work/alpha/generated.js';
    await ingest([
      heartbeat({ time: 100, entity, source: 'editor-files', actor: 'human' }),
      heartbeat({ time: 101, machine: 'other', entity, source: 'editor-files', actor: 'human' }),
      heartbeat({ time: 219, entity, source: 'codex-desktop', actor: 'agent' }),
    ]);

    const agent = (await detail({ dimension: 'actor', value: 'agent', from: 90, to: 110 })).body;
    assert.equal(agent.total, 60);
    assert.equal(agent.humanTotal, 0);
    assert.equal(agent.agentTotal, 60);
    assert.equal(agent.recent.length, 1);
    assert.equal(agent.recent[0].time, 100);
    assert.equal(agent.recent[0].actor, 'agent');
    assert.equal(agent.recent[0].source, 'codex-desktop');
    assert.deepEqual(agent.breakdowns.source.map(({ source, seconds }) => ({ source, seconds })), [
      { source: 'codex-desktop', seconds: 60 },
    ]);

    const source = (await detail({ dimension: 'source', value: 'codex-desktop', from: 90, to: 110 })).body;
    assert.equal(source.total, agent.total);
    assert.deepEqual(source.recent, agent.recent);

    const human = (await detail({ dimension: 'actor', value: 'human', from: 90, to: 110 })).body;
    assert.equal(human.total, 60);
    assert.deepEqual(human.recent.map((row) => row.machine), ['other']);
  });
});

test('dashboard drill-down markup compiles, escapes hostile values, and wires row clicks through URLSearchParams', async () => {
  await withServer(async ({ base }) => {
    const response = await fetch(base);
    assert.equal(response.status, 200);
    const html = await response.text();
    for (const id of ['detailCard', 'detailTitle', 'detailClose', 'detail']) {
      assert.match(html, new RegExp(`id=["']${id}["']`));
    }
    assert.match(html, /<option value="branch">branch<\/option>/);

    const script = html.match(/<script>([\s\S]*?)<\/script>/)?.[1];
    assert.ok(script, 'dashboard has an inline script');
    assert.doesNotThrow(() => new vm.Script(script, { filename: 'dashboard-inline.js' }));
    assert.match(script, /new URLSearchParams\(\{\s*days,\s*dimension,\s*value\s*\}\)/);
    assert.match(script, /closest\('\[data-detail-dimension\]'\)/);
    assert.match(script, /openDetail\(row\.dataset\.detailDimension,\s*row\.dataset\.detailValue\)/);
    assert.match(script, /detailClose['"]\)\.onclick/);
    assert.match(script, /const fields = \[[^\]]*'category'[^\]]*'branch'[^\]]*\]\.filter/);

    const helpersStart = script.indexOf('const fmt =');
    const helpersEnd = script.indexOf('let activeDetail');
    const rowsStart = script.indexOf('function detailRows');
    const rowsEnd = script.indexOf('async function openDetail');
    assert.ok(helpersStart >= 0 && helpersEnd > helpersStart && rowsEnd > rowsStart);
    const context = { input: `<img src=x onerror="alert(1)">&'`, output: null };
    vm.runInNewContext(`${script.slice(helpersStart, helpersEnd)}\noutput = esc(input);`, context);
    assert.equal(context.output, '&lt;img src=x onerror=&quot;alert(1)&quot;&gt;&amp;&#39;');

    const renderContext = { output: null };
    vm.runInNewContext(
      `${script.slice(helpersStart, helpersEnd)}\n${script.slice(rowsStart, rowsEnd)}\n`
      + `output = detailRows([{project:'<img src=x onerror="alert(1)">',seconds:60}], 'project');`,
      renderContext,
    );
    assert.doesNotMatch(renderContext.output, /<img/);
    assert.match(renderContext.output, /&lt;img src=x onerror=&quot;alert\(1\)&quot;&gt;/);

    // Title assignment avoids HTML interpretation, while all detail table values
    // and labels pass through the common escaping helper.
    assert.match(script, /detailTitle['"]\)\.textContent/);
    assert.match(script, /title="\$\{esc\(row\.entity\)\}"/);
    assert.match(script, /\$\{esc\(row\.actor\)\}/);
  });
});
