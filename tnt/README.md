# TNT server

The TNT server runs the Wumpa server. It is based on NixOS.

## Setup

Install and activate [mise](https://mise.jdx.dev), then install the repository tools:

```sh
mise trust
mise install
```

Also install Terraform (>=1.6, <2.0) and Google Cloud CLI (`gcloud`).
Authenticate with `gcloud auth login` and `gcloud auth application-default login`.
Your account needs project provisioning and IAP tunnel permissions.

For secrets, copy `psst.json.example` to `psst.json` if absent, then use
`mise exec -- psst` to configure them. Terraform secrets can use `TF_VAR_<name>`
environment variables; don't duplicate those values in `.tfvars`. Keep secrets
out of Git. The example only configures `GITHUB_TOKEN`, not Google credentials.

## Build the initial image

Requires an ARM Mac with Lima and QEMU:

```sh
./tnt/nixos/build.sh
```

`image.nix` packages `server.nix` into `tnt/nixos/wumpa.raw.tar.gz` (~30 minutes).
Arguments are ignored; existing output is overwritten. Every build deletes all
`wumpa-nix*` Lima VMs: reserve these names and don't build concurrently. Failed
builds leave their VM for inspection. nixpkgs is unpinned; builds may differ.

## Terraform

Copy `tnt/terraform/terraform.example.tfvars` to `terraform.tfvars` in the same
folder. Set the project details, your SSH public key, and `nixos_image_archive`
to `../nixos/wumpa.raw.tar.gz`. Match the backend bucket in `state.tf` to the project.

```sh
mise exec -- psst terraform -chdir=tnt/terraform init
mise exec -- psst terraform -chdir=tnt/terraform plan
mise exec -- psst terraform -chdir=tnt/terraform apply
```

For a new project without a state bucket: temporarily move `state.tf` outside the
Terraform folder, initialize/apply with local state, then restore it and run
`init -migrate-state` through the same psst wrapper. Keep local state out of Git.

Keep the archive at its configured path: plans require it, and renaming it forces
replacement of the uploaded object. Review every plan before applying.

## SSH

Get the IAP proxy command:

```sh
mise exec -- psst terraform -chdir=tnt/terraform output -raw ssh_proxy_command
```

Add to `~/.ssh/config`, replacing the key path and proxy placeholder:

```sshconfig
Host tnt
    HostName tnt
    User wumpa
    IdentityFile ~/.ssh/id_ed25519
    IdentitiesOnly yes
    ProxyCommand <paste ssh_proxy_command output here>
```

The private key must match `ssh_public_key`. Connect with `ssh tnt`; direct inbound
SSH is blocked, so `gcloud` must be available locally and authorized for IAP.

## Update the running server

```sh
./tnt/nixos/update.sh
```

Always targets SSH alias `tnt`; arguments are ignored. Sends only `server.nix`
to a temporary remote file, runs `sudo -n nixos-rebuild switch`, then removes it.
Requires passwordless sudo and the server's nixpkgs source; no local Nix needed.
Doesn't rebuild the image, run Terraform, update channels, or replace
`/etc/nixos/configuration.nix`. Use this script for subsequent updates.

Activation may restart services or interrupt SSH. Roll back with:

```sh
ssh tnt 'sudo nixos-rebuild switch --rollback'
```

The server includes Wumpa, Zsh, Starship, Git, mise, tmux, Node.js 24, and CLI utilities.
The system-wide shell alias `a` runs `wumpa agent --socket /run/wumpa/control.sock`.
