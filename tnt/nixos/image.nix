let
  nixos = import <nixpkgs/nixos> {
    configuration = ./server.nix;
    system = "x86_64-linux";
  };
in
  # Allow the image builder to use software emulation when KVM is unavailable.
  nixos.pkgs.lib.overrideDerivation
    nixos.config.system.build.googleComputeImage
    (_: { requiredSystemFeatures = []; })
