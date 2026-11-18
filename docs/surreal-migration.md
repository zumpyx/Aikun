# SurrealDB 迁移设计(阶段 S)

> 状态:**已完成**(2026-11-17 阶段 S 全部批次落地,默认已切 SurrealDB;
> 实施结果与设计偏差见 §十)。
> 上游依据:`docs/plan-next-steps.md` 阶段 S(2026-11-16 用户决策)、
> `docs/architecture-v0.5.md` §六(store trait 禁方言判据)、用户 2026-11-17
> 授权(新版 SDK、配置面已定、schema 可重构)。
> 红线(任何批次不许动):计费扣费/兑换码核销的事务原子性、渠道 key
> 加密存储、余额微元整数口径。

## 一、选型钉版:surrealdb = "4.0.1"

**结论:最新稳定线 4.0.x,钉 `4.0.1`,无降档。** 证据链(2026-11-17 实查):

* crates.io API(`GET /api/v1/crates/surrealdb`):`max_stable_version = 4.0.1`,
  `newest_version = 4.0.1`,发布于 2026-11-09;近期序列
  `4.0.1 / 4.0.0 / 3.3.5 / 3.2.6 / …`——4.0 是当前最新发布线。
* docs.rs 4.0.1 features 页 + `engine/local`、`engine/remote`、源码
  `engine/any/mod.rs`(钉版源码,cargo fetch 后本地核验):
  * 嵌入式:**`kv-surrealkv`** feature → `engine::local::SurrealKv`;
    4.0 起 `kv-rocksdb` / `kv-indxdb` / `kv-tikv` 已移除(源码测试
    `deprecated_schemes_open_with_surrealkv`:旧 scheme 一律以 SurrealKV
    打开),SurrealKV 是在盘引擎唯一选项(Rust 原生,无 C++ 工具链)。
  * 远程:默认 feature **`protocol-ws`**(Ws/Wss,自动重连);可选
    **`protocol-http`**(Http/Https)。
  * **Any 引擎按地址字符串运行期选引擎**(源码 `engine/any/mod.rs` 的
    `IntoEndpoint`):`mem://`/`memory` → 内存;`ws*`/`http*`/`grpc*`/
    `tikv` 前缀 → 远程 URL;其余(含 `surrealkv://<目录>`)→ 本地引擎
    路径。一个 `Surreal<Any>` 句柄覆盖全部部署形态,无需按引擎泛型化。
  * 连接:`surrealdb::engine::any::connect(address)`(源码 369 行,
    `-> Connect<Any, Surreal<Any>>`)→ 可选 `db.signin(Root{…})` →
    `db.use_ns(ns).use_db(db)`。
  * 事务:多语句单次 `db.query("BEGIN TRANSACTION; …; COMMIT TRANSACTION;")`,
    服务器端原子;语句级错误经 `IndexedResults::check()`(源码
    method/query.rs:771)带出,`take::<T>(i)` 取每语句结果
    (`R: SurrealValue`)。
  * 4.0 序列化面变化(相对旧教程):自定义结构走 **`SurrealValue`**
    (`use surrealdb::types::SurrealValue;` + derive),不再纯 serde;
    `serde_json::Value`、chrono `DateTime<Utc>`/`NaiveDate`、原始类型、
    `Option<T>`/`Vec<T>` 均有现成 impl,桩实现优先用这些免 derive。
