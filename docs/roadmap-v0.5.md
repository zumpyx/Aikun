# Aikun 0.5 重构路线图

> 状态:已定稿;阶段与纪律仍以此为准,**"目标架构"一节的模块布局已由 [`architecture-v0.5.md`](architecture-v0.5.md) 取代**(多 crate workspace + admin API spec-first,见该文档的决策记录)。
> 决策:0.5 为**重构版(还债版)**——不新增大的对外能力,以架构还债为主线,顺手用新抽象验证可扩展性。
> 基线:本文写作时为 v0.4.4(约 2 万行:Rust 后端 ~15k、前端 JS ~3.4k、测试 ~2.6k)。
> 原则:**每阶段结束 e2e 必须全绿,禁止推倒重写。** 38 个 e2e 用例(tests/e2e.rs)是整个重构的安全网,动任何模块前先确认它们可跑。

---

## 一、现状诊断(0.4)

项目底子不差,以下三项设计是资产,重构中必须保留:

1. **三端点单管线**:`/v1/chat/completions`、`/v1/messages`、`/v1/responses` 都是 7 行薄壳,共用 `proxy_completion` 一条链路;
2. **协议转换星型枢纽**:以 OpenAI chat 形状为中转,跨协议靠组合(如 `anthropic→responses = OarStreamState ∘ AnStreamState`),新增协议是 O(N) 而非 N²;
3. **微元整数计费 + 单事务落库**:查价/插日志/扣余额同一显式事务,无浮点漂移、不会丢账。

病根是**分层从未建立**:HTTP handler 里写着 SQL、业务规则和协议形状知识;"管线"隐式活在两个 300+ 行函数体内;状态散落在 DB、AppState 的 5 个 HashMap、以及靠 14 个参数手工传递的函数局部变量三处。

### P0 — 结构性债务(0.5 主体)

| # | 问题 | 证据 | 危害 |
|---|---|---|---|
| 1 | **passthrough 另起炉灶**,复刻整条管线:白名单/余额预检/限额/重试循环/失败记账各写一遍 | `src/api/proxy/passthrough.rs` ~320 行,8 处注释"与 chat 同口径";已漂移(缺整体超时补记、日志 `request_type` 语义不一致) | 口径靠人肉同步,计费/重试规则改动要改两处,漏一处即分叉 |
| 2 | **God function 与参数爆炸** | `src/api/proxy/chat.rs` 5 处 `#[allow(clippy::too_many_arguments)]`(L373/771/1125/1155/1188);`attempt_loop` ~300 行 14 参;`stream_response` ~230 行 11 参,内联 async_stream 块 ~190 行混 4 件事 | 无法单测、并发 guard 靠手工 `.take()` move、重试顺序靠 `Arc<Mutex<Vec>>` 跨越超时边界,易错 |
| 3 | **convert.rs 巨石 + 弱类型** | 3478 行单文件(主体 2265 + 测试 1213);同一语义映射(reasoning↔thinking、tool_calls↔tool_use)在 请求/响应/流式 × 双向 共 **6 份**;协议是 `&'static str` 非 enum,`convert_request`/`StreamConverter::new` 有静默兜底(`_ => body.clone()` / `_ => PassOpenAi`) | 路由逻辑变化时错误形状无声透传;改口径容易漏一处;单文件不可维护 |
| 4 | **SQL 直接写在 handler,无仓储层** | `src/api/**` 21 处直接 `conn.lock()`,管理面 ~44 处在 async executor 上同步持锁执行 SQL(热路径有 spawn_blocking 纪律但管理面没执行);行映射靠 SELECT 列序 + 手写 `row.get(i)`(provider 26 列);db 层启动时反向调用 billing 播种价格(层次倒挂) | 写锁被长事务占住时 worker 线程最长阻塞 5s;加列漏同步任意一处即静默错位;handler 无法脱离真库单测 |

### P1 — 性能与正确性隐患

| # | 问题 | 证据 |
|---|---|---|
| 5 | 热路径每请求全表扫描:选路每次(含每次重试)`SELECT ... WHERE is_active=1` 全量 + 逐行 AES 解密 + 两次 JSON parse(`src/router/selector.rs:199-227`);计费每次全表扫价格(`src/billing.rs:27-88`);日额度每次对 request_logs 做 SUM 子查询(`src/api/proxy/chat.rs:264-268`)。均无进程内缓存 | 渠道上百家后是明确吞吐拐点 |
| 6 | 潜伏语义 bug 3 个(见第三节,应在 0.4.5 先修) | — |

