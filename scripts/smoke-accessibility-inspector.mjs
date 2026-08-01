import { execFileSync } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import net from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const bin = resolve(process.argv[2] ?? 'target/debug/ios.exe');
if (!existsSync(bin)) throw new Error(`ios binary not found: ${bin}`);

const root = mkdtempSync(join(tmpdir(), 'rust-ios-ax-'));
const home = join(root, 'home');
const serverPort = await freePort();
const common = ['--server-home', home, '--server-addr', `127.0.0.1:${serverPort}`];
const env = {
  ...process.env,
  RUST_IOS_BACKEND: 'mock',
  RUST_IOS_MOCK_DEVICES: 'mock-phone',
};

function run(args) {
  const stdout = execFileSync(bin, [...args, ...common], {
    cwd: root,
    env,
    encoding: 'utf8',
    timeout: 15000,
    windowsHide: true,
    stdio: ['ignore', 'pipe', 'pipe'],
  }).trim();
  return stdout ? JSON.parse(stdout) : null;
}

function fail(args) {
  try {
    run(args);
  } catch (error) {
    return {
      status: error.status,
      value: JSON.parse(String(error.stderr).trim()),
    };
  }
  throw new Error(`command unexpectedly succeeded: ${args.join(' ')}`);
}

function delay(ms) {
  return new Promise(resolveDelay => setTimeout(resolveDelay, ms));
}

async function waitTask(id, state, generation = null) {
  const deadline = Date.now() + 10000;
  let task;
  while (Date.now() < deadline) {
    task = run(['task', 'show', id]).value.task;
    if (task.runtime_state === state && (generation === null || task.generation === generation)) {
      return task;
    }
    await delay(75);
  }
  throw new Error(`task ${id} did not reach ${state}/${generation}: ${JSON.stringify(task)}`);
}

async function waitOutput(id, minimumBytes, startedCount = 1) {
  const deadline = Date.now() + 10000;
  let output;
  while (Date.now() < deadline) {
    output = run(['task', 'output', id]).value;
    const text = Buffer.from(output.bytes_base64, 'base64').toString('utf8');
    const lines = text.trim().split('\n').filter(Boolean).map(line => JSON.parse(line));
    const starts = lines.filter(line => line.event === 'inspector_started').length;
    if (output.total_bytes >= minimumBytes && starts >= startedCount
        && lines.some(line => line.selector === 'hostInspectorCurrentElementChanged:')) {
      return { output, lines };
    }
    await delay(75);
  }
  throw new Error(`task ${id} output did not become ready: ${JSON.stringify(output)}`);
}

try {
  let task = run(['ax', '--udid', 'mock-phone']).value.task;
  task = await waitTask(task.id, 'running', 1);
  if (task.kind !== 'accessibility_inspector'
      || task.spec.kind !== 'accessibility_inspector') {
    throw new Error(`unexpected AX task: ${JSON.stringify(task)}`);
  }
  const first = await waitOutput(task.id, 1, 1);
  if (first.output.content_type !== 'application/x-ndjson') {
    throw new Error(`unexpected AX content type: ${first.output.content_type}`);
  }

  const conflict = fail(['ax', '--udid', 'mock-phone']);
  if (conflict.value.error?.code !== 'conflict') {
    throw new Error(`second AX task was not rejected: ${JSON.stringify(conflict)}`);
  }

  run(['kill-server']);
  await delay(300);
  task = await waitTask(task.id, 'running', 2);
  const restored = await waitOutput(task.id, first.output.total_bytes + 1, 2);

  run(['task', 'restart', task.id]);
  task = await waitTask(task.id, 'running', 3);
  const restarted = await waitOutput(task.id, restored.output.total_bytes + 1, 3);
  const listed = run(['task', 'list', '--kind', 'ax']).value.tasks;
  if (listed.length !== 1 || listed[0].id !== task.id) {
    throw new Error(`AX task kind filtering failed: ${JSON.stringify(listed)}`);
  }

  run(['task', 'stop', task.id]);
  const remaining = run(['task', 'list', '--kind', 'accessibility_inspector']).value.tasks;
  if (remaining.length !== 0) {
    throw new Error(`AX task remained after stop: ${JSON.stringify(remaining)}`);
  }
  console.log(
    `ACCESSIBILITY_INSPECTOR_SMOKE_OK task=${task.id} generation=${task.generation}`
      + ` bytes=${restarted.output.total_bytes}`,
  );
} finally {
  try {
    run(['kill-server']);
  } catch {
    // The isolated server may already be gone.
  }
  rmSync(root, { recursive: true, force: true });
}

function freePort() {
  return new Promise((resolvePort, reject) => {
    const server = net.createServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const address = server.address();
      server.close(error => {
        if (error) reject(error);
        else resolvePort(address.port);
      });
    });
  });
}
