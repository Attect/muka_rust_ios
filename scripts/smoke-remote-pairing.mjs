import { execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import net from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const bin = resolve(process.argv[2] ?? 'target/debug/ios.exe');
if (!existsSync(bin)) throw new Error(`ios binary not found: ${bin}`);

const home = mkdtempSync(join(tmpdir(), 'rust-ios-remote-pairing-'));
const serverPort = await freePort();
const common = ['--server-home', home, '--server-addr', `127.0.0.1:${serverPort}`];
const env = {
  ...process.env,
  RUST_IOS_BACKEND: 'mock',
  RUST_IOS_MOCK_DEVICES: 'mock-phone',
};

function run(args, expectedStatus = 0) {
  try {
    const stdout = execFileSync(bin, [...args, ...common], {
      env,
      encoding: 'utf8',
      timeout: 15000,
      windowsHide: true,
      stdio: ['ignore', 'pipe', 'pipe'],
    }).trim();
    if (expectedStatus !== 0) throw new Error(`command unexpectedly succeeded: ${args.join(' ')}`);
    return stdout ? JSON.parse(stdout) : null;
  } catch (error) {
    if (error.status !== expectedStatus) throw error;
    return JSON.parse(String(error.stderr).trim());
  }
}

function delay(ms) {
  return new Promise(resolveDelay => setTimeout(resolveDelay, ms));
}

async function waitTask(id, state) {
  const deadline = Date.now() + 8000;
  let task;
  while (Date.now() < deadline) {
    task = run(['task', 'show', id]).value.task;
    if (task.runtime_state === state) return task;
    await delay(75);
  }
  throw new Error(`task ${id} did not reach ${state}: ${JSON.stringify(task)}`);
}

function tunnelEcho(endpoint, text) {
  return new Promise((resolveEcho, reject) => {
    const [host, rawPort] = endpoint.split(':');
    const socket = net.createConnection({ host, port: Number(rawPort) });
    const chunks = [];
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error(`tunnel echo timed out on ${endpoint}`));
    }, 4000);
    socket.once('error', error => {
      clearTimeout(timer);
      reject(error);
    });
    socket.once('connect', () => {
      const address = Buffer.alloc(16);
      address[15] = 1;
      const port = Buffer.alloc(4);
      port.writeUInt32LE(8100);
      socket.write(Buffer.concat([address, port, Buffer.from(text)]));
    });
    socket.on('data', chunk => {
      chunks.push(chunk);
      if (Buffer.concat(chunks).length >= Buffer.byteLength(text)) {
        clearTimeout(timer);
        socket.end();
        resolveEcho(Buffer.concat(chunks).toString('utf8'));
      }
    });
  });
}

try {
  let task = run([
    'tunnel', 'start', '--remote-pairing', '--userspace-port=0', '--udid', 'mock-phone',
  ]).value.task;
  task = await waitTask(task.id, 'failed');
  if (!task.message.includes('RemotePairing identity is missing')) {
    throw new Error(`missing identity did not fail explicitly: ${JSON.stringify(task)}`);
  }
  run(['task', 'stop', task.id]);

  const denied = run(['tunnel', 'pair', '--udid', 'mock-phone'], 3);
  if (denied.error?.code !== 'invalid_request') {
    throw new Error(`manual authorization was not required: ${JSON.stringify(denied)}`);
  }

  const pairing = run(['tunnel', 'pair', '--manual', '--udid', 'mock-phone']);
  const identityPath = join(home, 'remote-pairing', 'selfIdentity.plist');
  if (pairing.udid !== 'mock-phone' || pairing.identity_path !== identityPath || !existsSync(identityPath)) {
    throw new Error(`unexpected pairing result: ${JSON.stringify(pairing)}`);
  }
  const stateAfterPair = readFileSync(join(home, 'state-v1.json'), 'utf8');
  if (stateAfterPair.includes('mock-remote-pairing-identity')) {
    throw new Error('RemotePairing identity leaked into task state');
  }
  if (run(['task', 'list']).value.tasks.length !== 0) {
    throw new Error('one-shot pairing created a persistent task');
  }

  task = run([
    'tunnel', 'start', '--remote-pairing', '--userspace-port=0', '--udid', 'mock-phone',
  ]).value.task;
  task = await waitTask(task.id, 'running');
  if (task.spec.configuration.transport !== 'remote_pairing_tcp') {
    throw new Error(`wrong tunnel transport: ${JSON.stringify(task.spec)}`);
  }
  if (await tunnelEcho(task.runtime.userspaceEndpoint, 'remote-before-restart') !== 'remote-before-restart') {
    throw new Error('RemotePairing tunnel echo failed before restart');
  }

  run(['kill-server']);
  const restored = await waitTask(task.id, 'running');
  if (restored.generation !== 2) {
    throw new Error(`RemotePairing task did not restore: ${JSON.stringify(restored)}`);
  }
  if (await tunnelEcho(restored.runtime.userspaceEndpoint, 'remote-after-restart') !== 'remote-after-restart') {
    throw new Error('RemotePairing tunnel echo failed after restart');
  }

  run(['task', 'stop', task.id]);
  if (run(['task', 'list']).value.tasks.length !== 0) throw new Error('tunnel task leaked');
  run(['kill-server']);
  await assertPortAvailable(serverPort);
  console.log(`REMOTE_PAIRING_RESTORED task=${task.id} generation=${restored.generation}`);
  console.log('REMOTE_PAIRING_PROCESS_SMOKE_OK');
} finally {
  if (existsSync(join(home, 'server.json'))) {
    try { run(['kill-server']); } catch {}
  }
  rmSync(home, { recursive: true, force: true });
}

function freePort() {
  return new Promise((resolvePort, reject) => {
    const server = net.createServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const port = server.address().port;
      server.close(error => error ? reject(error) : resolvePort(port));
    });
  });
}

function assertPortAvailable(port) {
  return new Promise((resolveAvailable, reject) => {
    const server = net.createServer();
    server.once('error', reject);
    server.listen(port, '127.0.0.1', () => {
      server.close(error => error ? reject(error) : resolveAvailable());
    });
  });
}
