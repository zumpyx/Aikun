# Changelog

本文件记录 Aikun 各版本的对外可见变化。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/),
版本号遵循 [SemVer](https://semver.org/lang/zh-CN/)。

约定(见 `docs/roadmap-v0.5.md`「纪律」):纯重构不改行为;任何行为变化必须单列条目。

## [Unreleased]
### 变更(ZCode 订阅通道:PAT 直连官方 BigModel 端点——GLM-5.3 大杯修复)

- **订阅档正确路径**(官方 zcode-builtin 模板 + 源码取证 + 活体验证
  GLM-5.3/Flash 双 200):个人 Coding Plan 的模型请求走
  `https://open.bigmodel.cn/api/anthropic`(标准 anthropic-messages,
  鉴权 = **PAT**),不是 zcode.z.ai 的 zcode-plan 专路径(订阅档在此
  一律 1005——那是免费桶通道)。
- **PAT 三步签发**(`aikun/src/zcode/pat.rs`,炼化自官方
  project-access-token-client):getCustomerInfo(**裸业务 token**,
  Bearer 会被误判)→ api_keys 列表/创建 `zcode-api-key` →
  `/{id}/access_tokens`(Bearer 业务 token,clientType=zcode)→
  短期 PAT(实测 ~5h);进程内缓存 + 每请求刷新语义。
- **转发策略(zcode 渠道)**:有业务 token → PAT 直连官方端点(订阅
  档,不排队不烧限额)→ 失败回退 off-peak 排队(免费桶)→ 再回退
  plan 端点保底;手动模型测试同序,周期后台诊断仍不碰取号限额。
- 实测:GLM-5.3(大杯)+ GLM-5.3-Flash 双车道 200「成功」;无业务
  token 的渠道行为不变(off-peak 优先)。
### 变更(ZCode:上游免费/订阅体系改版——off-peak 排队取号通道)

- **上游 2026-11-20 二次改版**:免费额度体系从「billing 桶 + zcode-plan
  端点」整体迁移到 **off-peak 排队取号制**(preview 已空、claim 通道
  下线;plan 端点对 Lite credits 一律 1005)。Aikun 接入新通道
  (`aikun/src/zcode/offpeak.rs`,活体验证全链 200):
  `POST /api/v1/off-peak/ticket`(task_id 须 UUID 形态;**每日取票
  限额**,用尽回 next_take_at)→ 轮询 status(实测 ~2.5 位/分钟,
  百人队 ≈ 40 分钟)→ ready(窗口 ~5 分钟)后调
  `/api/v1/off-peak/anthropic/v1/messages`——关键口径:
  **`x-coding-plan-api-key` 传 JWT 同值**(传 OAuth 业务 token 反而
  3001)+ `X-Off-Peak-Ticket-ID` + 常规伪装头族。票按渠道缓存、
  ready/active 窗口内复用、不主动 settle(保持窗口);过期丢弃重取。
- **转发策略**:zcode 渠道 chat 先走 off-peak(排队等待预算
  `AIKUN_ZCODE_OFFPEAK_WAIT_SECS`,默认 90s;0=不等待),不可恢复
  (限额/超时/票失效)自动回退 zcode-plan 端点保底(全量订阅用户
  仍有路)。
### 改进(反代详情:能力矩阵)

- **详情的模型区改为能力矩阵(zcode)**:订阅档 `GLM-5.3` 与
  `GLM-5.3-Flash`、闲时免费档 `GLM-5.3-Flash` 三条独立测试车道
  (免费档实时标注闲时窗口状态:窗口内「免费桶可用」/窗口外
  「此测试将走订阅额度」),点击即测、结果按车道内联隔离。
  语义:闲时网关优先消耗免费额度,免费桶并发超限自动落回订阅
  额度(上游按桶优先级自动保证,网关闲时 flash 对下游 0 元记账)。
### 变更(反代管理页:详情/编辑模型重构)

- **操作列只留【编辑、详情】**:领取/账号信息/测试/备注按钮收进
  详情窗口;「上次领取」列移除(proxy_task 数据与自动领取调度保留,
  仅不再展示)。
- **详情窗口**:条目信息(名称/类型/优先级权重/状态/端点)+ 账号
  信息(**持久化缓存**——新 `zcode_account` 侧表,GET
  /api/providers/{id}/zcode/account-info 读缓存零上游调用;POST 获取
  成功即刷新缓存,未获取时窗口内一键获取、已获取可反复重新获取;
  国内渠道带「领取闲时额度」)+ 可用模型(免费/订阅徽章,**点击
  即测**,结果内联显示)。
- **名称即备注**:编辑窗口只改名称,列表显示的就是它,可自由修改;
  自动生成的 zcode- 前缀名在获取账号信息时回填账号标识,手动改过
  后不再覆盖。原「备注」(note)编辑下线——note 收敛为内部归组
  标记([国内] 前缀),账号信息摘要不再写入 note。
### 改进(反代管理页)

- **行内「测试」改为逐模型菜单**:展开列出渠道配置的每个模型,
  zcode 渠道按档位标注徽章——名称含 flash = **免费**(闲时免费桶,
  免费 plan 的唯一能力),其余 = **订阅**(吃付费套餐/正常额度);
  点击对应模型即以该模型发起行内测试(此前是单按钮自动挑 flash,
  无法单独验证订阅档)。点击菜单外部自动收起。
### 修复(ZCode 上游协议)

- **上游 2026-11-20 收紧客户端校验导致全部 zcode 渠道不可用**:旧伪装
  版本 3.16.3 → 405+3012 "client not supported"(健康探测/模型测试
  表现为"上游响应不符合协议格式")。两处修复:① 伪装客户端版本升
  3.18.2(与 billing 家族统一为单一版本常量;实测 3.18.2 过、3.16.3
  拒);② system 身份文本换成 3.17.1 系口径(上游同日收紧了身份文本
  检查——旧 3.18.2 HAR 文本也被拒;新文本与 3 块 wire 形状炼化自
  TriDefender/zcode-api 的 bundle 逆向 + probe matrix:cliPrefix 裸块
  (3.15.1 起无 cache_control)+ stable 身份块 byte-exact(ephemeral)+
  dynamic 块 = 客户端 system 文本或空,ephemeral)+ 补 `x-zcode-trace-id`
  头(官方 bundle 头族成员)。新增 `normalize_body_wire_shape` 单测
  锚定 wire 形状。
- **模型测试错误文案透传上游信封 code/msg**:200+错误信封(配额
  1005 / 风控 3012 等)不再只报"上游响应不符合协议格式",带
  `上游 1005:exceed quota limit` 式明细——本次排查正是被泛化文案
  挡住了真实原因。
### 变更(数据库)

- **默认嵌入式 SurrealKV 改为仅快照模式(速度优先)**:默认端点
  `surrealkv://./data/data` → `surrealkv://./data/data?sync=60s`(SurrealDB
  4.0 引擎原生 `sync` 参数)。提交不再逐条 fsync,后台每 60 秒落盘一次;
  **正常停止(Ctrl-C/SIGTERM)在 HTTP 排空后显式关库**(flush WAL →
  close,15s 兜底),已提交写入全部落盘、零丢失;仅崩溃/断电可能丢
  最近一分钟内的写入(调用日志/用量明细级——渠道与反代凭证靠优雅
  关闭保全)。落盘节奏可调:`?sync=every`(全持久化,旧行为)/
  `?sync=never` / `?sync=5m` 等时长;`mem://` 纯内存与远程端点切换
  不受影响(sync 参数仅嵌入式有效,给远程端点会拒绝启动)。端点
  解析白名单新增 sync(非法值/非嵌入式直接拒绝启动);新增
  `snapshot_mode_shutdown_roundtrip` 单测证明"shutdown flush 后重开
  可见"。旧默认目录(`./data/aikun.surreal`/`./aikun.surreal`)沿用
  分支同样带上 `?sync=60s`。
### 新增(ZCode 订阅/额度/重置)

- **账号信息弹窗(订阅 + 5h/周额度 + 重置次数)**:反代页「账号信息」
  从 toast 升级为弹窗,聚合四路零消耗查询——billing/balance(免费
  额度桶,进度条)、coding-plan/reset/status(可用 5 小时/每周重置
  次数 + 最近使用,端点炼化自 ZCode 桌面客户端 3.18.2,要求双头认证
  `X-Bigmodel-Authorization` + `Bigmodel-Target-Type: PERSONAL`,缺任一
  上游 3001)、monitor 域订阅清单与用量额度(国内 bigmodel.cn / 国际
  api.z.ai 的 `/api/biz/subscription/list` 与
  `/api/monitor/usage/quota/limit`)。子查询失败降级 null + 说明,
  不阻断主响应。
- **「使用重置」按钮(消耗性)**:`POST /api/providers/{id}/zcode/
  reset-quota`(reset_type = FIVE_HOUR | WEEK)→ 上游 coding-plan/
  reset/use,服务端每次生成新幂等键防重放;成功后回读最新重置次数。
  仅由弹窗按钮 + confirm 触发,不自动化。
- **OAuth 业务 token 落库**:在线登录的 poll ready 响应携带家族
  业务 token(`data.bigmodel.access_token` / `data.zai.access_token`,
  官方客户端无此字段即判响应无效),加密存入新 `zcode_oauth` 侧表
  (每渠道一行;monitor 域订阅/5h·周额度查询凭证)。同一账号重复
  在线登录不再 409,而是定位既有渠道回填业务 token——旧渠道重新
  登录一次即可看到 Lite 订阅与 5h/周额度(批量粘贴 JWT 的渠道没有
  此凭证,弹窗中明确提示)。
### 修复

- **管理端白屏**:React 入口忘记包裹 `<AuthProvider>`——首屏
  `useAuth()` 即抛错,React 卸载整树,直接访问 `/` 只见空白页。
  修复 `frontend/src/main.tsx` 并重建 dist。e2e 只覆盖 HTTP 层
  (HTML/资产/头),渲染级问题不可见——本次经浏览器实测发现,
  前端改动此后必须过浏览器冒烟(登录页渲染 + 一次登录链路)。
### 新增(ZCode 渠道)

- **ZCode 渠道类型(`channel_kind = "zcode"`,GLM Coding Plan)**:接入
  zcode.z.ai 订阅通道——API Key 填 ZCode 客户端 JWT(同时进入
  `x-api-key` 与 `Authorization: Bearer`),仅勾选 Anthropic 协议,
  Base URL 填 `https://zcode.z.ai`;请求打到
  `/api/v1/zcode-plan/anthropic/v1/messages` 并携带 ZCode 客户端
  伪装头(anthropic-beta / User-Agent / X-ZCode-* 等)。健康检查走
  `/api/v1/zcode-plan/billing/balance`(**零消耗**,不打聊天端点烧额度);
  个人订阅额度与免费每日额度共用同一 JWT 凭证。协议细节炼化自社区
  D3-vin/Zcode2Api 与 zcode.z.ai 官方文档。
- **闲时免费计费(官方夜间畅用)**:北京时间 23:00–09:00 窗口内,
  zcode 渠道上名称含 "flash" 的模型(如 GLM-5.3-Flash)**对下游用户
  0 元记账**(tokens 照记、日志照落)——上游套餐额度消耗为 0 的官方
  规则传导到网关计费口径;窗口/模型判定由单测真值表锁定(按请求
  模型名判定,渠道模型映射不参与)。已裁剪:上游风控验证码不支持,
  触发时按上游失败故障转移。
- `ChannelAdapter` 新增 `extra_headers` 钩子(鉴权之外的附加头),
  transport(主/透传)与健康探测三处接线;前端渠道预设新增
  "ZCode(GLM)"。

## [Unreleased]
### 新增(渠道拆分:标准协议 + 反代)

- **管理视图拆分**:渠道页只列标准协议渠道(channel_kind 空/generic,
  `GET /api/providers?category=standard`);新增"反代"页(zcode /
  workbuddy / qoder 等订阅型,`category=proxy`)。同一张表、同一套
  选路计费,仅视图拆分;未来新适配器自动归入反代类。
- **批量操作**:`POST /api/providers/bulk`(enable/disable/delete,
  ids 1..=200,逐项报告;每项成功即失效拓扑快照)。反代页行多选 +
  批量工具条。
- **导入导出**:`GET /api/providers/export?category=`(⚠ **含明文
  api_key**,admin only,仅可信环境使用)/ `POST /api/providers/import`
  (整包或裸数组;同 key 已存在跳过,不覆盖)。
- **ZCode 在线登录**:`POST /api/providers/zcode/login-init` +
  `login-poll`(OAuth CLI init → 浏览器 authorize → 轮询;ready 即用
  Coding Plan JWT 自动建 zcode 渠道,name 默认授权邮箱)。OAuth base
  可配 `AIKUN_ZCODE_OAUTH_BASE`;captcha 不需要(origin 已 skip)。
  流程移植自 D3-vin/Zcode2Api。workbuddy/qoder 为占位协议,在线登录
  暂不支持(粘贴凭证)。

### 变更(管理端 UI)

- 导航改名:仪表盘 → **面板**、API Keys → **密钥**、接入文档 → **文档**。
- **兑换码独立页面移除**,管理功能(批量生成/筛选/禁用)并入钱包页,
  仅 admin 视角渲染(普通用户钱包页仍只有兑换充值);`#/redemption`
  路由删除。

### 变更(配置面)

- **`AIKUN_DATABASE_URL` / `--database-url` 恢复为数据库端点的正式
  配置**(用户裁决:数据库连接一个名字就够):非 `sqlite://` 值与
  `AIKUN_SURREAL_URL` 完全等价(surreal-url 优先),不再打"已废弃"
  警告;`sqlite://` 开头的旧值保持源库语义(迁移工具源 + 目录随迁)。
  迁移工具的 env 源库同样只接纳 `sqlite://` 值,防止新语义值被误当
  SQLite 打开。

### 变更(数据目录)

- **默认数据目录收进 `data/`**:嵌入式 SurrealKV 默认端点
  `surrealkv://./aikun.surreal` → `surrealkv://./data/aikun.surreal`
  (多级目录由引擎自动创建)。**旧部署兼容**:工作目录存在旧
  `./aikun.surreal/` 且 `./data/aikun.surreal` 不存在时自动沿用旧位置
  (info 日志提示),升级不落回空库;要迁移就把目录移动到 `data/` 下或
  显式设置 `AIKUN_SURREAL_URL`。docker 部署不受影响(容器内显式
  `/data/aikun.surreal`);迁移工具默认目标同步。

## [Unreleased]
### 变更(配置面重构:env 模板 + 默认库 ./data/data)

- **默认数据库端点** → `surrealkv://./data/data`(嵌入式 SurrealKV,
  进程内读写零网络往返=速度上限,数据本地持久);旧默认位置
  (data/aikun.surreal、aikun.surreal)自动沿用不落空库。`mem://`
  纯内存=最快但重启丢数据(模板注明适用场景)。
- **env.example 全量重构**:基础/数据库/上游请求/健康熔断/日志运维/
  响应缓存/ZCode 七组带注释模板;`.env`(本地实际值)gitignore。
- Dockerfile 默认端点同步 `surrealkv:///data/data`。

## [Unreleased]
### 修复(ZCode 模型调用分析 + 免费额度语义)

- **诊断结论**(2026-11-20 实测):GLM-5.3-Flash 在免费窗口内**可正常
  调用**(真实回复);GLM-5.3(大杯)429+3009 并发限制——免费计划
  entitlement 的 capabilities 只含 glm-5.3-flash,大杯需账号另有
  付费套餐(正常额度),免费账号必然 3009。
- **行内测试默认模型改为优先 flash**(原先取 models[0]=GLM-5.3,
  免费账号点测试必然失败)。
- **额度优先/并发回退语义确认**:上游按 bucket priority 自动先用
  免费额度;并发/超额(429 家族,含 3009)网关 classify=Retryable
  自动换渠道(其它渠道接住);200+code 错误信封按坏体转移,日志
  保留上游 code/msg。

## [Unreleased]
### 变更(渠道分级管理 + 测试内嵌)

- **ZCode 反代组**(用户设计):标准「渠道」页顶部新增组卡片——整体
  启用/禁用(批量)+ **组优先级**(平移组内账号 priority,保持组内
  相对顺序)→ ZCode 反代作为整体与其它标准渠道统一按优先级选路,
  零选路内核改动(沿用 provider priority 语义)。
- **反代页账号分级**:行内新增「编辑」(优先级/权重弹窗)与「测试」
  (真实对话,model-tests);账号间分级管理在反代页完成。
- **测试功能内嵌**:行内「测试」替代独立测试页;`/test` 路由与
  导航项删除。

## [Unreleased]
### 修复(ZCode 上游客户端校验破解:chat 3012 / claim 3007)

国内账号实测两道校验(2026-11-19,炼化自 Zcode2Api):
- **chat 405+3012 "client not supported"**:上游做**内容检查**——
  请求体必须带官方 3 个 system 身份块(23mac.har,恰 3 块,第 4 块
  3012)+ `metadata.user_id` 设备 JSON + string content 转 text block +
  末块 cache_control ephemeral。zcode 适配器 `prepare_request` 实现
  body 伪装(客户端 system 拼进官方末块);extra_headers 补会话族头
  (x-zcode-query-source/x-session-type/x-query-id/x-request-id)。
  实测:200 + 真实推理通过。
- **claim 3007 captcha verify failed**:领取需阿里验证码——嵌入
  Zcode2Api 的 node solver(happy-dom 跑设备指纹 VM,solver.js+
  package.json include_str! 进二进制,首次使用释放到
  `<data>/captcha_node/` 并 npm install,需 node ≥ 20;pe-stall 重试)。
  claim 带 X-Aliyun-Captcha-Verify-Param 头,3007 换 param 重试。
  实测:领取成功(ZCODE GLOBAL BUILD,3 亿 token,glm-5.3-flash,
  免费 23:00 窗口生效)。`AIKUN_ZCODE_CAPTCHA=skip` 为测试钩子。

## [Unreleased]
### 变更(领取闲时额度 = 国内专属)

- **「领取闲时额度」按钮只在国内 ZCode 页**(用户口径:免费额度只有
  国内账号有):位置移到「+ 批量添加」之后;行内「领取」按钮、
  「上次领取」列、说明文案一并移到国内页;国际页不再展示领取入口。
- **每日自动领取只跑国内渠道**(note 前缀 [国内] 的 zcode 渠道),
  国际渠道不再每天白跑。

## [Unreleased]
### 修复(国内渠道"无法使用":API 域 301)

- **根因**(2026-11-19 实测):`zcode.bigmodel.cn` 的全部 API 路径
  301 → bigmodel.cn 营销页——国内域没有部署 zcode-plan API。国内
  (BigModel 账号)登录建出的渠道 base 指向该域,聊天/领取/账号
  信息/健康检查全部失败。
- **修复**:国内渠道 API base 统一改回 `https://zcode.z.ai`(OAuth
  与 API 同源,账号体系差异只在登录侧);国内标记移到渠道 note
  前缀 `[国内]`(登录建渠道写入、账号信息回填保留、批量添加国内
  预设同),应用中心国内页按 note 前缀归组(base 不再区分)。

## [Unreleased]
### 变更(账号 = token 唯一必需 + 一键获取账号信息 + 手动备注)

用户裁决(2026-11-19):账号必须的部分只有 token;名称/账号 id 添加后
一键获取,备注可手动标记。
- **在线登录建渠道只依赖 token**:name 自动生成 `zcode-{6hex}`
  (不再依赖 email/phone——国内手机号账号 email 为空串曾致 400)。
- **一键获取账号信息**(`POST /api/providers/{id}/zcode/account-info`):
  token 查 billing/balance(零消耗),返回账号 id/标识/套餐,并回填
  ——名称仅当仍是自动生成前缀(手动命名不覆盖),note 写账号摘要。
  行操作新增「账号信息」按钮。
- **手动备注**:行操作「备注」按钮,弹窗编辑渠道 note。

## [Unreleased]
### 修复(在线登录国内账号 "name 不能为空" 400)

- 国内(智谱)账号多为手机号登录,OAuth ready 响应 user.email 常为
  **空串而非 null**(用户实测踩坑)——原兜底失效致建渠道 400。
  修复:授权账号标识按 email → phone → name/nickname 依次取第一个
  非空;name 三级兜底(显式 name → 账号标识 → zcode-user)。

## [Unreleased]
### 新增(在线登录支持国内 + 按钮改名)

- **国内(BigModel 账号)在线登录**(2026-11-19 实测:同一 OAuth 端点,
  `provider=bigmodel` → 授权页 bigmodel.cn/login;zai → chat.z.ai):
  login-init/login-poll 增加 `env`(intl|cn,缺省 intl);国内登录
  ready 后建渠道 base=zcode.bigmodel.cn(与账号体系同环境)。
- **按钮改名**:「ZCode 在线登录(国际)」→「**在线登录**」,
  国际/国内两个 ZCode 页都显示,env 随页自动判定。

## [Unreleased]
### 变更(ZCode 免费额度口径修正:窗口/额度/领取时刻/术语)

- **术语**:签到 → **领取免费额度**(按钮"领取"/"全部领取免费额度",
  列头"上次领取",说明文案同步)。
- **免费窗口**(2026-11-19 用户口径,`is_off_peak` 重写 + 真值表):
  工作日晚 23:00–次日 09:00(**3 亿** tokens);周末窗口周五 23:00
  连续至周一 09:00(**5 亿**,含周六/周日全天)。窗口内 Flash 对下游
  0 元记账口径不变。
- **自动领取时刻**:默认 `AIKUN_ZCODE_CLAIM_HOUR` 9 → **7**(工作日
  早 7 点领取当晚窗口额度,周五领取周末 5 亿;每天幂等执行)。

## [Unreleased]
### 新增(代理池与 SOCKS5 支持)

- **「代理」管理页**(侧边栏,admin):出网代理统一目录
  (socks5/socks5h/http/https;scheme 白名单校验),列表/添加/删除;
  URL 内嵌凭证在出参与页面打码(`socks5://u:***@host:port`)。
  `GET/POST /api/proxies` + `DELETE /api/proxies/{id}`。
- **渠道选用代理**:反代应用「批量添加」新增出网代理下拉(从池中
  选用,URL 拷贝进渠道 proxy_url——删池条目不影响存量渠道)。
  传输层 SOCKS5 本就支持(reqwest socks feature),此前只能手填 URL。

## [Unreleased]
### 变更(反代应用中心收敛)

- **移除 Z.AI Chat / 智谱清言占位页**(用户裁决:先只接 ZCode 的模型
  反代;两行分组收敛为 国际 ZCode/WorkBuddy/Qoder + 国内 ZCode,
  旧路由自动重定向到 ZCode)。ZCode 的 MCP 透传(/v1/mcp)已上线,
  无需改动,不在管理面展示。

## [Unreleased]
### 变更(反代应用中心:两行区域分组 + 领取端点跟渠道走)

- **应用 Tab 两行分组**(用户裁决):第一行国际(ZCode / Z.AI Chat /
  WorkBuddy / Qoder),第二行国内(ZCode / 智谱清言)。ZCode 拆成
  国际/国内两个独立应用页:列表按渠道 base 分域(JWT 形态两区域共用
  kind=zcode,base 含 bigmodel 即国内;zai-plan/bigmodel-plan 自带),
  批量添加预设各自只显示对应环境的两种。
- **领取(签到)端点改跟渠道自身 base**:国内渠道签到打
  zcode.bigmodel.cn 而非全局配置的国际端点(此前国内 JWT 签到必然
  401);`ZcodeClaimPort` 各方法增加 base 参数。

## [Unreleased]
### 新增(反代应用中心 + ZCode 国内外双环境)

- **反代页重构为应用中心**(`/proxies/:app`):每应用独立子页
  (ZCode / Z.AI Chat / WorkBuddy / Qoder),列表与导入导出按应用
  归组(channel_kind 多值过滤,`?kind=zcode,zai-plan,...`)。
- **ZCode 国内/国外双环境**(用户裁决"注意区分国内和国外的,都支持"):
  国际 = z.ai(ZCode 订阅 JWT `zcode.z.ai` / Coding Plan API Key
  `api.z.ai/api/anthropic`);国内 = 智谱 BigModel(ZCode 客户端 JWT
  `zcode.bigmodel.cn`(域名实测存在,端点族同构,零代码适配)/
  Coding Plan API Key `open.bigmodel.cn/api/anthropic`)。两侧凭证
  互不通用。新增 `zai-plan` / `bigmodel-plan` 适配器 kind(官方
  Anthropic 兼容端点,标准协议);批量添加四预设(环境 × 凭证形态)。
- **签到任务面板**:领取(签到)结果落 `proxy_task` 快照表
  (source = manual/oauth/auto),`GET /api/providers/zcode/tasks`
  查询;ZCode 应用页显示每渠道上次签到(来源/时间/明细悬浮)。
- **每日自动签到**:北京时间 `AIKUN_ZCODE_CLAIM_HOUR`(默认 9)点
  10 分对全部活跃 ZCode 订阅渠道串行领取(渠道间 2s);
  `AIKUN_ZCODE_AUTO_CLAIM=false` 可关。
- **渠道业务模块化**(用户要求):zcode 业务拆目录模块——
  channels `adapters/zcode/{mod,billing,mcp}`、admin
  `admin_providers/zcode.rs`、bin `src/zcode/{oauth,claim,scheduler}`;
  对外路径不变。新应用按此结构扩展。

### 变更(渠道拆分批次后续)

- ZCode 在线登录标注"国际"(国内 BigModel 账号暂用粘贴 JWT/API Key)。

## [Unreleased]
### 新增(ZCode 免费额度领取 + MCP 工具透传)

- **免费额度领取**(`POST /api/providers/{id}/zcode/claim`):官方
  免费/体验额度**不会自动到账**,需登录态 JWT 走领取序列——激活事件
  (app_launch/app_daily_active,上游按 device_mid+日期去重)→
  billing/balance 已持有清单 → billing/preview + marketing/touch 待领
  清单 → 逐项 billing/claim(1003=已领取视为成功)→ campaign 投放补
  touch/action confirm。反代页 zcode 行新增"领取额度"按钮,结果以
  领取/已持有/问题三分类反馈;**OAuth 在线登录建渠道成功后后台自动
  领取一遍**(fire-and-forget,失败可手动再领)。风控验证码(3007)
  不在网关侧破解,按问题报告。序列炼化自 Zcode2Api warm.go,经
  `ZcodeClaimPort` 端口注入(e2e 全序列 mock 验证)。
- **MCP 远程工具透传**(`GET /v1/mcp` + `POST /v1/mcp/{server_key}`):
  ZCode 订阅自带的远程 MCP 工具(官方插件 `.mcp.json` 揭示的
  `{base}/api/v1/mcp/server/{key}` 形态,JWT 同一登录态)可经网关
  调用——天眼查(`finance_tianyancha`)、图片搜索(`image_search`)等。
  MCP Streamable HTTP(JSON-RPC 2.0)字节级透传:按优先级自动选路
  zcode 渠道(传输错误/5xx/429 转移,4xx 协议错误原样透传),
  `Mcp-Session-Id`/`Mcp-Protocol-Version`/`Accept` 头转发,响应体
  (JSON 或 SSE)流式回传;0 元记账(日志照落,model 列 = server_key)。
  v1 仅 POST(initialize/tools/list/tools/call 均走 POST);文档页
  新增 MCP 调用示例。

## [0.7.0] - 2026-11-18

本版打包:roadmap 阶段 2(管线显式化)收尾 + 阶段 3 尾巴(熔断/计费幂等/
Limiter)+ /v1 代理面纳入 openapi-to-rust 生成(全量生成无白名单)+
管理端 React 重写。Gemini 等新协议与 /metrics 端点拍板不做(roadmap
阶段 4 待定项关闭)。

### 新增(阶段 3 尾巴)

- **渠道熔断**(9.5):连续计入健康的失败达到阈值(默认 3,`AIKUN_BREAKER_
  FAILURE_THRESHOLD`)后,冷却期内(默认 30s,`AIKUN_BREAKER_COOLDOWN_SECS`)
  选路优先跳过该渠道,到期恢复可选(半开);全部候选被熔断时回退为忽略
  熔断(不硬墙)。进程内状态,不取代自动禁用;429/客户端 4xx 不计入;
  管理端手动启停不受影响。
- **计费幂等**(9.3):用量行(total_tokens > 0 且带 request_id)以
  request_id 为键**只落一次**——重复落账(Guard 与正常收尾赛跑、崩溃
  恢复重放)不再双扣费/双计日额度。零用量失败行不受影响(同请求多次
  attempt 各留审计行)。
- **Limiter 组件**:AppState 四个限流 HashMap(登录失败/兑换失败/API key
  RPM/并发在途)合并为 `aikun-store::limiter` 统一组件(访问时清理 +
  删除时清理 + 锁中毒恢复)。行为收紧点仅锁中毒场景:RPM 窗口锁中毒
  原为跳过检查放行(fail-open),现为恢复数据继续限流。

### 变更(管理端 React 重写)

- 管理端前端从 vanilla JS(14 文件 3175 行)重写为 React(TS + Vite +
  Tailwind + shadcn/ui 风格组件 + TanStack Query + HashRouter)。页面与
  功能等价(登录/仪表盘/钱包/密钥/日志/文档/渠道/模型/测试/用户/价格/
  兑换码);providers 单建与批量两份重复表单合并;刷新/深链/后退可用
  (hash 路由)。构建产物 `frontend/dist` 提交进仓库并由 build.rs 嵌入,
  **单二进制零外部文件不变,Docker 构建无需 node**;CSP 维持仅同源。
- 二进制版本 0.5.0 → 0.7.0(`/api/system/version` 门禁语义不变,≥ 0.5)。

### 修复
- `aikun-migrate`:v3 早期库缺 `api_key_usage_daily` 表时中断且留半成品库——改为缺表按
  0 行迁移并 WARN(计数器可由明细重建,服务端启动本就重建当日值);其余表缺失仍报错。
  迁移演练:12 表行数对照全过、密文逐字节原样、幂等可重跑。

### 变更(/v1 代理面纳入 openapi-to-rust 生成,2026-11-18)

8 个 `/v1` 显式端点(chat/completions、messages、responses、models、embeddings、
moderations、images/generations、rerank)从手写薄壳改为 openapi-to-rust 生成
(装配层 `relay.rs` 实现生成 trait 并委托既有管线;鉴权、管线行为、SSE、
`X-Request-Id`、计费等全部不变)。请求侧为混合宽松 schema:核心字段
(model/stream/messages 等)类型化,未知字段经 `#[serde(flatten)]` 原样透传
(e2e 锚定顶层扩展字段到达上游)。同批:生成改为**全量模式**——spec 的全部
operationId(51 个)由 `gen-api.sh` 机械提取注入,`openapi-to-rust.toml` 不再
有手工 operations 清单,新增端点零配置遗漏。对外可见差异仅入口校验错误:

- **非法 JSON 请求**(此前 axum 默认 `text/plain` 400)→ 400 + OpenAI 厂商
  错误体 `{error:{message,type:"invalid_request_error",param,code}}`
  (`/v1/messages` 端点同为 OpenAI 形状;与管线自产错误体口径一致)。
- 缺 `content-type: application/json` → 415(厂商格式化 body,同上);
  body 超 2 MiB → 413(同上;axum 默认限额本就是 2 MiB,无实质变化)。
- 真畸形请求(声明字段类型不符,如 `messages` 非数组)→ 400 厂商格式
  (生成器 422 归一为 400;混合宽松 schema 下仅此类情形触发,缺 model 等
  业务校验仍在管线,错误体与状态码不变)。
- `x-aikun-cache` / `X-Request-Id` 等响应头与流式行为:`Raw` 透传变体
  原样返回管线 Response,零变化。

 - roadmap v0.5 阶段 2(管线显式化)

阶段 2 落地:`pipeline` 拆为 context / admission / dispatch(候选迭代)/
attempt(单次尝试)/ classify(失败分类)/ stream(流转发 / 记账收尾 /
错误帧)/ error(GatewayError)七个单元;通用端点透传(embeddings /
moderations / images / rerank)作为 `NoConversion + OpenAiOnly + NoStream`
模式并入同一管线,删除 `passthrough.rs` 的重复实现。对外行为变化逐条:

### 变更(可观测性,9.6)

- **新增 `X-Request-Id` 响应头**:所有 `/v1/*` 出口(成功 / 错误 / SSE 流)
  返回网关入口生成的 UUID v4;同值落 `request_logs.request_id`(见下方
  schema)并进入每请求 tracing span(`proxy_request`:request_id、
  protocol、model、channel、cache_hit、ttft_ms、total_ms、status)。
  一次多渠道重试可按 request_id + 日志拼出完整链。
- `request_logs` 表(surreal schema v1 内追加字段,幂等)新增
  `request_id option<string>`;旧行为空。管理面日志 API 未暴露该字段
  (openapi.yaml 冻结),读侧经 `aikun-store` 的 `LogRow.request_id` 可取。

### 变更(超时,9.2)

- 新增三个独立配置(环境变量 > CLI > 默认;非法值**拒绝启动**):
  `AIKUN_CONNECT_TIMEOUT_SECS`(TCP/TLS 连接超时,默认 15,原 15s 硬编码)、
  `AIKUN_TTFB_TIMEOUT_SECS`(流式等上游响应头的首字节超时,默认 60,
  原为渠道超时 clamp 1..60)、`AIKUN_IDLE_TIMEOUT_SECS`(流式 chunk 间
  空闲超时,0=沿用渠道级 timeout_secs 的旧行为,>0 全局覆盖,clamp
  1..600)。
- 客户端断开时上游请求立即中止(生成器 Drop 关闭上游连接),已捕获
  usage 照常按 499 补账且不重复记账(原行为,现由 ctx 生命周期与
  StreamAbortGuard 结构化保证)。

### 变更(启动校验,9.9;**行为收紧**)

- **非法配置由"warn 后回退默认值"改为拒绝启动**(`panic`):全部数值型
  `AIKUN_*` 配置(超时 / 重试 / 阈值 / 保留期等)与枚举型
  `AIKUN_CACHE_MODE` / `AIKUN_CACHE_SCOPE` / `AIKUN_CACHE_HIT_BILLING`。
  此前如 `AIKUN_CACHE_MODE=alway` 会回退 off 继续跑,现在直接退出并
  指明合法取值。合法配置行为不变。

### 变更(响应缓存,9.7;**键语义收紧**)

- 缓存键(`scope=user`)从按 user_id 隔离收紧为**带 API key 调用时按
  API key 隔离**(JWT 会话仍按 user_id):同一用户的不同 key 不再共享
  缓存;缓存键版本 v1→v2,升级后旧缓存条目自然失效(进程内缓存,
  重启即空,无迁移影响)。
- 带 `tools` 或多模态图片内容(`image_url` / `image`+`source` /
  `input_image`)的请求默认不入缓存(此前 deterministic/always 模式下
  会缓存)。
- `AIKUN_CACHE_SCOPE=global` 保留,文档(env.example)明确标注串缓存
  风险:仅单租户部署使用。

### 变更(凭证脱敏,9.8;**收紧**)

- 上游 API key 与代理 userinfo 在全部出口脱敏:上游错误消息进入
  request_logs / 客户端错误体 / 服务端 warn 日志前统一替换为 `***`
  (此前仅 401/403 的日志文案掩码,其余状态的上游消息原文透传——
  含 5xx 消息里回显渠道 key 的可能);reqwest 传输错误的 URL(查询串
  可能含 `key=…`)剥离后才进日志与错误帧。
- 透传端点的上游错误体此前**原文**回传客户端,现仅做凭证脱敏、形状
  不变(字节级语义保留)。

### 变更(透传并入管线)

- `/v1/{embeddings,moderations,images/generations,rerank}` 全部行为与
  chat 同源:准入(余额 402 / 限额 429 / 模型白名单 403)、故障转移、
  记账、request_id、X-Request-Id、整体重试 deadline、三类超时、脱敏。
  新增差异:透传请求此前无整体重试 deadline 与 request_id;缺 model 的
  请求日志 model 字段由空串改为 `unknown`(与 chat 统一);chat 端点
  对 `model:""` 此前会走到 503,现与透传统一为 400。
- 透传请求的错误响应仍原样回传上游错误体(不转 OpenAI 错误信封),
  但 401/403 穷尽后同样掩码为 502 通用文案(与 chat 一致,原行为)。

### 不变(红线复核)

- 计费扣费、兑换码核销、协议转换、登录链路行为不变(定向 e2e 复跑
  全绿:计费 / 兑换 / 协议转换 / 登录 / 透传 / 故障转移 / 缓存)。
- 9.4 硬边界单测锁定:未向客户端发出任何字节才允许换渠道;流式移交后
  不存在再次选路的代码路径(候选迭代在 200+SSE 头移交后即返回)。

## [0.6.0] - 2026-11-17

阶段 S(数据库迁移 SurrealDB,设计全文见 `docs/surreal-migration.md`)全部
落地:默认**嵌入式 SurrealKV**,主路径移除 SQLite,存量数据经迁移工具
`aikun-migrate` 一次性搬运。`/v1` 代理面与管理面 API(openapi.yaml)对外
契约不变;变化集中在存储、配置与部署面(逐条列下)。

### 变更(数据库:SQLite → SurrealDB,阶段 S)

- **存储引擎更换**:SQLite → SurrealDB(钉版 `surrealdb = 4.0.1`,
  BSL 1.1 宽松变体)。默认嵌入式 SurrealKV(数据目录 `./aikun.surreal/`,
  单二进制零外部依赖不变);远程 `ws://` `wss://` `http://` `https://`
  端点亦可用(ns/db 默认 `aikun`/`aikun`,URL 参数可覆盖)——是否多实例
  从架构决策降为部署决策。schema 重设计为 schemafull 12 表(原生
  `datetime` / `bool` / `array` / record 链接);版本闸门由
  `PRAGMA user_version` 改为 `store_meta:schema.version`,库版本比二进制
  新仍拒绝启动(降级保护语义不变)。
- **新增 `--surreal-url` / `AIKUN_SURREAL_URL`**(优先级 env > CLI >
  默认,同其余配置):默认 `surrealkv://./aikun.surreal`;非法端点(未知
  scheme / 空目录 / user 不成对 / 未知参数)**拒绝启动**。
  `AIKUN_DATABASE_URL` / `--database-url` **废弃**(显式设置时启动告警
  一次),此后仅迁移工具读取;`AIKUN_SURREAL_URL` 未显式设置而旧库指向
  非默认位置时,嵌入式目录自动随迁旧库同目录(docker 卷挂载部署的
  数据位置不变)。
- **迁移工具 `aikun-migrate`**(Docker 镜像内置):读旧 SQLite 库
  (**只读**,绝不写源库;源库 `user_version` 必须恰为 3,更低提示先用
  旧版二进制升满、更高拒绝)→ 按设计文档 §五字段映射全量写入目标库
  (目标先过 `ensure_schema` 版本闸门)。微元金额原样直搬、渠道 key
  密文免重加密、旧 uuid 沿用作 record id(管理面展示的旧 ID 不变);
  **幂等可重跑**(按 record id UPSERT 全字段覆盖),完成后打印各表
  行数对照,全部一致才报成功。用法见 README「数据与升级」。
- **备份/恢复故事变化**:嵌入式部署由"复制单个 aikun.db"改为**停机后
  整目录拷贝**(`./aikun.surreal/`),或 `surreal export`;远程部署用
  `surreal export` / `surreal import`。
- **store 能力 trait async 化**:仓储层能力 trait 由同步签名改为
  `#[async_trait]`(身份 / 渠道 / 计费三域,`Arc<dyn …>` 装配);影响面
  为 crate 内部签名与二进制装配,**对外 API 行为无变化**。红线逐条保持:
  计费扣费 / 兑换码核销仍是单次 `BEGIN…COMMIT` 原子事务,渠道 key 仍
  AES-256-GCM 密文落库,金额仍整数微元。
- **主路径移除 SQLite**:主二进制与 `aikun-store` 不再依赖 rusqlite
  (源码 grep 实测 0 引用);SQLite 读能力收口进迁移工具 crate
  `aikun-migrate`。
- **运行时栈配置**:嵌入式引擎的查询在调用任务栈上内联递归,主程序
  显式构建 tokio 多线程运行时并设 `thread_stack_size = 10MiB`(替代
  `#[tokio::main]` 默认 2MiB,防深查询栈溢出)。
- e2e 检查通道全部改经管理面 API(seed / 断言不再直连数据库文件,经
  `AIKUN_RESET_ADMIN_PASSWORD` 注入已知密码登录);store 单测以 `mem://`
  进程内实例直连断言库内行为(微元口径 / 事务原子性)。e2e 静态统计
  仍为 65 个 `#[tokio::test]`,行为锚点(登录链路 / 计费 / 兑换)断言
  不减。

### 性能(阶段 S bench,2026-11-17)

`scripts/bench.js`(mock 上游零延迟,非流式 `/v1/chat/completions`,
含完整计费链路),12 核 Apple M6 / node v26.13.1:

| 并发 | 请求数 | QPS | P50(ms) | P99(ms) |
|---|---|---|---|---|
| 10 | 1000 | 609 | 13.5 | 55.1 |
| 50 | 5000 | 576 | 78.9 | 202.0 |
| 100 | 10000 | 569 | 152.8 | 489.7 |
| 200 | 20000 | 539 | 329.1 | 1033.6 |

历史 SQLite 基线(≈2400 QPS @ 50 并发)出自不同机器(10 核
i9-12900H),**非同机对照**,不据此判定优劣;如实记录,同机对照待补
(详见 `docs/plan-next-steps.md` 阶段 S 验收核对)。

## [0.5.0] - 2026-11-17

`refactor/modular-gateway` 分支的还债版:roadmap-v0.5 阶段 0、阶段 1 与阶段 3 主体
(store 层与热路径归位)落地;architecture-v0.5 结构迁移(plan 步骤 1–8)全部完成——
多 crate workspace(根虚拟清单 + 二进制 `aikun/` + `crates/{aikun-openapi,
aikun-protocol,aikun-channels,aikun-admin,aikun-store}`)与 API spec / 生成管线;
**管理面 API spec-first 全量重建(对外行为变化,见下节)**。二进制版本 0.4.4 → 0.5.0,
版本门禁可用(`GET /api/system/version` 主次版本 ≥ 0.5,api-mapping 备注 11)。
阶段 2(pipeline 显式化)与阶段 4(发布整理)未开始。总结见
`docs/refactor-0.5-summary.md`。

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
  / `login` 四 op 仅退 401(login 存储不可用分支为 `invalid_credentials`,防枚举口径;其余三 op 为 `internal_error`)—— 前端对 401 会清
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
  目录端点 `GET /api/channel-kinds`(0.5.0 新路径;阶段 0 引入时为
  `GET /api/admin/channel-kinds`,随管理面迁移去 `/admin` 段)。
  空值 = generic,与旧行为完全一致。不是运行时插件系统。
- **响应缓存**:进程内内存缓存,默认 `AIKUN_CACHE_MODE=off`;可选 `deterministic` / `always`,
  按用户隔离,命中默认照常计费,响应头 `x-aikun-cache`。

### 工程(结构迁移:多 crate workspace 与生成管线,plan 步骤 2–8;无运行时行为变化)

- **多 crate workspace 完整化**(步骤 3):根 `Cargo.toml` 为虚拟清单
  (`members = ["aikun", "crates/*"]`,`default-members = ["aikun"]`,
  `resolver = "3"`),二进制移入 `aikun/`(包名与 `target/<T>/release/aikun`
  产物路径不变),`src/` `tests/` 随迁;根目录裸 `cargo build / test` 行为
  与迁移前一致。
- **`crates/aikun-protocol`**(步骤 4):`src/protocol/` 16 文件纯移动,
  deps 收敛为 `serde_json`(仅此);契约测试雏形建在最终位置
  `crates/aikun-protocol/tests/fixtures/`(非流式 OpenAI ↔ Anthropic 双向
  黄金样本 + OpenAI 官方 spec 快照参照;流式样本留阶段 2)。
- **`crates/aikun-channels`**(步骤 5):`src/channel/adapter/` 纯移动;
  `apply_auth` 改 `auth_headers` 纯数据签名;deps:`aikun-protocol` +
  `serde_json`,无 reqwest / rusqlite / tokio。
- **`crates/aikun-admin`**(步骤 6):管理面业务实现的家(impl 生成 trait),
  上述行为变化全部落在这里;装配在二进制 `main.rs`(Auth 裸挂、User 包
  `auth_middleware`、Admin 包 `auth_middleware + require_admin`)。
- **`crates/aikun-store`**(步骤 8):见下节"阶段 3"。
- **`openapi.yaml`**:仓库根手写 OpenAPI 3.2.1 全景 spec(单一真相):
  管理面(Auth / User / Admin 三层 tag)进生成白名单,`/v1` 代理面仅作文档
  (loose schema,不镜像厂商契约),`/api/health` 不入。统一口径
  (`{items,total}` 信封、创建 201 / 删除 204、PATCH 返回资源、RFC 9457
  problem+json 错误体、ID 不透明 string)与 0.4.4 实现的逐条对照见
  `docs/api-mapping.md`(含"全局语义变化"表 12 条)。
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
  `cargo test --locked --workspace`;新增 **regenerate 守卫 job**
  (钉版生成器重生成 + `git diff --exit-code`,生成物漂移即失败)。

### 工程(阶段 3,store 层与热路径归位;对外契约不变)

- **`crates/aikun-store` 落位**(plan 步骤 8):吸收二进制的连接池 / schema
  迁移 / 行映射 / 静态加密 / 口令哈希 / 默认价格播种(db 层不再反向调用
  billing),按**能力 trait** 暴露,签名零 SQL 方言(无 rusqlite 类型 /
  `?N` / `ON CONFLICT` / `PRAGMA`,为 SurrealDB 第二实现留缝)。
  `SCHEMA_VERSION` 仍为 3(新表幂等 `CREATE TABLE IF NOT EXISTS`);
  计费记账事务本体(`billing/ledger.rs`)仍留二进制(红线:计费事务不改)。
- **管理面 DB 访问收口 `spawn_blocking`**(Argon2 验密与 SQL 不占 async
  executor);`aikun/src/api/` 下 `conn.lock()` 清零。
- **渠道拓扑快照**(选路热路径,roadmap 决策"拓扑走缓存、事实走 DB"):
  启用渠道行 + 预解析模型清单建内存快照,选路零全表扫描、零逐行解密、
  零重复 JSON parse,命中渠道后解一次密;失效检测走表令牌 + 世代计数,
  EMA 镜像与自动禁用同步进快照。
- **价格缓存世代失效**:计费热路径查价先比对 (池, 世代, 表令牌),一致走
  内存通配匹配(免两次通配 SQL);管理端写价格推进世代,兜底同秒
  `updated_at` 撞值。
- **日额度计数器 `api_key_usage_daily`**:与记账同事务维护的日用量计数器,
  准入的日额度检查从 `SUM(request_logs)` 改为计数器点查;e2e 锚定计数器
  与日志明细合计一致。
- e2e 新增 `aikun/tests/e2e_store.rs` 3 项:管理端渠道 PATCH 对下一次请求
  生效、管理端改价对下一次计费生效、计数器与日志合计一致。

### 测试 / 工程

- e2e:登录链路用例(错误密码、改密踢下线、禁用账户;阶段 0 补于旧路径
  `/api/login` → `/api/me`,步骤 6 随新路径改写为 `/api/auth/login` →
  `/api/users/me`)。
- 回归测试:上述 ①②③ 各一。
- e2e 布局:随二进制移入 `aikun/tests/` 并按 tag 拆分(见上节管理面条目);
  静态统计 65 个 `#[tokio::test]`,全绿以 CI 门禁为准。
- rustfmt 全库格式化(51 文件)与 clippy 告警清零(32 条,自动修复);
  CI 新增独立 `lint` job:`cargo fmt --check` + `cargo clippy --all-targets -- -D warnings`。
- 依赖钉版以支持 rustc 1.85(`idna_adapter 1.2.0`、`time 0.3.41`、`simple_asn1 0.6.3`)。

### 文档

- `docs/refactor-plan.md` 补充最佳实践(协议契约测试、流式取消与超时、计费幂等与对账、故障转移可重试边界、渠道熔断、可观测性前置、响应缓存约束),并写入阶段 1–4 任务。
- 新增 `docs/architecture-v0.5.md`(0.5 目标结构定稿:多 crate workspace、管理面 API spec-first)与 `docs/api-mapping.md`(现有 API → 新 spec 逐条对照,前端与 impl 迁移清单);
  `roadmap-v0.5.md` 标注"目标架构"一节由前者取代。
- 新增 `docs/plan-next-steps.md`(结构迁移后续步骤,按依赖排序、每步带验收标准)。
- 新增 `docs/refactor-0.5-summary.md`(0.5 重构收尾总结:结构前后对照、crate 清单、遗留项)。

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

[0.6.0]: https://github.com/zumpyx/Aikun/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/zumpyx/Aikun/compare/v0.4.4...v0.5.0
[0.4.4]: https://github.com/zumpyx/Aikun/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/zumpyx/Aikun/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/zumpyx/Aikun/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/zumpyx/Aikun/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/zumpyx/Aikun/compare/v0.3.4...v0.4.0
