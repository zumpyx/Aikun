# 结构迁移后续步骤(2026-11-16 首批落地之后)

> 用途:architecture-v0.5 §五结构迁移的施工清单。按依赖排序,每步一个独立
> 提交(或一串小提交),带验收标准;完成一步回写 `architecture-v0.5.md` §五
> 与 `refactor-plan.md` 状态行。
> 纪律(继承 roadmap §四):每步结束 e2e 全绿、禁止推倒重写、行为变化单列
> CHANGELOG。测试取舍按本文步骤 9 的既定政策执行(旧测试不再神圣)。
> 优先级:`roadmap-v0.5.md`(阶段与纪律)> `architecture-v0.5.md`(目标结构)
> > `refactor-plan.md`(进度)> 本文(结构迁移的执行序)。

## 当前锚点(已完成,2026-11-16)

- 根包变体 workspace:根 `Cargo.toml` `[workspace] members = ["crates/*"]`,
  `resolver = "3"`;二进制仍在根包(`cargo metadata --no-deps` 实测成员 =
  `aikun` + `aikun-openapi`)。
- `openapi.yaml`(OpenAPI 3.2.1 全景 spec)+ `docs/api-mapping.md` 逐条对照。
- `crates/aikun-openapi`:openapi-to-rust 0.24.0 钉版生成,43 op 白名单,
  零手写零测试。
- `scripts/gen-api.sh` 可复现生成管线;CI lint/test 改全 workspace
  (`cargo fmt --all --check` / `cargo clippy --workspace --all-targets` /
  `cargo test --locked --workspace`)。
- 本次实跑验证:`cargo check -p aikun-openapi --locked` 与
  `cargo fmt --all --check` 通过。

依赖主线:提交评审(1)→ 守卫(2)→ workspace 完整化(3)→ protocol(4)→
channels(5)→ admin 逐 tag(6)⇄ 前端切换(7)→ store(8);测试政策(9)贯穿 3–8。
roadmap 阶段 2(pipeline 显式化)与步骤 3–5 无冲突,可并行推进
(architecture §五.7),其任务清单见 `refactor-plan.md` 7.3。

---

## 步骤 1:本次产物提交与评审

- **依赖**:无(前置)。
- **对应**:architecture §五.4 前半(spec 评审);无 roadmap 阶段。
- **内容**:把工作树里的未提交产物提交并送评:`openapi.yaml`、
  `openapi-to-rust.toml`、`crates/aikun-openapi/`、`scripts/gen-api.sh`、
  workspace 根包变体(`Cargo.toml` / `Cargo.lock`)、CI `--workspace`、
  文档四件(`architecture-v0.5.md` / `api-mapping.md` / `refactor-plan.md`
  进度行 / `roadmap-v0.5.md` 状态行)、`CHANGELOG.md` 与本文。
  **评审重点**(api-mapping「备注」里标了待拍板的):
  1. ID int64 迁移方案(备注 1,步骤 6 的硬前置);
  2. providers batch 状态码统一 200(备注 3);
  3. 软删除语义保留 DELETE(备注 4);
  4. 生成 crate 的 crate 级 clippy allow 清单(`lib.rs`:collapsible_if、
     double_must_use 两条,不得散进生成文件)。
- **验收**:工作树 `git status` 干净;提交可独立回滚;评审结论回写
  `openapi.yaml` / `api-mapping.md`——spec 如有改动,重跑
  `bash scripts/gen-api.sh` 后 `git diff` 仅生成物随之变化。

## 步骤 2:CI 增加 regenerate 守卫 job ✅(2026-11-16)

- **依赖**:步骤 1(脚本与生成 crate 已入库)。
- **对应**:architecture §五.4 末项、§二 CI 注释;纯 CI 变化,无行为对应阶段。
- **内容**:`ci.yml` 新增独立 job:安装钉版生成器
  (`cargo install openapi-to-rust --version 0.24.0 --locked`,可缓存
  `~/.cargo/bin`)→ `bash scripts/gen-api.sh` →
  `git diff --exit-code -- crates/aikun-openapi`,任何差异即失败。
  无 Java 运行时(openapi-to-rust 是 Rust 原生)。越早上越早防生成物漂移,
  与后续结构步骤无依赖冲突。
- **验收**:
  - 守卫 job 在当前 HEAD 上绿(生成物与入库副本逐字节一致);
  - 故意改 `openapi.yaml` 一行不重生成 → job 红;恢复后绿;
  - 手改 `crates/aikun-openapi/` 下任一文件 → 重生成覆盖后 diff 非空 → job 红。

