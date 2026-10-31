FROM registry.fedoraproject.org/fedora:37

# Fedora 37 has moved off the normal mirrors. Keep signature checks enabled.
RUN rm -f /etc/yum.repos.d/*.repo && \
    printf '%s\n' \
      '[fedora]' \
      'name=Fedora 37 archive' \
      'baseurl=https://archives.fedoraproject.org/pub/archive/fedora/linux/releases/37/Everything/$basearch/os/' \
      'gpgcheck=1' \
      'gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-fedora-37-$basearch' \
      '[updates]' \
      'name=Fedora 37 updates archive' \
      'baseurl=https://archives.fedoraproject.org/pub/archive/fedora/linux/updates/37/Everything/$basearch/' \
      'gpgcheck=1' \
      'gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-fedora-37-$basearch' \
      > /etc/yum.repos.d/fedora-archive.repo && \
    dnf install -y \
      ca-certificates curl gcc git make pkgconf-pkg-config \
      gtk3-devel xfce4-panel-devel xen-devel python3 \
      rpm-build systemd-rpm-macros \
      xorg-x11-server-Xvfb xorg-x11-xauth && \
    dnf clean all

# The compiler can be new while the linked system libraries stay on Fedora 37.
ARG RUST_VERSION=1.88.0
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup.sh && \
    sh /tmp/rustup.sh -y --profile minimal --default-toolchain "${RUST_VERSION}" \
      --component rustfmt --component clippy && \
    rm /tmp/rustup.sh
ENV PATH="/root/.cargo/bin:${PATH}" \
    CARGO_PROFILE_DEV_DEBUG=0 \
    CARGO_INCREMENTAL=0 \
    NO_AT_BRIDGE=1

RUN git config --global --add safe.directory /workspace
WORKDIR /workspace
CMD ["bash", "/workspace/.github/ci-build.sh"]
