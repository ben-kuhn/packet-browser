{ lib
, rustPlatform
, fetchFromGitHub
, pkg-config
, openssl
, stdenv
}:

rustPlatform.buildRustPackage rec {
  pname = "packet-browser-client";
  version = "0.2.0";

  src = fetchFromGitHub {
    owner = "yourusername";
    repo = "docker-packet-browser";
    rev = "v${version}";
    hash = lib.fakeHash; # Update with actual hash
  };

  cargoHash = lib.fakeHash; # Update with actual hash

  nativeBuildInputs = [
    pkg-config
  ];

  buildInputs = [
    openssl
  ];

  # Build only the client binary
  cargoBuildFlags = [ "--bin" "packet-browser-client" ];

  meta = with lib; {
    description = "Client component for Packet Browser - Web browser over AX.25 packet radio";
    homepage = "https://github.com/yourusername/docker-packet-browser";
    license = licenses.mit;
    maintainers = with maintainers; [ yourusername ];
    mainProgram = "packet-browser-client";
  };
}
