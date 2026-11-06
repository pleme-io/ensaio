{
  description = "Ensaio (Portuguese, trial/rehearsal/assay) — the typed architectural-trial primitive. Declares N ephemeral systems, wires them, runs a typed catalog of assertions, records a durable verdict, tears down. Ships the `ensaio` CLI and controller, the `ensaio-mcp` MCP surface, and the `utsuroi-lifecycle` library.";

  inputs = {
    substrate.url = "github:pleme-io/substrate";
    nixpkgs.follows = "substrate/nixpkgs";
  };

  outputs = { substrate, nixpkgs, ... }:
    let
      base = substrate.rust.library {
        src = ./.;
        member = "ensaio";
      };
      mcpBase = substrate.rust.library {
        src = ./.;
        member = "ensaio-mcp";
      };
      systems = [ "aarch64-darwin" "x86_64-darwin" "x86_64-linux" "aarch64-linux" ];
      packages = nixpkgs.lib.genAttrs systems (system:
        (base.packages.${system} or { }) // {
          ensaio-mcp = mcpBase.packages.${system}.default;
        });
      devShells = nixpkgs.lib.mapAttrs
        (system: shells:
          let pkgs = import nixpkgs { inherit system; };
          in shells // {
            default = shells.default.overrideAttrs (old: {
              nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.pkg-config ];
              buildInputs = (old.buildInputs or [ ]) ++ [ pkgs.openssl ];
            });
          })
        (base.devShells or { });
    in
    base // {
      inherit packages devShells;
    };
}
