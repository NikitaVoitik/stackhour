import fs from 'node:fs';
import path from 'node:path';

function gitDirFor(root) {
  const dotGit = path.join(root, '.git');
  try {
    if (fs.statSync(dotGit).isDirectory()) return dotGit;
    const match = fs.readFileSync(dotGit, 'utf8').trim().match(/^gitdir:\s*(.+)$/i);
    if (!match) return null;
    return path.resolve(root, match[1]);
  } catch { return null; }
}

function repositoryRoot(location) {
  let current;
  try {
    const stat = fs.statSync(location);
    current = stat.isDirectory() ? path.resolve(location) : path.dirname(path.resolve(location));
  } catch {
    current = path.resolve(location);
  }
  while (true) {
    if (gitDirFor(current)) return current;
    const parent = path.dirname(current);
    if (parent === current) return null;
    current = parent;
  }
}

function commonGitDir(gitDir) {
  try {
    const relative = fs.readFileSync(path.join(gitDir, 'commondir'), 'utf8').trim();
    return path.resolve(gitDir, relative);
  } catch { return gitDir; }
}

function remoteFromConfig(config) {
  let section = '';
  const remotes = [];
  for (const raw of config.split(/\r?\n/)) {
    const line = raw.trim();
    const header = line.match(/^\[remote\s+"([^"]+)"\]$/i);
    if (header) { section = header[1]; continue; }
    if (line.startsWith('[')) { section = ''; continue; }
    const url = section && line.match(/^url\s*=\s*(.+)$/i);
    if (url) remotes.push({ name: section, url: url[1].trim() });
  }
  return (remotes.find((remote) => remote.name === 'origin') || remotes[0])?.url || null;
}

export function normalizeGitRemote(remote) {
  if (!remote) return null;
  let host;
  let pathname;
  const scp = String(remote).trim().match(/^(?:[^@/\s]+@)?([^:/\s]+):(.+)$/);
  if (scp && !String(remote).includes('://')) {
    [, host, pathname] = scp;
  } else {
    try {
      const parsed = new URL(String(remote).trim());
      host = parsed.hostname;
      pathname = parsed.pathname;
    } catch { return null; }
  }
  const cleanPath = String(pathname || '').replace(/^\/+|\/+$/g, '').replace(/\.git$/i, '');
  if (!host || !cleanPath) return null;
  return `${host.toLowerCase()}/${cleanPath}`;
}

export function gitRemote(location) {
  const root = repositoryRoot(location);
  if (!root) return null;
  const gitDir = gitDirFor(root);
  try {
    return normalizeGitRemote(remoteFromConfig(fs.readFileSync(path.join(commonGitDir(gitDir), 'config'), 'utf8')));
  } catch { return null; }
}

function aliasFor(aliases, candidates) {
  for (const candidate of candidates) {
    if (!candidate) continue;
    if (Object.hasOwn(aliases, candidate)) return String(aliases[candidate]);
    const insensitive = Object.keys(aliases).find((key) => key.toLowerCase() === String(candidate).toLowerCase());
    if (insensitive) return String(aliases[insensitive]);
  }
  return null;
}

export function resolveProject(location, agentConfig = {}, fallback = null) {
  const value = String(location || '').trim();
  const aliases = agentConfig.projectAliases || {};
  const root = value && path.isAbsolute(value) ? repositoryRoot(value) : null;
  const remote = root ? gitRemote(root) : null;
  const remoteProject = remote?.split('/').slice(-2).join('/') || null;
  const pathFallback = root ? path.basename(root) : (value ? path.basename(value) : null);
  const name = fallback || pathFallback || 'unknown';
  return aliasFor(aliases, [value && path.resolve(value), root, remote, remoteProject, name])
    || remoteProject
    || name;
}

export function projectRoot(location) {
  return repositoryRoot(location);
}
