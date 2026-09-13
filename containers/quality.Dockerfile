# syntax=docker/dockerfile:1.7
# The launcher always pulls and reports the current manifest digest for this
# maintained quality base before it runs a derived image.
ARG BASE_IMAGE=ghcr.io/cpeter1207/rpt-advanced-quality-debian13:latest
FROM ${BASE_IMAGE}

ARG TARGETARCH
ARG RUSTUP_INIT_VERSION=1.28.2
ARG RUST_STABLE=1.85.0
ARG RUST_NIGHTLY=nightly-2025-02-20
ARG CARGO_LLVM_COV_VERSION=0.6.21

ENV RUSTUP_HOME=/opt/rustup
ENV CARGO_HOME=/opt/cargo
ENV PATH=/opt/cargo/bin:${PATH}

RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
		ca-certificates cargo curl debhelper dpkg-dev libusb-1.0-0-dev pkg-config rustc && \
	rm -rf /var/lib/apt/lists/*

# rustup-init is pinned and verified per supported native architecture.  It
# installs the exact stable project toolchain plus a coverage-only nightly.
RUN case "${TARGETARCH}" in \
		amd64) rustup_host=x86_64-unknown-linux-gnu; rustup_sha=20a06e644b0d9bd2fbdbfd52d42540bdde820ea7df86e92e533c073da0cdd43c ;; \
		arm64) rustup_host=aarch64-unknown-linux-gnu; rustup_sha=e3853c5a252fca15252d07cb23a1bdd9377a8c6f3efa01531109281ae47f841c ;; \
		*) echo "unsupported architecture: ${TARGETARCH}" >&2; exit 1 ;; \
	esac; \
	curl --proto '=https' --tlsv1.2 --fail --silent --show-error \
		"https://static.rust-lang.org/rustup/archive/${RUSTUP_INIT_VERSION}/${rustup_host}/rustup-init" \
		--output /tmp/rustup-init; \
	echo "${rustup_sha}  /tmp/rustup-init" | sha256sum --check --status; \
	chmod 0755 /tmp/rustup-init; \
	/tmp/rustup-init -y --no-modify-path --default-toolchain none; \
	rm -f /tmp/rustup-init

# Branch coverage remains a Rust nightly capability.  Keep it isolated from
# the project's stable build toolchain and pin both tools for repeatable gates.
RUN rustup toolchain install "${RUST_STABLE}" --profile minimal --component clippy --component rustfmt && \
	rustup toolchain install "${RUST_NIGHTLY}" --profile minimal --component llvm-tools-preview && \
	rustup default "${RUST_STABLE}" && \
	cargo +"${RUST_NIGHTLY}" install cargo-llvm-cov --version "${CARGO_LLVM_COV_VERSION}" --locked && \
	cargo +"${RUST_NIGHTLY}" llvm-cov --version

ENV RPTADV_COVERAGE_TOOLCHAIN=${RUST_NIGHTLY}

WORKDIR /workspace
