# syntax=docker/dockerfile:1
FROM --platform=$TARGETPLATFORM rust:1.99-alpine AS build
WORKDIR /src
RUN apk add --no-cache musl-dev
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release

FROM gcr.io/distroless/static-debian13:nonroot@sha256:e2e927ec666bae08560abb3c55d0659eceabb657f56b6782ab500a9fc7f555e3
COPY --from=build /src/target/release/llm-router /llm-router
COPY config.example.json /etc/llm-router/config.json
USER nonroot:nonroot
EXPOSE 8787
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s CMD ["/llm-router", "--healthcheck"]
ENTRYPOINT ["/llm-router"]
