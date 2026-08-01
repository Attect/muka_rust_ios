import { execFile } from 'node:child_process';
import { generateKeyPairSync } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { promisify } from 'node:util';

const execFileAsync = promisify(execFile);
const bin = resolve(process.argv[2] ?? 'target/debug/ios.exe');
if (!existsSync(bin)) throw new Error(`ios binary not found: ${bin}`);

const root = mkdtempSync(join(tmpdir(), 'rust-ios-asc-'));
const clientCwd = join(root, 'client');
const home = join(root, 'server');
const serverPort = await freePort();
const requests = [];
const profileBytes = Buffer.from('mock-mobileprovision-profile');
const api = http.createServer(async (request, response) => {
  try {
    const body = await readBody(request);
    const jsonBody = body.length ? JSON.parse(body.toString('utf8')) : null;
    const authorization = request.headers.authorization ?? '';
    const token = authorization.startsWith('Bearer ') ? authorization.slice(7) : '';
    const claims = verifyTokenShape(token);
    requests.push({ method: request.method, url: request.url, body: jsonBody, claims });

    const url = new URL(request.url, 'http://127.0.0.1');
    if (request.method === 'GET' && url.pathname === '/v1/bundleIds') {
      assert(url.searchParams.get('filter[identifier]') === 'com.example.runner', 'bundle filter');
      return sendJson(response, 200, { data: [] });
    }
    if (request.method === 'POST' && url.pathname === '/v1/bundleIds') {
      assert(jsonBody.data.attributes.identifier === 'com.example.runner', 'bundle create identifier');
      return sendJson(response, 201, { data: { type: 'bundleIds', id: 'bundle-1', attributes: {} } });
    }
    if (request.method === 'GET' && url.pathname === '/v1/devices') {
      assert(url.searchParams.get('filter[udid]') === 'mock-device', 'device filter');
      return sendJson(response, 200, { data: [] });
    }
    if (request.method === 'POST' && url.pathname === '/v1/devices') {
      assert(jsonBody.data.attributes.udid === 'mock-device', 'device create UDID');
      return sendJson(response, 201, { data: { type: 'devices', id: 'device-1', attributes: {} } });
    }
    if (request.method === 'POST' && url.pathname === '/v1/profiles') {
      const relationships = jsonBody.data.relationships;
      assert(relationships.bundleId.data.id === 'bundle-1', 'profile bundle relationship');
      assert(relationships.certificates.data[0].id === 'certificate-existing', 'profile certificate relationship');
      assert(relationships.devices.data[0].id === 'device-1', 'profile device relationship');
      return sendJson(response, 201, {
        data: {
          type: 'profiles',
          id: 'profile-1',
          attributes: { profileContent: profileBytes.toString('base64') },
        },
      });
    }
    if (request.method === 'POST' && url.pathname === '/v1/certificates') {
      assert(jsonBody.data.attributes.certificateType === 'IOS_DEVELOPMENT', 'certificate type');
      assert(jsonBody.data.attributes.csrContent.includes('BEGIN CERTIFICATE REQUEST'), 'certificate CSR');
      return sendJson(response, 409, {
        errors: [{ title: 'Certificate limit', detail: 'use an existing certificate or explicitly revoke' }],
      });
    }
    sendJson(response, 404, { errors: [{ title: 'Not Found', detail: `${request.method} ${url.pathname}` }] });
  } catch (error) {
    sendJson(response, 500, { errors: [{ title: 'Mock failure', detail: error.message }] });
  }
});

await listen(api);
const apiPort = api.address().port;
const { privateKey } = generateKeyPairSync('ec', { namedCurve: 'prime256v1' });
const privatePem = privateKey.export({ type: 'pkcs8', format: 'pem' }).toString();
mkdirSync(clientCwd, { recursive: true });
writeFileSync(join(clientCwd, 'AuthKey.p8'), privatePem, { mode: 0o600 });

const common = ['--server-home', home, '--server-addr', `127.0.0.1:${serverPort}`];
const env = {
  ...process.env,
  RUST_IOS_BACKEND: 'mock',
  RUST_IOS_MOCK_DEVICES: '',
  RUST_IOS_ASC_API_BASE_URL: `http://127.0.0.1:${apiPort}`,
};
const credentials = [
  '--asc-key-id', 'KEY123',
  '--asc-issuer-id', 'issuer-123',
  '--asc-private-key', 'AuthKey.p8',
];

async function run(args, expectedStatus = 0) {
  try {
    const { stdout } = await execFileAsync(bin, [...args, ...common], {
      cwd: clientCwd,
      env,
      encoding: 'utf8',
      timeout: 20000,
      windowsHide: true,
    });
    if (expectedStatus !== 0) throw new Error(`command unexpectedly succeeded: ${args.join(' ')}`);
    return stdout.trim() ? JSON.parse(stdout) : null;
  } catch (error) {
    const status = typeof error.code === 'number' ? error.code : error.status;
    if (status !== expectedStatus) throw error;
    return JSON.parse(String(error.stderr).trim());
  }
}

