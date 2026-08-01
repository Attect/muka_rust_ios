import { execFileSync } from 'node:child_process';
import { once } from 'node:events';
import {
  existsSync,
  mkdirSync,
  readFileSync,
} from 'node:fs';
import net from 'node:net';
import { isAbsolute, join, resolve } from 'node:path';

const bin = resolve(process.argv[2] ?? 'target/release/ios.exe');
const serverRoot = resolve(
  process.argv[3] ?? 'target/process-tests/dproxy-protocol-119-final',
);
const serverPort = Number(process.argv[4] ?? '60241');

if (!existsSync(bin)) throw new Error(`ios binary not found: ${bin}`);
if (!isAbsolute(serverRoot)) throw new Error(`server root must be absolute: ${serverRoot}`);
if (existsSync(serverRoot)) {
  throw new Error(`refusing to reuse or delete evidence directory: ${serverRoot}`);
}
if (!Number.isInteger(serverPort) || serverPort < 1 || serverPort > 65535) {
  throw new Error(`invalid server port: ${serverPort}`);
}
await assertPortAvailable(serverPort);
mkdirSync(serverRoot, { recursive: true });

const common = [
  '--server-home', serverRoot,
  '--server-addr', `127.0.0.1:${serverPort}`,
];
const env = Object.fromEntries(
  [
    'SystemRoot',
    'WINDIR',
    'PATH',
    'PATHEXT',
    'TEMP',
    'TMP',
  ]
    .filter(name => process.env[name] !== undefined)
    .map(name => [name, process.env[name]]),
);
env.RUST_IOS_BACKEND = 'mock';
env.RUST_IOS_MOCK_DEVICES = 'mock-phone';

const upstreamSockets = new Set();
const upstream = net.createServer(socket => {
  upstreamSockets.add(socket);
  socket.on('data', bytes => socket.write(bytes));
  socket.on('end', () => socket.end());
  socket.on('close', () => upstreamSockets.delete(socket));
});
await listen(upstream, 0);
const upstreamPort = upstream.address().port;
const masterOutput = join(serverRoot, 'dproxy-events.ndjson');
const binaryOutput = join(serverRoot, 'dproxy-binary-events.ndjson');

function run(args, expectedStatus = 0) {
  try {
    const stdout = execFileSync(bin, [...args, ...common], {
      env,
      encoding: 'utf8',
      timeout: 15000,
      windowsHide: true,
      stdio: ['ignore', 'pipe', 'pipe'],
    }).trim();
    if (expectedStatus !== 0) {
      throw new Error(`command unexpectedly succeeded: ${args.join(' ')}`);
    }
    return stdout ? JSON.parse(stdout) : null;
  } catch (error) {
    if (error.status !== expectedStatus) throw error;
    return JSON.parse(String(error.stderr).trim());
  }
}

function delay(milliseconds) {
  return new Promise(resolveDelay => setTimeout(resolveDelay, milliseconds));
}

async function waitTask(id, predicate, description) {
  const deadline = Date.now() + 8000;
  let task;
  while (Date.now() < deadline) {
    task = run(['task', 'show', id]).value.task;
    if (predicate(task)) return task;
    await delay(50);
  }
  throw new Error(`task ${id} did not reach ${description}: ${JSON.stringify(task)}`);
}

async function waitOutputContains(id, needle) {
  const deadline = Date.now() + 8000;
  let text = '';
  while (Date.now() < deadline) {
    const output = run(['task', 'output', id]).value;
    text = Buffer.from(output.bytes_base64, 'base64').toString('utf8');
    if (text.includes(needle)) return { output, text };
    await delay(50);
  }
  throw new Error(`task ${id} output did not contain ${needle}: ${text}`);
}

function usbmuxdPacket(messageType, tag) {
  const payload = Buffer.from(
    `<?xml version="1.0" encoding="UTF-8"?>\n` +
      '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" ' +
      '"http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n' +
      `<plist version="1.0"><dict><key>MessageType</key><string>${messageType}</string>` +
      '</dict></plist>',
    'utf8',
  );
  const header = Buffer.alloc(16);
  header.writeUInt32LE(header.length + payload.length, 0);
  header.writeUInt32LE(1, 4);
  header.writeUInt32LE(8, 8);
  header.writeUInt32LE(tag, 12);
  return Buffer.concat([header, payload]);
}

function openEcho(port, payload, closeAfterEcho) {
  return new Promise((resolveEcho, reject) => {
    const socket = net.createConnection({ host: '127.0.0.1', port });
    const chunks = [];
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error(`dproxy echo timed out on ${port}`));
    }, 5000);
    socket.once('error', error => {
      clearTimeout(timer);
      reject(error);
    });
    socket.once('connect', () => socket.write(payload));
    socket.on('data', chunk => {
      chunks.push(chunk);
      const received = Buffer.concat(chunks);
      if (received.length >= payload.length) {
        clearTimeout(timer);
        socket.removeAllListeners('data');
        if (!received.subarray(0, payload.length).equals(payload)) {
          socket.destroy();
          reject(new Error('dproxy echo payload mismatch'));
          return;
        }
        if (closeAfterEcho) socket.end();
        resolveEcho(socket);
      }
    });
  });
}

async function assertSocketCloses(socket) {
  if (socket.destroyed) return;
  await Promise.race([
    once(socket, 'close'),
    delay(3000).then(() => {
      throw new Error('active dproxy client did not close after task stop');
    }),
  ]);
}

