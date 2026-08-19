# dbmon 컨테이너 이미지 (ECS Fargate / Graviton).
#
# 목표
#   - **arm64 (Graviton)** 기본. 같은 성능에 20% 저렴하다
#   - 런타임 이미지에 컴파일러·소스·`curl` 이 없다
#   - 빌더와 런타임의 **glibc 버전이 같다**(bookworm) → 문서가 경고한 glibc 스큐 함정 회피
#   - 비루트 실행
#
# 이미지는 **public.ecr.aws 미러**를 쓴다. Docker Hub 가 인증된 풀만 허용하도록 바뀌어
# (`Enforced sign-in`) `docker login` 없이는 `rust:...` 를 받을 수 없다.
#
# 빌드
#   docker build -t dbmon:dev .
#   docker build --platform linux/arm64 -t dbmon:dev .

# ─────────────────────────────────────────────────────────────────────────────
FROM public.ecr.aws/docker/library/rust:1-slim-bookworm AS builder

# `mysql_async` 의 rustls 백엔드가 aws-lc-rs 를 빌드하므로 cmake·clang 이 필요하다.
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config cmake clang libclang-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# 소스 전체를 넣고 캐시 마운트로 증분 빌드한다.
#
# `cargo-chef` 나 더미 소스 트릭을 쓰지 않는 이유: 워크스페이스 크레이트가 4개라
# 의존성 목록을 따로 관리하는 비용이 캐시 이득보다 크다. BuildKit 캐시 마운트가
# 레지스트리와 `target/` 을 유지해 주므로 재빌드는 변경된 크레이트만 컴파일한다.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p dbmon \
 && cp /src/target/release/dbmon /dbmon

# 링크 상태를 기록해 둔다. 런타임 이미지에 없는 라이브러리를 조기에 발견한다.
RUN ldd /dbmon | tee /dbmon.ldd

# ─────────────────────────────────────────────────────────────────────────────
FROM public.ecr.aws/docker/library/debian:bookworm-slim AS runtime

# `ca-certificates` 는 AWS SDK 의 TLS 검증에 필요하다.
# `tzdata` 는 리포트 시간대 계산(`report_timezone`, 기본 Asia/Seoul)에 필요하다.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tzdata \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin dbmon

COPY --from=builder /dbmon /usr/local/bin/dbmon

USER 10001:10001
EXPOSE 8080

# ECS 태스크 정의의 healthCheck 가 이걸 호출한다.
# **`curl` 을 넣지 않기 위해** 바이너리에 서브커맨드를 뒀다.
HEALTHCHECK --interval=15s --timeout=3s --start-period=10s --retries=3 \
  CMD ["/usr/local/bin/dbmon", "healthcheck"]

# `exec` 형식이라 PID 1 이 dbmon 이다 → SIGTERM 이 셸을 거치지 않고 바로 온다.
# 셸 형식(`CMD dbmon serve`)이면 셸이 PID 1 이 되어 시그널을 전달하지 않는다.
ENTRYPOINT ["/usr/local/bin/dbmon"]
CMD ["serve"]
