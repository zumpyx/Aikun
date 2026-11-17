#!/usr/bin/env bash
# =============================================================================
# gen-api.sh — openapi.yaml → crates/aikun-openapi 可复现生成管线
#
# 流程:
#   1) 钉版生成器:已装且版本精确一致则跳过,缺失/版本漂移则
#      cargo install openapi-to-rust --version 0.24.0 --locked;
#   2) openapi-to-rust generate(读仓库根 openapi-to-rust.toml,
#      原始输出进 target/openapi-to-rust 暂存,不进 git);
#   3) Proxy 后处理(2026-11-18 /v1 纳入生成):python3 对暂存
#      server/errors.rs 的 8 个 Proxy 响应枚举注入 Raw(Response)
#      透传变体(变体 + IntoResponse 首分支),并做代理面正向清单校验
#      (8 条 /v1 路由必须齐,缺失即拒绝组装);
#   4) wrapper 组装独立 crate:<output>/{Cargo.toml, src/lib.rs, src/openapi/**}:
#      - Cargo.toml 与 lib.rs 由本脚本 heredoc 模板产出;
#      - 依赖取自生成器 REQUIRED_DEPS.toml 的 [dependencies] 原文,外加
#        v0.24.0 清单漏列、服务端脚手实际引用的补充项(wrapper 按引用探测);
#   5) 删除生成器带出的任何测试文件(生成物零测试,不修不跑);
#   6) rustfmt --edition 2024 归一(与 cargo fmt 同一二进制与口径;
#      用 rustfmt 直调是为了 --output 指到 workspace 外时也能跑)。
#
# 纪律:输出目录删掉重跑必须逐字节一致;一切调整只改本脚本或
# openapi-to-rust.toml,不许手改 crates/aikun-openapi 下任何文件。
#
# 用法:
#   bash scripts/gen-api.sh [--output <dir>]
#     --output  覆盖输出目录(默认 crates/aikun-openapi)。
# 复现性校验示例:
#   bash scripts/gen-api.sh --output /tmp/aikun-openapi-check \
#     && diff -r /tmp/aikun-openapi-check crates/aikun-openapi
# =============================================================================
set -euo pipefail

GENERATOR_PACKAGE="openapi-to-rust"
GENERATOR_PINNED_VERSION="0.24.0"
SPEC_PATH="openapi.yaml"
# 生成器暂存目录(不进 git):spec 副本 + 注入全量 operations 的临时 config
# 落 target/openapi-to-rust-config/,原始输出在其 out/ 子目录
FULL_CONFIG_DIR="target/openapi-to-rust-config"
STAGING_DIR="$FULL_CONFIG_DIR/out"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_DIR="${REPO_ROOT}/crates/aikun-openapi"

usage() {
  sed -n '2,29p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output)
      [[ $# -ge 2 ]] || { echo "gen-api: --output 需要一个目录参数" >&2; exit 2; }
      OUTPUT_DIR="$2"; shift 2 ;;
    --output=*)
      OUTPUT_DIR="${1#--output=}"; shift ;;
    -h|--help)
      usage; exit 0 ;;
    *)
      echo "gen-api: 未知参数: $1(见 --help)" >&2; exit 2 ;;
  esac
done

# rm -rf 安全闸:输出目录必须是非根、非仓库本体的具体路径
if [[ -z "$OUTPUT_DIR" || "$OUTPUT_DIR" == "/" || "$OUTPUT_DIR" == "$REPO_ROOT" ]]; then
  echo "gen-api: 非法输出目录: '${OUTPUT_DIR}'" >&2
  exit 2
fi

command -v cargo >/dev/null 2>&1 || { echo "gen-api: 需要 cargo 在 PATH" >&2; exit 1; }
command -v rustfmt >/dev/null 2>&1 || { echo "gen-api: 需要 rustfmt 在 PATH" >&2; exit 1; }
export PATH="$HOME/.cargo/bin:$PATH"

# --- 1) 钉版生成器检查/安装 -----------------------------------------------
installed_version="$("$GENERATOR_PACKAGE" --version 2>/dev/null || true)"
if [[ "$installed_version" == "$GENERATOR_PACKAGE $GENERATOR_PINNED_VERSION" ]]; then
  echo "gen-api: $GENERATOR_PACKAGE $GENERATOR_PINNED_VERSION 已安装,跳过安装"
else
  if [[ -n "$installed_version" ]]; then
    echo "gen-api: 检测到 $installed_version,与钉版 $GENERATOR_PINNED_VERSION 不符,重装钉版" >&2
  else
    echo "gen-api: 未找到 $GENERATOR_PACKAGE,安装钉版 $GENERATOR_PINNED_VERSION"
  fi
  cargo install "$GENERATOR_PACKAGE" --version "$GENERATOR_PINNED_VERSION" --locked
fi

