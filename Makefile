PREFIX ?= /usr
LIBDIR ?= $(PREFIX)/lib64
LIBEXECDIR ?= $(PREFIX)/libexec
UNITDIR ?= $(PREFIX)/lib/systemd/system
RPM_BUILD_DIR ?= $(CURDIR)/build/rpm/BUILD
CC ?= cc
CARGO ?= cargo
PKGS = gtk+-3.0 libxfce4panel-2.0
CFLAGS ?= -O2 -g
CPPFLAGS += $(shell pkg-config --cflags $(PKGS))
LDLIBS += $(shell pkg-config --libs $(PKGS)) -ldl -lpthread -lm
RUSTLIB = target/release/libmultiload_qubes.a
COLLECTOR = target/release/multiload-qubes-collector

.PHONY: all test check gui-test install rpm clean
all: build/libmultiload-qubes.so build/multiload-qubes-preview $(COLLECTOR)

$(RUSTLIB) $(COLLECTOR) &: Cargo.toml Cargo.lock snapshot/Cargo.toml $(wildcard src/*.rs src/bin/*.rs src/collector/*.rs guest/*.py snapshot/src/*.rs)
	$(CARGO) build --release --offline

build:
	mkdir -p build

build/libmultiload-qubes.so: native/plugin.c $(RUSTLIB) | build
	$(CC) $(CFLAGS) $(CPPFLAGS) -Wall -Wextra -Werror -fPIC -shared $(LDFLAGS) -o $@ $< $(RUSTLIB) $(LDLIBS) -Wl,-z,defs

build/multiload-qubes-preview: native/plugin.c $(RUSTLIB) | build
	$(CC) $(CFLAGS) $(CPPFLAGS) -Wall -Wextra -Werror -DPREVIEW $(LDFLAGS) -o $@ $< $(RUSTLIB) $(LDLIBS)

build/panel-smoke: tests/panel_smoke.c | build
	$(CC) $(CFLAGS) $(CPPFLAGS) -Wall -Wextra -Werror $(LDFLAGS) -o $@ $< $(LDLIBS)

gui-test: all build/panel-smoke
	python3 tests/gui_smoke.py

test:
	$(CARGO) test --workspace --offline
	python3 -m unittest discover -s tests -v

check: all test
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --offline --all-targets -- -D warnings

install: all
	install -Dm755 build/libmultiload-qubes.so $(DESTDIR)$(LIBDIR)/xfce4/panel/plugins/libmultiload-qubes.so
	install -Dm644 data/multiload-qubes.desktop $(DESTDIR)$(PREFIX)/share/xfce4/panel/plugins/multiload-qubes.desktop
	install -Dm755 $(COLLECTOR) $(DESTDIR)$(LIBEXECDIR)/multiload-qubes-collector
	install -Dm644 data/multiload-qubes-collector.service $(DESTDIR)$(UNITDIR)/multiload-qubes-collector.service

rpm:
	mkdir -p build/rpm/SOURCES
	$(CARGO) vendor --offline --locked build/vendor > /dev/null
	tar -czf build/rpm/SOURCES/multiload-qubes-0.5.1-vendor.tar.gz -C build vendor
	tar --exclude=./build --exclude=./target --exclude=./.git --exclude=./.agents --exclude=./.aws --exclude=./.codex --exclude='__pycache__' --transform='s,^\.,multiload-qubes-0.5.1,' -czf build/rpm/SOURCES/multiload-qubes-0.5.1.tar.gz .
	rpmbuild -ba --define '_topdir $(CURDIR)/build/rpm' --define '_builddir $(RPM_BUILD_DIR)' packaging/multiload-qubes.spec

clean:
	$(CARGO) clean