## 步骤 3:workspace 完整化(二进制移入 aikun/)✅(2026-11-16)

- **依赖**:步骤 1。
- **对应**:architecture §五.1 后半、§二目录树;roadmap 阶段 1 纪律
  (纯结构移动,无行为变化)。
- **内容**:根 `Cargo.toml` 改 virtual manifest(`members = ["aikun",
  "crates/*"]`,workspace 级 `package.metadata` / `edition` / rustfmt 设定随迁);
  `src/`、`tests/` 移入 `aikun/`;同步:
  - `.github/workflows/ci.yml`(工具链、缓存 key);
  - `.github/workflows/release.yml`(artifact 路径
    `target/<T>/release/aikun`——workspace 共享 target 目录,预期不变,需实测);
  - `Dockerfile`(`cargo zigbuild` 的成员选择按默认成员或显式 `-p aikun` 确认);
  - `env.example`、`docker-compose.yml`、README 中的路径引用如有。
- **验收**:
  - `cargo build --release` 产物名(`aikun`)与路径不变;
  - `cargo test --locked --workspace` 全绿,用例数量较迁移前不减
    (基线:148 单测 + 42 e2e,见 refactor-plan 状态行);
  - `grep -rn` 仓库脚本/CI/Dockerfile 无指向根 `src/`、根 `tests/` 的残留路径;
  - release 的 docker 路径验证 job(ci.yml 内嵌)绿。

## 步骤 4:crates/aikun-protocol 拆分(纯移动)✅(2026-11-16,含 9.1 契约测试雏形:非流式双向黄金样本 + 官方 spec 快照 f437d0ec)

- **依赖**:步骤 3(最终路径就位后一次移到位,避免二次改路径)。
- **对应**:architecture §五.2、§四迁移映射第 1 行;roadmap 阶段 1 纪律
  (纯移动);同时落 refactor-plan 9.1 的阶段 1 遗留(契约测试目录)。
- **内容**:`src/protocol/`(16 文件约 4.1k 行)→
  `crates/aikun-protocol/src/`,deps 收敛为 `serde_json`(仅此);
  单测随实现文件迁移;二进制以 path dependency 引用。
  契约测试目录直接建在最终位置
  `crates/aikun-protocol/tests/fixtures/`(先非流式 OpenAI ↔ Anthropic
  样本 + OpenAI 官方 spec 快照参照,流式样本留阶段 2),不在根 `tests/`
  中转。
- **验收**:
  - e2e 全绿;`git diff --stat` 显示纯移动(行数守恒);
  - `cargo tree -p aikun-protocol` 输出无 reqwest / rusqlite / tokio
    (architecture §六判据);
  - 假协议枚举值插入后编译器仍在全部 match 处报错(沿用阶段 1 验收,
    人工验证一次并记录处数)。

## 步骤 5:crates/aikun-channels 拆分(纯移动)✅(2026-11-16,apply_auth 改 auth_headers 纯数据签名以守纯净度)

- **依赖**:步骤 4(channels 依赖 protocol)。
- **对应**:architecture §五.3、§四迁移映射第 2 行;纯移动。
- **内容**:`src/channel/adapter/` → `crates/aikun-channels/src/adapters/`;
  deps:`aikun-protocol` + `serde_json`,无 I/O(不引入 reqwest——transport /
  selector / health / model_test 留在二进制 crate,architecture §二)。
- **验收**:
  - e2e 全绿;
  - `cargo tree -p aikun-channels` 无 reqwest / rusqlite / tokio;
  - 演练 §六判据:新增一个假适配器,git diff 只落在 `aikun-channels` 一个
    crate(演练后丢弃)。

## 步骤 6:crates/aikun-admin 落位 + Auth tag 试点 → 逐 tag 迁移 ✅(2026-11-17 全批完成回写)

- **验收核对(2026-11-17)**:`grep -rn "api/admin" aikun/src/` 命中 **7 处、
  全为注释**(main.rs 6 处 + models/mod.rs 1 处)、`grep -rn "/api/admin"
  aikun/src/frontend/` **0 命中**(收尾当日实测复核同值);行为变化已单列 CHANGELOG
  (路径 / 201 / 204 / PATCH 回资源 / problem+json / batch 200 / last_admin
  409 / 429 码与 Retry-After / 5xx→4xx 过渡映射,后者详见 api-mapping
  备注 13);二进制版本升 **0.5.0**(`aikun/Cargo.toml`),版本门禁可用。
  登录/兑换 429 的 `Retry-After` 已补;`/api/health` 带 version 字段的
  无鉴权探测(备注 11 建议)**未落地**——handler 在 main.rs,待集成侧
  处理。

