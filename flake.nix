{
  description = "Jerc, a keyboard-driven terminal email client written in Rust";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = f: nixpkgs.lib.genAttrs systems f;
    in
    {
      packages = eachSystem (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          jerc = pkgs.callPackage ./packaging/linux/nix/default.nix {
            src = self;
          };
        in
        {
          inherit jerc;
          default = jerc;
        });

      apps = eachSystem (system:
        let
          app = {
            type = "app";
            program = "${self.packages.${system}.jerc}/bin/jerc";
          };
        in
        {
          jerc = app;
          default = app;
        });

      checks = eachSystem (system: {
        jerc = self.packages.${system}.jerc;
      });
    };
}
