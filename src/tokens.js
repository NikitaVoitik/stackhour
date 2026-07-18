import fs from 'node:fs';
import { CONFIG_PATH } from './config.js';
import { generateToken, optionValues, writeConfig } from './setup.js';

function readServerConfig(configPath) {
  let config;
  try { config = JSON.parse(fs.readFileSync(configPath, 'utf8')); }
  catch (err) { throw new Error(`cannot read config: ${err.message}`); }
  if (!config.server) throw new Error('server is not initialized');
  if (config.server.tokens === undefined || config.server.tokens === null) config.server.tokens = {};
  if (typeof config.server.tokens !== 'object' || Array.isArray(config.server.tokens)) {
    throw new Error('server.tokens must be an object keyed by machine name');
  }
  return config;
}

function machineName(value) {
  const machine = String(value || '').trim();
  if (!machine || machine.length > 200 || /[\u0000-\u001f\u007f]/.test(machine)) {
    throw new Error('machine must be 1-200 printable characters');
  }
  return machine;
}

export function createMachineToken(machine, { configPath = CONFIG_PATH, force = false, token = generateToken() } = {}) {
  const name = machineName(machine);
  const config = readServerConfig(configPath);
  if (Object.hasOwn(config.server.tokens, name) && !force) {
    throw new Error(`token for ${name} already exists; pass --force to rotate it`);
  }
  const secret = String(token || '').trim();
  if (!secret) throw new Error('token cannot be empty');
  if (Object.entries(config.server.tokens).some(([other, value]) => other !== name && value === secret)) {
    throw new Error('token is already assigned to another machine');
  }
  config.server.tokens[name] = secret;
  writeConfig(configPath, config);
  return { machine: name, token: secret };
}

export function revokeMachineToken(machine, { configPath = CONFIG_PATH } = {}) {
  const name = machineName(machine);
  const config = readServerConfig(configPath);
  if (!Object.hasOwn(config.server.tokens, name)) throw new Error(`no token exists for ${name}`);
  delete config.server.tokens[name];
  writeConfig(configPath, config);
  return { machine: name };
}

export function listMachineTokens({ configPath = CONFIG_PATH } = {}) {
  const config = readServerConfig(configPath);
  return Object.keys(config.server.tokens).sort();
}

export function runToken(args, { configPath = CONFIG_PATH, stdout = process.stdout } = {}) {
  const command = args[0];
  const machine = args[1];
  if (command === 'create') {
    const result = createMachineToken(machine, {
      configPath,
      force: args.includes('--force'),
      token: optionValues(args, 'token').at(-1),
    });
    stdout.write(`Token for ${result.machine}: ${result.token}\n`);
    return result;
  }
  if (command === 'revoke') {
    const result = revokeMachineToken(machine, { configPath });
    stdout.write(`Revoked token for ${result.machine}\n`);
    return result;
  }
  if (command === 'list') {
    const machines = listMachineTokens({ configPath });
    for (const name of machines) stdout.write(`${name}\n`);
    return machines;
  }
  throw new Error('usage: stackhour token <create MACHINE|revoke MACHINE|list>');
}
