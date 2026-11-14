# Installing Jerc

Choose the installation method that fits your operating system or package manager.

[Linux](#linux) · [macOS](#macos) · [Windows](#windows) · [Cargo](#cargo-universal)

**Latest Release:** [GitHub Releases](https://github.com/HiFiveJazz/jerc/releases/latest)
## Linux

### Arch Linux, CachyOS, EndeavourOS, Manjaro (AUR)

**Prebuilt binary (recommended):**

```sh
yay -S jerc-bin
# paru -S jerc-bin
```

**Build from source:**

```sh
yay -S jerc
# paru -S jerc
```

### Debian, Ubuntu, Linux Mint, Pop!_OS (DEB)

Download the **amd64 `.deb`** from [GitHub Releases](https://github.com/HiFiveJazz/jerc/releases/latest), then install it from the download directory:

```sh
sudo apt install ./jerc_*.deb
jerc --version
```

> [!Note]
> `.deb` packages are tested on Debian 13 and Ubuntu 24.04 LTS before each release. If you are on another derivative and installation does not work, please open up an issue and I will work to fix it for you! 
### Fedora (RPM)

Download the **x86-64 `.rpm`** from [GitHub Releases](https://github.com/HiFiveJazz/jerc/releases/latest):

```sh
sudo dnf install ./jerc-*.rpm
jerc --version
```


> [!Note]
> Work is being done to release on official Fedora repositories and Copr! If this project is useful to you, consider helping out!

### openSUSE (RPM)

Download the same **x86-64 `.rpm`** from [GitHub Releases](https://github.com/HiFiveJazz/jerc/releases/latest):

```sh
sudo zypper install ./jerc-*.rpm
jerc --version
```

> [!Note]
> Work is being done to release on official openSUSE repositories!

### Nix / NixOS

Jerc ships a source-built flake for **x86_64-linux** and **aarch64-linux** (with Nix flakes enabled).

Try it without installing:

```sh
nix run github:HiFiveJazz/jerc -- --version
```

Install it in your profile:

```sh
nix profile install github:HiFiveJazz/jerc
```

For NixOS or Home Manager, add `github:HiFiveJazz/jerc` as a flake input, then add `jerc.packages.${pkgs.system}.default` to `environment.systemPackages` or `home.packages`. Pin the input through your flake lockfile. See the [Nix packaging guide](../packaging/linux/nix/README.md).

Jerc has **not** been merged into official `nixpkgs`; `nixpkgs#jerc` and `pkgs.jerc` are not available yet.

### Alpine Linux (APK)

Download the **x86-64 `.apk`** from [GitHub Releases](https://github.com/HiFiveJazz/jerc/releases/latest):

```sh
sudo apk add --allow-untrusted ./jerc-*.apk
jerc --version
```

The downloaded APK is unsigned; `--allow-untrusted` is needed for this local package. Verify its [SHA-256 checksum](#verify-downloads) first. This does not add Jerc to Alpine's `aports` repository.

### Void Linux (XBPS)

Download the matching package for **x86_64 (glibc)** or **x86_64-musl** from [GitHub Releases](https://github.com/HiFiveJazz/jerc/releases/latest). From the directory containing the downloaded `.xbps` file:

```sh
repo="$HOME/.cache/jerc-xbps"
mkdir -p "$repo"
cp jerc-*_1."$(xbps-uhelper arch)".xbps "$repo/"
xbps-rindex -a "$repo"/*.xbps
sudo xbps-install --repository="$repo" -y jerc
jerc --version
```

This creates a **local XBPS repository index** for the downloaded package. It does not configure a remotely managed update repository. See the [Void packaging guide](../packaging/linux/void/README.md).

### Gentoo (local Portage overlay)

Download `jerc-gentoo-overlay-vVERSION.tar.gz` from [GitHub Releases](https://github.com/HiFiveJazz/jerc/releases/latest), and extract the one you downloaded:

```sh
sudo mkdir -p /var/db/repos /etc/portage/repos.conf
sudo tar -xzf jerc-gentoo-overlay-v*.tar.gz -C /var/db/repos
printf '[jerc-local]\nlocation = /var/db/repos/jerc-overlay\nauto-sync = no\n' \
  | sudo tee /etc/portage/repos.conf/jerc-local.conf
sudo emerge -av mail-client/jerc-bin
```

For a source build, use `sudo emerge -av mail-client/jerc` instead. This is a **manually updated local overlay**, not an ebuild from the official Gentoo tree. The source ebuild has not yet had a full `emerge` build/install test in release CI. See the [Gentoo packaging guide](../packaging/linux/gentoo/README.md).

### Portable Linux binary (no package manager)

For **x86-64 Linux**, download `jerc-vVERSION-x86_64-unknown-linux-musl.tar.gz` from [GitHub Releases](https://github.com/HiFiveJazz/jerc/releases/latest). From the directory containing the downloaded archive:

```sh
tar -xzf jerc-v*-x86_64-unknown-linux-musl.tar.gz
sudo install -m 755 jerc-v*-x86_64-unknown-linux-musl/jerc /usr/local/bin/jerc
jerc --version
```

This binary is fully statically linked (musl), with no glibc or shared-library dependency. You still need system CA certificates for secure mail connections. To avoid using `sudo`, install it to `~/.local/bin` instead, ensuring that directory is in your `PATH`.

## macOS

Download the archive matching your Mac from [GitHub Releases](https://github.com/HiFiveJazz/jerc/releases/latest):

- **Apple Silicon (M-series):** `aarch64-apple-darwin.tar.gz`
- **Intel:** `x86_64-apple-darwin.tar.gz`

After downloading **one** matching archive, extract and install it:

```sh
tar -xzf jerc-v*-apple-darwin.tar.gz
mkdir -p "$HOME/.local/bin"
cp jerc-v*-apple-darwin/jerc "$HOME/.local/bin/jerc"
chmod +x "$HOME/.local/bin/jerc"
"$HOME/.local/bin/jerc" --version
```

Make sure `~/.local/bin` is in your `PATH` if you want to run `jerc` from any directory. macOS release binaries are currently **unsigned and not notarized**, so Gatekeeper may require explicit approval after you verify the download.

## Windows

Download `jerc-vVERSION-x86_64-pc-windows-msvc.zip` from [GitHub Releases](https://github.com/HiFiveJazz/jerc/releases/latest), extract it, and run `jerc.exe` from the extracted folder. To run Jerc from any PowerShell window, put the executable in a directory included in your user `PATH`.

The release pipeline also produces Scoop, WinGet, and Chocolatey packaging files. **Those manifests/packages are not yet listed in their public package-manager catalogs**, so `scoop install jerc`, `winget install ...`, and `choco install jerc` should not be documented as publicly available commands yet. See the [Windows packaging status](../packaging/windows/README.md).

## Cargo (Universal)

If you have [Rust and Cargo](https://rustup.rs/) installed (Rust **1.98 or newer**) and don't care about integration with your package manager(s), you can use the following regardless of distribution or Operating System. If you prefer a prebuilt binary, ensure [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall) is installed, 

```sh
cargo binstall jerc
```

If you prefer compiling from source, the following option also works.

```sh
cargo install jerc --locked
jerc --version
```

## First run

Verify the executable and launch the client:

```sh
jerc --version
jerc
```

For an IMAP, SMTP, or OAuth account, start from the [sample configuration](../config.example.toml) and consult the [configuration guide](configuration.md) and [OAuth guide](oauth.md). On Linux, the usual configuration path is `~/.config/jerc/config.toml`. If you have a local Maildir, you can try `jerc --maildir ~/Mail`.

Optional PDF previews use `pdftoppm` (usually from Poppler); links can open through your system browser. These helpers are not required for basic email use.
