# traum-haft runner (werk worker). Build from the repository root:
#   podman build -f containers/runner.Containerfile -t registry.isp-insoft.de/platform/runner .
# Drives the app user's podman through its socket (podman-remote), so the
# runner and the apps run as different users on the worker.
FROM docker.io/library/rust:1.97-slim-trixie AS build
ARG CARGO_HTTP_CAINFO
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY crates crates
COPY docs docs
RUN cargo build --release --locked -p traum-haft-runner

FROM docker.io/library/debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates podman-remote age \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/traum-haft-runner /usr/local/bin/traum-haft-runner
ENV RUST_LOG=info RUNNER_PODMAN=podman-remote CONTAINER_HOST=unix:///run/werk/podman.sock RUNNER_DATA_DIR=/srv/runner
ENTRYPOINT ["/usr/local/bin/traum-haft-runner"]
