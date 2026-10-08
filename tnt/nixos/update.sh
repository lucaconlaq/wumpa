#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
target=tnt

# Transfer only the configuration; evaluate and build with the server's nixpkgs.
# A unique temporary file keeps concurrent uploads from overwriting each other.
exec ssh "$target" '
  set -eu
  config=$(mktemp /tmp/wumpa-update.XXXXXX.nix)
  trap "rm -f -- $config" EXIT
  cat > "$config"
  sudo -n nixos-rebuild switch -I "nixos-config=$config"
' < "$script_dir/server.nix"
