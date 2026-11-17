#!/usr/bin/env node
// Aikun 网关吞吐基准:mock 上游(零业务延迟)+ 分级并发打 /v1/chat/completions
// 非流式请求,测量网关自身引入的开销与 SurrealDB 记账链路的上限。
//
// 用法: node scripts/bench.js [二进制路径]
// 依赖: target/release/aikun 已构建(cargo build --release)。
// 夹具注入走管理面 API(已知 admin 密码重置 → 登录 → 建用户/渠道/价格,
// 与 e2e 检查通道同源,docs/surreal-migration.md §七),无 sqlite3 CLI 依赖。
// 注意: 数字只在同机同环境下可比;上游真实延迟是生产瓶颈(见 docs「性能与容量」)。
const { spawn } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const AIKUN = path.resolve(process.argv[2] || path.join(__dirname, '..', 'target', 'release', 'aikun'));
const ADMIN_PASSWORD = 'bench-admin-password-2026';
const BENCH_USER = 'bench';
const BENCH_PASSWORD = 'bench-user-password-2026';
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

async function login(base, username, password) {
  const r = await fetch(`${base}/api/auth/login`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ username, password }),
  });
  if (!r.ok) throw new Error(`登录 ${username} 失败: HTTP ${r.status}`);
  return (await r.json()).token;
}

// 管理面请求封装(断言 2xx)。
async function adminReq(base, token, method, apiPath, body) {
  const r = await fetch(`${base}${apiPath}`, {
    method,
    headers: { 'content-type': 'application/json', authorization: `Bearer ${token}` },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!r.ok) throw new Error(`${method} ${apiPath} 失败: HTTP ${r.status} ${await r.text()}`);
  return r.status === 204 ? null : r.json();
}

// 夹具注入(管理面 API 通道):用户(大额余额)+ API key + 指向 mock 的
// 渠道 + gpt-4 价格行(默认价未含时补一条)。
async function seedFixtures(base, mockBase) {
  const admin = await login(base, 'admin', ADMIN_PASSWORD);
  const userId = (await adminReq(base, admin, 'POST', '/api/users', {
    username: BENCH_USER,
    password: BENCH_PASSWORD,
    display_name: BENCH_USER,
    role: 'user',
  })).id;
  // 余额 100 万元(微元整数口径经调账端点换算)
  await adminReq(base, admin, 'POST', `/api/users/${userId}/balance-transactions`, {
    amount: 1_000_000.0,
  });
  const userToken = await login(base, BENCH_USER, BENCH_PASSWORD);
  const apiKey = (await adminReq(base, userToken, 'POST', '/api/api-keys', {
    name: 'bench',
  })).key;
  await adminReq(base, admin, 'POST', '/api/providers', {
    name: 'bench-mock',
    openai_base_url: mockBase,
    api_key: 'mock-key',
    models: ['gpt-4'],
    protocols: ['openai'],
  });
  const prices = await adminReq(base, admin, 'GET', '/api/prices?limit=200');
  if (!prices.items.some((p) => p.model === 'gpt-4')) {
    await adminReq(base, admin, 'POST', '/api/prices', {
      model: 'gpt-4', prompt_price: 10.0, completion_price: 30.0,
    });
  }
  return apiKey;
}

async function benchLevel(base, concurrency, total, apiKey) {
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
        headers: { 'content-type': 'application/json', authorization: `Bearer ${apiKey}` },
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
  const appPort = await freePort();
  const appBase = `http://127.0.0.1:${appPort}`;
  const mock = await startMockUpstream();

  const child = spawn(AIKUN, [], {
    cwd: dir,
    env: {
      ...process.env,
      AIKUN_HOST: `127.0.0.1:${appPort}`,
      AIKUN_JWT_SECRET: 'bench-secret-bench-secret-bench-secret',
      // 阶段 S:嵌入式 SurrealKV(临时目录);检查通道密码重置供夹具登录
      AIKUN_SURREAL_URL: `surrealkv://${dir}/bench.surreal`,
      AIKUN_RESET_ADMIN_PASSWORD: ADMIN_PASSWORD,
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
    const apiKey = await seedFixtures(appBase, mock.base);

    console.log(`环境: ${os.cpus().length} 核 ${os.cpus()[0].model}, node ${process.version}, 非流式 /v1/chat/completions`);
    console.log('concurrency | requests | QPS | P50(ms) | P99(ms)');
    for (const c of CONCURRENCY_LEVELS) {
      const r = await benchLevel(appBase, c, c * REQUESTS_PER_CONCURRENCY, apiKey);
      console.log(`${r.concurrency} | ${r.requests} | ${r.qps} | ${r.p50} | ${r.p99}`);
    }
  } finally {
    cleanup();
  }
})().catch((e) => {
  console.error(e.message || e);
  process.exit(1);
});
