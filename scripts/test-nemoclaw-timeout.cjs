// Local fixtures only. Verify pinned adapter survives its former 30s total deadline.
const assert = require('node:assert/strict');
const http = require('node:http');
const path = require('node:path');
const { forwardOpenRouterRequest } = require(path.resolve(process.argv[2], 'dist/lib/inference/openrouter-runtime-adapter-forward.js'));

async function listen(server) {
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  return `http://127.0.0.1:${server.address().port}`;
}
async function fixture({ stream, timeout }) {
  const upstream = http.createServer((req, res) => {
    req.resume();
    if (!stream) return; // Deliberate stall: explicit short deadline must still fail closed.
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    res.write('data: fixture-start\n\n');
    const timer = setTimeout(() => res.end('data: fixture-end\n\n'), 31_000);
    res.on('close', () => clearTimeout(timer));
  });
  let proxy;
  try {
    const upstreamBaseUrl = await listen(upstream);
    proxy = http.createServer((req, res) => {
      forwardOpenRouterRequest({ req, res, upstreamBaseUrl, upstreamTimeoutMs: timeout });
    });
    const url = await listen(proxy);
    const started = Date.now();
    const result = await new Promise((resolve, reject) => {
      const req = http.request(`${url}/v1/chat/completions`, { method: 'POST' }, res => {
        let body = '';
        res.on('data', chunk => { body += chunk; });
        res.on('error', reject);
        res.on('end', () => resolve({ status: res.statusCode, body }));
      });
      req.on('error', reject);
      req.end('{}');
    });
    if (stream) {
      assert.equal(result.status, 200);
      assert.ok(result.body.includes('fixture-end'));
      assert.ok(Date.now() - started >= 31_000);
    } else {
      assert.equal(result.status, 504);
      assert.equal(JSON.parse(result.body).error.code, 'upstream_timeout');
    }
  } finally {
    for (const server of [proxy, upstream]) {
      if (server) {
        server.closeAllConnections();
        await new Promise(resolve => server.close(resolve));
      }
    }
  }
}
(async () => {
  await fixture({ stream: false, timeout: 25 });
  await fixture({ stream: true });
  console.log('PASS: streamed response survives 31s; explicit finite deadline still returns 504');
})().catch(err => { console.error(err); process.exitCode = 1; });
