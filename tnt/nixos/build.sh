#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
destination="$script_dir/wumpa.raw.tar.gz"
builder="wumpa-nix-builder-$(perl -MTime::HiRes=time -e 'printf "%.0f", int(time() * 1000)')"

lima_exec() {
  limactl shell "$builder" bash -lc "$1"
}

# Delete only VMs reserved for this build workflow, including failed builds.
existing_builders=$(limactl list --format '{{.Name}}')
builders=$(printf '%s\n' "$existing_builders" | awk '/^wumpa-nix/')
if [[ -n "$builders" ]]; then
  printf '%s\n' "$builders" | xargs limactl delete --force
fi

limactl start --name="$builder" \
  --vm-type=qemu --arch=x86_64 \
  --cpus=4 --memory=8 --disk=50 \
  --containerd=none --yes template:ubuntu

lima_exec '
  curl -fL https://nixos.org/nix/install -o /tmp/install-nix &&
  sh /tmp/install-nix --no-daemon
'

lima_exec '
  . ~/.nix-profile/etc/profile.d/nix.sh &&
  nix-channel --add https://nixos.org/channels/nixos-unstable nixpkgs &&
  nix-channel --update
'

# Escape the local path for the VM's Bash shell (including spaces and quotes).
printf -v source_dir '%q' "$script_dir"
lima_exec "
  mkdir -p ~/image-build &&
  cp $source_dir/server.nix $source_dir/image.nix ~/image-build/
"

lima_exec '
  . ~/.nix-profile/etc/profile.d/nix.sh &&
  cd ~/image-build &&
  nix-build image.nix
'

lima_exec '
  ls -lh ~/image-build/result/ &&
  gzip -t ~/image-build/result/*.raw.tar.gz &&
  tar -tzf ~/image-build/result/*.raw.tar.gz &&
  cp -L ~/image-build/result/*.raw.tar.gz /tmp/wumpa.raw.tar.gz
'

remote_checksum=$(lima_exec 'sha256sum /tmp/wumpa.raw.tar.gz')
remote_checksum=${remote_checksum%% *}

limactl copy "$builder:/tmp/wumpa.raw.tar.gz" "$destination"
local_checksum=$(shasum -a 256 "$destination")
local_checksum=${local_checksum%% *}

if [[ "$local_checksum" != "$remote_checksum" ]]; then
  printf 'Checksum mismatch: %s\nVM left running for inspection.\n' "$destination" >&2
  exit 1
fi

limactl stop "$builder"
limactl delete --force "$builder"
