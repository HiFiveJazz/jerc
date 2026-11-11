# Linux distribution packages

- `aur/jerc` and `aur/jerc-bin`: Arch AUR source recipes (`PKGBUILD` and
  `.SRCINFO`). The `publish-aur.yml` workflow publishes these automatically.
- `fedora/nfpm.yaml`: nFPM configuration for one RPM tested on Fedora and openSUSE.
- `debian/nfpm.yaml`: nFPM configuration for the downloadable Debian/Ubuntu DEB.

## Fedora / openSUSE RPM

The `Release` workflow packages the **same** statically linked Linux x86-64
musl executable already distributed as a `.tar.gz` into an unsigned RPM and
installs and exercises it in Fedora 44 and openSUSE Tumbleweed containers
before publishing.

To build a package locally, install nFPM v2.48.0, put the corresponding
`jerc-vVERSION-x86_64-unknown-linux-musl.tar.gz` archive in `dist/`, then run:

```sh
./scripts/package-linux-rpm.sh v0.6.2
```

The RPM contains `/usr/bin/jerc`, the GPLv3 license, README and example
configuration. It depends on `ca-certificates`; tools such as `pdftoppm` and
`xdg-open` remain optional. It does not create a separate debug RPM.

The same downloadable RPM can be installed with `sudo dnf install ./jerc-*.rpm`
on Fedora or `sudo zypper install ./jerc-*.rpm` on openSUSE (accepting the
unsigned local package when prompted).
It is **not** a Fedora/Copr repository package and does not configure automatic
updates. A native source RPM and Copr project are future tasks.

## Debian / Ubuntu DEB

The `Release` workflow uses the *same* static musl archive to generate an unsigned
`jerc_VERSION-1_amd64.deb`, then installs and tests it independently on Debian
13 (Trixie) and Ubuntu 24.04 LTS. To build locally with nFPM v2.48.0 and a
matching archive in `dist/`:

```sh
./scripts/package-linux-deb.sh v0.6.2
```

The DEB contains `/usr/bin/jerc`, documentation, config example, and GPLv3
license text. It depends on `ca-certificates` and has no separate debug package.
Install the downloaded DEB using `sudo apt install ./jerc_*.deb`.
It does not configure an APT repository or automatic upgrades.

## Validate existing releases without retagging

The **Test Linux distribution packages** GitHub Actions workflow downloads
an existing tagged Linux release, packages it into both formats, then runs
four parallel installation tests: Fedora 44, openSUSE Tumbleweed, Debian 13,
and Ubuntu 24.04. It does **not** publish or replace release assets.

```sh
gh workflow run test-linux-packages.yml -f tag=v0.6.2
```

Future release-tag workflows also require both installers and all distro
checks to pass before publishing GitHub assets. The RPM remains unsigned;
Copr/OBS and an APT repository are separate future milestones.

## Nix / NixOS flake

`nix/default.nix` builds Jerc from Rust source and the checked-in Cargo.lock.
The repository-root `flake.nix` exposes `packages.<system>.jerc` and
`apps.<system>.jerc` for x86-64 and ARM64 Linux. The dedicated `Nix package`
workflow verifies the build. See [nix/README.md](nix/README.md).
Official nixpkgs inclusion still requires an upstream pull request.
