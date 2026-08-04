# Aikun

**轻量、稳定的 AI 网关**——多渠道聚合、故障转移、双协议接入,单二进制零依赖部署。

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL%20v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Release](https://img.shields.io/github/v/release/Zumpyx/aikun)](https://github.com/Zumpyx/aikun/releases)
[![Build](https://img.shields.io/github/actions/workflow/status/Zumpyx/aikun/release.yml)](https://github.com/Zumpyx/aikun/actions/workflows/release.yml)

```
客户端(OpenAI / Anthropic SDK)──▶ Aikun ──加权选路 / 故障转移──▶ 渠道 A(OpenAI 协议)
                                      │                       ──▶ 渠道 B(Anthropic 协议)
                                      └── 协议自动双向转换 ──────▶ 渠道 C(Socks5 代理)
```

## 特性

- **多协议接入**:同时暴露 OpenAI(`/v1/chat/completions`)、OpenAI Responses(`/v1/responses`,可用于 codex)与 Anthropic(`/v1/messages`)兼容端点。请求在网关内部自动转换为渠道的原生协议,双向转换覆盖流式、工具调用与图片
- **多渠道聚合**:同一模型可挂多个渠道,按延迟、优先级、权重、健康度加权选路;失败自动故障转移,连续失败或凭证失效(401/403)自动禁用渠道
- **渠道级代理**:每个渠道可单独配置 Socks5 / HTTP 代理,并可一键创建渠道副本,方便管理同渠道的多个账号
- **健康观测**:渠道健康检查 + 模型级测试矩阵(绿/红/灰小方格一目了然),每 30 分钟自动探测,也可手动单点测试或一键全测
- **轻量稳定**:Rust 编写,单二进制内嵌管理前端与 SQLite,无外部服务依赖;musl 静态编译,解压即跑
- **安全默认**:API Key 哈希存储(明文仅在创建时展示一次)、JWT 改密即失效、登录限流、CSP / CORS 白名单、渠道凭证脱敏回显

## 快速开始

### 1. 下载预编译二进制

从 [Releases](https://github.com/Zumpyx/aikun/releases) 下载对应平台的压缩包:

| 平台 | 产物 |
|---|---|
| Linux x86_64 / ARM64(静态 musl) | `aikun-*-linux-musl.tar.gz` |
| Windows x86_64 / ARM64 | `aikun-*-windows-gnullvm.zip` |
| macOS x86_64 / Apple Silicon | `aikun-*-apple-darwin.tar.gz` |

### 2. 运行

```bash
./aikun                          # 默认监听 127.0.0.1:3000
./aikun --host 0.0.0.0:3000      # 需要对外提供服务时
```

浏览器打开 `http://localhost:3000` 即为管理端。首次启动会创建管理员账号 `admin`,**随机密码只在启动日志中打印一次**,请立即登录并修改。注意:Docker 部署下 `docker logs aikun` 在容器存活期内可随时回捞该密码,改密后按需清理/滚动容器日志。

### 3. Docker 部署(可选)

镜像发布在 GHCR(`ghcr.io/zumpyx/aikun`),多架构(linux/amd64、linux/arm64),约 20MB:

```bash
# 方式一:docker compose(推荐)——先创建 .env 设置 AIKUN_JWT_SECRET,然后:
docker compose up -d
docker logs aikun   # 首次启动查看随机 admin 密码

# 方式二:纯 docker
docker run -d --name aikun --restart unless-stopped \
  --user 1000:1000 \
  -p 3001:3000 \
  -e AIKUN_JWT_SECRET=$(openssl rand -hex 32) \
  -v "$(pwd)"/data:/data \
  ghcr.io/zumpyx/aikun:latest
```

容器内配置通过 `AIKUN_*` 环境变量注入(见下方配置表),SQLite 数据库直接落在 `./data/aikun.db`(bind 挂载,备份即复制该文件),容器默认监听 `0.0.0.0:3000`,宿主机端口改 `docker-compose.yml` 端口映射左侧即可。`--user`/compose 的 `user:` 用于让容器以宿主机用户身份写入 `./data`,uid 非 1000 时请对应修改。本地构建镜像:`docker compose build`。

配置加载口径:**裸机启动自动加载工作目录的 `.env`;Docker 部署经 compose 的 `env_file: .env` 注入容器**(也可直接写在 compose 的 `environment` 里,同名项 `environment` 优先)。

### 4. 从源码构建(可选)

```bash
cargo build --release                                        # 本机构建
cargo zigbuild --release --target x86_64-unknown-linux-musl  # 静态交叉编译
```

## ⚠️ 重要:修改默认 JWT_SECRET

未设置 `AIKUN_JWT_SECRET` 时,服务会使用**公开的默认值**签发令牌,任何人都可以伪造管理员身份接管平台。**部署到任何可访问环境前,务必设置强随机密钥:**

```bash
export AIKUN_JWT_SECRET=$(openssl rand -hex 32)
```

密钥至少 32 字符,过短将拒绝启动。`--jwt-secret` 命令行传参会暴露在进程列表(`ps`)中,生产环境请用环境变量。

该密钥缺省时同时用于派生渠道上游 key 的静态加密密钥(AES-256-GCM,密文以
`enc:v1:` 前缀落库):回退模式下**部署后请勿更换**,否则已加密的渠道 key
无法解密,所有已签发 JWT 也会同时失效。设置独立的 `AIKUN_ENCRYPTION_KEY`
(见配置表)后加密密钥即与之解耦,可自由轮换 JWT secret。

忘记 admin 密码时,设置 `AIKUN_RESET_ADMIN_PASSWORD=<新密码>` 重启即可重置
(旧会话全部失效,密码不会打印到日志);用完后请立即移除该变量再重启。

## 配置

配置优先级:**`AIKUN_*` 环境变量 > 命令行参数 > 默认值**。参数支持 `--key value` 与 `--key=value` 两种写法,完整列表见 `./aikun --help`。

| 参数 | 环境变量 | 默认值 | 说明 |
|---|---|---|---|
| `--host` | `AIKUN_HOST` | `127.0.0.1:3000` | 监听地址 |
| `--jwt-secret` | `AIKUN_JWT_SECRET` | (公开默认值,**必须修改**) | JWT 签名密钥 |
| `--encryption-key` | `AIKUN_ENCRYPTION_KEY` | (空,派生自 JWT secret) | 渠道 key 静态加密密钥 |
| `--jwt-expires-in` | `AIKUN_JWT_EXPIRES_IN` | `604800` | JWT 有效期(秒) |
| `--database-url` | `AIKUN_DATABASE_URL` | `sqlite://aikun.db?mode=rwc` | 数据库 URL |
| `--health-check-interval` | `AIKUN_HEALTH_CHECK_INTERVAL` | `30` | 健康检查间隔(秒) |
| `--max-retries` | `AIKUN_MAX_RETRIES` | `3` | 单请求最大渠道尝试次数 |
| `--auto-disable-threshold` | `AIKUN_AUTO_DISABLE_THRESHOLD` | `5` | 连续失败自动禁用阈值 |
| `--request-timeout-secs` | `AIKUN_REQUEST_TIMEOUT_SECS` | `120` | 单次上游请求超时(秒) |
| `--log-retention-days` | `AIKUN_LOG_RETENTION_DAYS` | `30` | 请求日志保留天数 |
| `--cors-allowed-origins` | `AIKUN_CORS_ALLOWED_ORIGINS` | (空,仅同源) | CORS 白名单,逗号分隔 |
| `--trust-x-forwarded-for` | `AIKUN_TRUST_X_FORWARDED_FOR` | (关) | 置 `1` 信任 X-Forwarded-For,仅限可信反代之后 |

也可将 `AIKUN_*` 变量写入工作目录的 `.env` 文件(参考 [env.example](env.example)),启动时自动加载,效果等同于 shell 环境变量(shell 中已存在的同名变量优先,不会被 `.env` 覆盖)。`.env` 已被 `.gitignore` 排除,不会被提交。

日志级别由 `RUST_LOG`(tracing EnvFilter,无 `AIKUN_` 前缀)控制,默认 `aikun=info,tower_http=info`;排查时可设 `RUST_LOG=aikun=debug`。

## 使用

接入方式与官方 API 完全兼容,将 base_url 指向网关即可。

**OpenAI 协议**

```bash
curl http://localhost:3000/v1/chat/completions \
  -H "Authorization: Bearer sk-你的APIKey" \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-4", "messages": [{"role": "user", "content": "hi"}]}'
```

**Anthropic 协议**

```bash
curl http://localhost:3000/v1/messages \
  -H "x-api-key: sk-你的APIKey" \
  -H "anthropic-version: 2023-06-01" \
  -H "Content-Type: application/json" \
  -d '{"model": "claude-3-5-sonnet", "max_tokens": 1024, "messages": [{"role": "user", "content": "hi"}]}'
```

无论上游渠道是哪种协议,网关都会自动双向转换——客户端协议与渠道协议可自由组合。

**OpenAI Responses 协议**(codex 等客户端使用)

```bash
curl http://localhost:3000/v1/responses \
  -H "Authorization: Bearer sk-你的APIKey" \
  -H "Content-Type: application/json" \
  -d '{"model": "gpt-5", "input": "hi", "stream": true}'
```

渠道勾选 `Responses` 支持协议时按原生透传(复用该渠道的 OpenAI Base URL,保留 reasoning 等 responses 专有特性);未勾选时自动降级为协议转换——openai 渠道转成 `/chat/completions`,anthropic 渠道经组合转换承接。

**协议转换边界**:跨协议转换时部分对方协议特有的字段会被静默丢弃——OpenAI→Anthropic 丢 `response_format`/`logit_bias`/`n`/`seed`/两个 penalty,`max_tokens` 缺省 4096(长输出需显式设置);Anthropic→OpenAI 丢 `thinking`/`cache_control`/`top_k`/`metadata`;`reasoning_content`(推理流)双向均不转发;Responses→chat/completions 丢 `store`/`include`/`reasoning`/`previous_response_id` 与 `web_search` 等内置工具。客户端与渠道同协议时不存在此问题。

API Key 的 `expires_at` 建议传带时区的 RFC3339;无时区的输入(如 `2026-08-01 12:00`)按 **UTC** 解释。

管理端功能:渠道管理(一键获取模型列表、创建副本)、模型健康矩阵、请求测试(流式/非流式)、API Key 管理、日志统计、用户管理(仅管理员创建,支持批量)、计费(价格表、余额调账)。

## 计费

- 在「计费」页维护模型价格(元/1M tokens),模型名支持 `*` 前缀通配(如 `gpt-*`);精确命中优先,其次取最长通配前缀,单独的 `*` 为兜底价(匹配一切、优先级最低)
- 价格表为空时(全新部署)启动会自动导入一份内置默认价格:源自 [LiteLLM 价格表](https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json) 的 210 个主流 chat 模型官方刊例价(快照见 `src/default_prices.json`),按汇率 7.2 折算为元;另含一条 `*` 兜底条目(输入 3 / 输出 7 元),快照未收录的模型按此计费。它们是售价基准而非渠道成本价,导入后可在「计费」页修改;只要表中已有任意条目就不会再导入
- 成功的请求按用量折算费用记入 `request_logs.cost`,并从用户余额中同步扣除(日志与扣费同一事务);无任何价格匹配(兜底条目也被删除)的模型按 0 元记账
- 缓存 token 单独计价:每条价格可设可空的缓存价(`cached_price`,元/1M tokens),留空时缓存 token 按输入价计。OpenAI 的 `prompt_tokens` 含命中缓存部分,会按 `prompt_tokens_details.cached_tokens` 拆出;Anthropic 的 `cache_read_input_tokens` 按缓存价、`cache_creation_input_tokens`(写缓存)按输入价计。缓存用量记入 `request_logs.cached_tokens`
- 余额为用户级,**不足时不拦截请求**(允许为负),只记账;充值/扣减由管理员在「用户」页手工调账,流水在「计费」页可查;用户本人在「钱包」页查看余额与近 30 天消费分析(趋势图、模型消费分布)
- 请求明细按 `AIKUN_LOG_RETENTION_DAYS`(默认 30 天)保留;**清理前会按 (用户, 日) 聚合进 `usage_daily` 永久保留**,保留期过后余额仍可对账(Σ充值 − Σ消费)

## 数据与升级

- 所有数据存放在单个 SQLite 文件中(默认 `./aikun.db`),备份只需复制该文件
- 数据库迁移在启动时自动完成;**升级前建议先备份数据库文件**

## 发布

推送 `v*` 标签即可触发 GitHub Action,使用 `cargo zigbuild` 交叉编译六个目标、自动创建 Release,并构建多架构 Docker 镜像推送到 GHCR:

```bash
git tag v0.1.0
git push origin v0.1.0
```

## 许可证

**AGPLv3 + 商业双授权**(LICENSE 文件为 [GNU Affero General Public License v3](LICENSE) 全文):

- ✅ **开源使用免费**:可自由使用、修改、分发,但二次开发必须以 AGPLv3 开源并署名;通过网络提供服务同样触发开源义务(覆盖 SaaS 场景,最严格的 copyleft)
- 💼 **商业/闭源使用需单独授权**:如果贵司的商用场景无法履行 AGPLv3 的开源义务(例如闭源分发或私有化部署二次开发),请联系作者购买商业许可证——联系方式见 GitHub 主页
