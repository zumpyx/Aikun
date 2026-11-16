# Changelog

本文件记录 Aikun 各版本的对外可见变化。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/),
版本号遵循 [SemVer](https://semver.org/lang/zh-CN/)。

约定(见 `docs/roadmap-v0.5.md`「纪律」):纯重构不改行为;任何行为变化必须单列条目。

## [Unreleased]

`refactor/modular-gateway` 分支,对应 roadmap-v0.5 阶段 0(可作为 0.4.5 发布)与阶段 1;
另含 architecture-v0.5 结构迁移:workspace 根包变体 + API spec / 生成管线(首批,无运行时
行为变化),以及**管理面 API spec-first 重建全批落地(行为变化,见下节;版本升至 0.5.0)**。

### 变更(管理面 API spec-first 重建,plan 步骤 6;对外行为变化)

管理面(登录 / 用户自助 / 管理员三层)按 `openapi.yaml` 全量迁移到
`crates/aikun-admin` + 生成路由,旧 `/api/admin/**` handler 与旧用户自助路径已删除,
内嵌前端同步切换。逐条对照(旧 → 新)见 `docs/api-mapping.md`。

- **路径**:去掉 `/admin` 段并资源化 —— `POST /api/login` → `/api/auth/login`、
  `GET /api/me` → `/api/users/me`、`GET /api/wallet` → `/api/users/me/wallet`、
  `POST /api/wallet/redeem` → `/api/users/me/redemptions`、`GET /api/prices`(用户
  只读价目)→ `/api/models/prices`(与管理端 `/api/prices` CRUD 分流,同路径语义
  翻转消除)、`GET /api/version` → `/api/system/version`、
  `GET /api/admin/stats` → `/api/system/stats`、其余 `/api/admin/x` → `/api/x`;
  动作端点转子资源(tests / model-tests / duplicates / balance-transactions,
  兑换码禁用转 `DELETE`)。
- **状态码**:创建统一 **201**(兑换、调账、渠道/模型探测);删除统一 **204**
  (api-keys / users / providers / prices / redemption-codes;用户删除仍是软禁用);
  api-keys / users / providers 的 **PATCH 返回更新后的资源**(旧 `{message:'updated'}`);
  providers 批量创建**统一 200**(旧全败 400 / 部分 201),成败全在 body;
  删除/禁用最后一个活跃管理员 `400 last_admin` → **`409 last_admin`**。
- **列表信封**:统一 `{items, total}` + `limit/offset` 分页(默认 50,clamp 1..=200;
  目录/聚合单例外)。
- **错误体**:管理面全部非 2xx(含装配层 401/403)改 **RFC 9457 problem+json**
  `{type,title,status,detail,code}`(旧 `{error,message}`);短码归一:登录 429
  `too_many_attempts` → **`rate_limited`**、`GET /api/me` 404 `user_not_found` →
  `not_found`、批量失败项字段 `error` → `code`。请求体校验由生成路由承担:
  缺必填字段 422 `request_validation_failed`、坏 JSON 400 `malformed_request`
  (旧 axum 裸 422 归一到统一错误体)。`/v1/**` 代理面错误体**不变**。
- **429 补 `Retry-After` 头**(登录、兑换;建议重试秒数)。
- **5xx→4xx 过渡映射(spec 缺口,勿当终态)**:生成响应枚举绝大多数 op 无 5xx
  变体,旧"DB 故障 → 500 internal_error/query_failed"过渡期 fail-closed 映射为
  枚举可得的 4xx(Admin 批 403/400/404、User 批写 400/删 404),body `code`
  保留真实 500 词表短码;`listApiKeys` / `getWalletStats` / `listPublicPrices`
  / `login` 四 op 仅退 401(body code=`internal_error`)—— 前端对 401 会清
  token,存储抖动可能误杀会话,待 spec 补 5xx 后重生成根治
  (详见 `docs/api-mapping.md` 备注 13)。
- **providers PATCH 原子性**:并发 PATCH 由服务级串行锁互斥(读库存 → 最终状态
  联动校验 → UPDATE 全程排队),等价旧实现持锁全程的管理端写互斥。
- **版本门禁**:二进制版本 0.4.4 → **0.5.0**;按 spec 生成的客户端以
  `GET /api/system/version` 主次版本 ≥ 0.5 判定兼容(api-mapping 备注 11)。
- e2e:旧管理面 / 用户自助用例随旧 handler 删除,新路径用例落位
  `aikun/tests/e2e_{auth,user,admin_users,admin_providers,admin_billing}.rs`
  (核心行为锚点——登录链路 / 计费 / 兑换——断言不减,按 plan 步骤 9 政策迁移)。

### 修复(阶段 0,行为变化)

- **O→A 响应 usage 丢失缓存维度** ①:OpenAI 上游 → Anthropic 客户端时,
  `input_tokens` 改为未缓存部分,缓存命中单列 `cache_read_input_tokens`
  (非流式与流式 `message_delta` 一致)。此前把含缓存的 `prompt_tokens`
  整个塞进 `input_tokens`。计费本就按上游原文提取用量,不受影响;变化只在客户端可见的 usage。
- **钱包统计在日志清理后缩水** ②:`/api/wallet` 的每日序列与今日/7 天/30 天
  合计改为合并 `usage_daily`(已清理明细的日聚合)与 `request_logs`,
  保留期短于 30 天时数据不再丢失。模型分布(top_models)仍只统计未清理明细。
- **日志清理长时间独占写锁** ③:过期日志改为每批 5000 行分批聚合 + 删除,
  批间释放写连接;截止时间只计算一次。

### 变更(阶段 1,纯重构,无行为变化)

- `src/proxy/convert.rs`(3.4k 行)拆为 `src/protocol/` 目录树:
  `sse.rs` / `usage.rs` / `stream.rs` / `error.rs`,`openai/` `anthropic/` `responses/`
  按源协议存放 request / response / stream;单测随实现文件迁移。
- 新增 `protocol::Protocol` 枚举取代 `&'static str` 协议常量,所有协议分支穷举 `match`,
  去掉静默 `_ =>` 兜底;字符串只在 DB / 日志 / JSON 边界出现。
- `protocol_error` / `convert_error` / `stream_error_event` / `stream_terminal_event`
  移入 `protocol/error.rs`。
- 两份 `read_json_limited` 合并为 `channel::transport::read_json_limited(resp, max_bytes)`,
  对外错误文案逐字不变。
- `RequestLog.cost` 改为整数微元(`i64`),只在 API 序列化边界换算成元。
- 模块按 roadmap 目标架构改名:`gateway/` → `pipeline/`(`mod.rs` / `dispatch.rs` /
  `stream.rs` / `admission.rs`),`channels/` + `router/` → `channel/`。

### 新增(用户需求,默认关闭 / 不改变既有行为)

- **渠道适配器**:`channel::adapter::ChannelAdapter` 编译期静态注册
  (generic / workbuddy / qoder 占位),渠道新增 `channel_kind` 列与管理端下拉,
  `GET /api/admin/channel-kinds`。空值 = generic,与旧行为完全一致。不是运行时插件系统。
- **响应缓存**:进程内内存缓存,默认 `AIKUN_CACHE_MODE=off`;可选 `deterministic` / `always`,
  按用户隔离,命中默认照常计费,响应头 `x-aikun-cache`。

### 工程(architecture-v0.5 结构迁移首批,无运行时行为变化)

- **多 crate workspace(根包变体)**:根 `Cargo.toml` 声明 `[workspace]`
  (`members = ["crates/*"]`,`resolver = "3"`),生成 crate 统一进门禁;
  二进制仍在根包(包名与构建产物路径不变),移入 `aikun/` 属后续步骤。
- **`openapi.yaml`**:仓库根新增手写 OpenAPI 3.2.1 全景 spec(单一真相):
  管理面(Auth / User / Admin 三层 tag)进生成白名单,`/v1` 代理面仅作文档
  (loose schema,不镜像厂商契约),`/api/health` 不入。目标口径(统一
  `{items,total}` 信封、创建 201 / 删除 204、PATCH 返回资源、RFC 9457
  problem+json 错误体、ID int64 等)与现行实现的逐条对照见 `docs/api-mapping.md`;
  spec 本身尚未接线,不改任何运行时行为。
- **`crates/aikun-openapi`**:管理面服务端脚手架,由 openapi-to-rust(钉版
  0.24.0,Rust 原生、无 Java)从 `openapi.yaml` 生成,43 个 operation 白名单
  (`/v1` 绝不入白名单);生成物提交进仓库(构建不依赖外部生成器),
  零手写、零测试。
- **`scripts/gen-api.sh`**:可复现生成管线:钉版安装生成器 → 生成(暂存
  `target/openapi-to-rust`)→ 组装 crate(依赖取自生成器清单 + 按引用防漏
  探测)→ rustfmt 归一;输出目录删除重跑逐字节一致,`/v1` 路由泄漏进
  生成物时拒绝组装。
- CI 门禁覆盖全 workspace:`cargo fmt --all --check`、
  `cargo clippy --workspace --all-targets --locked`、
  `cargo test --locked --workspace`。

### 测试 / 工程

- e2e:登录链路(`/api/login` → `/api/me`、错误密码、改密踢下线、禁用账户)。
- 回归测试:上述 ①②③ 各一。
- rustfmt 全库格式化(51 文件)与 clippy 告警清零(32 条,自动修复);
  CI 新增独立 `lint` job:`cargo fmt --check` + `cargo clippy --all-targets -- -D warnings`。
- 依赖钉版以支持 rustc 1.85(`idna_adapter 1.2.0`、`time 0.3.41`、`simple_asn1 0.6.3`)。

### 文档

- `docs/refactor-plan.md` 补充最佳实践(协议契约测试、流式取消与超时、计费幂等与对账、故障转移可重试边界、渠道熔断、可观测性前置、响应缓存约束),并写入阶段 1–4 任务。
- 新增 `docs/architecture-v0.5.md`(0.5 目标结构定稿:多 crate workspace、管理面 API spec-first)与 `docs/api-mapping.md`(现有 API → 新 spec 逐条对照,前端与 impl 迁移清单);
  `roadmap-v0.5.md` 标注"目标架构"一节由前者取代。
- 新增 `docs/plan-next-steps.md`(结构迁移后续步骤,按依赖排序、每步带验收标准)。

## [0.4.4] - 2026-10-19

### 新增
- 渠道亲和:同一 API Key 请求同一模型时固定走上次成功的渠道,失败后才切换并改钉。
- 登录用户可查看当前在售模型的展开价格(只读价目)。
- 添加渠道支持预设,只填 Key。

### 修复
- 跨协议转换保留 `reasoning_content`、`thinking` 与 `cache_control`。

## [0.4.3] - 2026-08-24

### 修复(Responses 协议审计)
- SSE 流内错误帧(HTTP 200 + `{"error":...}`)不再被吞:三种流式转换器输出协议对应的
  error 事件并终止,不再补发成功收尾;此类流按失败记账(502),已捕获 usage 照扣。
- Anthropic→Responses 补齐 `cached_tokens` 映射,缓存 token 不再按普通输入计费。
- 上游截断(`length` / `content_filter`)的 Responses 输出标记 `status: incomplete`,
  流式发 `response.incomplete`。
- 非标准错误类型统一映射为 `api_error`;token 计数负数按 0、超 i32 饱和,加法防溢出。
- `extract_usage_any` 的 Responses 分支判定收紧。

## [0.4.2] - 2026-08-12

### 变更
- 兑换码管理并入计费页;兑换码升级为 80-bit 格式。
- 钱包金额最多显示两位小数;渠道表操作列固定宽度。

## [0.4.1] - 2026-08-05

### 修复
- schema 版本闸门兼容 v0.1/v0.2 遗留的 `PRAGMA user_version` 标记(此前老库升级即拒绝启动);
  `SCHEMA_VERSION` 提到 3。

## [0.4.0] - 2026-08-05

### 新增
- 通用端点字节级透传:`POST /v1/{*path}`(embeddings / moderations / images / rerank 白名单),
  按 model 选路、模型映射、换渠道凭证,复用余额 / 限额 / 故障转移 / 计费口径。
- 兑换码:管理端批量生成(只存 sha256 + 掩码),用户钱包页兑换(失败限流)。
- API Key 并发在途上限(`max_concurrent`);渠道按 key 批量添加。
- 渠道 key 哈希查重(重复 key 返回 409);schema 版本降级闸门。
- 性能基准脚本 `scripts/bench.js`。

### 变更
- **计费整数微元化**:金额列改为整数微元(1 元 = 1e6),启动时幂等迁移,API 边界仍为元。
- 余额 ≤ 0 时代理入口直接 402 `insufficient_quota`。

[Unreleased]: https://github.com/zumpyx/Aikun/compare/v0.4.4...refactor/modular-gateway
[0.4.4]: https://github.com/zumpyx/Aikun/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/zumpyx/Aikun/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/zumpyx/Aikun/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/zumpyx/Aikun/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/zumpyx/Aikun/compare/v0.3.4...v0.4.0
