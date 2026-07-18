import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { CONFIG_PATH, expandHome, resolveStoragePaths } from './config.js';
import { installService } from './install.js';

function readExisting(configPath) {
  try { return JSON.parse(fs.readFileSync(configPath, 'utf8')); }
  catch (err) {
    if (err.code === 'ENOENT') return {};
    throw new Error(`cannot read existing config: ${err.message}`);
  }
}

export function writeConfig(configPath, config) {
  fs.mkdirSync(path.dirname(configPath), { recursive: true });
  const tmp = `${configPath}.${process.pid}.tmp`;
  try {
    fs.rmSync(tmp, { force: true });
    const fd = fs.openSync(tmp, 'wx', 0o600);
    try {
      fs.writeFileSync(fd, `${JSON.stringify(config, null, 2)}\n`);
      fs.fsyncSync(fd);
    } finally { fs.closeSync(fd); }
    fs.renameSync(tmp, configPath);
    fs.chmodSync(configPath, 0o600);
    let dirFd;
    try { dirFd = fs.openSync(path.dirname(configPath), 'r'); fs.fsyncSync(dirFd); }
    catch { /* directory fsync is unavailable on some platforms */ }
    finally { if (dirFd !== undefined) fs.closeSync(dirFd); }
  } finally { fs.rmSync(tmp, { force: true }); }
}

export function generateToken() {
  return crypto.randomBytes(32).toString('base64url');
}

function validPort(port) {
  const value = Number(port);
  if (!Number.isInteger(value) || value < 1 || value > 65535) throw new Error('port must be an integer from 1 to 65535');
  return value;
}

export function validUrl(serverUrl) {
  let parsed;
  try { parsed = new URL(serverUrl); } catch { throw new Error('server URL must be a valid http(s) URL'); }
  if (!['http:', 'https:'].includes(parsed.protocol)) throw new Error('server URL must use http or https');
  return parsed.href.replace(/\/$/, '');
}

function resolvedRoots(projectRoots) {
  return projectRoots.map((root) => {
    try {
      const resolved = fs.realpathSync(path.resolve(expandHome(String(root))));
      if (!fs.statSync(resolved).isDirectory()) throw new Error('not a directory');
      return resolved;
    } catch { throw new Error(`invalid project root (must be an existing directory): ${root}`); }
  });
}

export function createEnrollment({ serverUrl, machine, token }) {
  const payload = {
    v: 1,
    serverUrl: validUrl(serverUrl),
    machine: String(machine || '').trim(),
    token: String(token || '').trim(),
  };
  if (!payload.machine) throw new Error('enrollment machine cannot be empty');
  if (!payload.token) throw new Error('enrollment token cannot be empty');
  return Buffer.from(JSON.stringify(payload)).toString('base64url');
}

export function parseEnrollment(value) {
  let payload;
  try {
    const encoded = String(value || '');
    if (!encoded || encoded.length > 16_384 || !/^[A-Za-z0-9_-]+$/.test(encoded)) throw new Error('invalid encoding');
    payload = JSON.parse(Buffer.from(encoded, 'base64url').toString('utf8'));
  } catch { throw new Error('invalid enrollment code'); }
  if (!payload || payload.v !== 1 || typeof payload !== 'object' || Array.isArray(payload)) {
    throw new Error('unsupported enrollment code');
  }
  return {
    serverUrl: validUrl(payload.serverUrl),
    machine: String(payload.machine || '').trim(),
    token: String(payload.token || '').trim(),
  };
}