### P2 — 工程化缺口

| # | 问题 | 证据 |
|---|---|---|
| 7 | 测试哑铃形:130 个单测集中在核心纯函数 + 38 个进程级 e2e,**约 3900 行 handler 代码零单测**;e2e 缺登录链路(所有用例 `sign_jwt()` 伪造 token)、用户 CRUD、日志 API | tests/e2e.rs、tests/common/mod.rs:371-399 |
| 8 | CI 只有 `cargo test --locked`:无 fmt/clippy/audit 门禁;无 CHANGELOG;前端 3400 行 JS 无 lint/测试,providers.js 单建/批量弹窗近乎逐字复制两份 | .github/workflows/ci.yml |
| 9 | 可观测性只有日志:无 request_id/trace_id 贯穿(一次重试链多 attempt 靠人肉拼日志),无 metrics,错误处理靠 Option/元组/字符串 | — |

---

## 二、目标架构(0.5)

单 crate 内重组(2 万行规模拆 workspace 是过度设计),但模块边界要立起来:

```
src/
├── api/                # 薄 HTTP 层:路由、参数提取、响应序列化。无 SQL、无业务规则、无协议形状知识
│   ├── proxy/          #   /v1/* 端点(全部是薄壳,委托 pipeline)
│   ├── admin/          #   管理端(委托 store 仓储,不再手写 SQL)
│   └── ...
├── pipeline/           # 显式请求管线(从 chat.rs 提取)
│   ├── mod.rs          #   管线编排:authorize → quota → select → transform → dispatch → observe → bill
│   ├── context.rs      #   RequestCtx 结构体收敛原 14 参/11 参(含 ConcurrencyGuard、tried 顺序等生命周期状态)
│   ├── dispatch.rs     #   重试/failover/流式移交(原 attempt_loop 拆小)
│   ├── stream.rs       #   流式转发(原 stream_response 拆小)
│   └── error.rs        #   thiserror 集中错误枚举 + 各协议错误形状渲染(从 chat.rs 下沉)
├── protocol/           # 原 convert.rs 拆分,每协议一目录
│   ├── mod.rs          #   enum Protocol { OpenAi, Anthropic, Responses } + 穷尽 match,消灭静默兜底
│   ├── sse.rs          #   SSE 字节解析(原 convert.rs:1105-1180)
│   ├── usage.rs        #   usage 提取与缓存拆分口径(原 1044-1103,统一三处手写内联)
│   ├── openai/         #   request.rs / response.rs / stream.rs(含 OarStreamState)
│   ├── anthropic/      #   同上(含 AnStreamState)
│   └── responses/      #   同上
├── channel/
│   ├── registry.rs     #   进程内渠道快照缓存(Arc<RwLock<Registry>>),管理端变更/自动禁用时失效
│   └── selector.rs     #   纯函数化选路(吃 registry 快照,不再每请求碰 DB)
├── billing/            #   原 billing.rs + 价格缓存;日额度改专用计数器表(与记账同事务更新)
├── store/              #   仓储 trait + SQLite 实现,handler 不再直接写 SQL;行映射集中一处
└── state.rs            #   合并 AppState 的 5 个 HashMap 为统一 Limiter 组件(分片锁)
```

### 四条关键决策

1. **passthrough 变成管线的一种模式**(`NoConversion + OpenAiOnly + NoStream`),不再是第四条管线。消灭最大债务源。embeddings/moderations/images/rerank 的字节级透传语义不变,只是实现归一。
2. **`enum Protocol` + 穷尽 match**:杠杆最高的小改动。新增协议时编译器逼你补齐所有分支,替代现在的静默兜底。`convert_request`/`convert_response`/`StreamConverter::new`/`protocol_error` 等全部吃 enum。
3. **转换边界类型化,枢纽维持 Value 树**:入口 parse 成类型化请求(校验一次、字段拼写错误编译期可见),枢纽中转(OpenAI chat 形态)与流式状态机保持现有 Value + 组合管道设计——已验证可行,全面 IR 重写投入产出不成比例。
4. **拓扑走缓存、事实走 DB**:registry 缓存的是渠道拓扑(变更低频,管理端写路径天然可作失效点);落库的仍是事实(计费、EMA 指标、亲和)。价格表同理缓存。日额度从"SUM request_logs"改为专用计数器表,与记账同事务更新,消除每请求扫描。

