# Aikun

**轻量、稳定的 AI 网关** — 多渠道聚合、故障转移、双协议接入,单二进制零依赖部署。

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL%20v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Release](https://img.shields.io/github/v/release/Zumpyx/aikun)](https://github.com/Zumpyx/aikun/releases)

## 特性

- **双协议接入**:同时暴露 OpenAI (`/v1/chat/completions`) 与 Anthropic (`/v1/messages`) 兼容端点,请求在网关内部自动转换为渠道的原生协议,双向转换覆盖流式、工具调用、图片
- **多渠道聚合**:同一模型可挂多个渠道,按延迟 / 优先级 / 权重 / 健康度加权选路;失败自动故障转移,连续失败或凭证失效自动禁用渠道
- **渠道级代理**:每个渠道可单独配置 Socks5/HTTP 代理
- **健康观测**:渠道健康检查 + 模型级测试矩阵(绿/红/灰小方格一目了然),每 30 分钟自动探测,也可手动单点/一键全测
- **轻量稳定**:Rust 编写,单二进制内嵌前端与 SQLite,无外部服务依赖;musl 静态编译,解压即跑
- **安全默认**:API Key 哈希存储(明文只展示一次)、JWT 改密即失效、登录限流、CSP/CORS 白名单、渠道凭证脱敏回显

## 快速开始

### 下载预编译二进制

从 [Releases](https://github.com/Zumpyx/aikun/releases) 下载对应平台的压缩包:

| 平台 | 目标 |
|---|---|
| Linux x86_64 / ARM64(静态 musl) | `*-linux-musl` |
| Windows x86_64 / ARM64 | `*-windows-gnullvm` |
| macOS x86_64 / Apple Silicon | `*-apple-darwin` |

解压后直接运行:

```bash
./aikun --host 0.0.0.0:3000
```

### 首次登录

首次启动会创建管理员账号 `admin`,**随机密码只在启动日志中打印一次**,请立即登录并修改。

### 从源码构建

```bash
cargo build --release          # 本机
cargo zigbuild --release --target x86_64-unknown-linux-musl   # 静态交叉编译
```

## ⚠️ 重要:修改默认 JWT_SECRET

未设置 `AIKUN_JWT_SECRET` 时,服务会使用**公开的默认值**签发令牌,任何人都可以伪造管理员身份接管平台。**部署到任何可访问环境前,务必设置强随机密钥:**

```bash
export AIKUN_JWT_SECRET=$(openssl rand -hex 32)
```

## 配置

配置优先级:**`AIKUN_*` 环境变量 > 命令行参数 > 默认值**。支持 `--key value` 与 `--key=value` 两种写法,`--help` 查看完整列表。

| 参数 | 环境变量 | 默认值 | 说明 |
|---|---|---|---|
| `--host` | `AIKUN_HOST` | `127.0.0.1:3000` | 监听地址 |
| `--jwt-secret` | `AIKUN_JWT_SECRET` | (公开默认值,**必须修改**) | JWT 签名密钥 |
| `--jwt-expires-in` | `AIKUN_JWT_EXPIRES_IN` | `604800` | JWT 有效期(秒) |
| `--database-url` | `AIKUN_DATABASE_URL` | `sqlite://aikun.db?mode=rwc` | 数据库 URL |
| `--health-check-interval` | `AIKUN_HEALTH_CHECK_INTERVAL` | `30` | 健康检查间隔(秒) |
| `--max-retries` | `AIKUN_MAX_RETRIES` | `3` | 单请求最大渠道尝试次数 |
| `--auto-disable-threshold` | `AIKUN_AUTO_DISABLE_THRESHOLD` | `5` | 连续失败自动禁用阈值 |
| `--request-timeout-secs` | `AIKUN_REQUEST_TIMEOUT_SECS` | `120` | 单次上游请求超时(秒) |
| `--log-retention-days` | `AIKUN_LOG_RETENTION_DAYS` | `30` | 请求日志保留天数 |
| `--cors-allowed-origins` | `AIKUN_CORS_ALLOWED_ORIGINS` | (空,仅同源) | CORS 白名单,逗号分隔 |
| `--trust-x-forwarded-for` | `AIKUN_TRUST_X_FORWARDED_FOR` | (关) | 置 `1` 信任 X-Forwarded-For,仅限可信反代之后 |

也可将 `AIKUN_*` 变量写入工作目录的 `.env` 文件(见 [env.example](env.example)),启动时自动加载——效果等同于 shell 环境变量(shell 中已存在的同名变量优先,不会被 `.env` 覆盖)。`.env` 已被 `.gitignore` 排除,不会被提交。

## 使用

接入方式与官方 API 完全兼容,将 base_url 指向网关即可:

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

管理端通过浏览器访问 `http://localhost:3000`:渠道管理(含一键获取模型列表、创建渠道副本)、模型健康矩阵、请求测试(流式/非流式)、API Key、日志统计、用户管理。

## 发布

推送 `v*` 标签即可触发 GitHub Action,用 `cargo zigbuild` 交叉编译六个目标并自动创建 Release:

```bash
git tag v0.1.0
git push origin v0.1.0
```

## 许可证

**AGPLv3 + 商业双授权**(LICENSE 文件为 [GNU Affero General Public License v3](LICENSE) 全文):

- ✅ **开源使用免费**:可自由使用、修改、分发,但二次开发必须以 AGPLv3 开源并署名;通过网络提供服务同样触发开源义务(连 SaaS 场景也覆盖,最严格的 copyleft)
- 💼 **商业/闭源使用需单独授权**:如果贵司的商用场景无法履行 AGPLv3 的开源义务(例如闭源分发或私有化部署二次开发),请联系作者购买商业许可证——联系方式见 GitHub 主页

这也是 MySQL、Qt、MinIO 等项目的成熟模式:开源社区完全自由,商业闭源需求转化为授权收益。
