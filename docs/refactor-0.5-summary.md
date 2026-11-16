# 0.5 重构收尾总结(refactor/modular-gateway)

> 2026-11-17 收尾快照:结构前后对照、crate 清单、遗留项。
> 对应发布段 `CHANGELOG.md` **[0.5.0]**;施工清单与逐步验收标准见
> `docs/plan-next-steps.md`(步骤 1–8 全部 ✅);目标结构定义见
> `docs/architecture-v0.5.md`(§五进度已同步);阶段与纪律见
> `docs/roadmap-v0.5.md`。
>
> 本文数字除注明外均为 2026-11-17 在当前 HEAD(2260139)实测。

## 一、结构前后对照(0.4.4 tag → 0.5.0)

0.4.4(git tag `v0.4.4` 实查):单 crate,根 `Cargo.toml` 即二进制包,
`src/{api,auth,billing.rs,config.rs,crypto.rs,db,frontend,main.rs,models,proxy,router}`,
协议转换是 `src/proxy/convert.rs` 巨石,`tests/` 在仓库根。

| 维度 | 0.4.4 | 0.5.0 |
|---|---|---|
| 清单形态 | 根包单 crate | 虚拟 manifest:`members = ["aikun", "crates/*"]`,`default-members = ["aikun"]`(根目录裸 cargo 行为不变) |
| 二进制 | 根 `src/` + 根 `tests/` | `aikun/src/` + `aikun/tests/`(纯移动,产物名/路径不变) |
| 协议转换 | `src/proxy/convert.rs`(3.4k 行,`&'static str` 协议 + 静默兜底) | `crates/aikun-protocol`(`enum Protocol` 穷举 match;deps 仅 `serde_json`;契约测试 fixtures 就位) |
| 渠道差异 | URL/鉴权散在 proxy client / health / fetch-models | `crates/aikun-channels` 适配器(generic / workbuddy / qoder;纯描述、无 I/O,`auth_headers` 纯数据签名) |
| 管理面 API | 手写 `/api/admin/**` + 用户自助 handler,`{error,message}` 错误体、裸数组列表 | `openapi.yaml`(单一真相)→ `crates/aikun-openapi` 生成(43 op 白名单)+ `crates/aikun-admin` 实现;problem+json / `{items,total}` / 201 / 204 / PATCH 回资源(行为变化单列 CHANGELOG) |
| 存储 | handler 内 SQL;db 层反向调用 billing 播种 | `crates/aikun-store` 能力 trait(零 SQL 方言)+ SQLite impl;播种移出 db 层;管理面 DB 访问收口 `spawn_blocking`,`aikun/src/api/` 下 `conn.lock()` 清零 |
| 热路径(阶段 3) | 选路全表扫描、价格两次通配 SQL、日额度 `SUM(request_logs)` | 渠道拓扑快照(表令牌 + 世代失效)、价格缓存世代失效、`api_key_usage_daily` 计数器点查(与记账同事务维护) |
| CI | 无 fmt / clippy 门禁 | `cargo fmt --all --check` + `cargo clippy --workspace --all-targets --locked -D warnings` + `cargo test --locked --workspace` + regenerate 守卫(钉版生成器重生成 + `git diff --exit-code`) |
| 版本 | 0.4.4 | **0.5.0**(版本门禁:`GET /api/system/version` 主次版本 ≥ 0.5,api-mapping 备注 11) |

代码量(实测,`find … -name "*.rs" | xargs wc -l`):二进制 `aikun/src/` 9296 行;
五个 crate 合计约 2.07 万行,其中 `aikun-openapi` 8043 行为生成物。

## 二、crate 清单(2026-11-17 实测)

