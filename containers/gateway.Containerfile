# traum-haft gateway. Build from the repository root:
#   podman build -f containers/gateway.Containerfile -t registry.isp-insoft.de/platform/gateway .
FROM docker.io/library/rust:1.97-slim-trixie AS build
# Only for builds behind a TLS-intercepting proxy; empty otherwise.
ARG CARGO_HTTP_CAINFO
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY crates crates
RUN cargo build --release --locked -p traum-haft-gateway

FROM docker.io/library/debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /data --no-create-home gateway \
 && mkdir -p /data /config /secrets /srv/apps \
 && chown 10001:10001 /data
COPY --from=build /src/target/release/traum-haft-gateway /usr/local/bin/traum-haft-gateway
USER 10001:10001
ENV RUST_LOG=info GATEWAY_LISTEN=0.0.0.0:8080 GATEWAY_KEY_FILE=/data/signing-key.pem
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/traum-haft-gateway"]