export function initServer({
  configPath = CONFIG_PATH,
  force = false,
  host = '0.0.0.0',
  port = 4040,
  publicUrl,
  machine = os.hostname(),
  token = generateToken(),
  projectRoots = [],
} = {}) {
  const cleanHost = String(host || '').trim();
  const cleanMachine = String(machine || '').trim();
  const cleanToken = String(token || '').trim();
  if (!cleanHost) throw new Error('host cannot be empty');
  if (!cleanMachine) throw new Error('machine name cannot be empty');
  if (!cleanToken) throw new Error('token cannot be empty');
  const existing = readExisting(configPath);
  if (existing.server && !force) throw new Error('server config already exists; pass --force to replace it');
  const storage = resolveStoragePaths({ ...process.env, STACKHOUR_CONFIG: configPath }, os.homedir());
  const cleanPort = validPort(port);
  const cleanPublicUrl = validUrl(publicUrl || `http://127.0.0.1:${cleanPort}`);
  const server = {
    host: cleanHost,
    port: cleanPort,
    publicUrl: cleanPublicUrl,
    db: storage.dbPath,
    tokens: { [cleanMachine]: cleanToken },
  };
  const config = { ...existing, server };
  // A server commonly runs its own agent. Configure it on first setup while
  // preserving an explicitly initialized agent section.
  if (!existing.agent) {
    config.agent = {
      serverUrl: `http://127.0.0.1:${server.port}`,
      token: cleanToken,
      machine: cleanMachine,
      projectRoots: resolvedRoots(projectRoots),
    };
  }
  writeConfig(configPath, config);
  return { configPath, server, agent: config.agent, token: cleanToken };
}

export function initAgent({
  configPath = CONFIG_PATH,
  force = false,
  serverUrl,
  token,
  machine = os.hostname(),
  projectRoots = [],
  enrollment,
} = {}) {
  if (enrollment) {
    if (serverUrl || token || machine !== os.hostname()) throw new Error('do not combine --enrollment with server URL, token, or machine');
    ({ serverUrl, token, machine } = parseEnrollment(enrollment));
  }
  if (!serverUrl) throw new Error('--server-url is required');
  const cleanToken = String(token || '').trim();
  if (!cleanToken) throw new Error('--token is required');
  const existing = readExisting(configPath);
  if (existing.agent && !force) throw new Error('agent config already exists; pass --force to replace it');
  const agent = {
    serverUrl: validUrl(serverUrl),
    token: cleanToken,
    machine: String(machine || '').trim(),
    projectRoots: resolvedRoots(projectRoots),
  };
  if (!agent.machine) throw new Error('machine name cannot be empty');
  const config = { ...existing, agent };
  writeConfig(configPath, config);
  return { configPath, agent };
}

export function optionValues(args, name) {
  const prefix = `--${name}=`;
  return args.filter((arg) => arg.startsWith(prefix)).map((arg) => arg.slice(prefix.length));
}

export function runInit(args, { configPath = CONFIG_PATH, stdout = process.stdout, installer = installService } = {}) {
  const role = args[0];
  const value = (name) => optionValues(args, name).at(-1);
  const common = { configPath, force: args.includes('--force') };
  let result;
  if (role === 'server') {
    result = initServer({
      ...common,
      host: value('host'),
      port: value('port'),
      publicUrl: value('public-url'),
      machine: value('machine'),
      projectRoots: optionValues(args, 'project-root'),
    });
    stdout.write(`Created server config at ${result.configPath}\n`);
    stdout.write(`Public URL: ${result.server.publicUrl}\n`);
    stdout.write(`Local agent enrolled as ${result.agent.machine}\n`);
    if (args.includes('--install')) {
      installer('server');
      installer('agent');
      stdout.write('Installed and started stackhour-server and stackhour-agent\n');
    }
    stdout.write('Next: ./bin/stackhour token create <machine>\n');
  } else if (role === 'agent') {
    const enrollment = value('enrollment');
    result = initAgent({
      ...common,
      serverUrl: value('server-url'),
      token: enrollment ? undefined : (value('token') || process.env.STACKHOUR_TOKEN),
      machine: value('machine'),
      projectRoots: optionValues(args, 'project-root'),
      enrollment,
    });
    stdout.write(`Created agent config at ${result.configPath}\n`);
    stdout.write(`Enrolled ${result.agent.machine} with ${result.agent.serverUrl}\n`);
    if (args.includes('--install')) {
      installer('agent');
      stdout.write('Installed and started stackhour-agent\n');
    }
    stdout.write('Next: ./bin/stackhour doctor\n');
  } else {
    throw new Error('usage: stackhour init <server|agent> [options]');
  }
  return result;
}
