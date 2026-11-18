# API 映射表:现有路径 → 新路径(`openapi.yaml` v0.5.0)

> 用途:前端迁移(改哪些 URL/响应形状/错误处理)与 impl 迁移
> (architecture-v0.5 §四:`src/api/admin/**` + 用户自助端点 → `openapi.yaml`
> 重新设计 + `crates/aikun-admin`)的对照清单。
> 配套:仓库根 `openapi.yaml`(OpenAPI 3.2.1,单一真相)。
> 现状锚点:路由表 `src/main.rs:157-292`;各 handler 位置见每行备注。

## 全局语义变化(先读这个,表格里只写增量)

| # | 维度 | 现状 | 新口径 | 影响面 |
|---|---|---|---|---|
| 1 | 管理面前缀 | `/api/admin/**` 与 `/api/**` 并存 | 保留 `/api`,**取消 `/admin` 段**;admin 边界由 tag + `require_admin` 中间件承担 | 全部管理端前端调用 |
| 2 | ID 类型 | UUID 字符串(`src/models/*.rs`) | **integer int64** | 全部 `{id}` 路径参数与引用字段;**impl 迁移方案待定,见备注 1** |
> **已拍板(2026-11-16)**:ID 保持不透明 string,弃 int64——JS Number 仅 53 位安全整数(Google AIP-140 亦要求 int64 序列化为 string),且 SurrealDB record ID 天生字符串。零数据迁移,现网 UUID 直接沿用。
| 3 | 列表响应 | 绝大多数裸数组(仅 redemption-codes 已是 `{total,items}`) | 统一 `{items, total}` 信封 + `limit/offset` 分页(默认 50,clamp 1..=200) | 所有列表消费方 |
| 4 | 删除成功 | `200 {message:'deleted'/'disabled'}` | **204 No Content** | api-keys / users / providers / prices / redemption-codes 的删除分支 |
| 5 | 创建成功 | 混合(多数 201;调账 200;兑换 200;tests 200) | **统一 201**(创建资源/流水/测试记录) | balance-transactions、redemptions、tests、model-tests |
| 6 | PATCH 成功 | api-keys / users / providers 均回 `{message:'updated'}`(仅 prices 已回资源,src/api/admin/billing.rs:369) | 统一**返回更新后的资源** | updateApiKey / updateProvider / **adminUpdateUser** |
| 7 | 错误体 | `{error, message}` 自定义 | **RFC 9457 problem+json** `{type,title,status,detail,code}`;机器可读短码挪到 `code`,`message` → `detail` | 所有非 2xx 分支;码对照见文末 |
| 8 | last_admin 保护 | `400 last_admin` | **`409 last_admin`**(状态冲突语义) | update/delete user |
| 9 | 时间出参 | RFC3339 与 SQLite `datetime('YYYY-MM-DD HH:MM:SS')` 混存 | 统一 RFC3339 | providers / redemption-codes / billing_transactions 等表 |
| 10 | providers 批量状态码 | 全败 400 / 部分 201 | 统一 **200**,成败全在 body | 前端只需读 body |
| 11 | 金额单位 | 元(f64,库内微元换算) | 不变(元;价格每 1M tokens) | 无 |
| 12 | 429 响应头 | 登录/兑换的 429 分支**不发送** Retry-After(src/api/auth.rs:125-133、src/api/redemption.rs:411-416) | **新增 Retry-After 头**(建议重试秒数) | 前端可选用;impl 迁移需补发该头 |

`/v1/**` 代理面**路径、契约、operationId 全部不变**(v1 冻结,厂商兼容是硬约束);
`/api/health` 不入 spec、路径不变。

## Auth 层(裸挂,无 auth_middleware)

| 现有 | 新 | operationId | 语义变化 / 备注 |
|---|---|---|---|
| `GET /` | `GET /` | serveFrontend | 不变;CSP 与 no-cache 头保留 |
| `GET /static/{*path}` | `GET /static/{path}` | serveStaticAsset | 路径不变;spec 无 `{*}` 通配语法,参数文档注明允许嵌套;白名单外 404 纯文本保留 |
| `POST /api/login` | `POST /api/auth/login` | login | 路径归入 `/auth` 命名空间;429 码 `too_many_attempts` → **`rate_limited`**(与兑换、/v1 共用词表);401 invalid_credentials / 403 account_disabled / 防枚举时序均衡全部保留;axum 反序列化拒绝(旧实现会回 422)统一为 400 invalid_body |
| `GET /api/health` | —(不入 spec) | healthCheck | 路径与行为不变;spec 刻意排除(architecture-v0.5 决策 2) |

