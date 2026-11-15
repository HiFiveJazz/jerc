# Source-built Jerc for Nix. The root flake passes src = self; users of
# this expression outside flakes can rely on the repository-relative default.
{
  lib,
  rustPlatform,
  pkg-config,
  src ? ../../..,
}:
let
  manifest = builtins.fromTOML (builtins.readFile "${src}/Cargo.toml");
in
rustPlatform.buildRustPackage {
  pname = "jerc";
  version = manifest.package.version;
  inherit src;

  # No cargoHash maintenance: nixpkgs vendors dependencies from Cargo.lock.
  cargoLock.lockFile = "${src}/Cargo.lock";
  nativeBuildInputs = [ pkg-config ];
  doCheck = true;

  # Account-setup tests exercise private credential storage. Nix builders do
  # not have a writable user home, so provide an isolated XDG data directory.
  # Precreate the private credential folder to avoid parallel-test races.
  preCheck = ''
    export XDG_DATA_HOME="$TMPDIR/jerc-test-data"
    install -d -m 0700 "$XDG_DATA_HOME/jerc/credentials"
  '';

  meta = {
    description = "Keyboard-driven terminal email client written in Rust";
    homepage = "https://github.com/HiFiveJazz/jerc";
    license = [ lib.licenses.gpl3Only lib.licenses.cc-by-sa-40 ];
    mainProgram = "jerc";
    platforms = lib.platforms.linux;
  };
}