| crate | 职责 | 关键依赖 | LOC | 测试(`#[test]`/`#[tokio::test]`) |
|---|---|---|---|---|
| `aikun-openapi` | openapi-to-rust(钉版 0.24.0)从 `openapi.yaml` 生成的服务端脚手架(trait + 路由工厂 + 类型);零手写,`/v1` 绝不入白名单 | axum / jsonschema / serde 等,无 reqwest / rusqlite(axum 的 tokio feature 系脚手架所需) | 8043 | **0**(生成代码零测试,plan 步骤 9 政策;质量由 regenerate 守卫保证) |
| `aikun-protocol` | `/v1` 协议转换:`Protocol` 枚举、Value 树枢纽、sse / usage / stream / error、`{openai,anthropic,responses}/` | `serde_json`(仅此;`cargo tree` 实测无 reqwest / rusqlite / tokio) | 4139 | 72(含 `tests/contract.rs` 双向黄金样本 + OpenAI 官方 spec 快照参照) |
| `aikun-channels` | `ChannelAdapter` + 注册表 + 每渠道一个文件(generic / workbuddy / qoder);纯描述层 | `aikun-protocol` + `serde_json`(同上实测纯净) | 754 | 10 |
| `aikun-admin` | 管理面业务实现:impl 生成的 `AuthApi` / `UserApi` / `AdminApi`;装配在二进制 `main.rs`(Auth 裸挂、User 包 `auth_middleware`、Admin 包 `auth_middleware + require_admin`) | `aikun-openapi` + `aikun-channels` + axum | 5563 | 15 |
| `aikun-store` | 仓储能力 trait(零 SQL 方言,SurrealDB 第二实现留缝)+ SQLite impl:pool / schema / 行映射 / 静态加密 / 口令哈希 / 默认价格播种 | `aikun-channels` + rusqlite(仅 impl 侧)等 | 2196 | 20 |
| `aikun`(二进制) | 装配与运行时编排:pipeline / channel 运行时(transport / selector / health / registry 快照)/ billing / cache / auth / api(`/v1` 手写,永久)/ frontend | 上述全部 crate + axum / reqwest / rusqlite | 9296 | 54 单测 + 65 e2e(见下) |

依赖方向(architecture §三,单向无环):

```
aikun-protocol (serde_json)
   ↑                ↑
aikun-channels   aikun-openapi (不依赖任何兄弟 crate)
   ↑    ↑              ↑
   │    └── aikun-admin ──→ aikun-store (rusqlite)
   │            ↑              ↑
   └──────── aikun(二进制) ────┘
```

e2e 布局:`aikun/tests/` 7 个目标——`e2e.rs`(33)+ `e2e_{auth(5), user(5),
admin_users(7), admin_providers(8), admin_billing(4), store(3)}.rs`,
静态统计 **65** 个 `#[tokio::test]`(实测同值);旧管理面 / 用户自助用例随旧
handler 删除,登录链路 / 计费 / 兑换等行为锚点断言不减(plan 步骤 9 政策)。

## 三、验收快照(收尾当日实测)

| 判据 | 命令 | 结果 |
|---|---|---|
| 版本门禁 | `grep version aikun/Cargo.toml`;`Cargo.lock` | `version = "0.5.0"`(aikun/Cargo.toml:6),Cargo.lock 同步 |
| 旧管理路径清零(源码) | `grep -rn "api/admin" aikun/src/` | 7 命中,**全为注释**(main.rs 6 + models/mod.rs 1) |
| 旧管理路径清零(前端) | `grep -rn "/api/admin" aikun/src/frontend/` | 0 命中 |
| API 层锁清零(阶段 3) | `grep -rn "conn.lock()" aikun/src/api/` | 0 命中 |
| 纯净 crate 判据(architecture §六) | `cargo tree -p aikun-protocol` / `-p aikun-channels`(normal deps) | reqwest / rusqlite / tokio 均 0 命中 |
| 生成 crate 零测试 | `grep -rn "#\[test\]\|#\[tokio::test\]" crates/aikun-openapi` | 0 |
| regenerate 守卫 | `.github/workflows/ci.yml` 的 `regenerate` job | 已上(钉版 0.24.0 安装 → `bash scripts/gen-api.sh` → `git diff --exit-code`) |

