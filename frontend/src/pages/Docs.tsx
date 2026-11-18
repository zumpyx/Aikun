// 接入文档(docs.js 迁移)
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Table, TBody, TD, TH, THead, TR } from '@/components/ui/table'

function CodeBlock({ children }: { children: string }) {
  return (
    <pre className="mb-3 overflow-x-auto rounded-md bg-muted p-3.5 text-xs leading-relaxed">
      <code>{children}</code>
    </pre>
  )
}

export default function Docs() {
  const baseUrl = window.location.origin
  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-xl font-bold">接口文档</h2>
        <p className="text-sm text-muted-foreground">AI 代理接口使用指南</p>
      </div>
      <p className="text-sm text-muted-foreground">
        接口地址: <code className="rounded bg-muted px-2 py-0.5">{baseUrl}</code>
        — 使用 API 密钥调用,请求头携带 <code>Authorization: Bearer sk-...</code>(密钥在「密钥」页创建)
      </p>

      <Card>
        <CardHeader><CardTitle>AI 代理 — OpenAI 兼容接口</CardTitle></CardHeader>
        <CardContent className="space-y-2">
          <p className="text-sm text-muted-foreground">与 OpenAI API 格式完全兼容，可用任何 OpenAI SDK 直接调用。支持流式（stream: true）。同一把密钥请求同一个模型时固定走同一个渠道，这个渠道失败后才会切换。</p>
          <h3 className="pt-2 font-semibold">Chat Completions</h3>
          <CodeBlock>{`POST ${baseUrl}/v1/chat/completions
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
}`}</CodeBlock>
          <h3 className="pt-2 font-semibold">列出可用模型</h3>
          <CodeBlock>{`GET ${baseUrl}/v1/models
Authorization: Bearer sk-...

→ 200
{"object": "list", "data": [{"id": "gpt-4", "object": "model", "owned_by": "aikun"}]}`}</CodeBlock>
        </CardContent>
      </Card>

      <Card>
        <CardHeader><CardTitle>AI 代理 — OpenAI Responses 接口(codex 等客户端)</CardTitle></CardHeader>
        <CardContent>
          <p className="mb-3 text-sm text-muted-foreground">
            与 OpenAI Responses API 格式兼容，codex 等使用 <code>/v1/responses</code> 的客户端可直接接入，支持流式。
            渠道勾选 Responses 支持协议时按原生透传（保留 reasoning 等专有特性）；未勾选时自动降级为协议转换。
          </p>
          <CodeBlock>{`POST ${baseUrl}/v1/responses
Authorization: Bearer sk-...
Content-Type: application/json

{
  "model": "gpt-5",
  "input": "Hello",
  "stream": true
}`}</CodeBlock>
        </CardContent>
      </Card>

      <Card>
        <CardHeader><CardTitle>AI 代理 — 通用端点透传</CardTitle></CardHeader>
        <CardContent>
          <p className="mb-3 text-sm text-muted-foreground">
            chat 之外的 OpenAI 兼容端点按<b>字节级透传</b>:按 <code>model</code> 选路(同一把密钥、同一个模型固定渠道,失败才切换)、应用渠道模型映射、
            替换为渠道凭证原样转发，响应原样回传——<b>不做协议转换</b>,因此只会路由到勾选 openai 协议的渠道。
            响应里有 usage 的（embeddings）按价格表计费；没有的（images/moderations）按 0 元记账，日志照常落库。
          </p>
          <p className="mb-2 text-sm text-muted-foreground">支持以下路径（其余一律 404;files/batches/audio 等无 model 或 multipart 端点不透传）:</p>
          <CodeBlock>{`POST ${baseUrl}/v1/embeddings
POST ${baseUrl}/v1/moderations
POST ${baseUrl}/v1/images/generations
POST ${baseUrl}/v1/rerank
Authorization: Bearer sk-...
Content-Type: application/json

{
  "model": "text-embedding-3-small",
  "input": "Hello"
}`}</CodeBlock>
        </CardContent>
      </Card>

      <Card>
        <CardHeader><CardTitle>AI 代理 — Anthropic 兼容接口</CardTitle></CardHeader>
        <CardContent>
          <p className="mb-3 text-sm text-muted-foreground">与 Anthropic Messages API 格式兼容，可用 Anthropic SDK 调用（也接受 <code>x-api-key</code> 头代替 Bearer）。无论上游渠道是 OpenAI 还是 Anthropic 协议，网关都会自动双向转换（含流式、工具调用、图片）。</p>
          <h3 className="mb-2 font-semibold">Messages</h3>
          <CodeBlock>{`POST ${baseUrl}/v1/messages
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
}`}</CodeBlock>
        </CardContent>
      </Card>

      <Card>
        <CardHeader><CardTitle>错误语义</CardTitle></CardHeader>
        <CardContent>
          <p className="mb-3 text-sm text-muted-foreground">
            入口拒绝发生在响应开始之前，返回完整 JSON 错误（按客户端协议格式化，与官方形状一致）,不会产生半截流。
          </p>
          <Table>
            <THead><TR><TH>状态码</TH><TH>type</TH><TH>触发条件</TH></TR></THead>
            <TBody>
              <TR><TD>400</TD><TD><code>invalid_request_error</code></TD><TD>请求体缺少 model 字段等格式错误</TD></TR>
              <TR><TD>401</TD><TD><code>unauthorized</code></TD><TD>API Key 缺失、无效、已过期或被删除(返回简单 JSON,非协议错误形状)</TD></TR>
              <TR><TD>402</TD><TD><code>insufficient_quota</code></TD><TD>账户余额 ≤ 0,充值后自动恢复</TD></TR>
              <TR><TD>403</TD><TD><code>model_not_allowed</code></TD><TD>该 API Key 的模型白名单不含请求的模型</TD></TR>
              <TR><TD>429</TD><TD><code>rate_limit_error</code></TD><TD>触发 API Key 限额:每分钟请求数 / 每日 Token 额度 / 并发在途上限(均可在「密钥」页配置)</TD></TR>
              <TR><TD>502</TD><TD><code>upstream_error</code></TD><TD>所有可用渠道均失败(已达最大重试次数)</TD></TR>
              <TR><TD>503</TD><TD><code>provider_unavailable</code></TD><TD>没有任何已启用渠道声明支持该模型</TD></TR>
            </TBody>
          </Table>
        </CardContent>
      </Card>

      <Card>
        <CardHeader><CardTitle>钱包 — 兑换码充值</CardTitle></CardHeader>
        <CardContent>
          <p className="mb-3 text-sm text-muted-foreground">
            兑换码由管理员在「计费」页批量生成。同一用户 10 分钟内连续兑换失败 5 次将被限流(429);
            无效、已使用、已过期的码统一返回 <code>invalid_code</code>(400),不区分原因。
          </p>
          <CodeBlock>{`POST ${baseUrl}/api/users/me/redemptions
Authorization: Bearer <管理端登录令牌>(/api/* 仅接受登录会话,API Key 不适用)
Content-Type: application/json

{"code": "AK-XXXX-XXXX-XXXX-XXXX"}

→ 201
{"amount": 10.0, "balance": 25.5}`}</CodeBlock>
        </CardContent>
      </Card>
    </div>
  )
}
