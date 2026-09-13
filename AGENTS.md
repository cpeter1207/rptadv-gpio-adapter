# rptadv-gpio-adapter development rules

## Shared rpt_advanced project baseline

This baseline applies to every production, shared-library, and workflow
repository in the rpt_advanced project. Repository-specific rules may add
constraints but must not weaken it.

Run platform-independent formatting, lint, static analysis—including
Cppcheck—and Doxygen once, concurrently where independent. Do not run Cppcheck
in each platform job. Run platform-dependent build, tests, packaging, and
staged-install checks concurrently on native Debian 13 amd64 and arm64. Require
100% line and branch coverage of production code only on Debian 13 amd64; test
code is excluded from the coverage requirement. Debian 12 support is
aspirational: do not run automated Debian 12 tests or build Debian 12 packages
as part of ordinary pushes, pull requests, or releases. Build Debian 12
packages manually only when explicitly requested. Automated releases publish
Debian 13 packages only; node installations use Debian 13 arm64 packages.
Quality checks must not rewrite source files.

Before a push, run formatting, lint, and static analysis only; GitHub repeats
those fast checks for every push. Do not run the full platform gate locally
solely to prepare a push. The full quality gate runs for every pull request and
must pass before that pull request can merge. Releases are built only from a
merged main revision that has already passed the full pull-request gate, so the
release workflow does not repeat it. Local recovery commits may follow affected
targeted checks, but must not be represented as fully verified until the pull
request gate passes.

Treat compiler warnings as errors and fail applicable formatting, Rust Clippy,
ShellCheck, Cppcheck, Doxygen, tests, installation checks, and 100% line and
branch coverage of production code on Debian 13 amd64. Remove unreachable or
dead code instead of suppressing diagnostics or excluding it from coverage.

Update concise Doxygen comments, tests, user documentation, examples, and
build, install, and package artifacts whenever an interface changes. Consumers
must dynamically link the released, versioned shared object; do not vendor or
statically link a duplicate implementation. Preserve published ABI/API
compatibility whenever practical; when a change is necessary, document its
compatibility, SONAME/package consequences, and migration.

Start and clean only project-owned, labeled test containers deterministically.
Before a local container test, pull the required `:latest` image, inspect its
manifest digest, and use that freshly pulled image. Keep iteration evidence in
the ignored `/.work/` directory. Never deploy to a node or alter its
configuration without explicit approval.

## CM119 HID boundary

Follow ADRs 0005, 0011, 0012, 0018, 0021, 0022, 0027, 0028, and 0029 in
`rpt_advanced`. This repository is a separately versioned Rust `cdylib` with a
narrow C ABI. It dynamically links libusb but exposes no libusb types.
`librptadvradio` remains independent of libusb, PortAudio, ALSA, OSS, Asterisk,
Hamlib, and HID.

This adapter owns CM119 HID GPIO, logical PTT, COR/CTCSS snapshots, stable
CM119 discovery, CM119 EEPROM access, and explicitly configured Linux
parallel-port GPIO. One non-real-time service owner performs each libusb or
parallel-port I/O operation. Native ticks only use the lock-free
output-publication and input-snapshot functions. The adapter does not contain
PCM, mixer, DSP, Asterisk, or Hamlib code.

Workflow implementations belong in a dedicated companion repository; this
repository may contain only thin callers after that repository exists. Do not
modify a consumer until this library has a released ABI and Debian package,
except for an explicitly isolated proof-of-concept approved by the user.
