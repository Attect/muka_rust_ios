import { execFile, execFileSync } from 'node:child_process';
import { promisify } from 'node:util';
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import net from 'node:net';

const execFileAsync = promisify(execFile);
const bin = resolve(process.argv[2] ?? 'target/debug/ios.exe');
const home = mkdtempSync(join(tmpdir(), 'rust-ios-resilience-smoke-'));
const serverPort = await freePort();
const common = ['--server-home', home, '--server-addr', `127.0.0.1:${serverPort}`];
const recordFile = join(home, 'recording.jsonl');
const env = {
  ...process.env,
  RUST_IOS_BACKEND: 'mock',
  RUST_IOS_MOCK_DEVICES: 'mock-phone',
  RUST_IOS_RECORD_FILE: recordFile,
};

function commandArgs(args) {
  return [...args, ...common];
}

function run(args) {
  const output = execFileSync(bin, commandArgs(args), {
    env,
    encoding: 'utf8',
    timeout: 15000,
    windowsHide: true,
    stdio: ['ignore', 'pipe', 'pipe'],
  }).trim();
  return output ? JSON.parse(output) : null;
}

async function runAsync(args) {
  const { stdout } = await execFileAsync(bin, commandArgs(args), {
    env,
    encoding: 'utf8',
    timeout: 15000,
    windowsHide: true,
  });
  return stdout.trim() ? JSON.parse(stdout) : null;
}

function expectFailure(args, code) {
  try {
    run(args);
  } catch (error) {
    const payload = JSON.parse(String(error.stderr).trim());
    if (payload.error.code !== code) {
      throw new Error(`expected ${code}, got ${JSON.stringify(payload)}`);
    }
    return payload;
  }
  throw new Error(`command unexpectedly succeeded: ${args.join(' ')}`);
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

function holdPort() {
  return new Promise((resolveServer, reject) => {
    const server = net.createServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => resolveServer(server));
  });
}

function closeServer(server) {
  return new Promise((resolveClose, reject) => {
    server.close(error => error ? reject(error) : resolveClose());
  });
}

function delay(ms) {
  return new Promise(resolveDelay => setTimeout(resolveDelay, ms));
}

async function waitTask(id, expected = 'running') {
  const deadline = Date.now() + 6000;
  let task;
  do {
    task = run(['task', 'show', id]).value.task;
    if (task.runtime_state === expected) return task;
    await delay(75);
  } while (Date.now() < deadline);
  throw new Error(`task ${id} did not reach ${expected}: ${JSON.stringify(task)}`);
}

function echo(port, text) {
  return new Promise((resolveEcho, reject) => {
    const socket = net.createConnection({ host: '127.0.0.1', port });
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error(`echo timeout on ${port}`));
    }, 4000);
    socket.once('error', error => {
      clearTimeout(timer);
      reject(error);
    });
    socket.once('connect', () => socket.write(text));
    socket.once('data', data => {
      clearTimeout(timer);
      socket.end();
      resolveEcho(data.toString('utf8'));
    });
  });
}

try {
  run(['list']);
  const clients = await Promise.all(
    Array.from({ length: 16 }, (_, index) =>
      runAsync(index % 2 === 0 ? ['list', '--details'] : ['server', 'status'])
    )
  );
  if (clients.length !== 16) throw new Error('concurrent client count mismatch');
  console.log('CONCURRENT_CLIENTS=16');

  const blocker = await holdPort();
  const blockedPort = blocker.address().port;
  expectFailure(['forward', String(blockedPort), '8100', '--udid', 'mock-phone'], 'conflict');
  const tasksAfterConflict = run(['task', 'list']).value.tasks;
  if (tasksAfterConflict.length !== 0) throw new Error('failed forward leaked a task');
  await closeServer(blocker);
  console.log(`PORT_CONFLICT port=${blockedPort} leakedTasks=0`);

  const forward = run([
    'forward', String(blockedPort), '8100', '--udid', 'mock-phone'
  ]).value.tasks[0];
  const syslog = run(['syslog', '--udid', 'mock-phone']).value.task;
  await waitTask(syslog.id);
  await delay(125);
  const before = run(['task', 'output', syslog.id]).value.total_bytes;
  if (await echo(blockedPort, 'before-force-kill') !== 'before-force-kill') {
    throw new Error('forward failed before force kill');
  }

  run(['readpair', '--udid', 'mock-phone']);
  const status = run(['server', 'status']).value;
  process.kill(status.pid, 'SIGKILL');
  await delay(750);

  const restored = run(['task', 'list']).value.tasks;
  if (restored.length !== 2) throw new Error(`expected 2 restored tasks, got ${restored.length}`);
  await waitTask(forward.id);
  await waitTask(syslog.id);
  if (await echo(blockedPort, 'after-force-kill') !== 'after-force-kill') {
    throw new Error('forward failed after force kill');
  }
  await delay(125);
  const after = run(['task', 'output', syslog.id]).value.total_bytes;
  if (after <= before) throw new Error('syslog did not resume after force kill');
  console.log(`FORCE_KILL_RESTORE tasks=2 syslogBefore=${before} syslogAfter=${after}`);

  const recording = readFileSync(recordFile, 'utf8');
  if (!recording.includes('read_pair_record') || !recording.includes('"redacted":true')) {
    throw new Error('recording does not contain redacted PairRecord evidence');
  }
  if (recording.includes('mock-host')) throw new Error('recording leaked PairRecord material');
  console.log('RECORDING_REDACTION=ok');

  run(['task', 'stop', 'all']);
  run(['kill-server']);
  await delay(300);
  console.log('RESILIENCE_SMOKE_OK');
} finally {
  if (existsSync(join(home, 'server.json'))) {
    try { run(['kill-server']); } catch {}
    await delay(250);
  }
  rmSync(home, { recursive: true, force: true });
}
