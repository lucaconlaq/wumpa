{ lib, modulesPath, pkgs, ... }:

{
  imports = [(modulesPath + "/virtualisation/google-compute-image.nix")];

  # Also select the target platform when rebuilding from macOS.
  nixpkgs.hostPlatform = "x86_64-linux";

  # The GCE module enables OS Login by default; use metadata SSH keys instead.
  security.googleOsLogin.enable = lib.mkForce false;

  # The GCP guest agent needs mutable users to manage metadata SSH keys.
  users.mutableUsers = true;
  users.defaultUserShell = pkgs.zsh;
  users.users.wumpa = {
    isNormalUser = true;
    description = "Wumpa";
    extraGroups = [ "google-sudoers" ];
  };

  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "no";
      PasswordAuthentication = false;
      KbdInteractiveAuthentication = false;
    };
  };

  environment.systemPackages = with pkgs; [
    curl
    fd
    git
    jq
    mise
    nodejs_24
    ripgrep
    tree
    unzip
    zip
  ];

  # Support generic Linux binaries downloaded by development tools.
  programs.nix-ld.enable = true;

  programs.zsh = {
    enable = true;
    interactiveShellInit = ''
      eval "$(${pkgs.mise}/bin/mise activate zsh)"
    '';
  };

  programs.tmux = {
    enable = true;
    extraConfig = ''
      set -g status off
    '';
  };

  programs.starship = {
    enable = true;
    # Local Starship theme plus the server hostname; inline for SSH updates.
    settings = builtins.fromTOML ''
      # Show selected modules, with time right-aligned and input on a new line.
      format = '$hostname''${custom.tmux}$directory$git_branch$git_status$cmd_duration$status$jobs$fill$time$line_break$character'

      [hostname]
      ssh_only = false
      style = '#a6e3a1'
      format = '[$hostname]($style) '

      [custom.tmux]
      when = 'test -n "$TMUX"'
      command = 'tmux display-message -p -t "$TMUX_PANE" "#{session_name}"'
      shell = ['sh']
      symbol = ' '
      style = '#a6e3a1'
      format = '[\[$symbol$output\]]($style) '

      [status]
      disabled = false
      symbol = ' '
      format = '[$symbol$status]($style) '
      style = '#f38ba8'

      [jobs]
      disabled = false
      style = '#f9e2af'

      [fill]
      symbol = ' '

      [directory]
      style = '#89b4fa'
      read_only_style = '#f38ba8'
      truncate_to_repo = false
      truncation_length = 0
      format = '[ $path]($style)[$read_only]($read_only_style) '

      [git_branch]
      symbol = ' '
      style = '#cba6f7'

      [git_status]
      style = '#cba6f7'

      [character]
      success_symbol = '[❯](#a6e3a1)'
      error_symbol = '[❯](#f38ba8)'
      vimcmd_symbol = '[❮](#cba6f7)'

      [cmd_duration]
      min_time = 2_000
      style = '#f9e2af'

      [time]
      disabled = false
      style = '#9399b2'
      time_format = '%H:%M:%S'
      format = '[ $time]($style)'
    '';
  };

  programs.ssh.knownHosts.github = {
    hostNames = [ "github.com" ];
    publicKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
  };

  networking.hostName = "wumpa";

  networking.firewall = {
    enable = true;
    allowedTCPPorts = [ 22 ];
  };

  # Compatibility baseline for this new installation, not the nixpkgs version.
  system.stateVersion = "25.11";
}
