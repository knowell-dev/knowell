# Linux test runner for the Rust workspace (used through scripts/docker_test.py).
#
# Mirrors the CI Linux lane: the stable toolchain the workspace is developed with, git
# for the generated fixture repositories, clippy and rustfmt. Tests run as an
# unprivileged user because root bypasses the file-permission checks some tests make.
#
# The base image is pinned by digest; Dependabot (docker ecosystem) proposes updates.

ARG RUST_IMAGE=rust:1.98-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e
FROM ${RUST_IMAGE}

# Cargo writes its package-cache lock into CARGO_HOME itself, so the test user owns that
# directory (not the toolchain binaries in it). /src holds the source volume that
# scripts/docker_test.py fills; it is mounted read-only into the test run, hence the git
# safe.directory exception.
RUN rustup component add clippy rustfmt \
    && groupadd --gid 1000 tester \
    && useradd --uid 1000 --gid 1000 --create-home --shell /bin/bash tester \
    && install -d -o tester -g tester /src /cargo-target "$CARGO_HOME/registry" "$CARGO_HOME/git" \
    && chown tester:tester "$CARGO_HOME" \
    && git config --system --add safe.directory '*'

ENV CARGO_TARGET_DIR=/cargo-target \
    CARGO_INCREMENTAL=0 \
    CARGO_TERM_COLOR=always

USER tester
WORKDIR /src
CMD ["cargo", "test", "--workspace", "--locked"]
