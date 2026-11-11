# `jerc-bin` AUR packaging

This directory is the **source** for an AUR package. It installs Jerc's official
fully static Linux x86-64 musl release binary, not a rebuilt Rust executable.
It is independent of Cargo's default target and does not change Jerc's runtime.

## Before submitting

1. Verify the upstream release asset and its checksum. The SHA-256 pinned in
   `PKGBUILD` matches the digest shown on the [v0.6.0 GitHub release](https://github.com/HiFiveJazz/jerc/releases/tag/v0.6.0)
   for `jerc-v0.6.0-x86_64-unknown-linux-musl.tar.gz`.
2. Add your AUR maintainer name/contact to the `PKGBUILD` comment.
3. On Arch Linux, from this directory, regenerate metadata and test:

   ```sh
   makepkg --printsrcinfo > .SRCINFO
   makepkg -s
   namcap PKGBUILD
   namcap jerc-bin-*.pkg.tar.zst
   sudo pacman -U jerc-bin-*.pkg.tar.zst
   jerc --version
   ```

   `makepkg` requires network access to download the GitHub release archive.
   Inspect the package file list with `pacman -Qlp jerc-bin-*.pkg.tar.zst`.

## Submit to the AUR

Create an AUR account and add your SSH public key to your AUR account settings.
**Check that the `jerc-bin` name is available** before creating the package.
Then publish the two metadata files to the separate AUR Git repository:

```sh
mkdir -p ~/aur && cd ~/aur
git clone ssh://aur@aur.archlinux.org/jerc-bin.git
cd jerc-bin
cp ~/GitHub/jerc/packaging/aur/jerc-bin/PKGBUILD .
makepkg --printsrcinfo > .SRCINFO
git add PKGBUILD .SRCINFO
git commit -m 'Initial import: jerc-bin 0.6.0-1'
git push
```

The AUR repository should contain **only** `PKGBUILD` and `.SRCINFO` for this
package. Do not push compiled binaries or release archives there.

Once published, Arch users can install it with `yay -S jerc-bin` or by cloning
the AUR repository and running `makepkg -si`.

## Future Jerc releases

Bump `pkgver`, reset `pkgrel=1`, update the upstream archive's SHA-256 (e.g.
with `updpkgsums` from `pacman-contrib`, after verifying the release), then
regenerate `.SRCINFO` and push a commit to the AUR repository. AUR package
updates are not automatically published by GitHub Releases.
