# Changelog

本文件记录 Aikun 各版本的对外可见变化。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/),
版本号遵循 [SemVer](https://semver.org/lang/zh-CN/)。

约定(见 `docs/roadmap-v0.5.md`「纪律」):纯重构不改行为;任何行为变化必须单列条目。

## [Unreleased]

`refactor/modular-gateway` 分支,对应 roadmap-v0.5 阶段 0(可作为 0.4.5 发布)与阶段 1。

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

### 测试 / 工程

- e2e:登录链路(`/api/login` → `/api/me`、错误密码、改密踢下线、禁用账户)。
- 回归测试:上述 ①②③ 各一。
- 依赖钉版以支持 rustc 1.85(`idna_adapter 1.2.0`、`time 0.3.41`、`simple_asn1 0.6.3`)。

### 文档

- `docs/refactor-plan.md` 补充最佳实践(协议契约测试、流式取消与超时、计费幂等与对账、故障转移可重试边界、渠道熔断、可观测性前置、响应缓存约束),并写入阶段 1–4 任务。

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