* **阻断性验证已过**:`surreal/mod.rs` 的 `mem_connect_schema_and_gate`
  测试(mem:// 连接 → ensure_schema → 版本闸门拒绝降级)在 4.0.1 上
  编译并运行通过(见 §八自检记录)。

Cargo.toml(已落):`surrealdb = { version = "4.0.1", features =
["kv-surrealkv", "kv-mem", "protocol-http"] }`(默认面含 protocol-ws +
rustls;`kv-mem` 为测试进程内实例的检查通道,见 §七)。
许可(plan 阶段 S 已查证):BSL 1.1 宽松变体,可嵌入商业应用生产使用。

**栈配置(实测踩坑,集成批必办)**:嵌入式引擎的查询执行在**调用任务
的栈上内联递归**,默认 2MiB 线程栈执行 SCHEMA_SURQL 级别的多语句
查询即溢出(本批 `mem_connect_schema_and_gate` 测试实测,复现路径:
默认 `#[tokio::test]` → stack overflow;官方 engine::local 文档建议
~10MiB 栈)。因此:测试用大栈线程(已落,见该测试);**生产运行时
集成批必须**以 `tokio::runtime::Builder::new_multi_thread()
.thread_stack_size(…)` 显式建运行时替换 `#[tokio::main]` 宏默认值
(建议 ≥10MiB),否则深查询在 axum worker 线程上有溢出风险。

## 二、配置面(用户已定,已落地)

| 项 | 值 |
|---|---|
| CLI 参数 | `--surreal-url` |
| 环境变量 | `AIKUN_SURREAL_URL`(优先级:env > CLI > 默认,同其余配置) |
| 默认值 | `surrealkv://./aikun.surreal`(嵌入式 SurrealKV,目录) |
| 合法形式 | `surrealkv://<目录>` / `mem://`(测试)/ `ws://` `wss://` `http://` `https://` + 可选 `?ns=&db=&user=&pass=` |
| ns/db 约定 | 默认 `aikun`/`aikun`;URL 参数覆盖 |
| 校验 | `aikun_store::surreal::Endpoint::parse`;非法(未知 scheme/空目录/user 不成对/未知参数)在 `config.rs` **panic 拒绝启动**(与 host 解析同口径) |

* **位置语义沿用**:`AIKUN_SURREAL_URL` 未显式设置、而
  `AIKUN_DATABASE_URL` 显式指向非默认位置时,嵌入式目录推导为旧库
  同目录(`sqlite_dir_of`,`aikun/src/config.rs`)——docker 卷挂载部署
  的数据位置随迁,不落回工作目录。测试
  `surreal_dir_follows_database_url_location`。
* **废弃告警**:`AIKUN_DATABASE_URL`/`--database-url` 显式设置时启动
  告警一次(默认值不告警);解析行为保留——SQLite 后端(切换完成前)
  与迁移工具(读源库)仍在消费。
* `--help` 文案与 `env.example` 已同步。

## 三、ID 策略

* 对外 API 的 ID 仍是**不透明 string**,类型层零变化(openapi.yaml 不动)。
* Surreal 侧 record id 形如 `user:01J8…`(表名:ULID):新建记录用
  `ulid::Ulid::new()` 作 `RecordIdKey`(时间可排序)。
* **迁移的旧行沿用旧 uuid 作 key**(`user:<旧uuid>`):request_log 等
  软引用零翻译,管理面展示的旧 ID 不变;新旧 key 混存合法。
* 复合主键表用数组 record id(`api_key_usage_daily:[api_key:<id>,
  '<YYYY-MM-DD>']` 等),UPSERT 直接定位,替代 `ON CONFLICT DO UPDATE`。

## 四、schema 重设计(schemafull)

DDL 全文 = `crates/aikun-store/src/surreal/mod.rs` 的 `SCHEMA_SURQL`
(工人只读;变更走集成批)。表集:`store_meta / user / api_key /
provider / request_log / model_price / api_key_usage_daily / usage_daily /
channel_affinity / billing_transaction / redemption_code / model_health`。
表名单数小写,对齐 SDK 文档惯例。

类型归一(相对 SQLite 的现代化,发现即改):

* **时间**:`TEXT`(RFC3339 与 `' '` 空格格式混存,schema.rs:883 有案)
  → 原生 `datetime` / `option<datetime>`;日期键(usage_daily.date 等)
  用 `string 'YYYY-MM-DD'`(与旧 `substr(created_at,1,10)` 口径一致)。
* **布尔**:`INTEGER 0/1` → `bool`。
* **数组/对象**:`models`/`protocols` 的 JSON TEXT → `array<string>`;
  `model_mapping` 的 JSON TEXT → `object`。域类型
  `aikun_channels::Provider` 的对应字段仍是 JSON 字符串(registry
  契约)——**行映射边界做一次序列化/反序列化**,域类型零改动。
* **关联**:`TEXT` 软外键 → `record<表>` 链接。SurrealDB 无强制 FK:
  记账热路径**不再需要 EXISTS 预检降级**(SQLite 侧为躲 FK 约束的
  工作,ledger.rs:241-262);dangling 链接由 `query_logs` 读侧降级
  None,等价旧 `ON DELETE SET NULL` 的可观测语义。
* **版本闸门**:`PRAGMA user_version` → `store_meta:schema.version`;
  启动时库版本 > `SUPPORTED_SCHEMA_VERSION` 拒绝启动(降级运行保护,
  语义同 pool.rs:104-113)。
* 讨论后**保留**的旧设计(不重构):
  * `provider.provider_type`:openapi.yaml 冻结契约的 required 字段
    (openapi.yaml:3410),删列即破坏契约。
  * EMA/健康字段留在 `provider` 行:拓扑快照一次读取;缓存令牌
    (count+max updated_at+unhealthy 计数)单表聚合即可覆盖
    健康态跃迁;拆表会把令牌变成双表聚合且健康循环多一次写放大。
    迁移后再评估拆分。
  * `provider.api_key`(密文)+ `key_hash`(明文 sha256)二元结构:
    AES-256-GCM 随机 nonce 密文天然不可比对,重复检测必须独立确定性
    哈希;结构已最小。加密层(`crate::crypto`)原样复用,密文形态与
    SQLite 逐字节一致,**迁移免重加密**。
* 金额类型(红线):`user.balance`、`request_log.cost`、
  `billing_transaction.amount/balance_after`、`redemption_code.amount`
  一律 `int` 微元;`model_price.*` 保持 `float`(每 1M tokens 的价格率,
  非金额,与 SQLite 口径一致)。

## 五、新旧字段映射表(迁移工具按此搬运,不逐表复制)

时间戳解析:两种格式都要认——RFC3339(`2026-01-01T00:00:00Z` /
`+00:00`)与 SQLite `datetime('now')`(`2026-01-01 00:00:00`,按 UTC);
空串/NULL → None。金额:SQLite 侧已是微元整数(schema.rs 的
migrate_currency_micro 保证),**直搬不再乘 1e6**。

### users → user

| 旧列 | 新字段 | 变换 |
|---|---|---|
| id (TEXT uuid) | id key | 沿用作 record key |
| username | username | 唯一索引照旧 |
| password_hash | password_hash | 直搬(Argon2id PHC) |
| display_name / role / token_version | 同名 | 直搬 |
| is_active (INTEGER 0/1) | is_active (bool) | `0→false` |
| balance (INTEGER 微元) | balance (int 微元) | 直搬(红线) |
| created_at / updated_at (TEXT) | created_at / updated_at (datetime) | 双格式解析(见上) |

### api_keys → api_key

| 旧列 | 新字段 | 变换 |
|---|---|---|
| id | id key | 沿用 |
| user_id (TEXT) | user (record\<user\>) | `user_id → user:<id>` |
| key (sha256 hex) | key | 直搬;唯一索引照旧 |
| key_suffix / name | 同名 | 直搬 |
| is_active | is_active (bool) | 同上 |
| last_used_at / expires_at (TEXT) | option\<datetime\> | 空串/NULL → None |
| models (TEXT,JSON 数组或 '') | models (array\<string\>) | JSON parse;''/坏 JSON → [] |
| rate_limit_rpm / quota_daily_tokens / max_concurrent | 同名 (int) | 直搬 |
| created_at | created_at (datetime) | 双格式解析 |

### providers → provider

| 旧列 | 新字段 | 变换 |
|---|---|---|
| id | id key | 沿用 |
| name / provider_type | 同名 | 直搬(provider_type 契约必留) |
| channel_kind / openai_base_url / anthropic_base_url | 同名 | 直搬 |
| api_key (enc:v1: 密文) | api_key | **直搬不重加密** |
| key_hash (sha256 明文) | key_hash | 直搬 |
| models (TEXT JSON) | models (array\<string\>) | JSON parse → 数组 |
| model_mapping (TEXT JSON) | model_mapping (object) | JSON parse → 对象 |
| protocols (TEXT JSON) | protocols (array\<string\>) | 同 models |
| default_protocol / disabled_reason / note / website_url / proxy_url | 同名 | 直搬 |
| priority / max_retries / timeout_secs / consecutive_failures | 同名 (int) | 直搬 |
| weight / latency_ms / error_rate | 同名 (float) | 直搬 |
| is_active | is_active (bool) | 0/1 → bool |
| health_status | 同名 | 直搬 |
| last_health_check (TEXT) | last_health_check (option\<datetime\>) | 空串 → None |
| created_at / updated_at | datetime | 双格式解析 |

### request_logs → request_log

| 旧列 | 新字段 | 变换 |
|---|---|---|
| id | id key | 沿用 |
| user_id / api_key_id / provider_id (TEXT, nullable) | user / api_key / provider (option\<record\>) | `<表>:<id>`;NULL → None |
| model / request_type | 同名 | 直搬 |
| prompt/completion/total/cached_tokens、latency_ms、status_code | 同名 (int) | 直搬 |
| success (INTEGER) | success (bool) | 0/1 → bool |
| error_message | option\<string\> | NULL → None |
| cost (INTEGER 微元) | cost (int) | 直搬(红线) |
| created_at (TEXT) | created_at (datetime) | **双格式解析是本表关键**(两种格式混存量最大) |

### model_prices → model_price
id 沿用为 key;model 唯一;prompt/completion/cached_price 直搬
(cached NULL → None);created_at/updated_at → datetime。

### api_key_usage_daily → api_key_usage_daily
id = `[api_key:<id>, '<date>']`;字段 api_key(record)/date(string)冗余
存(查询面),tokens_spent 直搬。

### usage_daily → usage_daily
id = `[user:<id>, '<date>']`;user/date 冗余存;requests/tokens 直搬;
cost 微元直搬。

### channel_affinity → channel_affinity
id = `[api_key:<id>, '<model>']`;provider_id → provider(record\<provider\>);
updated_at → datetime。

### billing_transactions → billing_transaction
id 沿用;user_id → user(record);amount/balance_after 微元直搬(红线);
kind/note 直搬;created_at → datetime。

### redemption_codes → redemption_code
id 沿用;code_hash 唯一;code_suffix/batch/note 直搬;amount 微元直搬
(红线);status 直搬(unused/used/disabled);used_by → option\<record\>;
used_at/expires_at → option\<datetime\>;created_at → datetime。

### model_health → model_health
id = `[provider:<id>, '<model>']`;provider 冗余存(列表查询);
status/latency_ms/error 直搬;checked_at → option\<datetime\>。

**迁移工具口径**(后续批次实现):读 SQLite(AIKUN_DATABASE_URL 源)
→ 按上表变换 → 批量写入目标(AIKUN_SURREAL_URL);
写完 `store_meta:schema.version = SUPPORTED_SCHEMA_VERSION`;幂等
(按 record id UPSERT);结束后校验各表计数一致才报成功。
**user_version 闸门语义的替代**即 store_meta(§四);拒绝降级在
`ensure_schema` 启动路径同样生效。

## 六、trait async 化(结论:async trait,`#[async_trait]`)

**评估**。SurrealDB SDK 全 async;能力 trait 保持同步的两条路:
(a) **内部封装**(sync 签名 + 内部 block_on):`block_on` 在 async 上下文
panic/死锁;嵌入式引擎句柄绑定创建它的 runtime,每次调用跨 runtime hop;
admin 端口与 /v1 热路径全部要绕——持续性地雷,否。
(b) **async trait 化**:签名一次性调整,调用点机械改动,SQLite impl
保持同步内核。选 (b)。

**最小改动方案**(分层落地,本批已落第一层):

1. **本批(骨架)**:三个新能力 trait(`surreal/identity.rs` /
   `channels_store.rs` / `billing_store.rs`)以 `#[async_trait]` 定稿
   签名;连接管理(`SurrealHandle`)async。理由选 `async_trait`(而非
   原生 AFIT):Send bound 明确、dyn 兼容(集成期可 `Arc<dyn …>`),
   与 aikun-openapi / aikun-admin 生成面同款;每调用一次 box 相对一次
   DB 往返可忽略。
2. **集成批次(切换装配时)**:存量四个同步 trait(`ChannelTopology` /
   `PriceBook` / `KeyUsage` / `CacheTokens`,lib.rs)翻转为 async,
   调用点清单(全部已知,逐个过):
   * `aikun/src/channel/registry.rs`:token 比对 + `active_channels`
     快照重建(两处调用);
   * `aikun/src/billing/mod.rs`:`find_price_cached` → `price_table_token`
     + `price_rules`;
   * `aikun/src/pipeline/admission.rs`:`key_limits_and_usage`
     (现在 spawn_blocking 内;翻转后直接 await,删 spawn_blocking);
   * `aikun/src/billing/ledger.rs`:`insert_request_log` 事务 →
     `BillingStore::record_request`(spawn_blocking 整体删除,事务=
     单次 query 的 BEGIN…COMMIT,红线语义不变);
   * SQLite 侧 impl 翻转(async 方法体内同步 rusqlite 调用):短查询
     直接同步体(µs 级池互斥);ledger 事务走 spawn_blocking(Arc 克隆)。
   * aikun-admin 的六个端口 trait(AuthStore/UserStore/…)随装配适配器
     一起改 async(它们是 admin crate 的缝,由集成批统一翻,不属工人)。
3. **CHANGELOG 素材**(签名级调整,单列):
   * "store 能力 trait 由同步改 async(async_trait);对外 API 行为
     无变化——影响面为 crate 内部签名与二进制装配,SQLite 后端行为
     逐值不变"。

## 七、e2e 检查通道(已选:管理面 API)

**问题**:嵌入式 SurrealKV 文件被服务进程独占(单进程 LSM 锁),测试
进程不能像 rusqlite 那样直连 seed/断言(aikun/tests 的 e2e.rs 等 4 个
文件现用 rusqlite 直连)。

**方案(选定,后续批次执行)**:

1. **e2e 层:全部改经管理面 API**。测试拉起二进制时注入
   `AIKUN_RESET_ADMIN_PASSWORD=<已知值>`(机制已在,main.rs:94-112:
   启动即重置 admin 密码并 token_version+1)→ 登录 `/api/auth/login`
   拿 JWT → seed 与断言全走管理面 API(创建渠道/用户/价格/兑换码,
   断言经列表/统计/余额端点回读)。这也顺带把 e2e 断言面收到
   openapi.yaml 契约上(旧直连断言的"库内形态"检查移到下一层)。
2. **store 单测层:进程内直连**。工人在自己桩文件的 `#[cfg(test)]`
   用 `mem://`(或临时目录 surrealkv://)起**测试进程自己的**实例,
   直接断言库内行为(微元口径、事务原子性、令牌变化、dangling 降级)
   ——这是 rusqlite 直连能力的等价替代,且不起服务进程。
3. 备选(否):e2e 外起 `surreal` 服务进程用 ws 远程连——给 CI 加了
   二进制依赖,且远程路径与生产嵌入式不一致,测试保真度反而降。

## 八、分工边界与并行纪律(三路工人必读)

| 文件 | 指派 | 域 |
|---|---|---|
| `crates/aikun-store/src/surreal/identity.rs` | 工人·Identity | user / api_key + 认证路径 |
| `crates/aikun-store/src/surreal/channels_store.rs` | 工人·Channels | provider / model_health + 拓扑令牌 |
| `crates/aikun-store/src/surreal/billing_store.rs` | 工人·Billing | 价格/日志/计数器/调账/兑换/亲和/统计 |

**纪律**(违反即打回):

1. 工人只填**自己的桩文件**(todo!() 体 + 自己文件内的辅助类型/测试);
   **不碰** `mod.rs`、`SCHEMA_SURQL`、`config.rs`、`lib.rs`、彼此文件。
   需要新字段/新索引/新表 → 在自己文件头 TODO 注释 + 提交说明里记录,
   集成批统一裁决改 schema。
2. 签名以桩文件的 trait 定义为准;可在**自己文件内**增补方法与记录
   类型,不得改既有方法签名。
3. 语义基准 = SQLite 现行为(各桩文件头列了对应适配器/文件);对外
   API 契约(openapi.yaml)与管理面/代理行为不变。
4. 红线复述:计费扣费/兑换码核销原子性(billing_store.rs 文件头两条)、
   渠道 key 只存密文(channels_store.rs)、金额一律 i64 微元(三处)。
5. 测试:store 单测在自己文件内,`mem://` 起,断言库内行为
   (§七第 2 层);不跑完整 cargo test(门禁统一),迭代用
   `cargo check -p aikun-store` / `cargo test -p aikun-store surreal::<自己的模块>`。
6. 集成批(后续):装配切换(main.rs 适配器 → Surreal 能力)、存量
   四 trait async 翻转(§六清单)、**tokio 运行时显式配置
   `thread_stack_size`(≥10MiB,§一栈配置)**、e2e 检查通道改写(§七)、
   迁移工具、bench 对比(阶段 S 验收:计费热路径不劣于 SQLite 基线
   2400 QPS @50 并发)、备份故事重写(嵌入式:停机拷目录 +
   `surreal export`;远程:`surreal export/import`——替代"复制单文件",
   后续批次细化)。

## 九、本批(骨架)落地清单与自检

* `crates/aikun-store/Cargo.toml`:钉 surrealdb 4.0.1(features 见 §一)
  + async-trait + ulid + tokio(dev)。
* `crates/aikun-store/src/surreal/mod.rs`:Endpoint 解析/校验、
  SurrealHandle(connect/signin/use_ns/use_db)、SCHEMA_SURQL 全量 DDL、
  ensure_schema 版本闸门;单测(端点解析 + mem:// 连通性/闸门)。
* 三个桩文件(§八表);`lib.rs` 挂 `pub mod surreal`。
* `aikun/src/config.rs`:surreal_url 字段 + SETTINGS 文案 + 校验拒绝
  启动 + database_url 废弃告警 + 目录推导(sqlite_dir_of,带测试);
  `env.example` 同步。
* 自检(2026-11-17 实跑记录):
  * `cargo check --workspace` → 通过,0 warning;
  * `cargo fmt --all --check` → 通过;
  * `cargo clippy -p aikun-store --all-targets` / `cargo clippy -p aikun
    --all-targets` → 0 warning 0 error;
  * `cargo test -p aikun-store` → 22 passed 0 failed(含新增
    `surreal::tests::{parse_endpoints, mem_connect_schema_and_gate}` 与
    存量 20 个 SQLite 侧单测,证明共存无回归);
  * 完整测试门禁(`cargo test --locked --workspace` + e2e)由脚本统一跑,
    本批未跑。

## 十、实施结果(2026-11-17 收尾回写)

阶段 S 各批全部落地;下列条目为收尾当日在当前工作树实查(命令与结果
如实记录),设计偏差单列。

### 落地对照(逐节)

| 设计节 | 状态 | 实施位置与实查证据 |
|---|---|---|
| §一 选型钉版 + 栈配置 | ✅ | `crates/aikun-store/Cargo.toml` `surrealdb = "4.0.1"`,`Cargo.lock` 同值;主程序显式建运行时 `thread_stack_size(10MiB)`(`aikun/src/main.rs:58-66`),迁移工具 16MiB(`crates/aikun-migrate/src/main.rs:57-61`) |
| §二 配置面 | ✅ | `aikun/src/config.rs`:`surreal_url` 字段 + SETTINGS 文案 + 非法端点 panic 拒启(:233-235)+ `database_url` 废弃告警(:211-220)+ 目录推导 `sqlite_dir_of`(:328-341,单测 `surreal_dir_follows_database_url_location`);`env.example` 同步 |
| §三 ID 策略 | ✅ | 新记录 `ulid::Ulid::new()` 作 record key(`identity.rs:463`、`:683`);迁移旧行沿用旧 uuid(`aikun-migrate` 按 §五映射) |
| §四 schema | ✅ | `SCHEMA_SURQL` 12 表全部落 DDL(`surreal/mod.rs:317-470`);`SUPPORTED_SCHEMA_VERSION = 1`(`mod.rs:57`),库版本更新即拒启(两处闸门,`mod.rs:39-46`) |
| §五 字段映射 | ✅ | `crates/aikun-migrate`(main.rs 835 行):源库只读打开 + `user_version` 闸门(恰为 3)、双格式时间解析 `parse_dt`、幂等 UPSERT、各表行数对照全一致才报成功;微元直搬 / 密文免重加密(文件头红线复述) |
| §六 trait async 化 | ✅(有偏差,见下) | 三域 async trait 定稿:`IdentityStore`(`identity.rs:118`)、`ChannelStore`(`channels_store.rs:84`)、`BillingStore`(`billing_store.rs:150`),均 `#[async_trait]` + `Arc<dyn …>` 装配(`aikun/src/store.rs:18-26`) |
| §七 检查通道 | ✅ | e2e 全改管理面 API:spawn 注入 `AIKUN_RESET_ADMIN_PASSWORD` + 临时目录 `AIKUN_SURREAL_URL`(`aikun/tests/common/mod.rs:223-231`),dev-dependencies 无数据库直连 crate;store 单测 `mem://` 进程内直连 |
| §八 分工 | ✅ | 三域文件最终规模:`identity.rs` 1326 行 / `channels_store.rs` 2175 行 / `billing_store.rs` 2811 行 / `surreal/mod.rs` 565 行 |

### 设计偏差(如实记录)

1. **存量四同步 trait 未"翻转"而是被取代**:§六.2 原计划把
   `ChannelTopology` / `PriceBook` / `KeyUsage` / `CacheTokens` 四个同步
   trait 逐一翻转为 async;实施时四 trait 不再单独存在,其能力(拓扑
   令牌 / 价格规则 / 限额快照)并入三域 async trait 的方法,快照域类型
   (`PriceRule` / `KeyUsageSnapshot` / `CacheToken`)留在 `lib.rs` 共享。
   调用面结果与设计一致:registry / billing / admission 全部直接
   `await`(如 `billing/ledger.rs:243` 的 `record_request`),DB 访问不再
   包 `spawn_blocking`(`aikun/src` 内仅剩 2 处,均为 Argon2 口令哈希,
   CPU 密集属意留,`main.rs:447`、`:962`)。
2. **计费记账事务本体随切换收编**:`ledger.rs` 的记账从"二进制内 SQLite
   事务 + spawn_blocking"改为调用 `BillingStore::record_request` 的单次
   `BEGIN…COMMIT` 查询(事务原子性红线语义不变);0.5 收尾总结所述
   "ledger 留二进制"的遗留项至此消除。

### 验收数字(收尾实测)

- `grep -rn rusqlite aikun/src crates/aikun-store/src crates/aikun-admin/src
  crates/aikun-channels/src crates/aikun-protocol/src crates/aikun-openapi/`
  → **0 命中**(SQLite 收口进 `aikun-migrate`);
- `grep -rn "conn.lock()" aikun/src/api/` → **0 命中**;
- e2e:`aikun/tests/` 7 目标静态统计 **65** 个 `#[tokio::test]`
  (与阶段 S 前持平);store 侧 Surreal 单测 **29** 个
  (identity 7 + channels 9 + billing 11 + mod 2,`mem://`);
- bench(阶段 S 批次记录转录,含环境与"非同机对照"限定)见
  `docs/plan-next-steps.md` 阶段 S 验收核对;
- 完整测试门禁(`cargo test --locked --workspace` + e2e)由脚本统一跑,
  本收尾批未跑(仅文档回写)。

### 落地后的遗留

- bench 同机 SQLite ↔ SurrealDB 对照未做(旧基线出自另一台机器,
  见 plan-next-steps 阶段 S);
- 远程(ws/http)端点与 `?ns=&db=&user=&pass=` 已实现解析与连接,但
  多实例部署的运维故事(选路 / 缓存 owner 序号已留缝,`store.rs:24`)
  未在阶段 S 展开,留后续;
- SQLite 侧 pool.rs / schema.rs / provider_row.rs 已随主路径移除,
   读能力仅存于 `aikun-migrate`(再往后的 schema 演进只动 Surreal 侧)。
