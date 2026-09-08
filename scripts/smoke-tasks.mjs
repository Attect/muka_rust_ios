import { execFileSync } from 'node:child_process';
import {
  chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import net from 'node:net';

const bin = resolve(process.argv[2] ?? 'target/debug/ios.exe');
const home = mkdtempSync(join(tmpdir(), 'rust-ios-task-smoke-'));
const serverPort = await freePort();
const common = ['--server-home', home, '--server-addr', '127.0.0.1:' + serverPort];
const env = { ...process.env, RUST_IOS_BACKEND: 'mock', RUST_IOS_MOCK_DEVICES: 'mock-phone' };
const xctestrunPath = join(home, 'Demo.xctestrun');
const gpxPath = join(home, 'Demo.gpx');
const debugAppPath = join(home, 'Demo.app');
const lldbCapturePath = join(home, 'debug.lldb.capture');
const fakeLldbPath = join(home, process.platform === 'win32' ? 'fake-lldb.cmd' : 'fake-lldb');
mkdirSync(debugAppPath);
writeFileSync(join(debugAppPath, 'Info.plist'), `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.example.Demo</string>
<key>CFBundleExecutable</key><string>Demo</string>
</dict></plist>`);
writeFileSync(join(debugAppPath, 'Demo'), 'mock executable');
if (process.platform === 'win32') {
  writeFileSync(fakeLldbPath, `@echo off\r\nif not "%~1"=="-s" exit /b 9\r\ntype "%~2" > "${lldbCapturePath}"\r\n`);
} else {
  writeFileSync(fakeLldbPath, `#!/bin/sh\n[ "$1" = "-s" ] || exit 9\ncp "$2" "${lldbCapturePath}"\n`);
  chmodSync(fakeLldbPath, 0o755);
}
env.RUST_IOS_LLDB = fakeLldbPath;
writeFileSync(xctestrunPath, `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>__xctestrun_metadata__</key><dict><key>FormatVersion</key><integer>2</integer></dict>
<key>TestConfigurations</key><array><dict>
<key>Name</key><string>DemoScheme</string>
<key>TestTargets</key><array><dict>
<key>TestHostBundleIdentifier</key><string>com.example.DemoUITests.xctrunner</string>
<key>TestBundlePath</key><string>__TESTHOST__/PlugIns/DemoUITests.xctest</string>
<key>IsUITestBundle</key><true/>
<key>UITargetAppPath</key><string>__TESTROOT__/Debug-iphoneos/Demo.app</string>
</dict></array></dict></array>
</dict></plist>`);
writeFileSync(gpxPath, `<?xml version="1.0" encoding="UTF-8"?>
<gpx xmlns="http://www.topografix.com/GPX/1/1" version="1.1">
  <trk><name>Smoke</name><trkseg>
    <trkpt lat="31.2304" lon="121.4737"><time>2026-07-16T00:00:00Z</time></trkpt>
    <trkpt lat="31.2305" lon="121.4738"><time>2026-07-16T00:00:00.020Z</time></trkpt>
  </trkseg></trk>
</gpx>`);

function run(args) {
  const output = execFileSync(bin, [...args, ...common], {
    env,
    encoding: 'utf8',
    timeout: 15000,
    windowsHide: true,
    stdio: ['ignore', 'pipe', 'pipe'],
  }).trim();
  return output ? JSON.parse(output) : null;
}

function freePort() {
  return new Promise((resolvePort, reject) => {
    const server = net.createServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const address = server.address();
      const port = address.port;
      server.close(error => error ? reject(error) : resolvePort(port));
    });
  });
}

async function waitPortFree(port) {
  const deadline = Date.now() + 5000;
  let lastError;
  do {
    try {
      await new Promise((resolvePort, reject) => {
        const server = net.createServer();
        server.once('error', reject);
        server.listen(port, '127.0.0.1', () => {
          server.close(error => error ? reject(error) : resolvePort());
        });
      });
      return;
    } catch (error) {
      lastError = error;
      await delay(50);
    }
  } while (Date.now() < deadline);
  throw new Error('port ' + port + ' was not released after server stop: ' + lastError);
}