try {
  const noDevice = await run([
    'sign', 'provision', 'appstoreconnect', '--bundleid', 'com.example.runner', ...credentials,
  ], 3);
  assert(noDevice.error?.code === 'invalid_request', 'provision must require explicit UDID');
  assert(requests.length === 0, 'invalid command must not call Apple API');

  const unsafeReuse = await run([
    'sign', 'provision', 'appstoreconnect', '--bundleid', 'com.example.runner',
    '--certificate-id', 'certificate-existing', '--revoke-existing', '--udid', 'mock-device',
    ...credentials,
  ], 3);
  assert(unsafeReuse.error?.code === 'invalid_request', 'reuse and revoke must conflict');
  assert(requests.length === 0, 'unsafe reuse must fail before Apple API');

  const result = await run([
    'sign', 'provision', 'appstoreconnect',
    '--bundleid', 'com.example.runner',
    '--bundle-name', 'Example Runner',
    '--profile-name', 'rust-ios smoke profile',
    '--device-name', 'rust-ios smoke device',
    '--profile-output', 'assets/runner.mobileprovision',
    '--certificate-id', 'certificate-existing',
    '--udid', 'mock-device',
    ...credentials,
  ]);
  const profilePath = join(clientCwd, 'assets', 'runner.mobileprovision');
  assert(result.bundleID === 'com.example.runner', 'bundle ID result');
  assert(result.certificateID === 'certificate-existing', 'certificate ID result');
  assert(result.p12Path === null, 'reuse mode must not report a P12');
  assert(resolve(result.profilePath) === resolve(profilePath), 'profile path must resolve from client cwd');
  assert(readFileSync(profilePath).equals(profileBytes), 'profile bytes must match API response');
  assert(requests.length === 5, `expected five Apple API requests, got ${requests.length}`);
  assert(!requests.some(request => request.url.startsWith('/v1/certificates')), 'reuse mode must not mutate certificates');
  assert(requests.every(request => request.claims.aud === 'appstoreconnect-v1'), 'JWT audience');

  const rejectedCertificate = await run([
    'sign', 'certificate', 'appstoreconnect',
    '--p12-output', 'assets/should-not-exist.p12',
    ...credentials,
  ], 3);
  assert(rejectedCertificate.error?.code === 'invalid_request', 'Apple 409 must be structured');
  assert(rejectedCertificate.error.message.includes('Certificate limit'), 'Apple error detail');
  assert(!existsSync(join(clientCwd, 'assets', 'should-not-exist.p12')), 'failed certificate must not create P12');
  assert(requests.length === 6, `expected six Apple API requests, got ${requests.length}`);

  const tasks = await run(['task', 'list']);
  assert(tasks.value.tasks.length === 0, 'host provisioning must not create a task');
  const state = existsSync(join(home, 'state-v1.json')) ? readFileSync(join(home, 'state-v1.json'), 'utf8') : '';
  const log = existsSync(join(home, 'server.log')) ? readFileSync(join(home, 'server.log'), 'utf8') : '';
  assert(!state.includes(privatePem), 'private key leaked into state');
  assert(!log.includes(privatePem), 'private key leaked into server log');

  await run(['kill-server']);
  await assertPortAvailable(serverPort);
  console.log(`ASC_PROFILE_CREATED requests=${requests.length} certificate=certificate-existing`);
  console.log('APP_STORE_CONNECT_PROCESS_SMOKE_OK');
} finally {
  if (existsSync(join(home, 'server.json'))) {
    try { await run(['kill-server']); } catch {}
  }
  await new Promise(resolveClose => api.close(resolveClose));
  rmSync(root, { recursive: true, force: true });
}

function verifyTokenShape(token) {
  const parts = token.split('.');
  assert(parts.length === 3, 'JWT must have three segments');
  const header = JSON.parse(Buffer.from(parts[0], 'base64url').toString('utf8'));
  const claims = JSON.parse(Buffer.from(parts[1], 'base64url').toString('utf8'));
  assert(header.alg === 'ES256' && header.kid === 'KEY123', 'JWT header');
  assert(claims.iss === 'issuer-123' && claims.exp > claims.iat, 'JWT claims');
  assert(Buffer.from(parts[2], 'base64url').length === 64, 'JWT ES256 signature length');
  return claims;
}

function assert(condition, message) {
  if (!condition) throw new Error(`assertion failed: ${message}`);
}

function sendJson(response, status, value) {
  const body = JSON.stringify(value);
  response.writeHead(status, { 'content-type': 'application/json', 'content-length': Buffer.byteLength(body) });
  response.end(body);
}

function readBody(request) {
  return new Promise((resolveBody, reject) => {
    const chunks = [];
    request.on('data', chunk => chunks.push(chunk));
    request.once('end', () => resolveBody(Buffer.concat(chunks)));
    request.once('error', reject);
  });
}

function listen(server) {
  return new Promise((resolveListen, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolveListen);
  });
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
