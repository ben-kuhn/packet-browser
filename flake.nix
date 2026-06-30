{
  description = "Packet Browser - Secure web browser for packet radio";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.stable.latest.default;

        packet-browser = pkgs.rustPlatform.buildRustPackage {
          pname = "packet-browser";
          version = "0.2.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.rustfmt
            pkgs.perl
          ];
          buildInputs = [ pkgs.openssl ];

          doCheck = false;
        };

        dockerImage = pkgs.dockerTools.buildImage {
          name = "packet-browser";
          tag = "latest";

          copyToRoot = pkgs.buildEnv {
            name = "image-root";
            paths = [
              packet-browser
              pkgs.firefox-esr-unwrapped
              pkgs.geckodriver
              pkgs.dumb-init
              pkgs.logrotate
              pkgs.cacert
              pkgs.fontconfig
              pkgs.liberation_ttf
              pkgs.noto-fonts
            ];
            pathsToLink = [ "/bin" "/etc" "/share" ];
          };

          config = {
            Cmd = [ "/bin/dumb-init" "/bin/packet-browser-server" ];
            ExposedPorts = { "63004/tcp" = {}; };
            Env = [
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              "FONTCONFIG_FILE=${pkgs.fontconfig.out}/etc/fonts/fonts.conf"
              "FIREFOX_PATH=/bin/firefox"
              "GECKODRIVER_PATH=/bin/geckodriver"
              # Firefox writes its profile + caches here; tempdirs land under /tmp.
              "TMPDIR=/tmp"
            ];
            User = "1000:1000";
          };

          runAsRoot = ''
            mkdir -p /var/log/packet-browser
            mkdir -p /tmp
            chown 1000:1000 /var/log/packet-browser
          '';
        };
      in
      {
        packages = {
          default = packet-browser;
          server = packet-browser;
          client = packet-browser;
          docker-image = dockerImage;
        };

        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
            pkgs.pkg-config
            pkgs.openssl
            pkgs.firefox-esr-unwrapped
            pkgs.geckodriver
            pkgs.direwolf
            pkgs.pipewire
            pkgs.python3
            pkgs.python3Packages.pytest
            pkgs.python3Packages.pytest-asyncio
            pkgs.python3Packages.requests
          ];
        };
      }
    );
}