## User 层(JWT,包 auth_middleware)

| 现有 | 新 | operationId | 语义变化 / 备注 |
|---|---|---|---|
| `GET /api/me` | `GET /api/users/me` | getCurrentUser | 资源化;404 码 `user_not_found` → `not_found`;`me` 与 `{id}`(int64)无路由冲突 |
| `GET /api/version` | `GET /api/system/version` | getVersion | 归入 `/system/*` 单例族;仍刻意放鉴权后 |
| `GET /api/api-keys` | `GET /api/api-keys` | listApiKeys | 路径不变;响应裸数组 → `{items,total}` + 新增 limit/offset;`?user_id=` 仅 admin 生效保留;key 恒掩码 |
| `POST /api/api-keys` | `POST /api/api-keys` | createApiKey | 不变;201 + 明文 key 仅一次 |
| `PATCH /api/api-keys/{id}` | `PATCH /api/api-keys/{id}` | updateApiKey | 成功响应 `{message:'updated'}` → 返回资源;空 body 400 no_fields_to_update 保留;越权 404 保留 |
| `DELETE /api/api-keys/{id}` | `DELETE /api/api-keys/{id}` | deleteApiKey | 200 `{message}` → **204** |
| `GET /api/logs` | `GET /api/logs` | listLogs | 裸数组 → `{items,total}`;过滤参数(limit/offset/model/success/user_id/since)全部保留同语义 |
| `GET /api/logs/stats` | `GET /api/logs/stats` | getLogStats | 路径不变;与列表共用过滤参数保留 |
| `GET /api/wallet` | `GET /api/users/me/wallet` | getWalletStats | 资源化挂到当前用户;响应形状不变 |
| `POST /api/wallet/redeem` | `POST /api/users/me/redemptions` | redeemCode | 动作端点 → "我的兑换记录"子资源;200 → **201**;400 invalid_code / 429 rate_limited / 同事务核销+加余额+流水保留 |
| `GET /api/prices` | `GET /api/models/prices` | listPublicPrices | 与管理端价格 CRUD(`/api/prices`)分流为两个资源;裸数组 → `{items,total}` + 分页;通配展开、未定价三 null、不按 key 白名单过滤均保留 |

## Admin 层(JWT + role=admin;新路径无 `/admin` 段)