function endpointPort(endpoint) {
  const port = Number(String(endpoint).split(':').at(-1));
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error('invalid endpoint port: ' + endpoint);
  }
  return port;
}

function delay(ms) {
  return new Promise(resolveDelay => setTimeout(resolveDelay, ms));
}

async function fetchJsonWithRetry(url) {
  const deadline = Date.now() + 5000;
  let lastError;
  do {
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(1000) });
      if (!response.ok) throw new Error('HTTP ' + response.status);
      return await response.json();
    } catch (error) {
      lastError = error;
      await delay(75);
    }
  } while (Date.now() < deadline);
  throw new Error('CDP discovery did not become ready at ' + url + ': ' + lastError);
}

function cdpCommand(url, command) {
  return new Promise((resolveCommand, reject) => {
    const websocket = new WebSocket(url);
    const timer = setTimeout(() => {
      websocket.close();
      reject(new Error('CDP WebSocket timeout on ' + url));
    }, 5000);
    websocket.addEventListener('open', () => websocket.send(JSON.stringify(command)));
    websocket.addEventListener('message', event => {
      clearTimeout(timer);
      websocket.close();
      resolveCommand(JSON.parse(String(event.data)));
    });
    websocket.addEventListener('error', event => {
      clearTimeout(timer);
      reject(new Error('CDP WebSocket error: ' + (event.message ?? 'unknown error')));
    });
  });
}

async function waitTask(id, expected = 'running') {
  const deadline = Date.now() + 5000;
  let task;
  do {
    task = run(['task', 'show', id]).value.task;
    if (task.runtime_state === expected) return task;
    await delay(75);
  } while (Date.now() < deadline);
  throw new Error('task ' + id + ' did not reach ' + expected + ': ' + JSON.stringify(task));
}

function echo(port, text) {
  return new Promise((resolveEcho, reject) => {
    const socket = net.createConnection({ host: '127.0.0.1', port });
    const chunks = [];
    let length = 0;
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error('echo timeout on port ' + port));
    }, 4000);
    socket.on('connect', () => socket.write(text));
    socket.on('data', chunk => {
      chunks.push(chunk);
      length += chunk.length;
      if (length >= Buffer.byteLength(text)) {
        clearTimeout(timer);
        socket.end();
        resolveEcho(Buffer.concat(chunks).toString('utf8'));
      }
    });
    socket.on('error', error => {
      clearTimeout(timer);
      reject(error);
    });
  });
}

function tunnelEcho(endpoint, text) {
  return new Promise((resolveEcho, reject) => {
    const [host, rawPort] = endpoint.split(':');
    const socket = net.createConnection({ host, port: Number(rawPort) });
    const chunks = [];
    let length = 0;
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error('tunnel echo timeout on ' + endpoint));
    }, 4000);
    socket.on('connect', () => {
      const address = Buffer.alloc(16);
      address[15] = 1;
      const port = Buffer.alloc(4);
      port.writeUInt32LE(8100);
      socket.write(Buffer.concat([address, port, Buffer.from(text)]));
    });
    socket.on('data', chunk => {
      chunks.push(chunk);
      length += chunk.length;
      if (length >= Buffer.byteLength(text)) {
        clearTimeout(timer);
        socket.end();
        resolveEcho(Buffer.concat(chunks).toString('utf8'));
      }
    });
    socket.on('error', error => {
      clearTimeout(timer);
      reject(error);
    });
  });
}

function readMjpegFrame(endpoint) {
  return new Promise((resolveFrame, reject) => {
    const [host, rawPort] = endpoint.split(':');
    const socket = net.createConnection({ host, port: Number(rawPort) });
    let bytes = Buffer.alloc(0);
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error('MJPEG timeout on ' + endpoint));
    }, 5000);
    socket.on('connect', () => socket.write('GET / HTTP/1.1\r\nHost: localhost\r\n\r\n'));
    socket.on('data', chunk => {
      bytes = Buffer.concat([bytes, chunk]);
      if (bytes.includes(Buffer.from('multipart/x-mixed-replace')) && bytes.includes(Buffer.from([0xff, 0xd8]))) {
        clearTimeout(timer);
        socket.destroy();
        resolveFrame(bytes.length);
      }
    });
    socket.on('error', error => {
      clearTimeout(timer);
      reject(error);
    });
  });
}

