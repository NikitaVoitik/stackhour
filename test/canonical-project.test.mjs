import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { afterEach, test } from 'node:test';

import { gitRemote, normalizeGitRemote, projectRoot, resolveProject } from '../src/project.js';
import { watchClaude } from '../src/agent/watch-claude.js';
import { watchFiles } from '../src/agent/watch-files.js';

const tempDirs = [];

function tempDir() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'stackhour-project-test-'));
  tempDirs.push(dir);
  return dir;
}

function writeRepository(root, remotes = []) {
  const gitDir = path.join(root, '.git');
  fs.mkdirSync(gitDir, { recursive: true });
  fs.writeFileSync(path.join(gitDir, 'HEAD'), 'ref: refs/heads/main\n');
  fs.writeFileSync(path.join(gitDir, 'config'), remotes.map(({ name, url }) => [
    `[remote "${name}"]`,
    `\turl = ${url}`,
    '\tfetch = +refs/heads/*:refs/remotes/origin/*',
  ].join('\n')).join('\n'));
  return gitDir;
}

afterEach(() => {
  for (const dir of tempDirs.splice(0)) fs.rmSync(dir, { recursive: true, force: true });
});

test('normalizes HTTPS, SSH URL, and scp-style Git remotes to one identity', () => {
  const expected = 'github.com/Acme/widget';
  assert.equal(normalizeGitRemote('https://github.com/Acme/widget.git'), expected);
  assert.equal(normalizeGitRemote('ssh://git@github.com/Acme/widget.git'), expected);
  assert.equal(normalizeGitRemote('git@github.com:Acme/widget.git'), expected);
  assert.equal(normalizeGitRemote('github.com:Acme/widget/'), expected);
});

test('normalization strips only transport syntax and rejects unusable remotes', () => {
  assert.equal(normalizeGitRemote('  HTTPS://GitHub.COM/Owner/Repo.GIT/  '), 'github.com/Owner/Repo');
  assert.equal(normalizeGitRemote('https://git.example.test/groups/team/repo.git'), 'git.example.test/groups/team/repo');
  assert.equal(normalizeGitRemote('file:///srv/git/repo.git'), null);
  assert.equal(normalizeGitRemote('/srv/git/repo.git'), null);
  assert.equal(normalizeGitRemote(''), null);
  assert.equal(normalizeGitRemote(null), null);
});

test('reads origin in preference to other remotes regardless of config order', () => {
  const root = path.join(tempDir(), 'checkout');
  writeRepository(root, [
    { name: 'upstream', url: 'https://github.com/canonical/upstream.git' },
    { name: 'origin', url: 'git@github.com:personal/fork.git' },
  ]);
  assert.equal(gitRemote(root), 'github.com/personal/fork');
  assert.equal(resolveProject(root), 'personal/fork');
});

test('uses the first configured remote when origin is absent', () => {
  const root = path.join(tempDir(), 'checkout');
  writeRepository(root, [
    { name: 'company', url: 'ssh://git@gitlab.example.test/platform/service.git' },
    { name: 'backup', url: 'https://backup.example.test/archive/service.git' },
  ]);
  assert.equal(gitRemote(path.join(root, 'src', 'deep')), 'gitlab.example.test/platform/service');
  assert.equal(resolveProject(path.join(root, 'src', 'deep')), 'platform/service');
});

test('finds repository roots from nested files and directories', () => {
  const root = path.join(tempDir(), 'checkout');
  const nested = path.join(root, 'src', 'feature');
  const file = path.join(nested, 'index.js');
  writeRepository(root, [{ name: 'origin', url: 'https://github.com/acme/widget.git' }]);
  fs.mkdirSync(nested, { recursive: true });
  fs.writeFileSync(file, 'export default 1;\n');

  assert.equal(projectRoot(root), root);
  assert.equal(projectRoot(nested), root);
  assert.equal(projectRoot(file), root);
  assert.equal(resolveProject(file), 'acme/widget');
});

test('worktrees read the remote from the common Git directory', () => {
  const dir = tempDir();
  const checkout = path.join(dir, 'topic-checkout');
  const common = path.join(dir, 'main.git');
  const worktreeGitDir = path.join(common, 'worktrees', 'topic');
  fs.mkdirSync(checkout, { recursive: true });
  fs.mkdirSync(worktreeGitDir, { recursive: true });
  fs.writeFileSync(path.join(checkout, '.git'), `gitdir: ${path.relative(checkout, worktreeGitDir)}\n`);
  fs.writeFileSync(path.join(worktreeGitDir, 'commondir'), '../..\n');
  fs.writeFileSync(path.join(worktreeGitDir, 'HEAD'), 'ref: refs/heads/topic\n');
  fs.writeFileSync(path.join(common, 'config'), [
    '[remote "origin"]',
    '\turl = git@github.com:Acme/worktree-app.git',
  ].join('\n'));

  assert.equal(projectRoot(checkout), checkout);
  assert.equal(gitRemote(checkout), 'github.com/Acme/worktree-app');
  assert.equal(resolveProject(path.join(checkout, 'not-created-yet.js')), 'Acme/worktree-app');
});