| 现有 | 新 | operationId | 语义变化 / 备注 |
|---|---|---|---|
| `GET /api/admin/users` | `GET /api/users` | adminListUsers | 全量 → `{items,total}` + 分页;含 balance(元)不变 |
| `POST /api/admin/users` | `POST /api/users` | adminCreateUser | 201 保留;校验与 409 username_exists 保留 |
| `POST /api/admin/users/batch` | `POST /api/users/batch` | adminCreateUsersBatch | 保留 batch 动作子资源(x-action);整体 200;响应保留 `{created, failed, results}` 计数摘要 + 逐项结果(现实现已返回,src/api/admin/users.rs:390-398,前端 users.js 在消费);失败项 `error` 字段 → `code`;400 细分 empty_batch / batch_too_large |
| `GET /api/admin/users/{id}` | `GET /api/users/{id}` | adminGetUser | 404 保留 |
| `PATCH /api/admin/users/{id}` | `PATCH /api/users/{id}` | adminUpdateUser | 成功响应 `{message:'updated'}` → **返回更新后的 User 资源**(src/api/admin/users.rs:535);last_admin 由 400 → **409**;改密 token_version+1 保留 |
| `DELETE /api/admin/users/{id}` | `DELETE /api/users/{id}` | adminDeleteUser | **软禁用保留**(is_active=0),200 `{message:'disabled'}` → **204**;last_admin 409;不提供物理删除 |
| `GET /api/admin/billing/transactions` | `GET /api/balance-transactions` | adminListBillingTransactions | 升为一级资源;limit 由固定 200 → 可传(默认 50,clamp 1..=200);裸数组 → `{items,total}`;`?user_id=`(空=全部)保留 |
| `POST /api/admin/users/{id}/balance` | `POST /api/users/{id}/balance-transactions` | adminAdjustUserBalance | 动作端点 → 余额流水子资源;200 → **201**;同事务改余额+写流水、正 recharge 负 adjust 保留 |
| `GET /api/admin/providers` | `GET /api/providers` | adminListProviders | `{items,total}` + 分页;proxy_url 脱敏、api_key 永不出参、查询失败 500 保留 |
| `POST /api/admin/providers` | `POST /api/providers` | adminCreateProvider | 201 保留;全部跨字段校验与 409 duplicate_key 保留 |
| `POST /api/admin/providers/batch` | `POST /api/providers/batch` | adminCreateProvidersBatch | 保留 batch(x-action);**状态码统一 200**(旧:全败 400/部分 201),成败全在 body;失败项 `error` → `code` |
| `POST /api/admin/providers/fetch-models` | `POST /api/providers/fetch-models` | adminFetchUpstreamModels | **保留 POST 动作端点**(理由见备注 2);编辑态回退 / 502 只回显 host / 8MB 上限保留 |
| `GET /api/admin/providers/{id}` | `GET /api/providers/{id}` | adminGetProvider | 404 保留 |
| `PATCH /api/admin/providers/{id}` | `PATCH /api/providers/{id}` | adminUpdateProvider | 成功响应 `{message:'updated'}` → 返回资源;"最终状态"联动校验、空串清代理保留。并发 PATCH 由服务级串行锁互斥(读-改-写原子性等价旧实现的持锁全程;不走该服务的写者除外,同锁事务语义待 store 收编,见备注 13 末段) |
| `DELETE /api/admin/providers/{id}` | `DELETE /api/providers/{id}` | adminDeleteProvider | 物理删除保留;200 `{message}` → **204** |
| `POST /api/admin/providers/{id}/test` | `POST /api/providers/{id}/tests` | adminTestProvider | 动作端点 → tests 子资源;200 → **201**(创建探测结果);不更新 EMA 延迟保留 |
| `POST /api/admin/providers/{id}/test-model` | `POST /api/providers/{id}/model-tests` | adminTestProviderModel | 动作端点 → model-tests 子资源;200 → **201**;业务失败仍是成功响应(ok/status/error 在 body)保留;stream=true 时 text/event-stream |
| `POST /api/admin/providers/{id}/duplicate` | `POST /api/providers/{id}/duplicates` | adminDuplicateProvider | 动作端点 → duplicates 子资源(GitHub fork 模式);201 保留;运行态重置保留 |
| `GET /api/admin/model-health` | `GET /api/model-health` | adminListModelHealth | `{items,total}` + 分页 + `?provider_id=` 过滤;查询失败 500 保留 |
| `GET /api/admin/channel-kinds` | `GET /api/channel-kinds` | adminListChannelKinds | `{items,total}` 信封;**不设分页参数**(目录型,见备注 5) |
| `GET /api/admin/prices` | `GET /api/prices` | adminListPrices | `{items,total}` + 分页;`?in_use=1` 保留;cached_price 可 null 保留 |
| `POST /api/admin/prices` | `POST /api/prices` | adminCreatePrice | 201 保留;通配校验、409 duplicate_model 保留 |
| `PATCH /api/admin/prices/{id}` | `PATCH /api/prices/{id}` | adminUpdatePrice | **cached_price 三态**(缺省/null/数值)在 schema 用 optional+nullable 双层表达,行为不变 |
| `DELETE /api/admin/prices/{id}` | `DELETE /api/prices/{id}` | adminDeletePrice | 200 `{message}` → **204** |
| `GET /api/admin/redemption-codes` | `GET /api/redemption-codes` | adminListRedemptionCodes | 已是 `{total,items}`,对齐为 `{items,total}`;status 枚举含计算态 `expired` + 布尔 `expired` 字段保留;掩码永不还原保留 |
| `POST /api/admin/redemption-codes` | `POST /api/redemption-codes` | adminCreateRedemptionCodes | 201 保留;expires_at 只收 `YYYY-MM-DD[ HH:MM:SS]`(拒 RFC3339 'T')保留;明文仅一次保留;500 code_collision 保留 |
| `POST /api/admin/redemption-codes/{id}/disable` | `DELETE /api/redemption-codes/{id}` | adminDisableRedemptionCode | 动作端点 → **DELETE**(生命周期单调:unused → used/disabled,禁用即终结);200 `{message:'disabled'}` → **204**;409 not_unused / 404 保留 |
| `GET /api/admin/usage-stats` | `GET /api/usage-stats` | adminGetUsageStats | 仅去 `/admin` 段;响应形状不变 |
| `GET /api/admin/stats` | `GET /api/system/stats` | adminGetGlobalStats | 归入 `/system/*` 单例族(与 version 同族);响应形状不变 |

## Proxy 层(/v1,仅文档,绝不进生成白名单)

路径、operationId、请求/响应契约**全部不变**;openapi.yaml 中以 loose schema
(通用 object)+ `text/event-stream` 标注收录,description 注明
"代理端点,契约属各厂商,不入生成白名单"。