try {
  const webPages = run(['webinspector', 'list', '--udid', 'mock-phone']);
  if (webPages?.[0]?.page?.key !== 'mock-page-1') throw new Error('webinspector list failed');
  const webEval = run(['webinspector', 'eval', 'mock-page-1', '2+2', '--udid', 'mock-phone']);
  if (webEval !== 4) throw new Error('webinspector eval failed: ' + JSON.stringify(webEval));
  console.log(`WEBINSPECTOR page=${webPages[0].page.key} eval=${webEval}`);

  const forwardTask = run(['forward', '0', '8100', '--udid', 'mock-phone']).value.tasks[0];
  const forwardPort = forwardTask.spec.configuration.host_port;
  const firstEcho = await echo(forwardPort, 'forward-before-restart');
  if (firstEcho !== 'forward-before-restart') throw new Error('bad first forward echo: ' + firstEcho);
  console.log('FORWARD task=' + forwardTask.id + ' port=' + forwardPort + ' echo=' + firstEcho);

  let tunnelTask = run([
    'tunnel', 'start', '--userspace', '--userspace-port', '0', '--udid', 'mock-phone'
  ]).value.task;
  tunnelTask = await waitTask(tunnelTask.id);
  const tunnelResult = await tunnelEcho(tunnelTask.runtime.userspaceEndpoint, 'tunnel-echo');
  if (tunnelResult !== 'tunnel-echo') throw new Error('bad tunnel echo: ' + tunnelResult);
  console.log('TUNNEL task=' + tunnelTask.id + ' endpoint=' + tunnelTask.runtime.userspaceEndpoint + ' echo=' + tunnelResult);
  const rsdServices = run(['rsd', 'ls', '--udid', 'mock-phone']);
  if (rsdServices['com.apple.internal.dt.remote.debugproxy']?.Port !== 58785) {
    throw new Error('bad RSD service registry: ' + JSON.stringify(rsdServices));
  }
  console.log('RSD_SERVICES count=' + Object.keys(rsdServices).length + ' sharedTunnel=' + tunnelTask.id);
  const debugSession = run([
    'debug', '--stop-at-entry', debugAppPath, '--udid', 'mock-phone'
  ]);
  if (debugSession !== null) throw new Error('debug app session printed unexpected JSON');
  const lldbSource = readFileSync(lldbCapturePath, 'utf8');
  if (!lldbSource.includes('target create -- "' + debugAppPath.replaceAll('\\', '/') + '/Demo"')
      || !lldbSource.includes('/private/var/containers/Bundle/Application/MOCK-APP/Demo.app/Demo')
      || !lldbSource.includes('process launch --stop-at-entry')) {
    throw new Error('bad LLDB source: ' + lldbSource);
  }
  const debugTasksAfterSession = run(['task', 'list', '--kind', 'debug_proxy']).value.tasks;
  if (debugTasksAfterSession.length !== 0) {
    throw new Error('debug app session left a proxy task: ' + JSON.stringify(debugTasksAfterSession));
  }
  console.log('DEBUG_APP bundle=com.example.Demo cleanup=passed sourceBytes=' + lldbSource.length);
  const lockdownInfo = run(['info', 'lockdown', '--udid', 'mock-phone']);
  if (lockdownInfo.ProductVersion !== '18.0' || lockdownInfo.UniqueDeviceID !== 'mock-phone') {
    throw new Error('bad Lockdown info: ' + JSON.stringify(lockdownInfo));
  }
  const displayInfo = run(['info', 'display', '--udid', 'mock-phone']);
  if (!Array.isArray(displayInfo.displays) || displayInfo.displays.length !== 1
      || displayInfo.orientation !== 1 || displayInfo.backlightState !== 2) {
    throw new Error('bad CoreDevice display info: ' + JSON.stringify(displayInfo));
  }
  console.log('DEVICE_INFO lockdown=' + lockdownInfo.ProductVersion
    + ' displays=' + displayInfo.displays.length + ' orientation=' + displayInfo.orientation);
  let cdpTask = run([
    'webinspector', 'cdp', '--port=0', '--udid', 'mock-phone'
  ]).value.task;
  cdpTask = await waitTask(cdpTask.id);
  if (cdpTask.runtime.serviceTransport !== 'shared_rsd') {
    throw new Error('CDP task did not reuse shared RSD: ' + JSON.stringify(cdpTask));
  }
  const cdpTargets = await fetchJsonWithRetry(cdpTask.runtime.jsonUrl);
  if (cdpTargets[0]?.id !== 'mock-page-1') {
    throw new Error('bad CDP discovery: ' + JSON.stringify(cdpTargets));
  }
  const cdpResponse = await cdpCommand(cdpTargets[0].webSocketDebuggerUrl, {
    id: 91,
    method: 'Runtime.enable',
  });
  if (cdpResponse.id !== 91 || cdpResponse.result?.method !== 'Runtime.enable') {
    throw new Error('bad CDP response: ' + JSON.stringify(cdpResponse));
  }
  console.log('WEBINSPECTOR_CDP task=' + cdpTask.id + ' endpoint=' + cdpTask.runtime.endpoint);
  let debugProxyTask = run([
    'debug', 'proxy', '--port=0', '--udid', 'mock-phone'
  ]).value.task;
  debugProxyTask = await waitTask(debugProxyTask.id);
  const debugEcho = await echo(debugProxyTask.runtime.port, 'gdb-remote');
  if (debugEcho !== 'gdb-remote') throw new Error('bad debug proxy echo: ' + debugEcho);
  console.log('DEBUG_PROXY task=' + debugProxyTask.id + ' endpoint=' + debugProxyTask.runtime.endpoint + ' echo=' + debugEcho);

  let xctestTask = run([
    'runxctest',
    '--xctestrun-file-path=' + xctestrunPath,
    '--udid', 'mock-phone',
  ]).value.task;
  xctestTask = await waitTask(xctestTask.id, 'completed');
  const xctestOutput = run(['task', 'output', xctestTask.id]).value;
  const xctestText = Buffer.from(xctestOutput.bytes_base64, 'base64').toString('utf8');
  if (!xctestText.includes('"case_finished"') || xctestTask.runtime.outcome !== 'passed') {
    throw new Error('bad XCTest result: ' + JSON.stringify({ xctestTask, xctestText }));
  }
  run(['task', 'restart', xctestTask.id]);
  xctestTask = await waitTask(xctestTask.id, 'completed');
  if (xctestTask.generation !== 2) throw new Error('XCTest generation did not advance');
  console.log('XCTEST task=' + xctestTask.id + ' generation=' + xctestTask.generation + ' bytes=' + xctestOutput.total_bytes);
  // runtest unit filters are implemented (server capability `xctest_unit_filters`),
  // so the mock backend must run the filtered test to completion instead of
  // rejecting with not_implemented.
  const filterRun = run([
    'runtest',
    '--test-runner-bundle-id=com.example.DemoUITests.xctrunner',
    '--test-to-run=DemoUITests/testExample',
    '--udid', 'mock-phone',
  ]);
  if (!filterRun?.value?.task?.id) {
    throw new Error('runtest filter did not create a task: ' + JSON.stringify(filterRun));
  }
  let filterTask = await waitTask(filterRun.value.task.id, 'completed');
  if (filterTask.runtime.outcome !== 'passed') {
    throw new Error('bad runtest filter result: ' + JSON.stringify(filterTask));
  }
  console.log('XCTEST_FILTER task=' + filterTask.id + ' outcome=' + filterTask.runtime.outcome);

  let appProcessTask = run([
    'launch', 'com.example.Demo', '--wait', '--kill-existing',
    '--arg=--smoke', '--env=FEATURE=true', '--udid', 'mock-phone',
  ]).value.task;
  appProcessTask = await waitTask(appProcessTask.id, 'completed');
  const appProcessOutput = run(['task', 'output', appProcessTask.id]).value;
  const appProcessText = Buffer.from(appProcessOutput.bytes_base64, 'base64').toString('utf8');
  if (appProcessTask.runtime.pid !== 42424
      || appProcessTask.runtime.serviceTransport !== 'shared_rsd'
      || appProcessText !== 'mock app stdout\nmock app stderr\n') {
    throw new Error('bad app process result: ' + JSON.stringify({ appProcessTask, appProcessText }));
  }
  console.log('APP_PROCESS task=' + appProcessTask.id + ' pid=' + appProcessTask.runtime.pid + ' bytes=' + appProcessOutput.total_bytes);

  let locationTask = run([
    'setlocationgpx', `--gpxfilepath=${gpxPath}`, '--udid', 'mock-phone',
  ]).value.task;
  locationTask = await waitTask(locationTask.id, 'completed');
  const locationOutput = run(['task', 'output', locationTask.id]).value;
  const locationText = Buffer.from(locationOutput.bytes_base64, 'base64').toString('utf8');
  if (locationTask.runtime.completedPoints !== 2
      || locationTask.runtime.serviceTransport !== 'shared_rsd'
      || (locationText.match(/"event":"location_set"/g) ?? []).length !== 2
      || !locationText.includes('"event":"simulation_completed"')) {
    throw new Error('bad location simulation result: ' + JSON.stringify({ locationTask, locationText }));
  }
  run(['task', 'restart', locationTask.id]);
  locationTask = await waitTask(locationTask.id, 'completed');
  if (locationTask.generation !== 2) {
    throw new Error('location simulation explicit restart did not advance generation: ' + JSON.stringify(locationTask));
  }
  console.log('LOCATION_SIMULATION task=' + locationTask.id + ' generation=' + locationTask.generation + ' points=2');

  let wdaTask = run([
    'runwda', '--bundleid=com.example.WebDriverAgentRunner.xctrunner', '--udid', 'mock-phone'
  ]).value.task;
  wdaTask = await waitTask(wdaTask.id);
  if (wdaTask.runtime.wdaUrl !== 'http://127.0.0.1:18100') {
    throw new Error('bad WDA runtime: ' + JSON.stringify(wdaTask.runtime));
  }
  const wdaOutput = run(['task', 'output', wdaTask.id]).value;
  const wdaEvents = Buffer.from(wdaOutput.bytes_base64, 'base64').toString('utf8')
    .trim().split('\n').filter(Boolean).map(line => JSON.parse(line));
  if (wdaOutput.content_type !== 'application/x-ndjson'
      || wdaEvents.length !== 1
      || wdaEvents[0].event?.kind !== 'test_plan_started') {
    throw new Error('bad WDA event capture: ' + JSON.stringify({ wdaOutput, wdaEvents }));
  }
  console.log('WDA task=' + wdaTask.id + ' url=' + wdaTask.runtime.wdaUrl + ' bytes=' + wdaOutput.total_bytes);

  let syslogTask = run(['syslog', '--udid', 'mock-phone']).value.task;
  syslogTask = await waitTask(syslogTask.id);
  await delay(125);
  const syslogOutput = run(['task', 'output', syslogTask.id]).value;
  const syslogText = Buffer.from(syslogOutput.bytes_base64, 'base64').toString('utf8');
  if (!syslogText.includes('mock syslog mock-phone')) throw new Error('bad syslog output: ' + syslogText);
  console.log('SYSLOG task=' + syslogTask.id + ' bytes=' + syslogOutput.total_bytes);

  let ostraceTask = run([
    'ostrace', '--pid', '42', '--level=Error,fault,ERROR',
    '--subsystem=com.example.storage', '--match=timeout', '--exclude=ignored', '--follow',
    '--udid', 'mock-phone'
  ]).value.task;
  ostraceTask = await waitTask(ostraceTask.id);
  const ostraceOptions = ostraceTask.spec.configuration.os_trace;
  if (JSON.stringify(ostraceOptions.levels) !== JSON.stringify(['error', 'fault']) ||
      ostraceOptions.subsystem_contains !== 'com.example.storage' ||
      ostraceOptions.message_contains !== 'timeout' ||
      ostraceOptions.message_excludes !== 'ignored' || !ostraceOptions.follow) {
    throw new Error('bad ostrace task options: ' + JSON.stringify(ostraceOptions));
  }
  await delay(125);
  const ostraceOutput = run(['task', 'output', ostraceTask.id]).value;
  const ostraceText = Buffer.from(ostraceOutput.bytes_base64, 'base64').toString('utf8');
  const ostraceEntries = ostraceText.trim().split('\n').filter(Boolean).map(line => JSON.parse(line));
  if (ostraceEntries.length === 0 || ostraceEntries.some(entry =>
    entry.level !== 'Error' || entry.label?.subsystem !== 'com.example.storage' ||
    !entry.message.includes('timeout') || entry.message.includes('ignored'))) {
    throw new Error('bad filtered ostrace output: ' + ostraceText);
  }
  console.log('OSTRACE task=' + ostraceTask.id + ' bytes=' + ostraceOutput.total_bytes);

  let pcapTask = run(['pcap', '--udid', 'mock-phone']).value.task;
  pcapTask = await waitTask(pcapTask.id);
  await delay(125);
  const pcapOutput = run(['task', 'output', pcapTask.id]).value;
  const pcapBytes = Buffer.from(pcapOutput.bytes_base64, 'base64');
  if (!pcapBytes.subarray(0, 4).equals(Buffer.from([0xd4, 0xc3, 0xb2, 0xa1]))) {
    throw new Error('bad pcap header: ' + pcapBytes.subarray(0, 4).toString('hex'));
  }
  console.log('PCAP task=' + pcapTask.id + ' bytes=' + pcapOutput.total_bytes);

  let notificationsTask = run(['instruments', 'notifications', '--udid', 'mock-phone']).value.task;
  notificationsTask = await waitTask(notificationsTask.id);
  let sysmontapTask = run(['sysmontap', '--udid', 'mock-phone']).value.task;
  sysmontapTask = await waitTask(sysmontapTask.id);
  if (notificationsTask.runtime.serviceTransport !== 'shared_rsd' ||
      sysmontapTask.runtime.serviceTransport !== 'shared_rsd') {
    throw new Error('DVT streams did not reuse shared RSD: ' + JSON.stringify({ notificationsTask, sysmontapTask }));
  }
  await delay(125);
  const notificationsOutput = run(['task', 'output', notificationsTask.id]).value;
  const sysmontapOutput = run(['task', 'output', sysmontapTask.id]).value;
  const notificationsText = Buffer.from(notificationsOutput.bytes_base64, 'base64').toString('utf8');
  const sysmontapText = Buffer.from(sysmontapOutput.bytes_base64, 'base64').toString('utf8');
  if (!notificationsText.includes('mock notifications mock-phone')) {
    throw new Error('bad notifications output: ' + notificationsText);
  }
  if (!sysmontapText.includes('mock sysmontap mock-phone')) {
    throw new Error('bad sysmontap output: ' + sysmontapText);
  }
  console.log('DVT_STREAMS notifications=' + notificationsTask.id + ' sysmontap=' + sysmontapTask.id);

  let screenshotTask = run([
    'screenshot', '--stream', '--port', '0', '--udid', 'mock-phone'
  ]).value.task;
  screenshotTask = await waitTask(screenshotTask.id);
  if (screenshotTask.runtime.serviceTransport !== 'shared_rsd') {
    throw new Error('screenshot stream did not reuse shared RSD: ' + JSON.stringify(screenshotTask));
  }
  const screenshotEndpoint = screenshotTask.runtime.endpoint;
  const screenshotBytes = await readMjpegFrame(screenshotEndpoint);
  console.log('SCREENSHOT_STREAM task=' + screenshotTask.id + ' endpoint=' + screenshotEndpoint + ' bytes=' + screenshotBytes);

  const conditions = run(['devicestate', 'list', '--udid', 'mock-phone']);
  if (conditions[0]?.Identifier !== 'SlowNetworkCondition') {
    throw new Error('bad devicestate list: ' + JSON.stringify(conditions));
  }
  let deviceStateTask = run([
    'devicestate', 'enable', 'SlowNetworkCondition', 'SlowNetwork3GGood', '--udid', 'mock-phone'
  ]);
  deviceStateTask = await waitTask(deviceStateTask.id);
  if (deviceStateTask.runtime.serviceTransport !== 'shared_rsd') {
    throw new Error('device state did not reuse shared RSD: ' + JSON.stringify(deviceStateTask));
  }
  const deviceStateTasks = run(['task', 'list', '--kind', 'device_state']).value.tasks;
  if (deviceStateTasks.length !== 1 || deviceStateTasks[0].id !== deviceStateTask.id) {
    throw new Error('device_state task filter mismatch: ' + JSON.stringify(deviceStateTasks));
  }
  console.log('DEVICESTATE task=' + deviceStateTask.id + ' profile=SlowNetwork3GGood');

  run(['kill-server']);
  await Promise.all([
    waitPortFree(forwardPort),
    waitPortFree(endpointPort(tunnelTask.runtime.userspaceEndpoint)),
    waitPortFree(cdpTask.runtime.port),
    waitPortFree(debugProxyTask.runtime.port),
    waitPortFree(endpointPort(screenshotEndpoint)),
  ]);

  const restored = run(['task', 'list']).value.tasks;
  // 15 pre-existing task kinds plus the runtest filter task added above.
  if (restored.length !== 16) throw new Error('expected 16 restored tasks, got ' + restored.length);
  await waitTask(forwardTask.id);
  await waitTask(syslogTask.id);
  const restoredOstraceTask = await waitTask(ostraceTask.id);
  if (JSON.stringify(restoredOstraceTask.spec.configuration.os_trace) !== JSON.stringify(ostraceOptions)) {
    throw new Error('ostrace filters changed after restart: ' + JSON.stringify(restoredOstraceTask));
  }
  await waitTask(pcapTask.id);
  const restoredNotificationsTask = await waitTask(notificationsTask.id);
  const restoredSysmontapTask = await waitTask(sysmontapTask.id);
  const restoredScreenshotTask = await waitTask(screenshotTask.id);
  const restoredDeviceStateTask = await waitTask(deviceStateTask.id);
  for (const task of [restoredNotificationsTask, restoredSysmontapTask, restoredScreenshotTask, restoredDeviceStateTask]) {
    if (task.runtime.serviceTransport !== 'shared_rsd') {
      throw new Error('restored DVT task did not reuse shared RSD: ' + JSON.stringify(task));
    }
  }
  const restoredDebugProxy = await waitTask(debugProxyTask.id);
  const restoredCdp = await waitTask(cdpTask.id);
  const restoredWda = await waitTask(wdaTask.id);
  const restoredXctest = await waitTask(xctestTask.id, 'completed');
  const restoredAppProcess = await waitTask(appProcessTask.id, 'completed');
  const restoredLocation = await waitTask(locationTask.id, 'completed');
  if (restoredXctest.generation !== 2 || restoredXctest.runtime.outcome !== 'passed') {
    throw new Error('finite XCTest task was rerun or lost after restart: ' + JSON.stringify(restoredXctest));
  }
  if (restoredAppProcess.generation !== 1 || restoredAppProcess.runtime.pid !== 42424) {
    throw new Error('finite app process task was rerun or lost after restart: ' + JSON.stringify(restoredAppProcess));
  }
  if (restoredLocation.generation !== 2 || restoredLocation.runtime.completedPoints !== 2) {
    throw new Error('finite location task was rerun or lost after restart: ' + JSON.stringify(restoredLocation));
  }
  if (restoredWda.runtime.wdaUrl !== 'http://127.0.0.1:18100') {
    throw new Error('bad restored WDA runtime: ' + JSON.stringify(restoredWda.runtime));
  }
  const restoredWdaOutput = run(['task', 'output', wdaTask.id]).value;
  const restoredWdaEvents = Buffer.from(restoredWdaOutput.bytes_base64, 'base64').toString('utf8')
    .trim().split('\n').filter(Boolean).map(line => JSON.parse(line));
  if (restoredWdaOutput.total_bytes <= wdaOutput.total_bytes
      || restoredWdaEvents.length !== 2
      || restoredWdaEvents.some(event => event.event?.kind !== 'test_plan_started')) {
    throw new Error('WDA event capture did not append after restart: '
      + JSON.stringify({ wdaOutput, restoredWdaOutput, restoredWdaEvents }));
  }
  const restoredCdpTargets = await fetchJsonWithRetry(restoredCdp.runtime.jsonUrl);
  const restoredCdpResponse = await cdpCommand(
    restoredCdpTargets[0].webSocketDebuggerUrl,
    { id: 92, method: 'Runtime.enable' },
  );
  if (restoredCdpResponse.id !== 92) {
    throw new Error('restored CDP command failed: ' + JSON.stringify(restoredCdpResponse));
  }
  const restoredDebugEcho = await echo(restoredDebugProxy.runtime.port, 'gdb-after-restart');
  if (restoredDebugEcho !== 'gdb-after-restart') {
    throw new Error('bad restored debug proxy echo: ' + restoredDebugEcho);
  }
  const restoredScreenshotBytes = await readMjpegFrame(screenshotEndpoint);
  if (restoredScreenshotBytes <= 0) throw new Error('restored screenshot stream returned no data');
  const secondEcho = await echo(forwardPort, 'forward-after-restart');
  if (secondEcho !== 'forward-after-restart') throw new Error('bad restored forward echo: ' + secondEcho);
  await delay(125);
  const restoredSyslog = run(['task', 'output', syslogTask.id]).value;
  const restoredOstrace = run(['task', 'output', ostraceTask.id]).value;
  const restoredPcap = run(['task', 'output', pcapTask.id]).value;
  const restoredNotifications = run(['task', 'output', notificationsTask.id]).value;
  const restoredSysmontap = run(['task', 'output', sysmontapTask.id]).value;
  if (restoredSyslog.total_bytes <= syslogOutput.total_bytes) throw new Error('syslog did not append after restart');
  if (restoredOstrace.total_bytes <= ostraceOutput.total_bytes) throw new Error('ostrace did not append after restart');
  const restoredOstraceText = Buffer.from(restoredOstrace.bytes_base64, 'base64').toString('utf8');
  const restoredOstraceEntries = restoredOstraceText.trim().split('\n').filter(Boolean).map(line => JSON.parse(line));
  if (restoredOstraceEntries.some(entry => entry.level !== 'Error' ||
      entry.label?.subsystem !== 'com.example.storage' || !entry.message.includes('timeout') ||
      entry.message.includes('ignored'))) {
    throw new Error('restored ostrace output violated filters: ' + restoredOstraceText);
  }
  if (restoredPcap.total_bytes <= pcapOutput.total_bytes) throw new Error('pcap did not append after restart');
  if (restoredNotifications.total_bytes <= notificationsOutput.total_bytes) throw new Error('notifications did not append after restart');
  if (restoredSysmontap.total_bytes <= sysmontapOutput.total_bytes) throw new Error('sysmontap did not append after restart');
  console.log('RESTORED tasks=' + restored.length + ' echo=' + secondEcho);

  run(['task', 'stop', 'all']);
  run(['kill-server']);
  await delay(350);
  const state = JSON.parse(readFileSync(join(home, 'state-v1.json'), 'utf8'));
  if (state.tasks.length !== 0) throw new Error('state still contains tasks');
  console.log('STATE tasks=0');
  console.log('PROCESS_SMOKE_OK');
} finally {
  if (existsSync(join(home, 'server.json'))) {
    try { run(['kill-server']); } catch {}
    await delay(250);
  }
  rmSync(home, { recursive: true, force: true });
}