- **依赖**:步骤 2、5(`aikun-admin` 依赖 `aikun-openapi`、`aikun-channels`;
  过渡期直接用二进制 db 模块,不等 store)。
- **对应**:architecture §五.5、决策 2/3;非 roadmap 原生阶段(spec-first
  重设计),但遵守 roadmap 纪律:每批 e2e 全绿、行为变化单列 CHANGELOG。
- **内容**:
  1. 建 `crates/aikun-admin`(impl `openapi::server::{AuthApi, UserApi,
     AdminApi}` trait 的家);装配在二进制 `main.rs`:Auth 裸挂、User 包
     `auth_middleware`、Admin 包 `auth_middleware + require_admin`
     (architecture §三.3);
  2. **试点批次 = Auth tag**(login、serveFrontend、serveStaticAsset,3 个 op,
     无 DB 写路径、行为面最小),用它验证装配、中间件分层、problem+json
     错误渲染、e2e 改写与旧 handler 删除的全流程;
  3. 试点通过后按 architecture §五.5 顺序逐 tag:
     providers → prices → users → logs → redemption…,每 tag 一批,
     **存量 handler 迁一批删一批**;
  4. 前置拍板项(步骤 1 评审遗留):ID int64 迁移方案(api-mapping 备注 1)——
     users/api-keys/providers 等批次开工前必须定;
  5. impl 侧待办随批次带上:登录/兑换 429 补 `Retry-After` 头
     (api-mapping 全局语义 #12);`/api/health` 响应带 version 字段
     (备注 11 版本门禁的无鉴权探测)。
- **验收**(每批):
  - e2e 全绿:新路径新用例 + 该批旧 e2e 随存量 handler 删除(按步骤 9 政策,
    不为保留而保留、不改断言迁就);
  - `grep -rn "api/admin" aikun/src/`(步骤 3 之后二进制源码根)命中数随
    批次递减,最终批完成时归零;
  - CHANGELOG 单列该批对外行为变化(路径、状态码 201/204、PATCH 回资源、
    错误体 problem+json);
  - 全部批完成后版本门禁可用:`GET /api/system/version` 主次版本 ≥ 0.5。

## 步骤 7:前端路径按 api-mapping 切换 ✅(随步骤 6 各批同步完成;收尾实测 `grep -rn "/api/admin" aikun/src/frontend/` 0 命中)

- **依赖**:步骤 6(逐批跟进:每迁一个 tag,管理端对应页面同步切)。
- **对应**:architecture §五.5 伴随项、决策 7(前端保持 vanilla);
  roadmap 阶段 4 前端治理的路径适配子集(ESM / hash 路由 / providers.js
  弹窗去重仍是阶段 4 任务,不在本文)。
- **内容**:`aikun/src/frontend/*.js` 按 `docs/api-mapping.md` 切换:
  URL(去 `/admin` 段、资源化新路径)、列表解包(`{items,total}`)、
  删除/创建/PATCH 的状态码与新响应形状、错误处理(problem+json 读
  `code` / `detail`,不再读 `error` / `message`)。严格对齐「全局语义
  变化」表 #1/#3/#4/#5/#6/#7。
- **验收**:
  - e2e 全绿(内嵌前端由服务端测试覆盖);
  - `grep -rn "/api/admin" aikun/src/frontend/` 为 0;
  - `grep -rn` 前端无残留裸数组解析与 `{error,message}` 读取。

## 步骤 8:aikun-store 层(阶段 3)✅(2026-11-17 回写:`crates/aikun-store` 就位,`grep -rn "conn.lock()" aikun/src/api/` 收尾实测 0 命中;registry 快照 / 价格缓存世代失效 / `api_key_usage_daily` 计数器同批落地,防御测试见 `aikun/tests/e2e_store.rs`;`scripts/bench.js` 对比以 CI / 人工复跑为准)

- **依赖**:步骤 6(admin 迁移期用 db 模块;store 建立后逐步吸收)。
- **对应**:roadmap **阶段 3**(性能与状态归位);architecture §五.6、§二
  (`aikun-store` = 仓储 trait + SQLite impl)。
- **内容**:`src/db/` + `src/models/` → `crates/aikun-store/`(阶段 3
  逐步吸收,先模块后 crate 亦可,architecture §四);管理面 DB 访问收口
  spawn_blocking;`api/` 下 `conn.lock()` 清零;价格播种移出 db 层;
  同阶段配套:渠道 registry 快照、价格缓存、日额度计数器表
  (roadmap 阶段 3 全套,见 refactor-plan 7.4)。
- **验收**(= roadmap 阶段 3 验收原文):
  - e2e 全绿;
  - `scripts/bench.js` 50 并发档对比重构前不劣化(选路缓存生效后应有提升);
  - `grep -rn "conn.lock()" src/api/`(迁移后为 `aikun/src/api/`)为 0。

## 步骤 9(贯穿政策):测试策略(用户已定)

- **对应**:贯穿步骤 3–8;细化 roadmap §四.1「e2e 是唯一安全网」在新管理面
  API 之下的执行口径。
- **政策**:
  1. **旧测试不再神圣**:端点迁移到新 spec 时,对应旧 e2e 随存量 handler
     一起删除——不为保留而保留,不改断言迁就旧实现;
  2. **生成代码零测试**:`crates/aikun-openapi` 不写、不跑任何测试
     (`gen-api.sh` 已删除生成器自带测试),质量由 regenerate 守卫(步骤 2)、
     `/v1` 白名单防线与 spec 评审保证;
  3. **核心手写行为的测试必须保留并随迁**:计费扣费、兑换码、协议转换、
     登录链路(及其余 /v1 管线行为)——这些是手写实现的行为锚点,
     迁移中覆盖不得降低。
- **验收**:
  - 每批迁移的提交说明列明:删除了哪些旧 e2e、新增了哪些新路径用例;
  - 计费扣费 / 兑换码 / 协议转换 / 登录链路的用例数量与断言强度不减
    (基线:阶段 0 补的登录链路 e2e、修复 ①②③ 的回归测试均仍在);
  - `cargo test --locked --workspace` 中不出现 `aikun-openapi` 的任何测试目标。

## 阶段 S(0.5 之后):数据库迁移 SurrealDB ✅(2026-11-17 回写)

> 2026-11-16 用户决策:整个结构重构完成后,数据库从 SQLite 迁移到 SurrealDB。
> 路线:默认嵌入式 SurrealKV(保住单二进制零依赖),将来可切网络连接模式——
> 把"是否多实例"从架构决策降为部署决策。许可已查证:BSL 1.1 宽松变体,
> 允许嵌入商业应用并生产使用(仅禁 DBaaS 转售,4 年 lapse),与 AGPL 双授权无冲突。

前置条件:aikun-store 仓储层就位,且 trait **按能力定义、不泄漏 SQLite 方言**
(datetime 字符串运算 / ON CONFLICT / ?N 占位 / PRAGMA——设计评审时逐条过)。

**验收核对(2026-11-17 收尾实测)**:

- **版本钉死** ✅:`crates/aikun-store/Cargo.toml` `surrealdb = "4.0.1"`,
  `Cargo.lock` 实查同为 4.0.1。
- **迁移工具** ✅:`crates/aikun-migrate`(835 行)——源库只读打开 +
  `user_version` 闸门(必须恰为 3,低则提示升满、高则拒绝)、目标库
  `ensure_schema`(`store_meta:schema.version` 即 user_version 闸门的
  替代语义)、按设计文档 §五字段映射全量搬运、幂等 UPSERT 可重跑、
  完成后打印各表行数对照且全部一致才报成功。Docker 镜像已内置。
- **备份/恢复故事重写** ✅:README「数据与升级」改为嵌入式停机后整目录
  拷贝(`./aikun.surreal/`)或 `surreal export`,远程用 `surreal export` /
  `surreal import`;`docker-compose.yml` 数据卷挂 `/data/aikun.surreal`。
- **主路径 SQLite 清零** ✅:`grep -rn rusqlite aikun/src crates/aikun-store/src
  crates/aikun-admin/src crates/aikun-channels/src crates/aikun-protocol/src
  crates/aikun-openapi/` 0 命中(SQLite 读能力收口进 `aikun-migrate`);
  `grep -rn "conn.lock()" aikun/src/api/` 0 命中。
- **测试面随迁** ✅:e2e 检查通道全部改经管理面 API
  (`AIKUN_RESET_ADMIN_PASSWORD` 注入已知密码;dev-dependencies 无任何
  数据库直连 crate),静态统计 **65** 个 `#[tokio::test]`(与阶段 S 前
  持平);store 侧 Surreal 单测 **29** 个(`mem://` 进程内实例,断言
  微元口径 / 事务原子性),`aikun/src` 内 `spawn_blocking` 仅剩 2 处
  (Argon2 口令哈希,CPU 密集属意留)。
- **bench(阶段 S 批次记录,如实转录)**:`scripts/bench.js`(mock 上游
  零延迟、非流式 `/v1/chat/completions`、含完整计费链路),环境 12 核
  Apple M6 / node v26.13.1:

  | 并发 | 请求数 | QPS | P50(ms) | P99(ms) |
  |---|---|---|---|---|
  | 10 | 1000 | 609 | 13.5 | 55.1 |
  | 50 | 5000 | 576 | 78.9 | 202.0 |
  | 100 | 10000 | 569 | 152.8 | 489.7 |
  | 200 | 20000 | 539 | 329.1 | 1033.6 |

  「不劣于 SQLite 基线(2400 QPS @ 50 并发)」一项**不据此判定**:该基线
  出自 10 核 i9-12900H 的旧机记录,与本批 M6 非同机对照,阶段 S 各批
  notes 未提供同机 SQLite 对照数据——如实记录,同机对照待补。

原验收标准(存档):

- `scripts/bench.js` 同场景对比,计费热路径(高频小事务串行写)不劣于 SQLite 基线(2400 QPS @ 50 并发)
- SQLite → SurrealKV 数据迁移工具(含 user_version 闸门语义的替代:版本检测 + 拒绝降级)
- 备份/恢复故事重写(export/snapshot 替代"复制单文件")
- 版本钉死(3.0 起转向 AI agent memory 方向,演化快)

实施结果与设计偏差记录见 `docs/surreal-migration.md` §十;对外行为变化
逐条见 `CHANGELOG.md` [0.6.0]。

### 阶段 S 遗留(2026-11-17 独立复审)——已全部关闭(同日)

- ✅ **selector 失败分类守卫单测**:提取 `is_upstream_failure`/`counts_toward_disable`
  纯函数 + 真值表锁定(3e3b570)。
- ✅ **迁移演练**(合成旧库 12 表:真实 enc:v1 密文、混合时间戳):行数对照全过、
  微元/密文逐字节原样、时间归一、幂等 ×3、迁移库启动 + 迁移哈希 key 过 /v1 鉴权;
  发现并修复 v3 早期库缺表中断 bug(9672be8)。
- ✅ **远程模式实机验证**(surreal 服务端 v4.0.1,darwin-arm64):
  `AIKUN_SURREAL_URL=ws://127.0.0.1:8000?user=root&pass=root` 启动 → schema 播种
  211 条 → /api/health → 登录签发 JWT → 带 token 的 /api/users/me 全部走通。
- ✅ **roadmap 阶段 2(pipeline 显式化)**:RequestCtx/分类/单次 attempt/流式三单元/
  GatewayError/passthrough 并入(27 行薄壳)/request_id 贯穿/三类超时/脱敏/缓存收紧,
  全量 270 测试绿(7bbd2a6);**9.1 流式黄金样本**双向 7 用例(6a9ef68)。

### 后续批次(2026-11-18):/v1 代理面纳入 openapi-to-rust 生成 ✅

> 用户裁决,推翻 0.5 期"/v1 永远手写"(architecture-v0.5 决策 4 已重写留痕);
> 同时拍板 **Gemini 等新协议不做**(roadmap 阶段 4 待定项关闭)。

- **spec**:`/v1` 八端点 requestBody 换混合宽松 schema(核心字段 optional 类型化,
  **每层显式 `additionalProperties: true`**——0.24.0 实测生成 `#[serde(flatten)]`,
  漏写的层会静默丢字段);200 仍声明 json + text/event-stream。
- **生成管线**:全量生成(spec 全部 operationId 由 gen-api.sh 机械注入,无手工白名单);gen-api.sh 防线由"拒绝 /v1"反转为正向清单
  (8 条路由必须齐)+ wrapper 注入 `Raw(Response)` 透传变体(管线动态响应
  装不进生成枚举);复现性校验逐字节一致。
- **装配**:`aikun/src/relay.rs`(RelayService 实现 ProxyApi trait;Claims/
  AuthContext/headers 经 relay_scope task-local 注入,同一模式源自
  aikun-admin user.rs);problem+json 校验错误经中间件改写为厂商格式(422
  归一 400);`/v1/{*path}` 通配兜底保留手写;手写 handler 删 3 个文件。
- **验收**:e2e 全绿且既有断言零修改;新增 2 用例锁定新行为(非法 JSON →
  厂商格式 400;顶层扩展字段 flatten 透传到上游)。
