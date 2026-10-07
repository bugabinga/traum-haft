# traum-haft platform MCP. Build from the repository root:
#   podman build -f containers/mcp.Containerfile -t registry.isp-insoft.de/platform/mcp .
# No Rust or Node in the runtime image: app builds run in GitHub Actions.
FROM docker.io/library/rust:1.97-slim-trixie AS build
ARG CARGO_HTTP_CAINFO
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY crates crates
RUN cargo build --release --locked -p traum-haft-mcp

FROM docker.io/library/debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates git \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /data --no-create-home mcp \
 && mkdir -p /data /secrets /srv/apps /opt/traum-haft \
 && chown 10001:10001 /data
COPY --from=build /src/target/release/traum-haft-mcp /usr/local/bin/traum-haft-mcp
# The app template new apps are created from.
COPY template /opt/traum-haft/template
USER 10001:10001
ENV RUST_LOG=info MCP_LISTEN=0.0.0.0:8080 MCP_DATA_DIR=/data MCP_TEMPLATE_DIR=/opt/traum-haft/template MCP_BUILDER=actions
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/traum-haft-mcp"]