let primaryTaskId;
try {
  run(['start-server']);
  const status = run(['server', 'status']).value;
  if (status.protocol_major !== 1 || status.protocol_minor !== 19) {
    throw new Error(`unexpected protocol: ${JSON.stringify(status)}`);
  }

  let task = run([
    'dproxy',
    '--mode=usbmuxd',
    '--bind=127.0.0.1',
    '--port=0',
    `--upstream=127.0.0.1:${upstreamPort}`,
    `--output=${masterOutput}`,
  ]).value.task;
  primaryTaskId = task.id;
  task = await waitTask(
    task.id,
    current => current.runtime_state === 'running',
    'running',
  );
  const dproxyPort = task.runtime.port;
  const firstPacket = usbmuxdPacket('ListDevices', 41);
  await openEcho(dproxyPort, firstPacket, true);
  task = await waitTask(
    task.id,
    current => current.runtime.connectionsCompleted === 1,
    'one completed connection',
  );
  const firstOutput = await waitOutputContains(task.id, 'connection_closed');
  if (firstOutput.output.content_type !== 'application/x-ndjson') {
    throw new Error(`unexpected task output type: ${firstOutput.output.content_type}`);
  }

  const firstCapture = join(
    `${masterOutput}.d`,
    'generation-1',
    'connection-1',
  );
  if (!readFileSync(join(firstCapture, 'host-to-usbmuxd.bin')).equals(firstPacket)) {
    throw new Error('host-to-usbmuxd raw capture mismatch');
  }
  if (!readFileSync(join(firstCapture, 'usbmuxd-to-host.bin')).equals(firstPacket)) {
    throw new Error('usbmuxd-to-host raw capture mismatch');
  }
  const parsed = readFileSync(
    join(firstCapture, 'host-to-usbmuxd.ndjson'),
    'utf8',
  );
  if (!parsed.includes('ListDevices') || !parsed.includes('generation-1-connection-1')) {
    throw new Error(`parsed usbmuxd capture is incomplete: ${parsed}`);
  }

  run(['kill-server']);
  task = await waitTask(
    task.id,
    current => current.runtime_state === 'running' && current.generation === 2,
    'restored generation 2',
  );
  if (task.runtime.port !== dproxyPort) {
    throw new Error(`dproxy restore changed port: ${JSON.stringify(task.runtime)}`);
  }
  await openEcho(dproxyPort, usbmuxdPacket('ReadBUID', 42), true);
  await waitTask(
    task.id,
    current => current.runtime.connectionsCompleted === 1,
    'restored connection',
  );

  run(['task', 'restart', task.id]);
  task = await waitTask(
    task.id,
    current => current.runtime_state === 'running' && current.generation === 3,
    'explicit restart generation 3',
  );
  const active = await openEcho(
    dproxyPort,
    usbmuxdPacket('Listen', 43),
    false,
  );
  run(['task', 'stop', task.id]);
  await assertSocketCloses(active);
  await assertPortAvailable(dproxyPort);
  if (run(['task', 'list']).value.tasks.length !== 0) {
    throw new Error('primary dproxy task leaked after stop');
  }

  const binary = run([
    'dproxy',
    '--binary',
    '--mode=usbmuxd',
    `--upstream=127.0.0.1:${upstreamPort}`,
    `--output=${binaryOutput}`,
  ]).value.task;
  const binaryTask = await waitTask(
    binary.id,
    current => current.runtime_state === 'running',
    'binary task running',
  );
  const binaryPacket = usbmuxdPacket('ListDevices', 44);
  await openEcho(binaryTask.runtime.port, binaryPacket, true);
  await waitTask(
    binary.id,
    current => current.runtime.connectionsCompleted === 1,
    'binary connection',
  );
  const binaryCapture = join(
    `${binaryOutput}.d`,
    'generation-1',
    'connection-1',
  );
  if (!readFileSync(join(binaryCapture, 'host-to-usbmuxd.bin')).equals(binaryPacket)) {
    throw new Error('binary-only raw capture mismatch');
  }
  if (existsSync(join(binaryCapture, 'host-to-usbmuxd.ndjson'))) {
    throw new Error('binary-only mode unexpectedly wrote parsed NDJSON');
  }
  run(['task', 'stop', binary.id]);

  const utun = run(['dproxy', '--mode=utun'], 2);
  if (utun.error?.code !== 'not_implemented') {
    throw new Error(`utun did not fail explicitly: ${JSON.stringify(utun)}`);
  }
  const exposed = run([
    'dproxy',
    '--mode=usbmuxd',
    '--bind=0.0.0.0',
    `--upstream=127.0.0.1:${upstreamPort}`,
  ], 3);
  if (exposed.error?.code !== 'invalid_request') {
    throw new Error(`non-loopback bind was not rejected: ${JSON.stringify(exposed)}`);
  }
  if (run(['task', 'list']).value.tasks.length !== 0) {
    throw new Error('failed dproxy requests leaked tasks');
  }

  const finalStatus = run(['server', 'status']).value;
  run(['kill-server']);
  await assertPortAvailable(serverPort);
  console.log(
    `DPROXY_RESTORE task=${primaryTaskId} generations=1,2,3 port=${dproxyPort}`,
  );
  console.log(
    `DPROXY_CAPTURE masterBytes=${firstOutput.output.total_bytes} serverPid=${finalStatus.pid}`,
  );
  console.log(`DPROXY_EVIDENCE=${serverRoot}`);
  console.log('DPROXY_PROCESS_SMOKE_OK');
} finally {
  if (existsSync(join(serverRoot, 'server.json'))) {
    try {
      run(['kill-server']);
    } catch {
      // Preserve evidence and surface the original failure.
    }
  }
  for (const socket of upstreamSockets) socket.destroy();
  await close(upstream);
}

function listen(server, port) {
  return new Promise((resolveListen, reject) => {
    server.once('error', reject);
    server.listen(port, '127.0.0.1', resolveListen);
  });
}

function close(server) {
  return new Promise((resolveClose, reject) => {
    server.close(error => error ? reject(error) : resolveClose());
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
