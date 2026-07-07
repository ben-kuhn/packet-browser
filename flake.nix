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
              # The nix-wrapped Firefox script sets LD_LIBRARY_PATH so the
              # ELF binary can find NSS + p11-kit (for the CA trust store).
              # We can't use the wrapped script because geckodriver rejects
              # anything that isn't an ELF as --binary, so we ship the same
              # libraries here and set LD_LIBRARY_PATH in Env below.
              pkgs.nss
              pkgs.p11-kit
              # p11-kit variant of the CA bundle lives under
              # /etc/ssl/trust-source/ca-bundle.trust.p11-kit and is what
              # security.enterprise_roots.enabled=true will pick up via
              # p11-kit's trust module.
              pkgs.cacert.p11kit
            ];
            pathsToLink = [ "/bin" "/etc" "/share" "/lib" ];
          };

          config = {
            Cmd = [ "/bin/dumb-init" "/bin/packet-browser-server" ];
            ExposedPorts = { "63004/tcp" = {}; };
            Env = [
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
              "FONTCONFIG_FILE=${pkgs.fontconfig.out}/etc/fonts/fonts.conf"
              "FIREFOX_PATH=/bin/firefox"
              "GECKODRIVER_PATH=/bin/geckodriver"
              # geckodriver inherits this and Firefox inherits it from
              # geckodriver, giving NSS a runtime path to libnssckbi.so
              # (built-in root CAs) and p11-kit's trust modules.
              "LD_LIBRARY_PATH=/lib"
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
