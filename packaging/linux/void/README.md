# Void Linux XBPS

`package-linux-xbps.sh` uses the native `xbps-create` tool to generate two
packages from the verified Jerc static musl binary: `x86_64` for Void glibc,
and `x86_64-musl` for Void musl. The packages contain executable, license,
documentation and example configuration, and require `ca-certificates`.

To build, use a Void environment with `xbps-create` and the matching tarball
in `dist/`:

```sh
./scripts/package-linux-xbps.sh v0.6.3
```

The GitHub Release uploads `.xbps` archives, but these alone do **not** create a
signed update repository. For local installation, create an XBPS index:

```sh
mkdir -p /tmp/jerc-xbps
cp jerc-0.6.3_1.$(xbps-uhelper arch).xbps /tmp/jerc-xbps/
xbps-rindex -a /tmp/jerc-xbps/*.xbps
sudo xbps-install --repository=/tmp/jerc-xbps -y jerc
```

An official Void `void-packages` template and upstream review are required to
make Jerc available directly via regular `xbps-install -S jerc`.


## Native source recipe

`source/template.in` is the maintained xbps-src `build_style=cargo`
recipe. CI renders it to `dist/source-recipes/void/srcpkgs/jerc/template`
with the release's vendored archive SHA256. Void builds the Rust source
for its own libc/architecture. Replace the example maintainer address
before submitting to void-packages.
