#!/bin/sh
# Runs inside the distro container, with package files mounted in /packages.
# JERC_VERSION, JERC_PACKAGE_MANAGER=apk|xbps|gentoo are required.
set -eu
: "${JERC_VERSION:?missing JERC_VERSION}"
: "${JERC_PACKAGE_MANAGER:?missing JERC_PACKAGE_MANAGER}"

case "$JERC_PACKAGE_MANAGER" in
  apk)
    file="/packages/jerc-${JERC_VERSION}-r0.apk"
    test -s "$file"
    # The downloadable APK is unsigned; installation must explicitly opt in.
    apk add --no-cache --allow-untrusted "$file"
    apk info -L jerc | grep -E 'usr/bin/jerc$'
    ;;
  xbps)
    : "${JERC_XBPS_ARCH:?missing JERC_XBPS_ARCH}"
    test "$(xbps-uhelper arch)" = "$JERC_XBPS_ARCH"
    file="/packages/jerc-${JERC_VERSION}_1.${JERC_XBPS_ARCH}.xbps"
    test -s "$file"
    mkdir -p /tmp/jerc-repo
    cp "$file" /tmp/jerc-repo/
    xbps-rindex -a "/tmp/jerc-repo/${file##*/}"
    xbps-install -y --repository=/tmp/jerc-repo jerc
    xbps-query -f jerc | grep -E '^/usr/bin/jerc$'
    ;;
  gentoo)
    # This checks the archive layout and runs the executable in Gentoo.
    # The Portage ebuild is separately validated by a host-side Manifest test;
    # a full emerge test requires a working portage tree/profile in CI.
    overlay="/packages/jerc-gentoo-overlay-v${JERC_VERSION}.tar.gz"
    source="/packages/jerc-v${JERC_VERSION}-vendored.tar.gz"
    archive="/packages/jerc-v${JERC_VERSION}-x86_64-unknown-linux-musl.tar.gz"
    test -s "$overlay" && test -s "$archive" && test -s "$source"
    mkdir -p /tmp/jerc-gentoo
    tar -xzf "$overlay" -C /tmp/jerc-gentoo
    test -s "/tmp/jerc-gentoo/jerc-overlay/mail-client/jerc-bin/jerc-bin-${JERC_VERSION}.ebuild"
    test -s "/tmp/jerc-gentoo/jerc-overlay/mail-client/jerc/jerc-${JERC_VERSION}.ebuild"
    member="jerc-v${JERC_VERSION}-x86_64-unknown-linux-musl/jerc"
    tar -xOzf "$archive" "$member" > /tmp/jerc-gentoo/jerc
    chmod 0755 /tmp/jerc-gentoo/jerc
    /tmp/jerc-gentoo/jerc --version | grep -F "$JERC_VERSION"
    ;;
  *) echo "Unsupported package manager: $JERC_PACKAGE_MANAGER" >&2; exit 2 ;;
esac

if [ "$JERC_PACKAGE_MANAGER" != gentoo ]; then
  jerc --version | grep -F "$JERC_VERSION"
fi
echo "Validated $JERC_PACKAGE_MANAGER packaging/runtime"
