# sourcecraft-mcp в HTTP-режиме (serve --http).
#
#   docker build -t sourcecraft-mcp .
#   docker run --rm -p 127.0.0.1:8765:8765 sourcecraft-mcp
#
# Токен SourceCraft в образ не попадает: каждый агент передаёт свой в заголовке
# Authorization каждого запроса. Подробности — docs/http.md.
#
# Бинарник собирается в Alpine статически (musl, без libdbus и OpenSSL), как бинарники
# релизов. Итоговый образ — минимальный alpine: из пакетов только корневые сертификаты
# для rustls, а busybox wget нужен HEALTHCHECK.

ARG RUST_VERSION=1.99.1
ARG ALPINE_VERSION=3.22

FROM docker.io/library/rust:${RUST_VERSION}-alpine${ALPINE_VERSION} AS build

# musl-dev — libc; perl, make и cmake — сборка aws-lc-sys (криптография rustls).
RUN apk add --no-cache musl-dev perl make cmake

WORKDIR /src
# rust-toolchain.toml не копируется: тулчейн задаёт образ (RUST_VERSION).
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Цель musl по умолчанию линкуется статически (crt-static). Без символов бинарник
# заметно меньше.
ENV CARGO_PROFILE_RELEASE_STRIP=symbols \
    CARGO_TERM_COLOR=never
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p sourcecraft-mcp \
    && cp target/release/sourcecraft-mcp /usr/local/bin/sourcecraft-mcp
# Статическая сборка: у бинарника нет интерпретатора (PT_INTERP), ldd musl на нём
# завершается ошибкой. Динамический бинарник здесь — регрессия, сборка падает.
RUN if ldd /usr/local/bin/sourcecraft-mcp >/dev/null 2>&1; then \
        echo "sourcecraft-mcp is dynamically linked" >&2; exit 1; \
    fi

FROM docker.io/library/alpine:${ALPINE_VERSION}

# ca-certificates — rustls проверяет сертификаты SourceCraft API по системным корневым
# сертификатам. Других библиотек статическому бинарнику не нужно.
RUN apk add --no-cache ca-certificates \
    && adduser -D -H -u 10001 -s /sbin/nologin sourcecraft

COPY --from=build /usr/local/bin/sourcecraft-mcp /usr/local/bin/sourcecraft-mcp
RUN /usr/local/bin/sourcecraft-mcp --version

ENV SOURCECRAFT_MCP_LOG=warn

# Непривилегированный пользователь; файлов сервер не пишет, образ можно запускать read-only.
USER 10001:10001
EXPOSE 8765

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD wget -q -O /dev/null http://127.0.0.1:8765/health || exit 1

ENTRYPOINT ["/usr/local/bin/sourcecraft-mcp"]
# Внутри контейнера слушаем все интерфейсы; на хосте публикуйте порт только на loopback
# (-p 127.0.0.1:8765:8765) или за TLS reverse proxy.
CMD ["serve", "--http", "--listen", "0.0.0.0:8765"]