| 现有 = 新 | operationId |
|---|---|
| `POST /v1/chat/completions` | createChatCompletion |
| `POST /v1/messages` | createMessage |
| `POST /v1/responses` | createResponse |
| `GET /v1/models` | listModels |
| `POST /v1/embeddings` | createEmbedding |
| `POST /v1/moderations` | createModeration |
| `POST /v1/images/generations` | createImage |
| `POST /v1/rerank` | createRerank |

注:实现里 embeddings/moderations/images/rerank 经 `POST /v1/{*path}` 通配 +
白名单(`src/api/proxy/passthrough.rs:34`)。spec **不建模通配模板**
(`/v1/{path}` 会吞掉 `GET /v1/models` 的文档),只列显式端点。

## 错误码迁移对照(旧 `{error}` → 新 problem+json `code`)

绝大多数旧 `error` 值**原样成为 `code`**;仅以下归一:

| 旧 | 新 | 出处 |
|---|---|---|
| `too_many_attempts`(登录 429) | `rate_limited` | 与兑换 429、/v1 限额共用词表 |
| `user_not_found`(GET /api/me) | `not_found` | 与其余 404 统一 |
| 批量失败项字段 `error` | `code` | 与 Problem 对齐 |
| (新增)`invalid_body` | 请求体缺字段/非法 JSON 统一 400 | 旧实现此处由 axum 回 422,统一到 400 |

`/v1` 错误体**不迁移**:按客户端协议(OpenAI/Anthropic 厂商格式)原样返回,
openapi.yaml 用 loose 的 `ProxyError` 描述,短码
(`insufficient_quota` / `rate_limit_error` / `model_not_allowed` /
`provider_unavailable` / `upstream_error`)写在厂商格式的 code/type 字段里。

**401 例外**:auth 中间件在进入代理管线**之前**自产 401,错误体是网关扁平体
`{error:"<code>", message}`(`error` 是字符串,不是对象;`src/auth/mod.rs:332-339、
367-384`)—— 不是厂商格式,按厂商 SDK 的 `error.message` / `error.code`
解析会直接失败。spec 中 /v1 全部 401 引用独立的 `GatewayAuthError` schema
(短码:`invalid_token` = Bearer 路径 JWT 与 sk- key 均未通过;
`unauthorized` = 缺 Authorization 头或 x-api-key 通道无效)。

## 备注(拿不准的 / 保留的 / 待决的)

1. **ID int64 迁移方案待定(本表最大待决项)**。spec 按约束定为
   integer int64,但现库全是 UUID 主键。impl 迁移需二选一:新增自增/雪花列
   并保留 UUID 做外部引用,或一次性换主键(牵连 request_logs 等 ON DELETE
   外键)。**这是评审时需要拍板的点**,spec 只锁"对外是 int64"。
2. **`POST /api/providers/fetch-models` 保留动作端点**(未转子资源):
   它是"带凭据的查询"——api_key 不能进 query string(会落访问日志),
   只能走请求体;且不落库、无资源可寻址。已标 `x-action: fetch-upstream-models`。
3. **batch 端点保留**(`/api/users/batch`、`/api/providers/batch`):
   "提交一批,收一份逐项报告"没有干净的纯 REST 表达,业界惯例也是
   batch 子动作;已标 `x-action: batch-create`。providers batch 状态码
   统一为 200 是本次决策,请评审确认。
4. **`DELETE /api/users/{id}` 仍是软禁用**(HTTP 谓词与实现不完全对齐)。
   诚实起见 operationId 保留 `adminDeleteUser`、description 明写"软禁用,
   不提供物理删除"。若未来需要物理删除,建议保持 DELETE=软,物理删除走
   显式管理操作,避免语义漂移。
5. **`GET /api/channel-kinds` 不设分页**:编译期静态注册的目录(个位数),
   分页是纯噪音;响应仍用 `{items,total}` 信封保持客户端统一。
   同理 `GET /api/logs/stats`、`/api/usage-stats`、`/api/system/stats`
   为聚合单例,无分页。
6. **/v1 不套 problem+json**:约束"统一错误 envelope"与"/v1 仅作文档、
   字节级透传"冲突时取后者——OpenAI/Anthropic SDK 靠厂商格式错误体工作,
   换信封会破坏所有既有客户端。管理面全部非 2xx 引用 `Problem`,
   /v1 引用 loose `ProxyError`(**401 除外**,引用 `GatewayAuthError`,
   见文末错误码对照),短码词表在两者间保持同义对应。
7. **`GET /api/models/prices`(用户)与 `/api/prices`(管理)是两个资源**:
   前者是"健康渠道在售目录(通配展开、只读)",后者是"价格条目 CRUD
   (含通配原文)"。分流而非同路径按角色变形,避免文档与权限双重歧义。
