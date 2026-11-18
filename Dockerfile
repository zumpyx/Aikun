# ---- 构建阶段 --------------------------------------------------------------
# 在构建机架构(BUILDPLATFORM)上运行,zigbuild 原生交叉编译出静态 musl 二进制,
# 多架构镜像不需要 QEMU 模拟。与 .github/workflows/release.yml 同一工具链(钉同一版本)。
FROM --platform=$BUILDPLATFORM ghcr.io/rust-cross/cargo-zigbuild:0.23.0 AS builder
WORKDIR /app
COPY . .

# 基础镜像只捆绑 Rust 1.93(zigbuild tag 不含 Rust 版本,0.23.0–0.24.1 全是),
# 而 surrealdb 4 的传递依赖 sysinfo 0.39 MSRV 1.95(整条 0.39 线,不可降级)
# ——用镜像内 rustup 换钉版工具链(与 CI/本地一致)。独立 RUN 层,
# 不随源码变动,buildx gha 缓存可跨构建复用。
RUN rustup toolchain install 1.100.0 --profile minimal && rustup default 1.100.0

ARG TARGETPLATFORM
RUN case "$TARGETPLATFORM" in \
      "linux/amd64")  T=x86_64-unknown-linux-musl ;; \
      "linux/arm64")  T=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported platform: $TARGETPLATFORM" >&2; exit 1 ;; \
    esac && \
    rustup target add "$T" && \
    cargo zigbuild --release --target "$T" -p aikun -p aikun-migrate && \
    mkdir -p /out && \
    cp "target/$T/release/aikun" /out/aikun && \
    cp "target/$T/release/aikun-migrate" /out/aikun-migrate

# ---- 运行阶段 --------------------------------------------------------------
# 二进制全静态(嵌入式 SurrealKV + rustls webpki-roots 证书),不依赖任何
# 系统库;alpine 仅提供 shell 便于 exec 排查。
FROM alpine:3.21
RUN adduser -D -u 10001 aikun \
    && mkdir -p /data \
    && chown aikun:aikun /data

COPY --from=builder --chown=aikun:aikun /out/aikun /usr/local/bin/aikun
# 存量迁移工具(SQLite → SurrealDB;源库由 AIKUN_DATABASE_URL 指定)
COPY --from=builder --chown=aikun:aikun /out/aikun-migrate /usr/local/bin/aikun-migrate

USER aikun
# 工作目录设为 /data:即使外部传入相对路径的 AIKUN_SURREAL_URL,
# 数据目录也会落在可写的 /data 卷中,而不是容器根目录(无写权限)。
WORKDIR /data
# 容器内必须监听 0.0.0.0 才能接受宿主机转发;嵌入式 SurrealKV 数据目录
# 落在 /data 卷中持久化(阶段 S 默认切换,docs/surreal-migration.md §二)。
ENV AIKUN_HOST=0.0.0.0:3000 \
    AIKUN_SURREAL_URL=surrealkv:///data/aikun.surreal
EXPOSE 3000
VOLUME ["/data"]

# 纯 docker run 场景也有健康检查(compose 里另有定义,以此为准其一即可);
# busybox wget 是 alpine 自带,无需额外安装。
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD wget -q -O /dev/null http://127.0.0.1:3000/api/health || exit 1

ENTRYPOINT ["aikun"]