### 明确不做(0.5 范围外)

- 多实例部署 / 状态外置 Redis / PostgreSQL 支持(仓储 trait 留好缝,不做第二实现)
- 插件系统
- 前端框架化(保持 vanilla,只做模块化治理:ESM 化、hash 路由、providers.js 弹窗去重)
- 新管理端大功能

---

## 三、阶段计划

每阶段独立可合入、可发版;顺序执行;任何阶段不得带着红的 e2e 进入下一阶段。

### 阶段 0:准备(可先发 0.4.5)

- [ ] CI 加 `cargo fmt --check` + `cargo clippy --workspace -- -D warnings`(存量 warning 先清零)
- [ ] 修潜伏 bug ①:O→A 响应 usage 口径——`openai_resp_to_anthropic`(convert.rs ~L964-978)把含缓存的 `prompt_tokens` 直接当 `input_tokens` 回填且丢缓存维度,应按 `prompt_tokens_details.cached_tokens` 拆分(参照反方向 `anthropic_resp_to_openai` L876-884 的正确口径);流式 O→A finalize(~L1695-1698)同步修
- [ ] 修潜伏 bug ②:`wallet_stats` 改读 `usage_daily`(不足 30 天部分回退 request_logs),保留期 < 30 天时月度统计不再静默缩水(api/logs.rs:444-500)
- [ ] 修潜伏 bug ③:日志清理改分批 DELETE(如每批 5000 行、循环至清空,批间让出写锁),替换 health.rs 单事务全量删
- [ ] 建 `CHANGELOG.md`(回填 0.4.x 要点,之后每阶段追加)
- [ ] e2e 补登录链路用例(/api/login、/api/me、改密踢会话 token_version)——重构动 auth 前必须有覆盖

**验收**:clippy/fmt 门禁上线且为零告警;三个 bug 各有一个回归测试;`cargo test` 全绿。

### 阶段 1:结构重组(纯移动,不改行为)

- [ ] 拆 `convert.rs` → `protocol/` 目录树(函数原样搬移,路径变化除外);1213 行测试随各自实现文件走
- [ ] 引入 `enum Protocol`,常量与字符串匹配全部替换;`_ =>` 兜底分支改为穷尽 match(或显式 `unreachable!` + 日志)
- [ ] `chat.rs` 的 `protocol_error`/`convert_error`/`stream_error_event`/`stream_terminal_event` 下沉到 `protocol/`(协议形状知识归位)
- [ ] `read_json_limited` 两份合一(model_test.rs 版为参数化实现);health.rs 的鉴权头复制改用 client.rs 的 `apply_auth`
- [ ] `RequestLog.cost: f64` 改 `i64`(微元),消灭模型类型与存储口径的错位

**验收**:e2e 全绿;`grep -rn "&'static str" src/protocol/` 无协议标识残留;新增一个假协议枚举值时编译器能在所有 match 处报错(人工验证一次)。

### 阶段 2:管线显式化(0.5 主体)

- [ ] 定义 `RequestCtx`(或 `PipelineCtx`)结构体:auth 上下文、key 限额状态、tried 顺序、并发 guard、计费句柄等,收敛 14 参/11 参
- [ ] `attempt_loop` 拆分为:候选迭代(选路调用方)、单次 attempt(转换+发送+形状校验)、失败分类(可重试/不可重试/凭证死亡)三个单元;每单元可独立单测
- [ ] `stream_response` 拆分:流转发循环、记账收尾、错误帧处理三个单元;`StreamAbortGuard` 生命周期挂到 ctx 上,消灭手工 `.take()`
- [ ] **passthrough 并入管线**:作为 `NoConversion` 模式实现,白名单/余额/限额/重试/记账全部复用管线;`/v1/{*path}` 路由handler 变薄壳;删除 passthrough.rs 的重复实现
- [ ] `pipeline/error.rs`:thiserror 错误枚举(`GatewayError`),统一"哪些状态码可重试、哪些响应给客户端什么形状"的口径(原 chat.rs:626 与 passthrough.rs:320 两份合一)
- [ ] request_id(UUID)贯穿:入口生成 → tracing span → request_logs 落库 → 透传上游 header;响应头回带(`X-Request-Id`)
- [ ] 管线各单元补单测(此阶段结束时 handler 层零单测的债务主要在此偿还)

