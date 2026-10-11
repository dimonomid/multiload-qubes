PREFIX ?= /usr
LIBDIR ?= $(PREFIX)/lib64
LIBEXECDIR ?= $(PREFIX)/libexec
UNITDIR ?= $(PREFIX)/lib/systemd/system
RPM_BUILD_DIR ?= $(CURDIR)/build/rpm/BUILD
CC ?= cc
CARGO ?= cargo
PKGS = gtk+-3.0 libxfce4panel-2.0
CFLAGS ?= -O2 -g
CPPFLAGS += -Ibuild $(shell pkg-config --cflags $(PKGS))
LDLIBS += $(shell pkg-config --libs $(PKGS)) -ldl -lpthread -lm
RUSTLIB = build/libmultiload_qubes.a
COLLECTOR = target/release/multiload-qubes-collector
PREVIEW = target/release/multiload-qubes-preview
PANEL_HEADER = build/panel-api.h

.PHONY: all test check gui-test install rpm clean
all: build/libmultiload-qubes.so build/multiload-qubes-preview $(COLLECTOR)

$(RUSTLIB) $(COLLECTOR) $(PREVIEW) $(PANEL_HEADER) &: build.rs cbindgen.toml Cargo.toml Cargo.lock snapshot/Cargo.toml $(wildcard src/*.rs src/bin/*.rs src/collector/*.rs src/graphs/*.rs src/panel/*.rs guest/*.py snapshot/src/*.rs) | build
	$(CARGO) build --release --offline --no-default-features --bin multiload-qubes-collector
	$(CARGO) build --release --offline --features panel --lib --bin multiload-qubes-preview
	cp target/release/libmultiload_qubes.a $(RUSTLIB)

build:
	mkdir -p build

build/libmultiload-qubes.so: native/plugin.c $(RUSTLIB) $(PANEL_HEADER) | build
	$(CC) $(CFLAGS) $(CPPFLAGS) -Wall -Wextra -Werror -fPIC -shared $(LDFLAGS) -o $@ $< $(RUSTLIB) $(LDLIBS) -Wl,-z,defs

build/multiload-qubes-preview: $(PREVIEW) | build
	cp $< $@

build/panel-smoke: tests/panel_smoke.c | build
	$(CC) $(CFLAGS) $(CPPFLAGS) -Wall -Wextra -Werror $(LDFLAGS) -o $@ $< $(LDLIBS)

gui-test: all build/panel-smoke
	python3 tests/gui_smoke.py

test:
	$(CARGO) test --workspace --offline --features panel
	python3 -m unittest discover -s tests -v

check: all test
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --offline --all-targets --features panel -- -D warnings

install: all
	install -Dm755 build/libmultiload-qubes.so $(DESTDIR)$(LIBDIR)/xfce4/panel/plugins/libmultiload-qubes.so
	install -Dm644 data/multiload-qubes.desktop $(DESTDIR)$(PREFIX)/share/xfce4/panel/plugins/multiload-qubes.desktop
	install -Dm755 $(COLLECTOR) $(DESTDIR)$(LIBEXECDIR)/multiload-qubes-collector
	install -Dm644 data/multiload-qubes-collector.service $(DESTDIR)$(UNITDIR)/multiload-qubes-collector.service

rpm:
	mkdir -p build/rpm/SOURCES
	$(CARGO) vendor --offline --locked build/vendor > /dev/null
	tar -czf build/rpm/SOURCES/multiload-qubes-0.11.1-vendor.tar.gz -C build vendor
	tar --exclude=./build --exclude=./target --exclude=./.git --exclude=./.agents --exclude=./.aws --exclude=./.codex --exclude='__pycache__' --transform='s,^\.,multiload-qubes-0.11.1,' -czf build/rpm/SOURCES/multiload-qubes-0.11.1.tar.gz .
	rpmbuild -ba --define '_topdir $(CURDIR)/build/rpm' --define '_builddir $(RPM_BUILD_DIR)' packaging/multiload-qubes.spec

clean:
	$(CARGO) clean
