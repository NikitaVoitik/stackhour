// Bridge installer/operator CLI, invoked as `stackhour bridge <command> <role>`.
import { spawnSync } from 'node:child_process';
import { accessSync, chmodSync, constants, copyFileSync, existsSync, mkdirSync, readFileSync, realpathSync, renameSync, statSync, writeFileSync } from 'node:fs';
import { homedir, platform, userInfo } from 'node:os';
import { dirname, join } from 'node:path';
import { createInterface } from 'node:readline/promises';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import {
  LAUNCHD_LABEL, SERVICE_NAME, binaryPath, findExecutable, mergedPath,
  renderLaunchAgent, renderSystemdUnit, shellQuote,
  validateCoordinatorConfig, validateWorkerConfig,
} from './setup-lib.mjs';

const ENTRY_DIR = dirname(fileURLToPath(import.meta.url));
const HOME = homedir();
const DEFAULT_RUNTIME = join(HOME, '.local', 'share', 'stackhour', 'bridge');
const ROLES = new Set(['coordinator', 'worker']);

function usage(code = 0) {
  const out = code ? console.error : console.log;
  out(`stackhour bridge — install and operate the Telegram Claude + Codex bridge

Usage:
  stackhour bridge install <coordinator|worker> [--runtime-dir PATH] [--reconfigure] [--no-start]
  stackhour bridge doctor <coordinator|worker> [--runtime-dir PATH]
  stackhour bridge status <coordinator|worker>
  stackhour bridge restart <coordinator|worker>

Non-interactive setup:
  Add --non-interactive and provide the environment variables documented in README.md.`);
  process.exit(code);
}

let values, command, role, runtimeDir;

async function ask(label, defaultValue = '') {
  if (values['non-interactive']) return defaultValue;
  const rl = createInterface({ input: process.stdin, output: process.stdout });
  const suffix = defaultValue ? ` [${defaultValue}]` : '';
  const answer = (await rl.question(`${label}${suffix}: `)).trim();
  rl.close();
  return answer || defaultValue;
}

async function askRequired(label, envName, defaultValue = '') {
  const value = process.env[envName] || await ask(label, defaultValue);
  if (!value) throw new Error(`${label} is required (set ${envName} for non-interactive setup).`);
  return value;
}

async function askSecret(label, envName, optional = false) {
  if (process.env[envName] !== undefined) return process.env[envName];
  if (values['non-interactive']) {
    if (optional) return '';
    throw new Error(`${envName} is required for non-interactive setup.`);
  }
  if (!process.stdin.isTTY || !process.stdout.isTTY || !process.stdin.setRawMode) {
    throw new Error(`Cannot read ${label} securely here; set ${envName} and retry.`);
  }
  process.stdout.write(`${label}${optional ? ' (optional)' : ''}: `);
  process.stdin.setRawMode(true);
  process.stdin.resume();
  process.stdin.setEncoding('utf8');
  return new Promise((resolve, reject) => {
    let value = '';
    const finish = (error) => {
      process.stdin.off('data', onData);
      process.stdin.setRawMode(false);
      process.stdin.pause();
      process.stdout.write('\n');
      if (error) reject(error);
      else if (!value && !optional) reject(new Error(`${label} is required.`));
      else resolve(value);
    };
    const onData = (chunk) => {
      for (const char of [...chunk]) {
        if (char === '\u0003') return finish(new Error('Setup cancelled.'));
        if (char === '\r' || char === '\n') return finish();
        if (char === '\u007f' || char === '\b') {
          const chars = [...value];
          if (chars.length) { chars.pop(); value = chars.join(''); process.stdout.write('\b \b'); }
        } else if (char >= ' ') {
          value += char;
          process.stdout.write('*');
        }
      }
    };
    process.stdin.on('data', onData);
  });
}

function run(program, args, { allowFailure = false, inherit = true } = {}) {
  const result = spawnSync(program, args, { encoding: 'utf8', stdio: inherit ? 'inherit' : 'pipe' });
  if (result.error && !allowFailure) throw result.error;
  if ((result.status ?? 1) !== 0 && !allowFailure) throw new Error(`${program} ${args.join(' ')} failed.`);
  return result;
}

function ensureDir(path, mode = 0o700) {
  mkdirSync(path, { recursive: true, mode });
  chmodSync(path, mode);
}

function writeAtomic(path, content, mode = 0o600) {
  mkdirSync(dirname(path), { recursive: true });
  const tmp = `${path}.tmp-${process.pid}`;
  writeFileSync(tmp, content, { mode });
  chmodSync(tmp, mode);
  renameSync(tmp, path);
}