**验收**:e2e 全绿 **且** passthrough 相关用例(e2e.rs:1853、:1943)不改断言通过;`grep -c "与 chat 同口径" src/` 为 0;attempt/stream 各单元有单测;一次多渠道重试请求可在日志中按 request_id 拼出完整链。

### 阶段 3:性能与状态归位

- [ ] `channel/registry.rs`:进程内渠道快照(Arc<RwLock>),管理端 CRUD/批量/副本/自动禁用/健康状态变更时失效重建;`selector` 改为吃快照的纯函数——选路路径零 SQL、零重复解密(密钥保留密文,选中后解一次)
- [ ] 价格表缓存(billing 内 Arc<RwLock<Vec<PriceRule>>>),价格 CRUD 失效;`find_price` 不再每请求扫表
- [ ] 日额度计数器表 `(api_key_id, date, tokens_spent)`,与 `insert_request_log` 同事务 UPSERT;`enforce_key_limits` 的 SUM 子查询删除
- [ ] `state.rs`:login_attempts/api_key_rate/redeem_attempts/api_key_inflight 合并为 Limiter 组件(分片 HashMap 或 dashmap),统一"访问时清理 + 删除时清理"策略与测试
- [ ] 管理面全部 DB 访问走 store 仓储层(内部统一 spawn_blocking),api/ 下 `conn.lock()` 清零
- [ ] db 层不再调用 billing 播种价格(移到启动编排层),消除层次倒挂

**验收**:e2e 全绿;bench(scripts/bench.js,50 并发档)对比重构前无回归(目标:不劣化,选路缓存生效后应有小幅提升);`grep -rn "conn.lock()" src/api/` 为 0。

### 阶段 4:收尾与验证(按需)

- [x] 前端治理 ✅(2026-11-18 React 重写一并完成:ESM 天然达成、HashRouter 深链/后退可用、providers 单建/批量表单合并为一个组件;见 architecture 决策 7)
- [x] ~~(待定)用新 protocol 抽象接入一个新协议(如 Gemini)作为 0.5 架构的验收测试~~ → **2026-11-18 拍板:不做**。协议面维持 OpenAI 系 + Anthropic;/v1 八端点已纳入 openapi-to-rust 生成(见 architecture-v0.5 决策 4),扩展以适配器(`aikun-channels`)而非新协议方向演进
- [x] ~~(待定)/metrics 端点~~ → **2026-11-18 拍板:不做**(与 Gemini 同批关闭;可观测性由阶段 2 的 request_id/span 日志链承担,需要时再议)
- [x] CHANGELOG 整理 ✅(0.5.0/0.6.0 已记;0.7.0 = 阶段 2 + /v1 生成化 + 阶段 3 尾巴 + React 重写,2026-11-18)

**验收**:全部 e2e 绿;若做了新协议,新协议有与现有协议同等密度的单测 + 至少 2 个 e2e(透传、跨协议转换)。

---

## 四、风险与纪律

1. **e2e 是唯一安全网**:动模块前先本地全量跑一遍确认绿;每阶段合入前再跑。发现 e2e 本身有 bug 先修 e2e。
2. **禁止顺手改行为**:阶段 1 严格纯移动;行为变更(哪怕看起来是改进)单独开分支、单独测试、单独条目记 CHANGELOG。
3. **passthrough 并入是最高风险步骤**(阶段 2):先补齐它当前行为的 e2e 快照(超时语义、日志字段差异要么保留要么显式变更记录),再动实现。
4. **每个阶段一个 PR/一串小 commit**,保证可回滚、可 review;不搞长命分支。
5. 渠道 registry 缓存(阶段 3)要写一个"缓存与 DB 不一致"的防御测试:管理端改渠道后,下一请求必须立即看到新配置。

---

## 附:文档目录约定

`docs/` 存放设计与决策文档:

| 文件 | 内容 |
|---|---|
| `roadmap-v0.5.md` | 本文,0.5 重构路线图与决策记录 |
| (后续)`adr/*.md` | 架构决策记录,一决策一文件(如"为什么单 crate 不拆 workspace"、"为什么枢纽维持 Value 树") |
