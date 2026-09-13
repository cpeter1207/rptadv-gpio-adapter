# Quality checks

The shared project baseline in [AGENTS.md](AGENTS.md) is authoritative.

Before a push, run the non-mutating fast checks:

```sh
make lint static-analysis
```

The pull-request gate runs formatting, static analysis, Doxygen, native Debian
13 amd64/arm64 builds, tests, package validation, and staged installation.
Production Rust source requires 100% line and branch coverage on Debian 13
amd64 only.  The coverage job uses the pinned nightly toolchain in
`containers/quality.Dockerfile`.

Use the labeled launcher for a disposable local container run:

```sh
make container-coverage
```

It removes only stale containers belonging to this workspace before and after
the command.  The launcher pulls the maintained `:latest` base image and
prints its resulting digest before ordinary remote-image runs.
