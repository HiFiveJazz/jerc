# Gentoo (local Portage overlay)

Each tagged GitHub Release can ship `jerc-gentoo-overlay-vVERSION.tar.gz`,
containing a versioned `app-misc/jerc-bin` ebuild, repository metadata, and a
`Manifest` with hashes of the actual static x86-64 musl release archive.
The `jerc-bin` ebuild downloads that archive and installs Jerc using Portage.
This is a **binary** overlay, not a source-built official Gentoo package.

On Gentoo (with an existing working Portage profile and main repository):

```sh
sudo mkdir -p /var/db/repos
sudo tar -xzf jerc-gentoo-overlay-v0.6.3.tar.gz -C /var/db/repos
sudo mkdir -p /etc/portage/repos.conf
printf '[jerc-local]\nlocation = /var/db/repos/jerc-overlay\nauto-sync = no\n' \
  | sudo tee /etc/portage/repos.conf/jerc-local.conf
sudo emerge -av app-misc/jerc-bin
```

This is a manually downloaded, version-specific overlay: to upgrade, extract
an overlay from a newer release and run `emerge -avu jerc-bin`. It does not
provide automatic repository synchronization. Upstream inclusion requires
submitting a separate ebuild to Gentoo maintainers. Only amd64 is supported
by the binary overlay until an ARM64 Linux build is published.

`smoke-extra-linux-package.sh` validates the overlay layout and binary runtime
on Gentoo, while `verify-gentoo-overlay.py` verifies Manifest hashes. A full
Portage `emerge` build/install smoke test remains a separate follow-up.
