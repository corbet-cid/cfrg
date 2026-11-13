# Nix packaging of cfrg: the binaries `cfrg` and `cfrg-serve`, a container image of cfrg-serve and a small NixOS module.
# The workspace is read as it is (Cargo.lock beside this file); no import from derivation. The only git dependency (cqlt) needs its hash below:
# when the cqlt rev in Cargo.toml moves, set the hash to lib.fakeHash once and copy the one Nix reports.
{
  description = "cfrg: the forge bridge (cfrg, cfrg-serve)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = f: nixpkgs.lib.genAttrs systems (s: f s nixpkgs.legacyPackages.${s});
      rev = self.rev or "unversioned"; # a dirty tree builds as "unversioned", like a local cargo build
      mk = pkgs: { pname, package, extraNative ? [ ] }:
        pkgs.rustPlatform.buildRustPackage {
          inherit pname;
          version = "0.1.0-${builtins.substring 0 8 rev}";
          src = self;
          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes."cqlt-0.1.0" = "sha256-CNoV6S+339tZGgSCU57kDc1F8r7T0RWc7Ee9nrHkf34=";
            outputHashes."cglb-0.1.0" = "sha256-JjiroSMe3+Va4ytu4mxRQFpc/DW099qpuemYln4+iuM=";
            outputHashes."cghb-0.1.0" = "sha256-E8wkOZZjEapEepkPafKN+Tcooz6VQcMfbLuF6JLVYQc=";
            outputHashes."cfgj-0.1.0" = "sha256-RiOocvcPKA8oZIitLahym2mz4amPGKvPblnlxZdcwSc=";
            outputHashes."cbkt-0.1.0" = "sha256-BuSpc2CDzo5b816b5w2oAd54idHtfzPOqDWi3Nb71rk=";
          };
          cargoBuildFlags = [ "-p" package ];
          env = pkgs.lib.optionalAttrs (self ? rev) { CFRG_SOURCE_REVISION = self.rev; };
          nativeBuildInputs = [ pkgs.pkg-config pkgs.cmake ] ++ extraNative;
          dontUseCmakeConfigure = true;
          preBuild = "unset AWS_LC_SYS_USE_SYSTEM"; # newer nixpkgs hooks demand a system AWS-LC; aws-lc-sys builds its own copy
          doCheck = false; # the repository's own CI runs the tests
          passthru = { inherit rev; };
          meta = { mainProgram = pname; license = pkgs.lib.licenses.lgpl3Only; };
        };
    in
    {
      packages = forAll (system: pkgs:
        let
          cfrg = mk pkgs { pname = "cfrg"; package = "cfrg-cli"; }; # cmake: aws-lc-sys may build with it
          cfrg-serve = mk pkgs { pname = "cfrg-serve"; package = "cfrg-serve"; };
        in
        {
          inherit cfrg cfrg-serve;
          default = cfrg;
          # The receiver image: the binary and the CA bundle, nothing else.
          cfrg-serve-image = pkgs.dockerTools.buildLayeredImage {
            name = "corbet/cfrg-serve";
            tag = builtins.substring 0 8 rev;
            contents = [ cfrg-serve pkgs.cacert pkgs.dockerTools.fakeNss ];
            extraCommands = "mkdir -m 1777 tmp";
            config = {
              Env = [ "PATH=/bin" "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
              User = "65534:65534";
            };
          };
        });

      # `programs.cfrg.enable` puts cfrg on the system path; services are the deployer's own configuration.
      nixosModules.default = { config, lib, pkgs, ... }: {
        options.programs.cfrg.enable = lib.mkEnableOption "the cfrg command line";
        config = lib.mkIf config.programs.cfrg.enable {
          environment.systemPackages = [ self.packages.${pkgs.stdenv.hostPlatform.system}.cfrg ];
        };
      };
    };
}