function copyRuntime(roleName) {
  const files = roleName === 'coordinator'
    ? ['coordinator.mjs', 'claim.mjs', 'return.mjs', 'tg-send.mjs']
    : ['worker.mjs', 'tg-send.mjs'];
  ensureDir(runtimeDir);
  for (const file of files) {
    const source = join(ENTRY_DIR, file);
    const destination = join(runtimeDir, file);
    if (realpathOrSelf(source) !== realpathOrSelf(destination)) copyFileSync(source, destination);
    chmodSync(destination, 0o755);
  }
}

function realpathOrSelf(path) {
  try { return realpathSync(path); } catch { return path; }
}

function loadConfig(roleName) {
  const name = roleName === 'coordinator' ? 'config.json' : 'worker-config.json';
  const path = join(runtimeDir, name);
  if (!existsSync(path)) return { path, config: null };
  return { path, config: JSON.parse(readFileSync(path, 'utf8')) };
}

function saveConfig(path, config) {
  if (existsSync(path)) {
    const backup = `${path}.backup-${new Date().toISOString().replace(/[:.]/g, '-')}`;
    copyFileSync(path, backup);
    chmodSync(backup, 0o600);
    console.log(`Backed up existing config to ${backup}`);
  }
  writeAtomic(path, `${JSON.stringify(config, null, 2)}\n`);
}

async function executablePrompt(label, envName, commandName) {
  const detected = process.env[envName] || findExecutable(commandName) || '';
  const value = await askRequired(label, envName, detected);
  const resolved = findExecutable(value);
  if (!resolved) throw new Error(`${label} is not executable: ${value}`);
  return resolved;
}

function ensureWorkdir(path) {
  try { if (statSync(path).isDirectory()) return; } catch {}
  throw new Error(`Working directory does not exist: ${path}`);
}

async function coordinatorConfig() {
  const token = await askSecret('Telegram bot token', 'TELEGRAM_BOT_TOKEN');
  const rawChatId = await askRequired('Authorized Telegram chat ID', 'TELEGRAM_CHAT_ID');
  const chatId = Number(rawChatId);
  if (!Number.isSafeInteger(chatId)) throw new Error('Telegram chat ID must be an integer.');
  const cwd = await askRequired('Linux agent working directory', 'BRIDGE_WORKDIR', process.cwd());
  ensureWorkdir(cwd);
  const claudeBin = await executablePrompt('Claude Code executable', 'CLAUDE_BIN', 'claude');
  const codexBin = await executablePrompt('Codex executable', 'CODEX_BIN', 'codex');
  const permissionMode = await ask('Permission mode (default or bypassPermissions)', process.env.BRIDGE_PERMISSION_MODE || 'default');
  if (!['default', 'bypassPermissions'].includes(permissionMode)) throw new Error('Invalid permission mode.');
  const elevenLabsApiKey = await askSecret('ElevenLabs API key', 'ELEVENLABS_API_KEY', true);
  const extraPath = mergedPath(binaryPath(claudeBin), binaryPath(codexBin), process.env.PATH);
  return {
    token,
    chatId,
    defaultTarget: 'gcp',
    maxMediaBytes: 512 * 1024 * 1024,
    elevenLabsApiKey,
    targets: {
      gcp: { label: 'Linux', type: 'local', cwd, claudeBin, codexBin, extraPath, permissionMode, model: null, codexModel: null },
      mac: { label: 'Mac', type: 'remote', permissionMode },
    },
  };
}