# --- 2) 生成(全量注入;config 内相对路径按 config 所在目录解析)------------
cd "$REPO_ROOT"
[[ -f "$SPEC_PATH" ]] || { echo "gen-api: 仓库根找不到 $SPEC_PATH" >&2; exit 1; }
rm -rf "$STAGING_DIR"
# 全量生成(2026-11-18,无手工白名单):生成器 0.24.0 的 [server].operations
# 必填且不支持通配 —— 从 spec 机械提取全部 operationId 注入临时 config。
# 临时目录 = target/openapi-to-rust-config/(不进 git),内放 spec 副本与
# config,相对路径字面量与仓库根一致(生成物文件头记录 "openapi.yaml",
# 换机器不泄漏绝对路径,重跑逐字节一致)。
FULL_DIR="$REPO_ROOT/target/openapi-to-rust-config"
mkdir -p "$FULL_DIR"
cp "$SPEC_PATH" "$FULL_DIR/openapi.yaml"
python3 - "$SPEC_PATH" openapi-to-rust.toml "$FULL_DIR/openapi-to-rust.toml" <<'PYEOF'
import re, sys

spec, base, out = sys.argv[1], sys.argv[2], sys.argv[3]
ops = re.findall(r"^\s*operationId:\s*(\S+)\s*$", open(spec).read(), re.M)
if not ops:
    print("gen-api: spec 里提取不到任何 operationId")
    sys.exit(1)
cfg = open(base).read()
if re.search(r"^operations\s*=", cfg, re.M):
    print("gen-api: openapi-to-rust.toml 不应再出现手工 operations 清单")
    sys.exit(1)
cfg = cfg.replace('output_dir = "target/openapi-to-rust"', 'output_dir = "out"')
lines = "\n".join('  "%s",' % o for o in ops)
cfg = cfg.rstrip("\n") + "\noperations = [\n%s\n]\n" % lines
open(out, "w").write(cfg)
print("gen-api: 全量注入 %d 个 operationId(注入顺序 = spec 文档顺序)" % len(ops))
PYEOF
"$GENERATOR_PACKAGE" generate -c "$FULL_DIR/openapi-to-rust.toml"

# 暂存产物完整性闸
for f in REQUIRED_DEPS.toml mod.rs types.rs server/mod.rs server/api.rs \
         server/errors.rs server/router.rs server/validation.rs; do
  [[ -f "$STAGING_DIR/$f" ]] || { echo "gen-api: 生成器输出缺少 $f" >&2; exit 1; }
done

# 代理面正向清单:8 条 /v1 路由必须齐(2026-11-18 Proxy 纳入生成)
for p in /v1/chat/completions /v1/messages /v1/responses /v1/models \
         /v1/embeddings /v1/moderations /v1/images/generations /v1/rerank; do
  if ! grep -qF "\"$p\"" "$STAGING_DIR/server/router.rs"; then
    echo "gen-api: 代理路由缺失:$p(白名单/spec 与预期不符),拒绝组装" >&2
    exit 1
  fi
done

# --- 3) Proxy 后处理:响应枚举注入 Raw(Response) 透传变体 ------------------
# 代理面响应由网关管线动态产出(SSE 流 / X-Request-Id 等响应头 / 厂商格式
# 错误体),生成的类型化枚举变体(Ok/OkStream:强制 Content-Type、丢响应头、
# Sse<Event流> 无法从已序列化的管线响应装配)装不下;为每个 Proxy 响应枚举
# 注入 Raw 变体,装配层 impl 只走 Raw。确定性文本处理(锚点缺失即失败),
# 重跑逐字节一致;生成器升级时需复核锚点。
python3 - "$STAGING_DIR/server/errors.rs" <<'PYEOF'
import sys

path = sys.argv[1]
# openapi-to-rust 0.24.0 的 operationId → 响应枚举名(驼峰规则)
ENUMS = [
    "CreateChatCompletionResponse",
    "CreateMessageResponse",
    "CreateResponseResponse",
    "ListModelsResponse",
    "CreateEmbeddingResponse",
    "CreateModerationResponse",
    "CreateImageResponse",
    "CreateRerankResponse",
]
VARIANT = (
    "    /// 网关透传变体(gen-api.sh wrapper 注入,勿手改):管线产出的\n"
    "    /// 完整 Response 原样返回(SSE/headers/错误体均由管线决定)。\n"
    "    Raw(::axum::response::Response),\n"
)
ARM = "            Self::Raw(response) => response,\n"

src = open(path).read()
for name in ENUMS:
    enum_anchor = "pub enum %s {\n" % name
    impl_anchor = "impl IntoResponse for %s {" % name
    if enum_anchor not in src or impl_anchor not in src:
        print("gen-api: errors.rs 缺少 Proxy 响应枚举锚点:%s" % name)
        sys.exit(1)
    src = src.replace(enum_anchor, enum_anchor + VARIANT, 1)
    # IntoResponse 块内首个 match self { 之后插透传分支
    i = src.index(impl_anchor)
    m = src.index("match self {", i)
    m_end = m + len("match self {\n")
    src = src[:m_end] + ARM + src[m_end:]