**未在收尾时运行**(以 CI 门禁 / 人工复跑为准,如实记录):

- `cargo test --locked --workspace`(全量测试)与 `cargo check`:脚本统一门禁,
  收尾只做文档与版本整理,未改任何 Rust 代码;
- `scripts/bench.js` 50 并发档对比(plan 步骤 8 验收项):未跑,阶段 3 性能
  归位的量化对比以 CI / 人工复跑为准。

## 四、遗留项

按去向归类;前三项是用户点名的三项:

1. **roadmap 阶段 2(pipeline 显式化)未开始**:passthrough 复刻整条管线
   (P0-1)、God function 拆小(`RequestCtx` / `GatewayError`,`dispatch.rs` /
   `stream.rs` 仍是纯移动未拆,P0-2)、可观测性打点前置(第 9.6 节)。
   任务清单见 `docs/refactor-plan.md` 7.3(阶段 2:管线显式化)。
2. **契约测试流式样本**:`crates/aikun-protocol/tests/fixtures/` 目前只有
   非流式 OpenAI ↔ Anthropic 双向黄金样本 + 官方 spec 快照;流式样本按计划
   留阶段 2(与 stream 状态机改造同批)。
3. **SurrealDB 阶段 S**:SQLite → SurrealDB(默认嵌入式 SurrealKV,保单二进制
   零依赖)。前置条件已就绪——`aikun-store` 的能力 trait 签名零 SQL 方言;
   验收标准(bench 不劣化、迁移工具、备份故事重写、版本钉死)见
   `docs/plan-next-steps.md`「阶段 S」。
4. **spec 5xx 缺口(5xx→4xx 过渡映射)**:生成响应枚举绝大多数 op 无 5xx
   变体,DB 故障 fail-closed 映射为 4xx;`listApiKeys` / `getWalletStats` /
   `listPublicPrices` / `login` 四 op 仅退 401,前端清 token 会误杀会话。
   根治需 spec 补 5xx 响应后重生成,裁决前客户端不得把上述 4xx/401 当终态
   契约(`docs/api-mapping.md` 备注 13)。spec 已定稿,改动属人工裁决事项。
5. **`/api/health` version 无鉴权探测未落地**:版本门禁目前需鉴权
   (`GET /api/system/version`);health 响应带同一 version 字段作无鉴权探测
   的建议仍待集成侧实现(handler 在 `main.rs`,api-mapping 备注 11)。
6. **store 收编未完**:计费记账事务本体(`aikun/src/billing/ledger.rs`)仍留
   二进制(红线:计费事务不改),后续批次再收编为 `UsageLedger` 能力
   (`crates/aikun-store/src/lib.rs` 注释明示);providers PATCH 与后台健康检查
   共享的写互斥语义待 store 原子端口方法回归(api-mapping 备注 13 末段)。
7. **roadmap 阶段 4(发布整理)未开始**:前端治理(ESM / hash 路由 /
   providers.js 弹窗去重)、`/metrics` 从 span 汇总等,见 `docs/refactor-plan.md`
   7.5(阶段 4:收尾与验证)。

## 五、发布说明要点

- 对外行为变化全部单列 `CHANGELOG.md` [0.5.0] 段;逐条路径/状态码/错误体
  对照见 `docs/api-mapping.md`(含「全局语义变化」表 12 条)。
- **0.4.x 管理面客户端与 0.5.0 完全不兼容**:按 spec 生成的客户端必须以
  `GET /api/system/version` 主次版本 ≥ 0.5 门禁,不得指向 0.4.x 实例
  (最危险的坑:同路径 `GET /api/prices` 语义翻转,api-mapping 备注 11)。
- `/v1/**` 代理面契约、路径、错误体全部不变(v1 冻结)。
