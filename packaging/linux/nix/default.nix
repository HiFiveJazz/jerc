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

  meta = {
    description = "Keyboard-driven terminal email client written in Rust";
    homepage = "https://github.com/HiFiveJazz/jerc";
    license = [ lib.licenses.gpl3Only lib.licenses.cc-by-sa-40 ];
    mainProgram = "jerc";
    platforms = lib.platforms.linux;
  };
}