open(path, "w").write(src)
print("gen-api: 已为 %d 个 Proxy 响应枚举注入 Raw 变体" % len(ENUMS))
PYEOF

# --- 3) 组装 crate(先整目录清空,保证重跑逐字节一致)-----------------------
rm -rf "$OUTPUT_DIR"
mkdir -p "$OUTPUT_DIR/src/openapi"
cp "$STAGING_DIR/mod.rs" "$STAGING_DIR/types.rs" "$OUTPUT_DIR/src/openapi/"
cp -R "$STAGING_DIR/server" "$OUTPUT_DIR/src/openapi/server"

# 生成物零测试:生成器带出的测试文件直接删除(不修不跑)
find "$OUTPUT_DIR" -type d -name tests -prune -exec rm -rf {} +
find "$OUTPUT_DIR" -type f \( -name '*_test.rs' -o -name 'test_*.rs' \) -delete
rm -f "$OUTPUT_DIR/REQUIRED_DEPS.toml" "$OUTPUT_DIR/src/REQUIRED_DEPS.toml"

# 依赖 = REQUIRED_DEPS.toml 的 [dependencies] 原文(去空行)
deps_body="$(sed -n '/^\[dependencies\]$/,$p' "$STAGING_DIR/REQUIRED_DEPS.toml" \
             | tail -n +2 | sed -e '/^[[:space:]]*$/d')"
if [[ -z "$deps_body" ]]; then
  echo "gen-api: REQUIRED_DEPS.toml 里没有 [dependencies] 内容" >&2
  exit 1
fi
if grep -qE '^\[' <<<"$deps_body"; then
  echo "gen-api: REQUIRED_DEPS.toml 出现多张 TOML 表,wrapper 提取逻辑需更新" >&2
  exit 1
fi
# 防漏补齐:若生成代码引用了某依赖而 REQUIRED_DEPS.toml 未列出,按引用
# 探测追加(幂等:清单已含则不重复;当前 v0.24.0 的 43-op 清单是全的,
# 此段只为清单回退兜底)。
extra_deps=""
if grep -rqF -- '::futures_core::' "$STAGING_DIR" \
   && ! grep -qF 'futures-core' <<<"$deps_body"; then
  extra_deps+=$'futures-core = "0.3"\n'
fi
if grep -rqF -- '::serde_urlencoded::' "$STAGING_DIR" \
   && ! grep -qF 'serde_urlencoded' <<<"$deps_body"; then
  extra_deps+=$'serde_urlencoded = "0.7"\n'
fi

{
  cat <<'EOF'
# 由 scripts/gen-api.sh 生成 —— 勿手改;重跑脚本会整目录覆盖。
[package]
name = "aikun-openapi"
# 与 openapi.yaml info.version 对齐
version = "0.5.0"
edition = "2024"
description = "Aikun 管理面(Auth/User/Admin)API 脚手架,由 openapi-to-rust 从 openapi.yaml 生成(scripts/gen-api.sh)"
publish = false

# 依赖 = 生成器 REQUIRED_DEPS.toml 的 [dependencies] 原文
# + 防漏补齐:生成代码引用而清单未列的依赖(wrapper 按引用探测,幂等)
[dependencies]
EOF
  printf '%s\n' "$deps_body"
  printf '%s' "$extra_deps"
} > "$OUTPUT_DIR/Cargo.toml"

cat > "$OUTPUT_DIR/src/lib.rs" <<'EOF'
//! Aikun 管理面 API 脚手架 —— 本文件与整个 crate 由 scripts/gen-api.sh
//! 生成,勿手改;调整一律走生成器配置(openapi-to-rust.toml)或 wrapper。
//!
//! `openapi/` 是 openapi-to-rust 按 openapi.yaml 生成的服务端代码:
//! 管理面(Auth / User / Admin tag)与代理面(Proxy tag,/v1,2026-11-18
//! 纳入生成)。实现侧在装配层实现 `openapi::server::{AuthApi, UserApi,
//! AdminApi, ProxyApi}` trait,用 `openapi::server::{auth_api_router, …}`
//! 挂路由(装配与中间件分层见 docs/architecture-v0.5.md §三.3);
//! Proxy 响应枚举的 `Raw` 变体由 gen-api.sh wrapper 注入(代理面响应由
//! 管线动态产出,类型化变体装不下,impl 只走 Raw)。

// 生成代码触发的已知 clippy 告警(v0.24.0:嵌套 if let 校验块、
// double #[must_use])。crate 级 allow 由 wrapper 模板携带,
// 不允许散进任何生成文件。
#![allow(clippy::collapsible_if, clippy::double_must_use)]

pub mod openapi;

pub use openapi::*;
EOF

# --- 4) rustfmt 归一 --------------------------------------------------------
find "$OUTPUT_DIR/src" -type f -name '*.rs' -print0 | xargs -0 rustfmt --edition 2024

echo "gen-api: 已生成 $OUTPUT_DIR(生成器 $GENERATOR_PINNED_VERSION,全量生成 = spec 全部 operationId)"
