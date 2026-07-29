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
      <div class="card-header"><h2>AI 代理 — Anthropic 兼容接口</h2></div>
      <p style="font-size:14px;color:var(--muted);margin-bottom:12px">与 Anthropic Messages API 格式兼容，可用 Anthropic SDK 调用。无论上游渠道是 OpenAI 还是 Anthropic 协议，网关都会自动双向转换（含流式、工具调用、图片）。</p>

      <h3 style="margin:12px 0 8px">Messages</h3>
      <div class="code-block">POST ${baseUrl}/v1/messages
Authorization: Bearer sk-...
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
  `;
}


