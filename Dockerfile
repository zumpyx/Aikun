# ---- 构建阶段 --------------------------------------------------------------
# 在构建机架构(BUILDPLATFORM)上运行,zigbuild 原生交叉编译出静态 musl 二进制,
# 多架构镜像不需要 QEMU 模拟。与 .github/workflows/release.yml 同一工具链。
FROM --platform=$BUILDPLATFORM ghcr.io/rust-cross/cargo-zigbuild:0.23.0 AS builder
WORKDIR /app
COPY . .

ARG TARGETPLATFORM
RUN case "$TARGETPLATFORM" in \
      "linux/amd64")  T=x86_64-unknown-linux-musl ;; \
      "linux/arm64")  T=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported platform: $TARGETPLATFORM" >&2; exit 1 ;; \
    esac && \
    rustup target add "$T" && \
    cargo zigbuild --release --target "$T" && \
    mkdir -p /out && \
    cp "target/$T/release/aikun" /out/aikun

# ---- 运行阶段 --------------------------------------------------------------
# 二进制全静态(rusqlite bundled SQLite + rustls webpki-roots 证书),
# 不依赖任何系统库;alpine 仅提供 shell 便于 exec 排查,总体积约 20MB。
FROM alpine:3.21
RUN adduser -D -u 10001 aikun \
    && mkdir -p /data \
    && chown aikun:aikun /data

COPY --from=builder --chown=aikun:aikun /out/aikun /usr/local/bin/aikun

USER aikun
# 容器内必须监听 0.0.0.0 才能接受宿主机转发;库文件落在 /data 卷中持久化。
ENV AIKUN_HOST=0.0.0.0:3000 \
    AIKUN_DATABASE_URL=sqlite:///data/aikun.db?mode=rwc
EXPOSE 3000
VOLUME ["/data"]

ENTRYPOINT ["aikun"]
