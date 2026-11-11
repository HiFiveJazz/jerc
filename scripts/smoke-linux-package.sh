#!/usr/bin/env bash
# Runs INSIDE a Linux distro container with the package mounted at /packages.
# JERC_VERSION=0.6.3 JERC_PACKAGE_FORMAT=deb|rpm JERC_PACKAGE_MANAGER=apt|dnf|zypper
set -euo pipefail
: "${JERC_VERSION:?Missing JERC_VERSION}"
: "${JERC_PACKAGE_FORMAT:?Missing JERC_PACKAGE_FORMAT}"
: "${JERC_PACKAGE_MANAGER:?Missing JERC_PACKAGE_MANAGER}"

case "$JERC_PACKAGE_FORMAT:$JERC_PACKAGE_MANAGER" in
  deb:apt)
    package="/packages/jerc_${JERC_VERSION}-1_amd64.deb"
    test -s "$package"
    test "$(dpkg-deb -f "$package" Package)" = jerc
    test "$(dpkg-deb -f "$package" Version)" = "${JERC_VERSION}-1"
    test "$(dpkg-deb -f "$package" Architecture)" = amd64
    dpkg-deb -c "$package" | grep -E '/usr/bin/jerc$'
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get install -y "$package"
    dpkg -L jerc | grep -Fx /usr/bin/jerc
    ;;
  rpm:dnf|rpm:zypper)
    package="/packages/jerc-${JERC_VERSION}-1.x86_64.rpm"
    test -s "$package"
    test "$(rpm -qp --qf '%{NAME}' "$package")" = jerc
    test "$(rpm -qp --qf '%{VERSION}-%{RELEASE}' "$package")" = "${JERC_VERSION}-1"
    test "$(rpm -qp --qf '%{ARCH}' "$package")" = x86_64
    test "$(rpm -qp --qf '%{LICENSE}' "$package")" = GPL-3.0-only
    rpm -qlp "$package" | grep -Fx /usr/bin/jerc
    if [[ "$JERC_PACKAGE_MANAGER" == dnf ]]; then
      dnf install -y "$package"
    else
      # This is an unsigned downloadable RPM, not an OBS-signed repository RPM.
      zypper --non-interactive install --allow-unsigned-rpm "$package"
    fi
    rpm -ql jerc | grep -Fx /usr/bin/jerc
    ;;
  *)
    echo "Unsupported package/manager pair: $JERC_PACKAGE_FORMAT:$JERC_PACKAGE_MANAGER" >&2
    exit 2
    ;;
esac

jerc --version | grep -F "$JERC_VERSION"
echo "Validated $JERC_PACKAGE_FORMAT installation with $JERC_PACKAGE_MANAGER"