async function workerConfig() {
  if (platform() !== 'darwin') throw new Error('The worker installer currently targets macOS.');
  const gcpSsh = await askRequired('Linux SSH destination (user@host)', 'BRIDGE_GCP_SSH');
  const sshUser = gcpSsh.includes('@') ? gcpSsh.split('@')[0] : userInfo().username;
  const defaultRemote = sshUser === 'root' ? '/root/.local/share/stackhour/bridge' : `/home/${sshUser}/.local/share/stackhour/bridge`;
  const gcpKey = await askRequired('SSH private key', 'BRIDGE_GCP_KEY', join(HOME, '.ssh', 'id_ed25519'));
  try { accessSync(gcpKey, constants.R_OK); } catch { throw new Error(`SSH key is not readable: ${gcpKey}`); }
  const remoteDir = await askRequired('Remote bridge runtime directory', 'BRIDGE_REMOTE_DIR', defaultRemote);
  const remoteNode = await askRequired('Remote Node.js executable', 'BRIDGE_REMOTE_NODE', '/usr/local/bin/node');
  const cwd = await askRequired('Mac agent working directory', 'BRIDGE_WORKDIR', process.cwd());
  ensureWorkdir(cwd);
  const claudeBin = await executablePrompt('Claude Code executable', 'CLAUDE_BIN', 'claude');
  const codexBin = await executablePrompt('Codex executable', 'CODEX_BIN', 'codex');
  const permissionMode = await ask('Permission mode (default or bypassPermissions)', process.env.BRIDGE_PERMISSION_MODE || 'default');
  if (!['default', 'bypassPermissions'].includes(permissionMode)) throw new Error('Invalid permission mode.');
  return {
    gcpSsh, gcpKey, remoteDir, remoteNode, claudeBin, codexBin, cwd,
    extraPath: mergedPath(binaryPath(claudeBin), binaryPath(codexBin), process.env.PATH),
    permissionMode, model: null, codexModel: null,
  };
}

function installCoordinatorService(config, start) {
  if (platform() !== 'linux') throw new Error('The coordinator service installer requires Linux with systemd.');
  const systemctl = findExecutable('systemctl');
  const nodePath = findExecutable('node');
  if (!systemctl) throw new Error('systemctl was not found.');
  if (!nodePath) throw new Error('Node.js was not found.');
  const unitDir = join(HOME, '.config', 'systemd', 'user');
  ensureDir(unitDir);
  const unitPath = join(unitDir, SERVICE_NAME);
  writeAtomic(unitPath, renderSystemdUnit({
    nodePath, runtimeDir, home: HOME, pathValue: config.targets.gcp.extraPath,
  }), 0o600);
  run(systemctl, ['--user', 'daemon-reload']);
  if (start) run(systemctl, ['--user', 'enable', '--now', SERVICE_NAME]);
  console.log(`Installed user service: ${unitPath}`);
  const loginctl = findExecutable('loginctl');
  if (loginctl) {
    const linger = run(loginctl, ['show-user', userInfo().username, '-p', 'Linger', '--value'], { allowFailure: true, inherit: false });
    if (linger.stdout?.trim() !== 'yes') {
      console.log(`Note: run "sudo loginctl enable-linger ${userInfo().username}" once to keep the coordinator running after logout.`);
    }
  }
}

function installWorkerService(config, start) {
  const nodePath = findExecutable('node');
  if (!nodePath) throw new Error('Node.js was not found.');
  const agents = join(HOME, 'Library', 'LaunchAgents');
  ensureDir(agents, 0o755);
  const plistPath = join(agents, `${LAUNCHD_LABEL}.plist`);
  writeAtomic(plistPath, renderLaunchAgent({
    nodePath, runtimeDir, home: HOME, pathValue: config.extraPath,
  }), 0o600);
  run('plutil', ['-lint', plistPath]);
  if (start) {
    const domain = `gui/${process.getuid()}`;
    run('launchctl', ['bootout', `${domain}/${LAUNCHD_LABEL}`], { allowFailure: true });
    run('launchctl', ['bootstrap', domain, plistPath]);
    run('launchctl', ['kickstart', '-k', `${domain}/${LAUNCHD_LABEL}`]);
  }
  console.log(`Installed LaunchAgent: ${plistPath}`);
}

async function install(roleName) {
  const current = loadConfig(roleName);
  let config = current.config;
  const validate = roleName === 'coordinator' ? validateCoordinatorConfig : validateWorkerConfig;
  if (!config || values.reconfigure) {
    config = roleName === 'coordinator' ? await coordinatorConfig() : await workerConfig();
    const errors = validate(config);
    if (errors.length) throw new Error(errors.join('\n'));
    saveConfig(current.path, config);
  } else {
    const errors = validate(config);
    if (errors.length) throw new Error(`Existing config is invalid:\n- ${errors.join('\n- ')}`);
    console.log(`Reusing ${current.path}; pass --reconfigure to replace it.`);
  }
  copyRuntime(roleName);
  if (roleName === 'coordinator') installCoordinatorService(config, !values['no-start']);
  else installWorkerService(config, !values['no-start']);
  console.log(`\n✓ ${roleName} installed in ${runtimeDir}`);
  console.log(`  Run: stackhour bridge doctor ${roleName}`);
}

function check(condition, message, failures) {
  console.log(`${condition ? '✓' : '✗'} ${message}`);
  if (!condition) failures.push(message);
}

function privateMode(path) {
  try { return (statSync(path).mode & 0o077) === 0; } catch { return false; }
}

