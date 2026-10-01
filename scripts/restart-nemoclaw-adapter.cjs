// Restart only pinned local OpenRouter adapter; native lifecycle handles opaque auth state.
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const source = path.resolve(process.argv[2]);
const sha = execFileSync('git', ['-C', source, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
if (sha !== '6f3cced4230ae9660c049cc11804daf37797c595') throw new Error('Unsupported NemoClaw revision');
const dist = path.join(source, 'dist/lib');
const common = require(path.join(dist, 'inference/openrouter-runtime-adapter-common.js'));
const { run, runCapture } = require(path.join(dist, 'runner.js'));
const { killLocalAdapterPid } = require(path.join(dist, 'inference/local-adapter-lifecycle.js'));
const { ensureOpenRouterRuntimeAdapter } = require(path.join(dist, 'inference/openrouter-runtime-adapter-lifecycle.js'));
(async () => {
  killLocalAdapterPid({
    pidPath: common.PID_PATH,
    processMatcher: /openrouter-runtime-adapter-entry\.js/,
    run,
    runCapture,
  });
  await ensureOpenRouterRuntimeAdapter();
  console.log('OpenRouter adapter restarted; auth-bound health check passed.');
})().catch(() => {
  console.error('Adapter restart failed; inspect native diagnostics without exposing credentials.');
  process.exitCode = 1;
});
