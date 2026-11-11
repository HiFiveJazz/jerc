# Gentoo (local Portage overlay)

Each tagged GitHub Release can ship `jerc-gentoo-overlay-vVERSION.tar.gz`,
containing versioned `mail-client/jerc` (source) and `mail-client/jerc-bin`
ebuilds, repository metadata, and a
`Manifest` with hashes of the actual static x86-64 musl release archive.
The `jerc-bin` ebuild downloads that binary archive; the `jerc` source ebuild
uses Gentoo's cargo.eclass with a vendored Rust source archive and pinned
Cargo.lock. Both are **local overlay** packages, not official Gentoo packages.

On Gentoo (with an existing working Portage profile and main repository):

```sh
sudo mkdir -p /var/db/repos
sudo tar -xzf jerc-gentoo-overlay-v0.6.3.tar.gz -C /var/db/repos
sudo mkdir -p /etc/portage/repos.conf
printf '[jerc-local]\nlocation = /var/db/repos/jerc-overlay\nauto-sync = no\n' \
  | sudo tee /etc/portage/repos.conf/jerc-local.conf
sudo emerge -av mail-client/jerc
```

This is a manually downloaded, version-specific overlay: to upgrade, extract
an overlay from a newer release and run `emerge -avu jerc`. It does not
provide automatic repository synchronization. Upstream inclusion requires
submitting a separate ebuild to Gentoo maintainers. Only amd64 is supported by the
binary ebuild until an ARM64 Linux build is published; the source recipe can
be adapted for other Gentoo architectures after native testing.

`smoke-extra-linux-package.sh` validates the overlay layout and binary runtime
on Gentoo, while `verify-gentoo-overlay.py` verifies Manifest hashes. A full
Portage `emerge` build/install smoke test remains a separate follow-up.


## Native source recipe

The generated overlay also contains a `mail-client/jerc` source-build
ebuild. It uses Gentoo's `cargo.eclass` against the **vendored** locked
source release archive, so Cargo can compile offline. Use:

```sh
sudo emerge -av mail-client/jerc
```

`mail-client/jerc-bin` remains an optional precompiled alternative.
These are local overlay packages, not yet in the official Gentoo tree.
