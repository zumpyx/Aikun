# 管理面 API 迁移手册(逐 tag 批次)

> 用途:plan-next-steps 步骤 6 逐 tag 迁移的**标准作业程序**。Auth 批
> (login / serveFrontend / serveStaticAsset,2026-11-16)是试点,本手册
> 由该批沉淀:impl 怎么组织、错误怎么映射、e2e 怎么拆、前端怎么切、
> 并行纪律与已知踩坑。后续批次照此执行,发现新坑回写本文。
>
> 配套:`docs/plan-next-steps.md` 步骤 6(批次顺序与验收)、
> `docs/api-mapping.md`(路径/语义对照,逐条对照着迁)、
> `docs/architecture-v0.5.md` §三(装配分层)。

## 〇、并行纪律(先读这个)

四路并行批次(Providers / Users / Billing / Stats)+ User 批共用同一轨道。
**每个工人只写三类文件:**

1. `crates/aikun-admin/src/<自己的 impl 文件>.md`——即 `user.rs` /
   `admin_providers.rs` / `admin_users.rs` / `admin_billing.rs` /
   `admin_stats.rs` 中**指派给你的那一个**(文件头有指派注释);
2. `aikun/tests/e2e_<tag>.rs`——自己批次的独立集成测试目标
   (参照 `aikun/tests/e2e_auth.rs` 的写法);
3. 指派给你的前端 js(`aikun/src/frontend/js/*.js`,按 api-mapping
   表逐页切换)。

**禁止碰**:`main.rs`、`aikun-admin/src/lib.rs`、`aikun-admin/Cargo.toml`
(依赖已一次配齐)、`aikun/Cargo.toml`、`tests/e2e.rs`、其他工人的 impl
文件、存量旧 handler(`aikun/src/api/**`)、`openapi.yaml` 与
`crates/aikun-openapi`(spec 已定稿;疑似缺陷记 notes 报人工裁决)。
**装配(挂路由/包中间件)与删旧(旧路由 + 旧 handler + 旧 e2e)由集成
工人在批间统一做**——工人提交的 impl 只要 trait/固有方法签名正确,
集成工人一行挂载即可亮起来。

Admin tag 注意:路由工厂 `admin_api_router` 只收**一个** `AdminApi`
实现者,所以 Admin 的四个方法组写成**固有方法**(签名 = trait 方法去掉
`self` 之后的参数,返回同名 `*Response` 枚举),`lib.rs` 的
`AdminService` facade 一行委托到 `self.providers / self.users /
self.billing / self.stats`——委托已写好,不要改 lib.rs。User tag 独享
trait,直接在自己文件里 `impl UserApi for UserService`。

## 一、impl 文件组织(aikun-admin)

以 `auth.rs` 为参照:

```
crates/aikun-admin/src/auth.rs
├── 文件头注释:装配契约(哪些行为在中间件/装配层,为什么)
├── 过渡期端口 trait:pub trait AuthStore { 查库/验密/签发 JWT }
│     ↑ aikun-admin 不依赖 rusqlite/jsonwebtoken/argon2;
│       适配器(DbAuthStore)写在 main.rs,由集成工人装配;
│       aikun-store 就位后由 store 实现收编(阶段 3)
├── pub struct XxxService<S: XxxStore>:业务字段 + 端口 + 共享状态
│   (限流窗口等与 AppState 同源的 Arc 注入)
├── 业务方法(do_login 等):编排顺序、限流、防枚举时序均衡……全在这里
└── #[async_trait] impl XxxApi for XxxService:一行委托业务方法
```

要点:

* **签名以生成代码为准**:`crates/aikun-openapi/src/openapi/server/api.rs`
  是 trait 签名的唯一真相;请求/域类型在 `openapi/types.rs`,每 op 的
  响应枚举(`*Response`)在 `openapi/server/errors.rs`(注意两类类型
  分家,import 别搞混)。路径参数是 `String`(ID 已拍板不透明 string,
  api-mapping 备注 2)。
* **CPU 密集/持锁操作走 `spawn_blocking`**(参照 `do_login` 的验密):
  aikun-admin 的 tokio 只开了 `rt` feature,够用。
