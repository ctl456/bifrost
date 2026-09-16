# Bifrost as an image.
#
# Two stages: one that has a compiler and never ships, and one that has the binary
# and almost nothing else. The build is the command the README gives, run with
# `--locked`, so what ends up in the image is the dependency set in Cargo.lock rather
# than whatever resolved on the day it was built.
#
# What is deliberately not here is a configuration file. The defaults are the ones a
# container wants — listen on 0.0.0.0:3050 and forward the caller's key per request —
# and a deployment that needs more names its own file with `--config`, which is what
# the entrypoint at the bottom of this file is the binary itself for.

FROM rust:1-bookworm AS builder
WORKDIR /build

# The toolchain is the one in the image rather than the one rust-toolchain.toml names.
# That file names the `stable` channel and two components; copying it in would have
# rustup install both while this builds, and nothing in this stage runs rustfmt or
# clippy. The image's tag is that same channel, so the compiler here is the compiler a
# `cargo test` ran on and the two cannot disagree about what compiles.
#
# `cmake` and `perl` are for aws-lc-sys, the C library rustls verifies with: it can
# drive its build through either, and a version bump that chose the other path should
# not be the thing that breaks the image. That library is also where the minutes in
# this stage go, which is the reason for the shape of the two steps below it.
RUN apt-get update \
 && apt-get install --no-install-recommends -y cmake perl \
 && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
# A crate of the same shape with nothing in it, so that this layer holds every
# compiled dependency and the next one holds only this crate. Without it, editing a
# line of src/ recompiles the C crypto library, which is minutes rather than seconds.
RUN mkdir -p src/bin \
 && : > src/lib.rs \
 && echo 'fn main() {}' > src/bin/bifrost.rs \
 && cargo build --release --locked --bin bifrost \
 && rm -rf src

COPY src/ src/
# Cargo decides what to rebuild by modification time, and a `COPY` carries the timestamps
# of the build context rather than the time of the copy. Those come from the checkout,
# which is older than the placeholder build a moment ago, and an older source reads to
# cargo as an unchanged one -- so without this line the image ships the placeholder: a
# binary that does nothing and exits, which is what a container built from it then did.
# Touching the sources is what makes them newer than the artifact built in their absence.
RUN find src -type f -exec touch {} + \
 && cargo build --release --locked --bin bifrost

# The runtime is the Debian release the binary was compiled on, which is what makes a
# binary linked against the system libc certain to find the one it was linked against.
# `slim` drops the toolchain and the rest of the build; the two packages added back
# are the two things this process opens that are not in a bare system.
FROM debian:bookworm-slim

# `ca-certificates` is not optional. reqwest verifies api.commandcode.ai through
# rustls-platform-verifier, which on Linux reads the system trust store rather than a
# compiled-in one, so an image without this file fails its first turn with an unknown
# issuer — which reads like an upstream problem and is not one. `curl` is here so the
# healthcheck below has something to probe with, and so that an operator inside the
# container can ask whether the upstream is reachable from where this actually runs.
RUN apt-get update \
 && apt-get install --no-install-recommends -y ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

# The account deploy/bifrost.service creates, with its ids named rather than left to
# `useradd`. A volume is created holding whatever ownership the image's directory has,
# so a uid that moved between builds would leave a rebuild unable to write the state
# directory the previous build had been writing.
RUN groupadd --gid 10001 bifrost \
 && useradd --uid 10001 --gid 10001 --home-dir /var/lib/bifrost --create-home \
      --shell /usr/sbin/nologin bifrost

COPY --from=builder /build/target/release/bifrost /usr/local/bin/bifrost

# The working directory is the state directory, for the reason the unit gives for the
# same line: the default configuration's `var/tokens.json` is a relative path, and
# this is the one place the process may write.
WORKDIR /var/lib/bifrost
USER bifrost

EXPOSE 3050

# The port is read from the environment rather than written twice. `PORT` is a setting
# this build honours, and a probe pinned to 3050 would call a container unhealthy the
# moment someone moved it.
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
  CMD curl -fsS "http://127.0.0.1:${PORT:-3050}/health" || exit 1

# There is no `CMD`: the entrypoint is the binary, so the argument list is the command
# line this build already has — `--check`, `--print-config`, `--token-new`,
# `--token-list`, `--token-revoke`, `--help` — and no argument means serve. A shim that
# ran `--check` first would add a process to reap for a check the binary already makes
# before it binds.
ENTRYPOINT ["/usr/local/bin/bifrost"]
