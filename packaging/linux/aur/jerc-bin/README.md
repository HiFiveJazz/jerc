# `jerc-bin` AUR packaging

This directory is the **source** for an AUR package. It installs Jerc's official
fully static Linux x86-64 musl release binary, not a rebuilt Rust executable.
It is independent of Cargo's default target and does not change Jerc's runtime.

## Before submitting

1. Verify that the upstream release asset exists and that `PKGBUILD` has the
   matching SHA-256 checksum (updated automatically by GitHub Actions).
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
cp ~/GitHub/jerc/packaging/linux/aur/jerc-bin/PKGBUILD .
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

The `publish-aur.yml` GitHub Actions workflow automates release updates:
`scripts/update-aur-release.sh` refreshes versions and checksums, computes
`pkgrel` against the published AUR package, and regenerates `.SRCINFO`.
`scripts/publish-aur.sh` pushes the two metadata files to AUR. Packaging-only
changes pushed to `main` trigger this workflow without a new GitHub release.