* 生成路由工厂:`auth_api_router` / `user_api_router` / `admin_api_router`
  (`openapi/server/router.rs`),传实现者即可,`with_state` 已在内部做好。

## 二、错误映射写法(problem+json)

所有非 2xx 用 `aikun_admin::problem(status, code, detail)` 构造
`Problem`(lib.rs 共享助手),塞进响应枚举的对应变体:

```rust
use aikun_admin::problem;
LoginServerResponse::Unauthorized(problem(401, "invalid_credentials", "Invalid username or password"))
```

* `code` 词表以 `types.rs` 的 `Problem` 文档注释为准(旧 `error` 值绝大多数
  原样成为 `code`,归一对照见 api-mapping 文末);
* 429 一律带 `Retry-After`(api-mapping #12;变体有专用 Headers 结构体,
  中间件自建响应时手动插头);
* **坑:响应枚举可能缺变体**。`LoginServerResponse` 没有 5xx 变体,旧实现
  的"存储不可用 → 500"只能 fail-closed 401 + `tracing::error!` 留痕
  (见踩坑 #2)。你那批的枚举缺什么变体,先记 notes 报裁决,别自创状态码。
* 请求体校验**不在 impl 里做**:生成器已挂 jsonschema 运行时校验
  (缺字段 422 / 坏 JSON 400,见踩坑 #3),impl 拿到的 body 一定过了 schema。

## 三、e2e 独立文件的写法(参照 `aikun/tests/e2e_auth.rs`)

```
aikun/tests/e2e_auth.rs           ← Auth 批(试点,已交付)
aikun/tests/e2e_user.rs           ← User 批
aikun/tests/e2e_admin_providers.rs ← Providers 批
…
```

* 文件头 `mod common;` 复用基建(`TestApp::spawn` 拉真实二进制、
  `admin_jwt()`、`seed_api_key` 等),与 e2e.rs 完全同一套;
* **核心手写行为的断言不减**(登录链路/计费/兑换/协议转换,plan 步骤 9):
  Auth 批把 e2e.rs 的登录链路用例整体迁入 e2e_auth.rs 并升级断言
  (problem+json 的 code、Retry-After、RFC3339 时间),e2e.rs 同步删除旧
  用例——**迁路径的用例跟着新 impl 走,不为保留而保留**;
* 断言生成器**实际**行为(422 request_validation_failed),并在注释里
  指出与 spec 示例的差异 + 指向本文踩坑条目——e2e 是行为锚点,锚实测;
* 每个 `#[tokio::test]` 独立 `TestApp::spawn()`(独立进程/端口/临时库,
  内存限流窗口互不污染);
* 你的批次的用例只测**新路径**;旧路径用例留在 e2e.rs 等集成工人随存量
  handler 删除。

## 四、前端切换模式(参照 `core.js` 的 login 段)

按 api-mapping 全局语义 #1/#3/#4/#5/#6/#7,逐页三件事:

1. **URL**:去 `/admin` 段、换资源化新路径(如 `/api/login` →
   `/api/auth/login`);
2. **响应解包**:列表 `{items, total}` + 分页;删除/创建/PATCH 的状态码
   (204/201/PATCH 回资源);
3. **错误处理**:problem+json 读 `code` / `detail`,不再读 `error` /
   `message`(`core.js` 的 `api()` 归一逻辑已兼容两类信封——两种都透传,
   仅空体兜底 `unknown_error`;401 踢回登录页的豁免路径同步换新)。

`grep -rn "/api/admin" aikun/src/frontend/` 归零是你的批次的验收项之一
(该页未迁完就不归零,只统计你指派的文件)。

## 五、装配契约(集成工人视角;工人了解即可)

试点批在 `main.rs` 落地的模式,后续批次照抄:

```rust
// Auth:裸挂(无鉴权中间件)
let auth_routes = auth_api_router(auth_service)
    .layer(middleware::from_fn_with_state(svc, aikun_admin::auth::login_ip_guard))
    .layer(middleware::from_fn(aikun_admin::auth::frontend_header_fixup));
// User:包 auth_middleware;Admin:包 auth_middleware + require_admin
// (architecture §三.3)
```

* 生成路由工厂返回**已出state**的 `Router<()>`,合入带状态的 app 用
  `.merge(xxx.with_state(()))`(踩坑 #5);
* 批次完成时删除对应旧路由与旧 handler,跑验收 grep:
  `grep -rn "api/admin" aikun/src/` 命中数随批次递减,最终批归零。

## 六、踩坑清单(实测记录,遇到先查这里)

1. **`/static/{path}` 生成路由只匹配单段路径**。matchit 0.8 的 `{path}`
   参数不含 `/`(实测:`/static/js/core.js` MISS,且与 `/static/{*path}`
   互斥不能并挂)。嵌套静态资产由应用级 fallback 调
   `AuthService::static_fallback` 兜底(aikun-admin/src/auth.rs +
   main.rs)。前端全部 JS 都是嵌套路径,没有 fallback 前端直接白屏。
2. **`LoginServerResponse` 无 5xx 变体**:旧实现"DB 锁中毒/token 签发失败
   → 500"无法表达。当前折衷:fail-closed 401 + `tracing::error!`
   (不泄漏内部状态,登录端点本就不该回显内部错误)。**spec 疑似缺陷,
   待裁决**:若拍板要 500,需 spec 加 5xx 响应后重生成。
3. **请求体校验状态码与 spec 示例不一致**:spec 的 login 400 示例写
   `invalid_body`,但生成器实测行为是——缺必填字段(过 JSON 解析、挂
   schema 校验)→ **422 `request_validation_failed`**;坏 JSON → 400
   `malformed_request`。e2e 按实测锚定(e2e_auth.rs::
   login_rejects_body_missing_fields)。**spec 疑似缺陷,待裁决**。
4. **trait 方法签名拿不到请求上下文**:`login(&self, body)` 没有
   headers/ConnectInfo,IP 维度限流与自定义响应头都进不了 impl。解法:
   装配层中间件(`login_ip_guard` 拿 ConnectInfo + XFF;`frontend_header_fixup`
   归一 CSP/no-cache/charset,顺带补 404 的纯文本 body)。需要请求上下文
   的语义先想中间件,别硬塞 impl。
5. **带 state 的子路由合入 app**:`auth_api_router()` 返回已出 state 的
   `Router<()>`,直接 `.merge()` 进 `Router<AppState>` 编译不过;要
   `.merge(auth_routes.with_state(()))`(axum 0.8 `with_state<S2>` 输出
   泛型可重定向)。
6. **响应枚举的 content-type 是定死的**:`ServeStaticAssetResponse::Ok`
   恒 `text/css`、`ServeFrontendResponse::Ok` 无 charset、NotFound 变体
   无 body——行为修正全部在 `frontend_header_fixup`。你的批次若需要
   自定义头,同样走中间件,别试图绕过枚举。
7. **过渡期 DB/加密依赖走端口 trait**:`aikun-admin` 的 Cargo.toml 没有
   rusqlite/jsonwebtoken/argon2,也不许加(依赖纯净度,architecture §三)。
   适配器写 `main.rs`(参照 `DbAuthStore`),集成工人装配;阶段 3 store
   收编时适配器整体删除。
8. **include_str! 跨 crate 指向二进制源码目录**:`aikun-admin/src/auth.rs`
   用 `../../../aikun/src/frontend/...` 内嵌前端资产。路径 fragile,
   前端目录挪动时要同步(阶段 4 前端治理时留意)。
9. **锁中毒语义**:限流窗口/DB 连接锁中毒时 `unwrap_or_else(|e| e.into_inner())`
   取回数据继续干(不 fail-open 也不 fail-closed),这是旧行为,随迁移保留。

## 七、每批验收清单(交给脚本门禁前自查)

- [ ] `cargo check --workspace` 绿(fmt/clippy/test 由 CI 门禁);
- [ ] 新路径 e2e 用例在 `e2e_<tag>.rs`,断言覆盖该批语义变化
      (problem+json code / 状态码 / 信封);
- [ ] 核心行为锚点用例数量与强度不减(登录链路/计费/兑换/协议转换);
- [ ] 前端指派文件 `grep /api/admin` 归零、无 `error`/`message` 读取
      (problem+json 切换完成);
- [ ] 对外行为变化单列 CHANGELOG(集成工人统一提交时落笔);
- [ ] 新踩坑回写本手册。
