{
  description = "wisp";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    utils.url = "github:numtide/flake-utils";

    fenix = {
      url = "github:nix-community/fenix/monthly";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs:
    inputs.utils.lib.eachDefaultSystem (system: let
      pkgs = inputs.nixpkgs.legacyPackages.${system};

      rustToolchain = with inputs.fenix.packages.${system};
        latest.withComponents [
          "rustc"
          "cargo"
          "rustfmt"
          "clippy"
          "rust-src"
        ];

      bevyLibs = with pkgs; [
        udev
        alsa-lib
        vulkan-loader
        libGL
        libxkbcommon
        wayland
        xorg.libX11
        xorg.libXcursor
        xorg.libXi
        xorg.libXrandr
        stdenv.cc.cc.lib
      ];
    in {
      devShells.default = pkgs.mkShell {
        LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath bevyLibs;

        RUSTFLAGS = "-Clink-arg=-fuse-ld=${pkgs.mold}/bin/mold";
        RUSTC_WRAPPER = "${pkgs.sccache}/bin/sccache";

        packages = with pkgs; [
          rustToolchain
          sccache
          mold
          clang
          lld
          pkg-config
        ] ++ bevyLibs;
      };

      formatter = pkgs.nixpkgs-fmt;
    });
}