function isDirectory(path) {
  try { return statSync(path).isDirectory(); } catch { return false; }
}

function isExecutable(path) {
  try { accessSync(path, constants.X_OK); return true; } catch { return false; }
}

function doctor(roleName) {
  const failures = [];
  const major = Number(process.versions.node.split('.')[0]);
  check(major >= 22, `Node.js >=22 (${process.version})`, failures);
  const { path, config } = loadConfig(roleName);
  check(Boolean(config), `Config exists: ${path}`, failures);
  if (!config) return 1;
  const errors = roleName === 'coordinator' ? validateCoordinatorConfig(config) : validateWorkerConfig(config);
  check(errors.length === 0, errors.length ? `Config validation: ${errors.join('; ')}` : 'Config validation', failures);
  check(privateMode(path), 'Config permissions exclude group/other access', failures);
  const local = roleName === 'coordinator' ? config.targets.gcp : config;
  check(isDirectory(local.cwd), `Working directory: ${local.cwd}`, failures);
  check(isExecutable(local.claudeBin), `Claude Code executable: ${local.claudeBin}`, failures);
  check(isExecutable(local.codexBin), `Codex executable: ${local.codexBin}`, failures);
  const runtimeFiles = roleName === 'coordinator'
    ? ['coordinator.mjs', 'claim.mjs', 'return.mjs']
    : ['worker.mjs'];
  for (const file of runtimeFiles) check(isExecutable(join(runtimeDir, file)), `Installed ${file}`, failures);
  if (roleName === 'coordinator' && platform() === 'linux') {
    const active = run('systemctl', ['--user', 'is-active', '--quiet', SERVICE_NAME], { allowFailure: true, inherit: false });
    check(active.status === 0, `User service active: ${SERVICE_NAME}`, failures);
  }
  if (roleName === 'worker') {
    check(existsSync(config.gcpKey) && privateMode(config.gcpKey), `Private SSH key: ${config.gcpKey}`, failures);
    const remoteCheck = `test -x ${shellQuote(config.remoteNode)} && test -f ${shellQuote(join(config.remoteDir, 'claim.mjs'))} && test -f ${shellQuote(join(config.remoteDir, 'return.mjs'))}`;
    const ssh = run('ssh', [
      '-i', config.gcpKey, '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8',
      config.gcpSsh, remoteCheck,
    ], { allowFailure: true, inherit: false });
    check(ssh.status === 0, `SSH and remote coordinator helpers: ${config.gcpSsh}`, failures);
    if (platform() === 'darwin') {
      const loaded = run('launchctl', ['print', `gui/${process.getuid()}/${LAUNCHD_LABEL}`], { allowFailure: true, inherit: false });
      check(loaded.status === 0, `LaunchAgent loaded: ${LAUNCHD_LABEL}`, failures);
    }
  }
  console.log(failures.length ? `\n${failures.length} check(s) failed.` : '\n✓ Ready.');
  return failures.length ? 1 : 0;
}

function serviceAction(roleName, action) {
  if (roleName === 'coordinator') {
    if (platform() !== 'linux') throw new Error('Coordinator service commands require Linux.');
    run('systemctl', ['--user', action, SERVICE_NAME]);
  } else {
    if (platform() !== 'darwin') throw new Error('Worker service commands require macOS.');
    const target = `gui/${process.getuid()}/${LAUNCHD_LABEL}`;
    if (action === 'status') run('launchctl', ['print', target]);
    else run('launchctl', ['kickstart', '-k', target]);
  }
}

export async function runBridgeCli(args) {
  ({ values, positionals: [command, role] = [] } = parseArgs({
    args,
    allowPositionals: true,
    options: {
      'runtime-dir': { type: 'string' },
      reconfigure: { type: 'boolean', default: false },
      'no-start': { type: 'boolean', default: false },
      'non-interactive': { type: 'boolean', default: false },
      help: { type: 'boolean', short: 'h', default: false },
    },
  }));
  if (values.help) usage();
  if (!['install', 'doctor', 'status', 'restart'].includes(command) || !ROLES.has(role)) usage(1);
  runtimeDir = values['runtime-dir'] || process.env.STACKHOUR_BRIDGE_HOME || DEFAULT_RUNTIME;

  try {
    if (command === 'install') await install(role);
    else if (command === 'doctor') process.exitCode = doctor(role);
    else serviceAction(role, command);
  } catch (error) {
    console.error(`\nError: ${error.message}`);
    process.exitCode = 1;
  }
}