test('aliases resolve by repository path, nested input path, remote, short remote, and name', () => {
  const dir = tempDir();
  const root = path.join(dir, 'renamed-checkout');
  const nested = path.join(root, 'src');
  writeRepository(root, [{ name: 'origin', url: 'git@github.com:Acme/widget.git' }]);
  fs.mkdirSync(nested, { recursive: true });

  assert.equal(resolveProject(nested, { projectAliases: { [root]: 'path-canonical' } }), 'path-canonical');
  assert.equal(resolveProject(nested, { projectAliases: { [nested]: 'nested-canonical' } }), 'nested-canonical');
  assert.equal(resolveProject(root, { projectAliases: { 'github.com/Acme/widget': 'remote-canonical' } }), 'remote-canonical');
  assert.equal(resolveProject(root, { projectAliases: { 'Acme/widget': 'short-remote-canonical' } }), 'short-remote-canonical');
  assert.equal(resolveProject('/workspace/local-label', { projectAliases: { 'local-label': 'name-canonical' } }), 'name-canonical');
});

test('alias keys match case-insensitively and exact path aliases win over remote aliases', () => {
  const root = path.join(tempDir(), 'Checkout');
  writeRepository(root, [{ name: 'origin', url: 'https://GitHub.com/Acme/Widget.git' }]);
  const aliases = {
    [root.toUpperCase()]: 'by-path',
    'GITHUB.COM/ACME/WIDGET': 'by-remote',
    'acme/widget': 'by-short-remote',
  };
  assert.equal(resolveProject(root, { projectAliases: aliases }), 'by-path');
  assert.equal(resolveProject(root, { projectAliases: { 'GITHUB.COM/ACME/WIDGET': 'by-remote' } }), 'by-remote');
});

test('non-Git locations fall back predictably without throwing', () => {
  const missing = path.join('/', `.stackhour-no-repo-${process.pid}-${Date.now()}`, 'plain-project');
  assert.equal(projectRoot(missing), null);
  assert.equal(gitRemote(missing), null);
  assert.equal(resolveProject(missing), 'plain-project');
  assert.equal(resolveProject('display label'), 'display label');
  assert.equal(resolveProject('', {}, 'explicit-fallback'), 'explicit-fallback');
  assert.equal(resolveProject(null), 'unknown');
});

test('remote owner keeps same-basename repositories from colliding', () => {
  const dir = tempDir();
  const one = path.join(dir, 'one', 'app');
  const two = path.join(dir, 'two', 'app');
  writeRepository(one, [{ name: 'origin', url: 'https://github.com/alpha/app.git' }]);
  writeRepository(two, [{ name: 'origin', url: 'https://github.com/beta/app.git' }]);

  assert.equal(path.basename(one), path.basename(two));
  assert.equal(resolveProject(one), 'alpha/app');
  assert.equal(resolveProject(two), 'beta/app');
  assert.notEqual(resolveProject(one), resolveProject(two));
});

test('file watcher emits canonical remote identity and honors aliases', async () => {
  const dir = tempDir();
  const projects = path.join(dir, 'projects');
  const project = path.join(projects, 'local-folder');
  const source = path.join(project, 'src', 'index.js');
  writeRepository(project, [{ name: 'origin', url: 'git@github.com:Acme/widget.git' }]);
  fs.mkdirSync(path.dirname(source), { recursive: true });
  fs.writeFileSync(source, 'export default 1;\n');
  const now = Date.now() / 1000;
  fs.utimesSync(source, now, now);
  const cfg = {
    agent: {
      projectRoots: [projects],
      projectAliases: { 'github.com/Acme/widget': 'widget-canonical' },
      ignoreDirs: [],
      maxScanDepth: 8,
      intervalSeconds: 20,
    },
  };

  const rows = await watchFiles(cfg, { filesLastScan: now - 1 });
  assert.equal(rows.length, 1);
  assert.equal(rows[0].project, 'widget-canonical');
  assert.equal(rows[0].entity, source);
});

test('Claude watcher canonicalizes transcript cwd through agent aliases', async () => {
  const dir = tempDir();
  const projectsDir = path.join(dir, 'claude-projects');
  const repo = path.join(dir, 'checkout');
  const transcript = path.join(projectsDir, 'session.jsonl');
  writeRepository(repo, [{ name: 'origin', url: 'ssh://git@github.com/Acme/widget.git' }]);
  fs.mkdirSync(projectsDir, { recursive: true });
  fs.writeFileSync(transcript, `${JSON.stringify({ type: 'seed' })}\n`);
  const state = {};
  const cfg = { agent: { projectAliases: { 'Acme/widget': 'canonical-widget' } } };
  const now = Date.parse('2026-07-18T12:00:00Z') / 1000;
  assert.deepEqual(await watchClaude(cfg, state, { projectsDir, now }), []);

  fs.appendFileSync(transcript, `${JSON.stringify({
    type: 'user',
    timestamp: '2026-07-18T11:59:59Z',
    cwd: repo,
    message: { content: 'implement it' },
  })}\n`);
  const rows = await watchClaude(cfg, state, { projectsDir, now });
  assert.equal(rows.length, 1);
  assert.equal(rows[0].project, 'canonical-widget');
  assert.equal(rows[0].actor, 'human');
});
