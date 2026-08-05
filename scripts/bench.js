#!/usr/bin/env node
// Aikun 网关吞吐基准:mock 上游(零业务延迟)+ 分级并发打 /v1/chat/completions
// 非流式请求,测量网关自身引入的开销与 SQLite 记账链路的上限。
//
// 用法: node scripts/bench.js [二进制路径]
// 依赖: sqlite3 CLI(夹具注入),target/release/aikun 已构建(cargo build --release)。
// 注意: 数字只在同机同环境下可比;上游真实延迟是生产瓶颈(见 docs.js「性能与容量」)。
const { spawn, execFileSync } = require('child_process');
const crypto = require('crypto');
const fs = require('fs');
const os = require('os');
const path = require('path');

const AIKUN = path.resolve(process.argv[2] || path.join(__dirname, '..', 'target', 'release', 'aikun'));
const API_KEY = 'sk-bench-key';
const CONCURRENCY_LEVELS = [10, 50, 100, 200];
// 每级请求数:并发 × 100,保证每级至少有统计学意义的样本
const REQUESTS_PER_CONCURRENCY = 100;

const MOCK_BODY = JSON.stringify({
  id: 'chatcmpl-bench',
  object: 'chat.completion',
  model: 'gpt-4',
  choices: [{ index: 0, message: { role: 'assistant', content: 'OK' }, finish_reason: 'stop' }],
  usage: { prompt_tokens: 5, completion_tokens: 3, total_tokens: 8 },
});

function freePort() {
  return new Promise((resolve) => {
    const srv = require('net').createServer().listen(0, '127.0.0.1', () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

async function startMockUpstream() {
  const http = require('http');
  const srv = http.createServer((req, res) => {
    req.resume();
    req.on('end', () => {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(MOCK_BODY);
    });
  });
  const port = await freePort();
  await new Promise((r) => srv.listen(port, '127.0.0.1', r));
  return { srv, base: `http://127.0.0.1:${port}` };
}

async function waitReady(base, deadlineMs = 15000) {
  const deadline = Date.now() + deadlineMs;
  for (;;) {
    try {
      const r = await fetch(`${base}/api/health`);
      if (r.ok) return;
    } catch {}
    if (Date.now() > deadline) throw new Error('aikun 未在 15s 内就绪');
    await new Promise((r) => setTimeout(r, 100));
  }
}

async function benchLevel(base, concurrency, total) {
  const latencies = [];
  let done = 0;
  const started = process.hrtime.bigint();
  async function worker() {
    while (done < total) {
      const i = done++;
      if (i >= total) break;
      const t0 = process.hrtime.bigint();
      const r = await fetch(`${base}/v1/chat/completions`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', authorization: `Bearer ${API_KEY}` },
        body: JSON.stringify({ model: 'gpt-4', messages: [{ role: 'user', content: 'hi' }] }),
      });
      await r.arrayBuffer();
      if (r.status !== 200) throw new Error(`请求失败: HTTP ${r.status}`);
      latencies.push(Number(process.hrtime.bigint() - t0) / 1e6);
    }
  }
  await Promise.all(Array.from({ length: concurrency }, worker));
  const wallMs = Number(process.hrtime.bigint() - started) / 1e6;
  latencies.sort((a, b) => a - b);
  const pct = (p) => latencies[Math.min(latencies.length - 1, Math.floor(latencies.length * p))];
  return {
    concurrency,
    requests: latencies.length,
    qps: Math.round((latencies.length / wallMs) * 1000),
    p50: pct(0.5).toFixed(1),
    p99: pct(0.99).toFixed(1),
  };
}

(async () => {
  if (!fs.existsSync(AIKUN)) {
    console.error(`未找到网关二进制: ${AIKUN}\n请先 cargo build --release,或以参数指定路径: node scripts/bench.js <二进制路径>`);
    process.exit(1);
  }
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'aikun-bench-'));
  const dbPath = path.join(dir, 'bench.db');
  const appPort = await freePort();
  const appBase = `http://127.0.0.1:${appPort}`;
  const mock = await startMockUpstream();

  const child = spawn(AIKUN, [], {
    cwd: dir,
    env: {
      ...process.env,
      AIKUN_HOST: `127.0.0.1:${appPort}`,
      AIKUN_JWT_SECRET: 'bench-secret-bench-secret-bench-secret',
      AIKUN_DATABASE_URL: `sqlite://${dbPath}?mode=rwc`,
      AIKUN_HEALTH_CHECK_INTERVAL: '3600',
    },
    stdio: 'ignore',
  });
  let cleanedUp = false;
  const cleanup = () => {
    if (cleanedUp) return;
    cleanedUp = true;
    try { child.kill(); } catch {}
    try { mock.srv.close(); } catch {}
    fs.rmSync(dir, { recursive: true, force: true });
  };
  process.on('exit', cleanup);

  try {
    await waitReady(appBase);
    // 夹具:用户(大额余额)+ API key(sha256 落库)+ 指向 mock 的渠道 + 一条价格
    const keyHash = crypto.createHash('sha256').update(API_KEY).digest('hex');
    execFileSync('sqlite3', [dbPath, `
      INSERT INTO users (id, username, password_hash, role, balance) VALUES ('u-bench', 'bench', 'x', 'admin', 1000000000000);
      INSERT INTO api_keys (id, user_id, key, name) VALUES ('k-bench', 'u-bench', '${keyHash}', 'bench');
      INSERT INTO providers (id, name, provider_type, openai_base_url, anthropic_base_url, api_key, models,
                             health_status, protocols, default_protocol)
        VALUES ('p-bench', 'mock', 'openai', '${mock.base}', '${mock.base}', 'mock-key', '["gpt-4"]',
                'healthy', '["openai"]', 'openai');
      INSERT OR REPLACE INTO model_prices (id, model, prompt_price, completion_price) VALUES ('mp-bench', 'gpt-4', 10.0, 30.0);
    `]);

    console.log(`环境: ${os.cpus().length} 核 ${os.cpus()[0].model}, node ${process.version}, 非流式 /v1/chat/completions`);
    console.log('concurrency | requests | QPS | P50(ms) | P99(ms)');
    for (const c of CONCURRENCY_LEVELS) {
      const r = await benchLevel(appBase, c, c * REQUESTS_PER_CONCURRENCY);
      console.log(`${r.concurrency} | ${r.requests} | ${r.qps} | ${r.p50} | ${r.p99}`);
    }
  } finally {
    cleanup();
  }
})().catch((e) => {
  console.error(e.message || e);
  process.exit(1);
});
