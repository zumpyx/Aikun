# Aikun 0.5 目标项目结构(定稿)

> 状态:**已定稿**。本文取代 `roadmap-v0.5.md` 中"目标架构"一节的模块布局,并取代 `refactor-plan.md` 的"明确不做"清单中已被后续决策推翻的条目(多 crate 化、管理面 API spec-first)。
> 阶段纪律不变:每步 e2e 全绿、禁止推倒重写、行为变化单列 CHANGELOG。
> 与既有文档的优先级:`roadmap-v0.5.md`(阶段与纪律)> **本文(目标结构)** > `refactor-plan.md`(落地进度)。

## 一、决策记录(为什么是这个结构)

| # | 决策 | 理由 |
|---|---|---|
| 1 | **多 crate workspace**(取代 roadmap 原定的"单 crate 内重组") | ① protocol 层已验证零耦合(仅依赖 serde_json),crate 边界把"纪律"升级为编译器强制;② 管理面 API 决定 spec-first 重建,生成物需要独立 crate;③ 渠道会持续增多(含 WorkBuddy/Qoder 类反向代理),适配器需要纯净的家 |
| 2 | **管理面 API spec-first**(范围 = 公开 login + 用户自助 + 管理员三层):手写 OpenAPI 3.2.1 spec → openapi-to-rust 生成;`/v1/*` 进 spec **仅作文档**(loose schema、不镜像厂商 spec、不入生成白名单),`/api/health` 不进 | Aikun 是 AGPL 开源自托管项目,管理面 API 按演化规律会长出第三方消费者(脚本/面板),spec 是对外契约的正确形态;重构窗口内无兼容包袱,按新设计重建优于逐端点迁移 |
| 3 | **生成器选 openapi-to-rust(v0.24.0 钉版),五支柱纪律不变** | 2026-11 spike 实测:官方 openapi-generator 拒绝 3.2.1(8 个校验错误)且无 SSE 服务端生成(issue #21967);[openapi-to-rust](https://github.com/gpu-cli/openapi-to-rust)(Rust 原生、无 Java)接受 3.2.1、SSE 服务端一等生成(`OkStream` 包 axum 原生 Sse)、axum 0.8 与现有栈同大版本、RFC 9457 错误、jsonschema 运行时校验、`[server].operations` 按操作白名单。代价:spec 3.2.1 与该生成器绑定(官方生成器不可用),以钉版本缓解。生成到专用 crate(`crates/aikun-openapi/`)且零手写;配置(TOML)而非编辑输出;CI 重生成 + `git diff --exit-code`,全程无 Java |
| 4 | **`/v1/*` 永远手写、不进 spec** | 代理端点的契约属于 OpenAI/Anthropic 官方规范;流式 SSE 不适合生成器 |
| 5 | **channels 独立 crate,编译期静态注册** | 适配器只描述差异(URL/鉴权/归一化钩子),不做 I/O;不是动态插件系统 |
| 6 | **pipeline 留在二进制 crate** | 只被二进制消费且阶段 2 大改中,过早 crate 化是纯摩擦 |
| 7 | **前端保持 vanilla** | 路径随 spec 演化;TS 类型消费(vite + spec 生成)推迟到管理端 UI 大扩张时再评估 |
| 8 | 仍然不做:Redis / PostgreSQL / 动态插件加载 / 每渠道一个 crate / 前端框架化 | 与 roadmap 一致,未推翻 |

## 二、最终项目结构

```
aikun/                                # 仓库根
├── Cargo.toml                        # [workspace] members: aikun、crates/*
├── Cargo.lock
├── openapi.yaml                      # ★ 手写 spec(仓库根,与 Cargo.toml 平级的单一真相):
│                                     #   系统 API 全景图(OpenAPI 3.2.1,RESTful 资源化);
│                                     #   管理面三层(Auth/User/Admin tag)→ 生成;/v1/* 进 spec 仅作
│                                     #   文档(loose schema:路径/鉴权/媒体类型,不镜像厂商 schema,
│                                     #   不入生成白名单);/api/health 不进;统一错误 envelope;
│                                     #   code review 的对象
├── scripts/
│   ├── gen-api.sh              # 生成 wrapper:cargo install openapi-to-rust --version 0.24.0(支柱 1,无 Java)
│   └── bench.js                      # 性能基准
│
├── crates/
│   ├── aikun-openapi/                # ★ openapi-to-rust 生成物(从 openapi.yaml,仅管理面
│   │                                 #   ops):workspace 成员,零手写、可整体删除重建;
│   │                                 #   生成物提交进仓库(构建无需任何外部生成器)
│   ├── aikun-protocol/               # /v1 协议转换:enum Protocol、Value 树枢纽、流式状态机、
│   │   └── src/                      #   sse/usage/stream/error + {openai,anthropic,responses}/
│   │                                 #   deps: serde_json(仅此)
│   │                                 #   tests/contract.rs + tests/fixtures/:黄金样本契约测试
│   │                                 #   (含 OpenAI 官方 spec 快照的静态形状参照;不另立测试 crate)
│   ├── aikun-channels/               # ChannelAdapter trait + ChannelRegistry + 每渠道一个文件
│   │   └── src/adapters/             #   generic / workbuddy / qoder / azure / …
│   │                                 #   纯描述层(无 I/O);deps: aikun-protocol, serde_json
│   │                                 #   预留扩展点:流解码钩子(NDJSON→SSE)、有状态鉴权
│   ├── aikun-admin/                  # 管理面 API 业务实现:impl PricesApi for PriceService…
│   │                                 #   (用户自助与管理员两类 impl 都在这里);90% 代码的家
│   │                                 #   deps: aikun-openapi, aikun-store, aikun-channels
│   └── aikun-store/                  # 仓储 trait + SQLite impl(rusqlite、迁移)
│                                     #   阶段 3 形态,可后建;建之前 db/ 模块临时承担
│
├── aikun/                            # 二进制 crate:装配与运行时编排
│   ├── src/
│   │   ├── main.rs                   # 装配:AppState、路由挂载(aikun-openapi 路由外面包中间件)、
│   │   │                             #   后台任务监督、优雅关停
│   │   ├── config.rs                 # AIKUN_* 配置
│   │   ├── pipeline/                 # /v1 请求管线:admission / dispatch / stream
│   │   │                             #   (+阶段 2 的 context.rs、error.rs)
│   │   ├── channel/                  # 运行时编排(编排 I/O 与 DB,不进 channels crate):
│   │   │                             #   transport(HTTP client)/ selector / health / model_test
│   │   │                             #   / registry 快照缓存(阶段 3)
│   │   ├── billing/                  # 价格、微元折算、ledger(日志+扣费同事务)
│   │   ├── cache/                    # 响应缓存(进程内,默认关闭)
│   │   ├── auth/  crypto.rs          # JWT/API Key、AES-GCM
│   │   ├── api/                      # 薄 HTTP 层:/v1/* 手写端点(永久)+ 迁移期存量管理面端点
│   │   ├── db/  models/              # 迁移、连接池、行模型(阶段 3 起逐步被 store 吸收)
│   │   └── frontend/                 # 内嵌管理前端(vanilla JS,路径随 spec 演化)
│   └── tests/                        # e2e(真实二进制 + mock 上游)+ common 基建
│
├── docs/                             # roadmap-v0.5 / architecture-v0.5(本文)/ refactor-plan / adr
├── .github/workflows/                # ci.yml(lint: fmt+clippy / test / regenerate 守卫)+ release.yml
│                                     #   regenerate 守卫已上:重生成 + git diff --exit-code,漂移即失败
├── Dockerfile  docker-compose.yml  env.example  CHANGELOG.md
```

## 三、依赖方向(单向、无环)

```
aikun-protocol (serde_json)
   ↑                ↑
aikun-channels   aikun-openapi (axum/serde,不依赖任何兄弟 crate)
   ↑    ↑              ↑
   │    └── aikun-admin ──→ aikun-store (rusqlite)
   │            ↑              ↑
   └──────── aikun(二进制) ────┘
```

规则:

1. **纯 crate 不沾 I/O**:`aikun-protocol`、`aikun-channels`、`aikun-openapi` 的 Cargo.toml 里不得出现 reqwest / rusqlite / tokio——依赖纯净度由 crate 边界编译期强制,这是多 crate 化的核心收益;
2. **`aikun-openapi` 零手写**:业务逻辑、中间件、状态接线一律在 `aikun-admin` 与二进制装配层;对生成行为的调整只通过生成器配置(`--additional-properties` / `--template-dir`);
3. **扩展只落一处**:新增渠道只动 `aikun-channels`(一个文件 + 一行注册);管理面 API(公开/用户自助/管理员三层)演化只动 `openapi.yaml`;`/v1` 端点与管线永远在二进制 crate。spec 的 tag 按鉴权层切,二进制装配时 Auth 裸挂、User 包 `auth_middleware`、Admin 包 `auth_middleware + require_admin`;`/v1/*` 与 `/api/health` 不进 spec。
4. **流式的边界(2026-11 按选型更新)**:openapi-to-rust 可生成管理面 SSE 端点**骨架**(响应枚举带 `OkStream` 变体,直接包 axum 原生 `Sse`),未来管理面流式端点(日志尾随、测试 playground)可进生成白名单;但 SSE 事件**内容**永远手写——事件流元素是通用 `axum::response::sse::Event`,逐事件负载不做类型化。`/v1/*` 的事件转换状态机在 `aikun-protocol`,**绝不生成**:① handler 是进管线的代理壳,trait 无东西可装;② 生成物自带 jsonschema 运行时请求校验,对代理端点启用会误拒第三方扩展字段(`reasoning_content` 等),直接打坏透传;③ 镜像厂商 schema 进自己 spec = 第二份真相,必漂移——故 `/v1` 在 spec 里只做 loose 文档(路径/鉴权/媒体类型 + `text/event-stream` 标注)。

## 四、现存代码 → 目标位置的迁移映射

| 现位置(refactor/modular-gateway 单 crate) | 目标位置 | 性质 |
|---|---|---|
| `src/protocol/`(16 文件 4.1k 行) | `crates/aikun-protocol/src/` | 纯机械,一次提交 |
| `src/channel/adapter/` | `crates/aikun-channels/src/adapters/` | 纯机械,一次提交 |
| `src/api/admin/**` + 用户自助端点(`src/api/auth.rs`、`api/logs.rs`、`api/redemption.rs` 的非 /v1 部分) | `openapi.yaml`(重新设计,三层 tag)+ `crates/aikun-admin/` | **重设计**,按 tag 分批迁移,存量端点迁移完即删 |
| `src/db/` + `src/models/` | `crates/aikun-store/` | 阶段 3 逐步吸收,先建模块后建 crate |
| `src/pipeline/` `src/channel/{transport,selector,health,model_test}` `src/billing/` `src/cache/` `src/api/proxy/` `src/auth/` `src/db/` `src/frontend/` | 留在 `aikun/src/` 原位 | 不动(仅随阶段 2/3 演进) |
| `tests/`(根目录) | `aikun/tests/` | workspace 化时随二进制 crate 移动 |

## 五、与阶段计划的整合

结构迁移本身按 roadmap 阶段纪律编排,每步独立提交、e2e 全绿。

**进度(2026-11-16 首批落地)**:本次落地是**根包 + workspace 变体**——workspace 已在根 `Cargo.toml` 声明,但二进制仍在根包、**尚未移入 `aikun/`**,完整迁移移到后续步骤;生成器为 openapi-to-rust(非最初设想的 Java 系官方生成器,见决策 3)。后续完整清单与逐步验收标准见 `docs/plan-next-steps.md`。

**进度更新(2026-11-17 回写,plan 步骤 2–8 全部落地)**:workspace 已完整化(根
`Cargo.toml` 为虚拟清单,`members = ["aikun", "crates/*"]`,`default-members = ["aikun"]`,
二进制移入 `aikun/`);protocol / channels / admin / store 四个 crate 就位;管理面
API 全部 tag 迁移完成——`grep -rn "api/admin" aikun/src/` 仅余注释、前端
`grep -rn "/api/admin" aikun/src/frontend/` 归零,旧 handler 删除;对外行为变化已
单列 CHANGELOG(路径 / 201/204 / PATCH 回资源 / problem+json / batch 200 /
last_admin 409 / 429 码与 Retry-After / 5xx→4xx 过渡映射),二进制版本升 **0.5.0**
(版本门禁可用,api-mapping 备注 11)。e2e 拆分为 `e2e_{auth,user,admin_users,
admin_providers,admin_billing,store}.rs` + `e2e.rs`(旧管理面用例随旧 handler
删除,按 plan 步骤 9 政策),全绿以 CI 门禁为准。

| # | 步骤 | 状态 | 说明 |
|---|---|---|---|
| 1 | workspace 化 | ✅ 已完整化(2026-11-16 首批根包变体 → 同日步骤 3 移入 `aikun/`) | 虚拟清单 `members = ["aikun", "crates/*"]` + `default-members = ["aikun"]`,根目录裸 cargo 行为不变 |
| 2 | protocol crate 拆出(机械) | ✅ 已落地(plan 步骤 4,含 9.1 契约测试雏形:非流式双向黄金样本 + 官方 spec 快照) | |
| 3 | channels crate 拆出(机械) | ✅ 已落地(plan 步骤 5,apply_auth 改 auth_headers 纯数据签名) | |
| 4 | spec 设计 → 生成 crate → 生成管线 → CI 守卫 | ✅ 全部落地 | `openapi.yaml` + `docs/api-mapping.md` 对照;`crates/aikun-openapi/`(openapi-to-rust 0.24.0 钉版,43 op 白名单,零手写零测试);`scripts/gen-api.sh`;**CI regenerate 守卫 job 已上**(`ci.yml`) |
| 5 | 逐 tag 迁移:providers → prices → users → logs → redemption…,每 tag 一批,存量 handler 删一批;`aikun-admin` 依赖 `aikun-store`(或过渡期 db 模块) | ✅ 全批完成(2026-11-16/17) | Auth 试点 → User / Providers / Billing / Stats 各批;旧 `/api/admin/**` 路由与旧用户自助 handler 清零,前端切换完成;行为变化单列 CHANGELOG;版本门禁 0.5.0 满足 |
| 6 | store crate 落位(阶段 3,与缓存/计数器同阶段) | ✅ 已落地(plan 步骤 8 / roadmap 阶段 3) | `crates/aikun-store`(pool / schema / 行映射 / 静态加密 / 口令哈希 / 默认价格播种 / `api_key_usage_daily` 日用量计数器);`grep conn.lock() aikun/src/api/` 归零;渠道 registry 快照、价格表缓存世代失效同批落位。`scripts/bench.js` 对比以 CI / 人工复跑为准 |
| 7 | 阶段 2(pipeline 显式化)与上述 1–3 无冲突,可并行推进 | — | |

本次落地验证(2026-11-16 实跑):`cargo check -p aikun-openapi --locked` 通过;`cargo fmt --all --check` 通过;`crates/aikun-openapi/Cargo.toml` 直接依赖无 reqwest / rusqlite / tokio(axum 的 `tokio` feature 系服务端脚手所需,经由 axum 引入,非直接依赖)。

CI 变化(已全部落地):lint job 升级为全 workspace——`cargo fmt --all --check`、`cargo clippy --workspace --all-targets --locked`;test job 改 `cargo test --locked --workspace`;regenerate 守卫 job 已上(`ci.yml` 的 `regenerate` job,plan 步骤 2)——生成器是 openapi-to-rust(Rust 原生,**无需 Java 运行时**),跑 `bash scripts/gen-api.sh` 后 `git diff --exit-code`,任何差异即失败。

## 六、检验标准(结构是否"还对"的判据)

- 任取两个 crate,依赖方向都能用一句话解释,无环;
- `cargo tree -p aikun-channels` 与 `-p aikun-protocol` 输出里没有 reqwest/rusqlite/tokio;
- 删掉 `crates/aikun-openapi/` 重跑 `scripts/gen-api.sh`,`git status` 干净;
- 新增一个渠道适配器,git diff 只落在 `aikun-channels` 一个 crate;
- 管理面 API 加一个字段,只改 `openapi.yaml` + `aikun-admin` 的 impl(前端路径随之更新)。
