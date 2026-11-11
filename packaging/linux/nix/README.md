# Nix / NixOS

Jerc ships a **source-built** Nix flake for `x86_64-linux` and
`aarch64-linux`, using `rustPlatform.buildRustPackage` and the checked-in
`Cargo.lock`. It does not use the precompiled musl archive and does not need
manually maintained Cargo dependency hashes.

With flakes enabled in Nix (`nix-command flakes`):

```sh
# Try it without installing
nix run github:HiFiveJazz/jerc -- --version

# Install into your user profile
nix profile install github:HiFiveJazz/jerc

# Test a local checkout
nix build .#jerc
./result/bin/jerc --version
```

To use Jerc declaratively in another NixOS flake, add an input:

```nix
inputs.jerc.url = "github:HiFiveJazz/jerc";
```

Then add `jerc.packages.${pkgs.system}.default` to
`environment.systemPackages` in a NixOS module (or to
`home.packages` in Home Manager). Pin this flake input using your system's
`flake.lock` as normal.

**Not yet in official nixpkgs.** `nix profile install nixpkgs#jerc` and
`environment.systemPackages = [ pkgs.jerc ];` will only work after a
package is submitted, reviewed, and merged upstream in
[NixOS/nixpkgs](https://github.com/NixOS/nixpkgs). That submission requires
its own `pkgs/by-name/je/jerc/package.nix` using fixed-source hashes and
nixpkgs maintainers' review. The Jerc flake is immediately usable without
waiting for that submission.

`nix flake check` and `nix build .#jerc` are exercised in GitHub Actions.
The repository flake currently tracks `nixos-unstable`; generate and commit
a root `flake.lock` with `nix flake lock` to pin that dependency before
relying on a fully reproducible long-term build. The GitHub CI runner can
be used to test the initial Nix build even without local Nix installed.
