.DEFAULT_GOAL := all

CARGO ?= cargo
CARGO_FMT ?= cargo fmt
CARGO_CLIPPY ?= cargo clippy
CARGO_LLVM_COV ?= cargo llvm-cov
DOXYGEN ?= doxygen
SHELLCHECK ?= shellcheck
CPPCHECK ?= cppcheck
READELF ?= readelf
CC ?= cc
PYTHON ?= python3

PACKAGE := rptadv-gpio-adapter
CRATE := rptadv_gpio_adapter
PACKAGE_VERSION ?= 0.1.0-alpha.1
SOVERSION := 1
PREFIX ?= /usr/local
DESTDIR ?=
LIBDIR ?= $(PREFIX)/lib
CARGO_TARGET_DIR ?= target
TARGET_RELEASE := $(CARGO_TARGET_DIR)/release
LIBRARY_BASENAME := lib$(CRATE)
TARGET_LIBRARY := $(TARGET_RELEASE)/$(LIBRARY_BASENAME).so
LIBRARY_VERSIONED := build/$(LIBRARY_BASENAME).so.$(SOVERSION).$(PACKAGE_VERSION)
LIBRARY_SONAME := build/$(LIBRARY_BASENAME).so.$(SOVERSION)
LIBRARY_LINK := build/$(LIBRARY_BASENAME).so
HEADER := include/rptadv_gpio_adapter/rptadv_gpio_adapter.h
PC_TEMPLATE := rptadv_gpio_adapter.pc.in
PC_FILE := build/rptadv_gpio_adapter.pc
RUST_SOURCES := $(wildcard src/*.rs)
C_SMOKE_SOURCE := tests/descriptor_smoke.c
C_SMOKE_BINARY := build/descriptor-smoke
DEBIAN_VERSION = $(shell dpkg-parsechangelog -S Version)
DEBIAN_ARCH = $(shell dpkg-architecture -qDEB_HOST_ARCH)
DEBIAN_MULTIARCH = $(shell dpkg-architecture -qDEB_HOST_MULTIARCH)
DEBIAN_SOURCE_PARENT = build/debian-source
DEBIAN_SOURCE_DIR = $(DEBIAN_SOURCE_PARENT)/$(PACKAGE)-$(PACKAGE_VERSION)
DEBIAN_OUTPUT_DIR = $(abspath $(DEBIAN_SOURCE_PARENT))
DEBIAN_RUNTIME_DEB = $(DEBIAN_OUTPUT_DIR)/librptadv-gpio-adapter1_$(DEBIAN_VERSION)_$(DEBIAN_ARCH).deb
DEBIAN_DEV_DEB = $(DEBIAN_OUTPUT_DIR)/librptadv-gpio-adapter-dev_$(DEBIAN_VERSION)_$(DEBIAN_ARCH).deb
DEBIAN_STAGE = build/debian-package-stage
COVERAGE_TOOLCHAIN ?= nightly-2025-02-20
COVERAGE_DIR = build/coverage
COVERAGE_TARGET_DIR = build/llvm-cov-target
COVERAGE_JSON = $(COVERAGE_DIR)/coverage.json
COVERAGE_PRODUCTION_ROOT = $(CURDIR)/src
COVERAGE_TEST_MODULE = $(COVERAGE_PRODUCTION_ROOT)/tests.rs
QUALITY_BASE_IMAGE ?= ghcr.io/cpeter1207/rpt-advanced-quality-debian13:latest
QUALITY_IMAGE ?= $(PACKAGE)-quality:local
QUALITY_LAUNCHER = tools/run-in-quality-container.sh

SONAME_RUSTFLAGS = $(RUSTFLAGS) -C link-arg=-Wl,-soname,$(LIBRARY_BASENAME).so.$(SOVERSION)

.PHONY: all quality lint static-analysis docs test coverage install install-check \
	debian-package-check dist distcheck platform-verify ci quality-image container-coverage clean FORCE

all: $(LIBRARY_VERSIONED) $(LIBRARY_SONAME) $(LIBRARY_LINK)

build:
	mkdir -p $@

$(TARGET_LIBRARY): Cargo.toml Cargo.lock $(RUST_SOURCES)
	RUSTFLAGS="$(SONAME_RUSTFLAGS)" $(CARGO) build --release --locked

$(LIBRARY_VERSIONED): $(TARGET_LIBRARY) | build
	cp $(TARGET_LIBRARY) $@

$(LIBRARY_SONAME): $(LIBRARY_VERSIONED)
	ln -sf $(notdir $<) $@

$(LIBRARY_LINK): $(LIBRARY_SONAME)
	ln -sf $(notdir $<) $@

$(PC_FILE): $(PC_TEMPLATE) FORCE | build
	sed -e 's|@PREFIX@|$(PREFIX)|' -e 's|@LIBDIR@|$(LIBDIR)|' \
		-e 's|@VERSION@|$(PACKAGE_VERSION)|' $< > $@

quality: lint static-analysis docs

lint:
	$(CARGO_FMT) --check
	$(SHELLCHECK) tools/run-in-quality-container.sh

static-analysis:
	$(CARGO_CLIPPY) --all-targets --all-features -- -D warnings
	$(CPPCHECK) --force --enable=warning,style,performance,portability \
		--error-exitcode=1 --std=c11 -Iinclude $(C_SMOKE_SOURCE)

docs: | build
	$(DOXYGEN) Doxyfile
	test ! -s build/doxygen-warnings.log

test:
	$(CARGO) test --all-targets --locked
	$(MAKE) $(C_SMOKE_BINARY)
	./$(C_SMOKE_BINARY)

$(C_SMOKE_BINARY): $(C_SMOKE_SOURCE) $(HEADER) $(LIBRARY_LINK) | build
	$(CC) -std=c11 -Wall -Wextra -Werror -Iinclude $< -Lbuild \
		-l$(CRATE) -Wl,-rpath,'$$ORIGIN' -o $@

coverage:
	rm -rf $(COVERAGE_DIR) $(COVERAGE_TARGET_DIR)
	mkdir -p $(COVERAGE_DIR)
	RUSTUP_TOOLCHAIN=$(COVERAGE_TOOLCHAIN) CARGO_LLVM_COV_TARGET_DIR=$(abspath $(COVERAGE_TARGET_DIR)) \
		$(CARGO_LLVM_COV) --all-targets --locked --branch --json --output-path $(COVERAGE_JSON)
	$(PYTHON) -c 'import json, os, sys; report=json.load(open(sys.argv[1], encoding="utf-8")); root=os.path.realpath(sys.argv[2]); test=os.path.realpath(sys.argv[3]); files={}; [files.setdefault(path, entry["summary"]) for datum in report.get("data", []) for entry in datum.get("files", []) for path in (os.path.realpath(entry["filename"]),) if os.path.commonpath((root,path)) == root and path != test and "{}tests{}".format(os.path.sep,os.path.sep) not in path]; failures=[(path,metric,summary.get(metric,{})) for path,summary in sorted(files.items()) for metric in ("lines","branches") if not isinstance(summary.get(metric),dict) or summary[metric].get("covered") != summary[metric].get("count")]; print("verified production coverage for {} source files".format(len(files))); [print("{}: {} {}/{}".format(path,metric,values.get("covered","missing"),values.get("count","missing")),file=sys.stderr) for path,metric,values in failures]; raise SystemExit(1 if not files or failures else 0)' $(COVERAGE_JSON) $(COVERAGE_PRODUCTION_ROOT) $(COVERAGE_TEST_MODULE)

quality-image:
	docker image pull $(QUALITY_BASE_IMAGE)
	docker build --pull --build-arg BASE_IMAGE=$(QUALITY_BASE_IMAGE) --tag $(QUALITY_IMAGE) \
		--file containers/quality.Dockerfile containers

container-coverage: quality-image
	RPTADV_CONTAINER_PULL=0 $(QUALITY_LAUNCHER) $(QUALITY_IMAGE) $(MAKE) coverage

install: all $(PC_FILE)
	install -d $(DESTDIR)$(LIBDIR) $(DESTDIR)$(PREFIX)/include/rptadv_gpio_adapter \
		$(DESTDIR)$(LIBDIR)/pkgconfig
	install -m 0755 $(LIBRARY_VERSIONED) $(DESTDIR)$(LIBDIR)/
	ln -sf $(notdir $(LIBRARY_VERSIONED)) $(DESTDIR)$(LIBDIR)/$(notdir $(LIBRARY_SONAME))
	ln -sf $(notdir $(LIBRARY_SONAME)) $(DESTDIR)$(LIBDIR)/$(notdir $(LIBRARY_LINK))
	install -m 0644 $(HEADER) $(DESTDIR)$(PREFIX)/include/rptadv_gpio_adapter/
	install -m 0644 $(PC_FILE) $(DESTDIR)$(LIBDIR)/pkgconfig/

install-check: all
	rm -rf build/stage
	$(MAKE) DESTDIR=$(CURDIR)/build/stage PREFIX=/usr LIBDIR=/usr/lib install
	test -f build/stage/usr/lib/$(notdir $(LIBRARY_VERSIONED))
	test -L build/stage/usr/lib/$(notdir $(LIBRARY_SONAME))
	test -L build/stage/usr/lib/$(notdir $(LIBRARY_LINK))
	test ! -e build/stage/usr/lib/$(LIBRARY_BASENAME).a
	$(READELF) -d build/stage/usr/lib/$(notdir $(LIBRARY_VERSIONED)) | grep -F '$(LIBRARY_BASENAME).so.$(SOVERSION)'
	$(READELF) -d build/stage/usr/lib/$(notdir $(LIBRARY_VERSIONED)) | grep -F 'libusb-1.0.so'
	! $(READELF) -d build/stage/usr/lib/$(notdir $(LIBRARY_VERSIONED)) | grep -E 'res_usbradio.so|libportaudio.so|libasound.so|libhamlib.so'
	test -f build/stage/usr/include/rptadv_gpio_adapter/$(notdir $(HEADER))
	test -f build/stage/usr/lib/pkgconfig/rptadv_gpio_adapter.pc

debian-package-check: dist
	rm -rf $(DEBIAN_SOURCE_PARENT) $(DEBIAN_STAGE)
	mkdir -p $(DEBIAN_SOURCE_PARENT)
	build_root=$$(mktemp -d); \
	trap 'rm -rf "$$build_root"' EXIT; \
	tar -C "$$build_root" -xzf build/$(PACKAGE)-$(PACKAGE_VERSION).tar.gz; \
	source_dir="$$build_root/$(PACKAGE)-$(PACKAGE_VERSION)"; \
	chmod 0644 "$$source_dir"/debian/changelog "$$source_dir"/debian/control \
		"$$source_dir"/debian/copyright "$$source_dir"/debian/*.docs \
		"$$source_dir"/debian/*.install "$$source_dir"/debian/source/*; \
	chmod 0755 "$$source_dir"/debian/rules; \
	cd "$$source_dir" && dpkg-buildpackage -us -uc -b; \
	cp "$$build_root"/*.deb "$(DEBIAN_OUTPUT_DIR)/"
	test -f "$(DEBIAN_RUNTIME_DEB)"
	test -f "$(DEBIAN_DEV_DEB)"
	rm -rf $(DEBIAN_STAGE)
	mkdir -p $(DEBIAN_STAGE)
	dpkg-deb --extract "$(DEBIAN_RUNTIME_DEB)" $(DEBIAN_STAGE)
	dpkg-deb --extract "$(DEBIAN_DEV_DEB)" $(DEBIAN_STAGE)
	test ! -e "$(DEBIAN_STAGE)/usr/lib/$(DEBIAN_MULTIARCH)/$(LIBRARY_BASENAME).a"
	test -L "$(DEBIAN_STAGE)/usr/lib/$(DEBIAN_MULTIARCH)/$(LIBRARY_BASENAME).so"
	$(READELF) -d "$(DEBIAN_STAGE)/usr/lib/$(DEBIAN_MULTIARCH)/$(LIBRARY_BASENAME).so.$(SOVERSION)" | grep -F '$(LIBRARY_BASENAME).so.$(SOVERSION)'
	$(READELF) -d "$(DEBIAN_STAGE)/usr/lib/$(DEBIAN_MULTIARCH)/$(LIBRARY_BASENAME).so.$(SOVERSION)" | grep -F 'libusb-1.0.so'
	! $(READELF) -d "$(DEBIAN_STAGE)/usr/lib/$(DEBIAN_MULTIARCH)/$(LIBRARY_BASENAME).so.$(SOVERSION)" | grep -E 'res_usbradio.so|libportaudio.so|libasound.so|libhamlib.so'

dist: | build
	rm -rf build/dist
	mkdir -p build/dist/$(PACKAGE)-$(PACKAGE_VERSION)
	tar --exclude=.git --exclude=.work --exclude=build --exclude=target \
		--exclude=debian/.debhelper --exclude=debian/debhelper-build-stamp \
		--exclude=debian/files --exclude=debian/tmp \
		--exclude=debian/librptadv-gpio-adapter1 --exclude=debian/librptadv-gpio-adapter-dev \
		--exclude='debian/*.substvars' --exclude='debian/*.debhelper.log' \
		--transform='s|^|$(PACKAGE)-$(PACKAGE_VERSION)/|' -czf build/$(PACKAGE)-$(PACKAGE_VERSION).tar.gz \
		AGENTS.md Cargo.lock Cargo.toml COPYING Doxyfile Makefile QUALITY.md README.md \
		rptadv_gpio_adapter.pc.in rust-toolchain.toml containers debian include src tests tools

distcheck: dist
	! tar -tzf build/$(PACKAGE)-$(PACKAGE_VERSION).tar.gz | \
		grep -E '/debian/(\.debhelper/|debhelper-build-stamp$$|files$$|tmp/|librptadv-gpio-adapter(1|-dev)/|.*\.(substvars|debhelper\.log)$$)'
	rm -rf build/dist-unpacked
	mkdir -p build/dist-unpacked
	tar -C build/dist-unpacked -xzf build/$(PACKAGE)-$(PACKAGE_VERSION).tar.gz
	$(MAKE) -C build/dist-unpacked/$(PACKAGE)-$(PACKAGE_VERSION) install-check

platform-verify: test install-check debian-package-check distcheck

ci: quality platform-verify

clean:
	rm -rf build target

FORCE:
