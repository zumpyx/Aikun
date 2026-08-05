// ============ API Docs ============
function renderApiDocs(container) {
  const baseUrl = window.location.origin;

  container.innerHTML = `
    <div class="page-head"><h2>接口文档</h2><p>AI 代理接口使用指南</p></div>
    <p style="color:var(--muted);margin-bottom:20px;font-size:14px">
      接口地址: <code style="background:#eef0f4;padding:2px 8px;border-radius:6px">${baseUrl}</code>
      — 使用 API 密钥调用,请求头携带 <code>Authorization: Bearer sk-...</code>(密钥在「密钥」页创建)
    </p>

    <div class="card">
      <div class="card-header"><h2>AI 代理 — OpenAI 兼容接口</h2></div>
      <p style="font-size:14px;color:var(--muted);margin-bottom:12px">与 OpenAI API 格式完全兼容，可用任何 OpenAI SDK 直接调用。支持流式（stream: true）。</p>

      <h3 style="margin:12px 0 8px">Chat Completions</h3>
      <div class="code-block">POST ${baseUrl}/v1/chat/completions
Authorization: Bearer sk-...
Content-Type: application/json

{
  "model": "gpt-4",
  "messages": [{"role": "user", "content": "Hello"}],
  "stream": false
}

→ 200
{
  "id": "chatcmpl-...",
  "object": "chat.completion",
  "choices": [{"message": {"role": "assistant", "content": "..."}}],
  "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
}</div>

      <h3 style="margin:12px 0 8px">列出可用模型</h3>
      <div class="code-block">GET ${baseUrl}/v1/models
Authorization: Bearer sk-...

→ 200
{"object": "list", "data": [{"id": "gpt-4", "object": "model", "owned_by": "aikun"}]}</div>
    </div>

    <div class="card">
      <div class="card-header"><h2>AI 代理 — OpenAI Responses 接口(codex 等客户端)</h2></div>
      <p style="font-size:14px;color:var(--muted);margin-bottom:12px">
        与 OpenAI Responses API 格式兼容，codex 等使用 <code>/v1/responses</code> 的客户端可直接接入，支持流式。
        渠道勾选 Responses 支持协议时按原生透传（保留 reasoning 等专有特性）；未勾选时自动降级为协议转换。
      </p>
      <div class="code-block">POST ${baseUrl}/v1/responses
Authorization: Bearer sk-...
Content-Type: application/json

{
  "model": "gpt-5",
  "input": "Hello",
  "stream": true
}</div>
    </div>

    <div class="card">
      <div class="card-header"><h2>AI 代理 — Anthropic 兼容接口</h2></div>
      <p style="font-size:14px;color:var(--muted);margin-bottom:12px">与 Anthropic Messages API 格式兼容，可用 Anthropic SDK 调用（也接受 <code>x-api-key</code> 头代替 Bearer）。无论上游渠道是 OpenAI 还是 Anthropic 协议，网关都会自动双向转换（含流式、工具调用、图片）。</p>

      <h3 style="margin:12px 0 8px">Messages</h3>
      <div class="code-block">POST ${baseUrl}/v1/messages
x-api-key: sk-...
anthropic-version: 2023-06-01
Content-Type: application/json

{
  "model": "claude-sonnet-4-20250514",
  "max_tokens": 1024,
  "messages": [{"role": "user", "content": "Hello"}],
  "stream": false
}

→ 200
{
  "id": "msg_...",
  "type": "message",
  "role": "assistant",
  "content": [{"type": "text", "text": "..."}],
  "stop_reason": "end_turn",
  "usage": {"input_tokens": 10, "output_tokens": 20}
}</div>
    </div>

    <div class="card">
      <div class="card-header"><h2>错误语义</h2></div>
      <p style="font-size:14px;color:var(--muted);margin-bottom:12px">
        入口拒绝发生在响应开始之前，返回完整 JSON 错误（按客户端协议格式化，与官方形状一致）,不会产生半截流。
      </p>
      <div class="table-wrap">
        <table>
          <thead><tr><th>状态码</th><th>type</th><th>触发条件</th></tr></thead>
          <tbody>
            <tr><td>400</td><td><code>invalid_request_error</code></td><td>请求体缺少 model 字段等格式错误</td></tr>
            <tr><td>401</td><td><code>unauthorized</code></td><td>API Key 缺失、无效、已过期或被删除(返回简单 JSON,非协议错误形状)</td></tr>
            <tr><td>402</td><td><code>insufficient_quota</code></td><td>账户余额 ≤ 0,充值后自动恢复</td></tr>
            <tr><td>403</td><td><code>model_not_allowed</code></td><td>该 API Key 的模型白名单不含请求的模型</td></tr>
            <tr><td>429</td><td><code>rate_limit_error</code></td><td>触发 API Key 限额:每分钟请求数 / 每日 Token 额度 / 并发在途上限(均可在「密钥」页配置)</td></tr>
            <tr><td>502</td><td><code>upstream_error</code></td><td>所有可用渠道均失败(已达最大重试次数)</td></tr>
            <tr><td>503</td><td><code>provider_unavailable</code></td><td>没有任何已启用渠道声明支持该模型</td></tr>
          </tbody>
        </table>
      </div>
    </div>

    <div class="card">
      <div class="card-header"><h2>钱包 — 兑换码充值</h2></div>
      <p style="font-size:14px;color:var(--muted);margin-bottom:12px">
        兑换码由管理员在「兑换码」页批量生成。同一用户 10 分钟内连续兑换失败 5 次将被限流(429);
        无效、已使用、已过期的码统一返回 <code>invalid_code</code>(400),不区分原因。
      </p>
      <div class="code-block">POST ${baseUrl}/api/wallet/redeem
Authorization: Bearer <管理端登录令牌>(/api/* 仅接受登录会话,API Key 不适用)
Content-Type: application/json

{"code": "AK-XXXX-XXXX-XXXX"}

→ 200
{"amount": 10.0, "balance": 25.5}</div>
    </div>

    <div class="card">
      <div class="card-header"><h2>性能与容量</h2></div>
      <p style="font-size:14px;color:var(--muted);margin-bottom:12px">
        以下数字在本机实测(<b>10 核 i9-12900H,release 二进制</b>),上游为<b>零业务延迟的 mock</b>,
        打的是非流式 <code>/v1/chat/completions</code>,每请求走完整链路:认证 → 选路 → 转发 →
        计费(查价 + 写日志 + 扣余额,同一事务)。先 <code>cargo build --release</code> 再用
        <code>node scripts/bench.js</code> 在你的机器上复现(需 sqlite3 CLI)。
      </p>
      <div class="table-wrap" style="margin-bottom:12px">
        <table>
          <thead><tr><th>并发</th><th>QPS</th><th>P50 延迟</th><th>P99 延迟</th></tr></thead>
          <tbody>
            <tr><td>10</td><td>≈ 2,000</td><td>4.6 ms</td><td>13 ms</td></tr>
            <tr><td>50</td><td>≈ 2,400</td><td>19 ms</td><td>47 ms</td></tr>
            <tr><td>100</td><td>≈ 1,400</td><td>68 ms</td><td>173 ms</td></tr>
            <tr><td>200</td><td>≈ 700</td><td>266 ms</td><td>673 ms</td></tr>
          </tbody>
        </table>
      </div>
      <p style="font-size:14px;color:var(--muted);margin-bottom:8px">
        <b>怎么读这些数字:</b>并发 ≤ 50 时网关自身引入的开销只有几毫秒,吞吐 2,000+ QPS;
        并发升到 200 时吞吐下降,瓶颈是 <b>SQLite 单写者</b>:每个请求的计费(日志 + 扣费)是一
        次串行写事务,在途请求多了就在写锁上排队——这是有意的设计取舍(计费必须随响应落库,
        不能异步丢账),不是网络或协议转换的开销。
      </p>
      <p style="font-size:14px;color:var(--muted)">
        <b>容量估算(上游无限速度时):</b>真实场景里上游响应普遍以秒计,网关承接的是"长连接占
        位"而非 CPU 吞吐。按上游平均 5 秒/请求估算,支撑 200 个并发在途只需约 40 QPS——离
        700 QPS 的上限很远。折算用户规模:重度用户每 10 秒一次请求约可支撑 <b>7,000 名同时
        活跃用户</b>;典型使用(人均每分钟一次)可支撑 <b>4 万+ 在线用户</b>。也就是说单机 SQLite
        完全够用,<b>瓶颈永远在上游渠道的速度,而不是这台网关</b>。
      </p>
    </div>
  `;
}
