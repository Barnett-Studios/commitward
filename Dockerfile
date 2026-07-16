# commitward — deterministic fail-open HITL commit gate.
# Multi-stage: build the CLI on the pinned Rust toolchain, ship a slim runtime
# with git (the gate shells `git diff`) and the default checkpoint baseline.
FROM rust:1.94-slim-bookworm AS builder
WORKDIR /build
COPY . .
RUN cargo build --release --bin commitward

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 commitward
COPY --from=builder /build/target/release/commitward /usr/local/bin/commitward
COPY --from=builder /build/checkpoints.yaml /etc/commitward/checkpoints.yaml
# The default global registry lives beside no binary in a container, so point at
# the baked baseline explicitly. A mounted repo can still add repo-local overrides.
ENV COMMITWARD_REGISTRY=/etc/commitward/checkpoints.yaml
USER commitward
WORKDIR /repo
ENTRYPOINT ["commitward"]
CMD ["--help"]
