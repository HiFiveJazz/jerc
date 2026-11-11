# Linux distribution packages

- `aur/jerc` and `aur/jerc-bin`: Arch AUR source recipes (`PKGBUILD` and
  `.SRCINFO`). The `publish-aur.yml` workflow publishes these automatically.
- `fedora/nfpm.yaml`: nFPM configuration for the downloadable Fedora RPM.

## Fedora RPM

The `Release` workflow packages the **same** statically linked Linux x86-64
musl executable already distributed as a `.tar.gz` into an unsigned RPM and
installs and exercises it in a Fedora 44 container before publishing.

To build a package locally, install nFPM v2.48.0, put the corresponding
`jerc-vVERSION-x86_64-unknown-linux-musl.tar.gz` archive in `dist/`, then run:

```sh
./scripts/package-linux-rpm.sh v0.6.2
```

The RPM contains `/usr/bin/jerc`, the GPLv3 license, README and example
configuration. It depends on `ca-certificates`; tools such as `pdftoppm` and
`xdg-open` remain optional. It does not create a separate debug RPM.

The downloadable RPM can be installed with `sudo dnf install ./jerc-*.rpm`.
It is **not** a Fedora/Copr repository package and does not configure automatic
updates. A native source RPM and Copr project are future tasks.
