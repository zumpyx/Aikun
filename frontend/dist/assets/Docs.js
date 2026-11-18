import{j as e,C as i,a as d,b as t,d as c}from"./app.js";import{T as a,a as x,b as r,c as l,d as h,e as s}from"./table.js";function o({children:n}){return e.jsx("pre",{className:"mb-3 overflow-x-auto rounded-md bg-muted p-3.5 text-xs leading-relaxed",children:e.jsx("code",{children:n})})}function p(){const n=window.location.origin;return e.jsxs("div",{className:"space-y-4",children:[e.jsxs("div",{children:[e.jsx("h2",{className:"text-xl font-bold",children:"接口文档"}),e.jsx("p",{className:"text-sm text-muted-foreground",children:"AI 代理接口使用指南"})]}),e.jsxs("p",{className:"text-sm text-muted-foreground",children:["接口地址: ",e.jsx("code",{className:"rounded bg-muted px-2 py-0.5",children:n}),"— 使用 API 密钥调用,请求头携带 ",e.jsx("code",{children:"Authorization: Bearer sk-..."}),"(密钥在「密钥」页创建)"]}),e.jsxs(i,{children:[e.jsx(d,{children:e.jsx(t,{children:"AI 代理 — OpenAI 兼容接口"})}),e.jsxs(c,{className:"space-y-2",children:[e.jsx("p",{className:"text-sm text-muted-foreground",children:"与 OpenAI API 格式完全兼容，可用任何 OpenAI SDK 直接调用。支持流式（stream: true）。同一把密钥请求同一个模型时固定走同一个渠道，这个渠道失败后才会切换。"}),e.jsx("h3",{className:"pt-2 font-semibold",children:"Chat Completions"}),e.jsx(o,{children:`POST ${n}/v1/chat/completions
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
}`}),e.jsx("h3",{className:"pt-2 font-semibold",children:"列出可用模型"}),e.jsx(o,{children:`GET ${n}/v1/models
Authorization: Bearer sk-...

→ 200
{"object": "list", "data": [{"id": "gpt-4", "object": "model", "owned_by": "aikun"}]}`})]})]}),e.jsxs(i,{children:[e.jsx(d,{children:e.jsx(t,{children:"AI 代理 — OpenAI Responses 接口(codex 等客户端)"})}),e.jsxs(c,{children:[e.jsxs("p",{className:"mb-3 text-sm text-muted-foreground",children:["与 OpenAI Responses API 格式兼容，codex 等使用 ",e.jsx("code",{children:"/v1/responses"})," 的客户端可直接接入，支持流式。 渠道勾选 Responses 支持协议时按原生透传（保留 reasoning 等专有特性）；未勾选时自动降级为协议转换。"]}),e.jsx(o,{children:`POST ${n}/v1/responses
Authorization: Bearer sk-...
Content-Type: application/json

{
  "model": "gpt-5",
  "input": "Hello",
  "stream": true
}`})]})]}),e.jsxs(i,{children:[e.jsx(d,{children:e.jsx(t,{children:"AI 代理 — 通用端点透传"})}),e.jsxs(c,{children:[e.jsxs("p",{className:"mb-3 text-sm text-muted-foreground",children:["chat 之外的 OpenAI 兼容端点按",e.jsx("b",{children:"字节级透传"}),":按 ",e.jsx("code",{children:"model"})," 选路(同一把密钥、同一个模型固定渠道,失败才切换)、应用渠道模型映射、 替换为渠道凭证原样转发，响应原样回传——",e.jsx("b",{children:"不做协议转换"}),",因此只会路由到勾选 openai 协议的渠道。 响应里有 usage 的（embeddings）按价格表计费；没有的（images/moderations）按 0 元记账，日志照常落库。"]}),e.jsx("p",{className:"mb-2 text-sm text-muted-foreground",children:"支持以下路径（其余一律 404;files/batches/audio 等无 model 或 multipart 端点不透传）:"}),e.jsx(o,{children:`POST ${n}/v1/embeddings
POST ${n}/v1/moderations
POST ${n}/v1/images/generations
POST ${n}/v1/rerank
Authorization: Bearer sk-...
Content-Type: application/json

{
  "model": "text-embedding-3-small",
  "input": "Hello"
}`})]})]}),e.jsxs(i,{children:[e.jsx(d,{children:e.jsx(t,{children:"AI 代理 — Anthropic 兼容接口"})}),e.jsxs(c,{children:[e.jsxs("p",{className:"mb-3 text-sm text-muted-foreground",children:["与 Anthropic Messages API 格式兼容，可用 Anthropic SDK 调用（也接受 ",e.jsx("code",{children:"x-api-key"})," 头代替 Bearer）。无论上游渠道是 OpenAI 还是 Anthropic 协议，网关都会自动双向转换（含流式、工具调用、图片）。"]}),e.jsx("h3",{className:"mb-2 font-semibold",children:"Messages"}),e.jsx(o,{children:`POST ${n}/v1/messages
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
}`})]})]}),e.jsxs(i,{children:[e.jsx(d,{children:e.jsx(t,{children:"AI 代理 — MCP 工具透传(ZCode 渠道)"})}),e.jsxs(c,{children:[e.jsxs("p",{className:"mb-3 text-sm text-muted-foreground",children:["ZCode 订阅自带的远程 MCP 工具(天眼查企业查询 ",e.jsx("code",{children:"finance_tianyancha"}),"、图片搜索 ",e.jsx("code",{children:"image_search"})," 等) 可直接经网关调用:MCP Streamable HTTP(JSON-RPC 2.0)字节级透传,鉴权用",e.jsx("b",{children:"你的网关 API Key"}),", 上游自动挂 ZCode 渠道的登录态 JWT(消耗套餐权限,网关侧 0 元记账)。按渠道优先级自动选路,失败转移。 目录查询与调用示例:"]}),e.jsx(o,{children:`GET ${n}/v1/mcp
Authorization: Bearer sk-...
→ 200 {"servers": [{"key": "finance_tianyancha", "description": "天眼查企业信息查询(工商/股权/涉诉/舆情)"}, ...]}

POST ${n}/v1/mcp/finance_tianyancha
Authorization: Bearer sk-...
Mcp-Session-Id: <initialize 返回的会话 ID,后续请求携带>
Content-Type: application/json

{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {"name": "search_companies", "arguments": {"query": "智谱"}}
}

→ 200(JSON-RPC 结果原样回传;initialize / tools/list 同理)}`}),e.jsxs("p",{className:"mt-2 text-sm text-muted-foreground",children:["注:v1 仅透传 POST(initialize / tools/list / tools/call 均走 POST);",e.jsx("code",{children:"Mcp-Session-Id"}),"、",e.jsx("code",{children:"Mcp-Protocol-Version"}),"、",e.jsx("code",{children:"Accept"})," 头会原样送达上游。无可用 ZCode 渠道时返回 503。"]})]})]}),e.jsxs(i,{children:[e.jsx(d,{children:e.jsx(t,{children:"错误语义"})}),e.jsxs(c,{children:[e.jsx("p",{className:"mb-3 text-sm text-muted-foreground",children:"入口拒绝发生在响应开始之前，返回完整 JSON 错误（按客户端协议格式化，与官方形状一致）,不会产生半截流。"}),e.jsxs(a,{children:[e.jsx(x,{children:e.jsxs(r,{children:[e.jsx(l,{children:"状态码"}),e.jsx(l,{children:"type"}),e.jsx(l,{children:"触发条件"})]})}),e.jsxs(h,{children:[e.jsxs(r,{children:[e.jsx(s,{children:"400"}),e.jsx(s,{children:e.jsx("code",{children:"invalid_request_error"})}),e.jsx(s,{children:"请求体缺少 model 字段等格式错误"})]}),e.jsxs(r,{children:[e.jsx(s,{children:"401"}),e.jsx(s,{children:e.jsx("code",{children:"unauthorized"})}),e.jsx(s,{children:"API Key 缺失、无效、已过期或被删除(返回简单 JSON,非协议错误形状)"})]}),e.jsxs(r,{children:[e.jsx(s,{children:"402"}),e.jsx(s,{children:e.jsx("code",{children:"insufficient_quota"})}),e.jsx(s,{children:"账户余额 ≤ 0,充值后自动恢复"})]}),e.jsxs(r,{children:[e.jsx(s,{children:"403"}),e.jsx(s,{children:e.jsx("code",{children:"model_not_allowed"})}),e.jsx(s,{children:"该 API Key 的模型白名单不含请求的模型"})]}),e.jsxs(r,{children:[e.jsx(s,{children:"429"}),e.jsx(s,{children:e.jsx("code",{children:"rate_limit_error"})}),e.jsx(s,{children:"触发 API Key 限额:每分钟请求数 / 每日 Token 额度 / 并发在途上限(均可在「密钥」页配置)"})]}),e.jsxs(r,{children:[e.jsx(s,{children:"502"}),e.jsx(s,{children:e.jsx("code",{children:"upstream_error"})}),e.jsx(s,{children:"所有可用渠道均失败(已达最大重试次数)"})]}),e.jsxs(r,{children:[e.jsx(s,{children:"503"}),e.jsx(s,{children:e.jsx("code",{children:"provider_unavailable"})}),e.jsx(s,{children:"没有任何已启用渠道声明支持该模型"})]})]})]})]})]}),e.jsxs(i,{children:[e.jsx(d,{children:e.jsx(t,{children:"钱包 — 兑换码充值"})}),e.jsxs(c,{children:[e.jsxs("p",{className:"mb-3 text-sm text-muted-foreground",children:["兑换码由管理员在「计费」页批量生成。同一用户 10 分钟内连续兑换失败 5 次将被限流(429); 无效、已使用、已过期的码统一返回 ",e.jsx("code",{children:"invalid_code"}),"(400),不区分原因。"]}),e.jsx(o,{children:`POST ${n}/api/users/me/redemptions
Authorization: Bearer <管理端登录令牌>(/api/* 仅接受登录会话,API Key 不适用)
Content-Type: application/json

{"code": "AK-XXXX-XXXX-XXXX-XXXX"}

→ 201
{"amount": 10.0, "balance": 25.5}`})]})]})]})}export{p as default};