8. **兑换写流水的 kind='recharge'**(`src/api/redemption.rs:471-475`),
   与管理员正向调账同 kind,未拆独立枚举值;若审计需要区分,加
   'redeem' 枚举是低成本的后续项。
9. **`in_use` / `success` 等布尔语义的字符串参数保留原样**
   (`in_use=1`、`success=0/1`),不做 breaking 改动;前端迁移时无需动。
   两者严格度**刻意不同**:`success` 非 0/1 是真实 400(实现如此);
   `in_use` 非 '1' 值被实现静默忽略、等同缺省(src/api/admin/billing.rs:89),
   spec 按宽容语义描述,**未**严格化为 400。
10. **登录限流/兑换限流说明**:内存滑动窗口、重启清零、尽力而为
    (src/main.rs:145-152);spec 在 429 响应描述中注明,不承诺精确配额。
11. **版本门禁(消费者侧硬要求)**:本表是 0.5.0 目标态,历史 0.4.4 二进制
    的管理面与它完全不兼容 —— 按本 spec 生成的客户端**不得**指向 0.4.x
    实例(会静默拿裸数组/错误语义而非报错;最危险的坑:`GET /api/prices`
    在 0.4.4 是用户只读价目,在 0.5.0 是 Admin CRUD,同路径语义翻转)。
    门禁手段:`GET /api/system/version` 主次版本 ≥ 0.5(需鉴权)。
    **状态(2026-11-17)**:二进制已随 plan 步骤 6 全批完成升版
    **0.5.0**(`aikun/Cargo.toml`),门禁可用;`/api/health` 响应带同一
    version 字段作**无鉴权探测**的建议仍未落地(handler 在 `main.rs`,
    impl 侧待办)——health 本身不入 spec,不违反"health 不进 spec"的
    约束,spec 只在此记录建议。
12. **logs 的 `since` 是词法比较,不是时间解析**(`src/api/logs.rs:74-79`):
    库内 created_at 是 `to_rfc3339()` 输出(+00:00 带亚秒)。发 `Z` 后缀值
    会漏掉同秒的全部行('Z' > '.'),发非 UTC 偏移(如 +08:00)会静默漏数
    小时。spec 对 `since` **刻意不加** `format: date-time`(避免生成客户端
    把值"规范化"成 Z 后缀反而踩坑),并在参数描述中写明应发与库同构的
    UTC +00:00 形态。根治(服务端解析成 DateTime 再比较)是可选的 impl
    改进项,迁移期先按现行为如实描述。
13. **5xx→4xx 过渡映射(spec 缺口,待裁决;复审 2026-11-17 单列)**:
    spec 的 Problem 词表里有 500 短码(`internal_error` / `query_failed` /
    `create_failed` / `update_failed` / `delete_failed`),但除
    providers / model-health 列表与 `adminCreateRedemptionCodes` 外,
    **绝大多数 op 的响应枚举没有 5xx 变体** —— 旧实现"DB 锁中毒 / SQL
    失败 → 500"在新契约下无法表达。过渡期各批统一折衷(勿当定案):
    **fail-closed 映射到枚举可得的非 401 变体**(Admin 批 403/400/404;
    User 批写路径 400、删除 404;`getCurrentUser` 的 404 系旧行为),
    body `code` 保留真实 500 词表短码、`status` 与 HTTP 一致(RFC 9457
    自洽),`tracing::error!` 留痕。逐 op 对照见各 impl 文件头
    (`crates/aikun-admin/src/{admin_users,admin_billing,admin_providers,user}.rs`)。
    **已知副作用**:`listApiKeys` / `getWalletStats` / `listPublicPrices`
    三个 op 枚举只有 Unauthorized 变体,存储故障只能回 401(body
    code=`internal_error`)—— 前端 core.js 对任何 401 清 token 踢会话,
    DB 抖动会误杀登录态;`login` 同为 fail-closed 401(踩坑 #2,但登录
    页 401 不触发踢会话,影响小)。**根治需 spec 为相关 op 补 5xx 响应后
    重生成**,各批 notes 已上报待人工裁决;裁决前客户端不得把上述
    4xx/401 当作终态契约。另:providers PATCH 的读-改-写原子性由
    aikun-admin 服务级串行锁对并发 PATCH 互斥恢复;不走该服务的写者
    (后台健康检查/自动禁用)仍可能在读与写之间穿插,同锁事务语义待
    阶段 3 store 收编时由原子端口方法回归。
