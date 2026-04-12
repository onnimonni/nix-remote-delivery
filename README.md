# nix-remote-delivery

Deploy NixOS by syncing source to remote servers and building there — for when your local machine has slow internet but your server has a fast datacenter connection.

## Why?

Traditional NixOS deployment tools (`nixos-rebuild`, `deploy-rs`, `colmena`) either:
- **Build locally** and copy the closure (~GB) over your uplink — painful on slow connections
- **Build remotely** via `nix build --store ssh-ng://` — but still evaluate and transfer derivations through your machine

`nix-remote-delivery` takes a different approach:
1. **Evaluates** config locally using what's already in your Nix store (no downloads, ~15s)
2. **Syncs** only git-tracked source files via tar-over-ssh (~400KB compressed)
3. **Builds** entirely on the server using its fast CPU and datacenter bandwidth
4. **Caches** built paths to external storage so server resets don't mean rebuilding from scratch

## Install

### As a flake input (recommended)

```nix
# flake.nix
{
  inputs.nix-remote-delivery = {
    url = "github:onnimonni/nix-remote-delivery";
    inputs.nixpkgs.follows = "nixpkgs";
  };
}
```

Then use the package:
```nix
# In your NixOS config, devenv.nix, or shell
nix-remote-delivery = inputs.nix-remote-delivery.packages.${system}.default;
```

Or use the overlay for a simpler package reference:
```nix
# flake.nix
{
  inputs.nix-remote-delivery.url = "github:onnimonni/nix-remote-delivery";

  outputs = { self, nixpkgs, nix-remote-delivery, ... }: {
    overlays.default = nix-remote-delivery.overlays.default;
  };
}
```

```nix
# devenv.nix
{ pkgs, ... }:
{
  packages = [
    pkgs.nix-remote-delivery
  ];
}
```

You can also run the tool directly from the flake without adding it to `PATH`:
```bash
nix run github:onnimonni/nix-remote-delivery -- --help
```

### With devenv

```yaml
# devenv.yaml
inputs:
  nix-remote-delivery:
    url: github:onnimonni/nix-remote-delivery
    inputs:
      nixpkgs:
        follows: nixpkgs
```

```nix
# devenv.nix
{ inputs, pkgs, ... }:
let
  nix-remote-delivery = inputs.nix-remote-delivery.packages.${pkgs.system}.default;
in {
  packages = [ nix-remote-delivery ];
}
```

### From GitHub Releases (prebuilt binaries)

Binaries are built for `x86_64-linux`, `aarch64-linux`, `x86_64-darwin`, `aarch64-darwin`:

```bash
curl -L https://github.com/onnimonni/nix-remote-delivery/releases/latest/download/nix-remote-delivery-aarch64-darwin.tar.gz | tar xz
sudo mv nix-remote-delivery /usr/local/bin/
```

### From source

```bash
cargo install --git https://github.com/onnimonni/nix-remote-delivery
```

## Usage

```bash
# Deploy: eval → sync → build on server → activate
nix-remote-delivery my-server --flake .

# With binary cache (Hetzner Storage Box)
nix-remote-delivery my-server --flake . --cache u123@u123.your-storagebox.de

# Skip eval when iterating fast
nix-remote-delivery my-server --flake . --skip-eval

# Verbose output (shows SSH commands, file lists, timing)
nix-remote-delivery my-server --flake . -v

# Override the SSH control socket directory when /tmp is unavailable
NIX_REMOTE_DELIVERY_SSH_CONTROL_DIR="$PWD/.nrd-sockets" \
  nix-remote-delivery my-server --host 1.2.3.4 --flake .

# Initial install on a fresh server (kexec → disko → nixos-install)
nix-remote-delivery install my-server --host 1.2.3.4 --flake .
```

## How it works

### Deploy mode (default)

```
┌─────────────────────────────────────────────────────────────┐
│ Local machine                    │ Remote server            │
│                                  │                          │
│ 1. nix eval (validate config)   │                          │
│    ↓ (parallel)                  │                          │
│ 2. git ls-files → tar.gz        │                          │
│    ────── tar-over-ssh ────────→ │ 3. extract to           │
│                                  │    /etc/nixos/project    │
│                                  │                          │
│                                  │ 4. nixos-rebuild switch  │
│                                  │    (builds from source   │
│    ←── streaming output ──────── │     using server CPU +   │
│                                  │     datacenter internet) │
│                                  │                          │
│                                  │ 5. system activated      │
└─────────────────────────────────────────────────────────────┘
```

### Install mode

For provisioning a fresh server (any Linux → NixOS):

1. Downloads a [kexec](https://github.com/nix-community/nixos-images) tarball **on the server** (fast datacenter download)
2. Boots into a temporary NixOS installer via kexec (preserves SSH keys + network)
3. Syncs your flake source to the installer
4. Runs [disko](https://github.com/nix-community/disko) to partition and format disks
5. Runs `nixos-install` to install your NixOS config
6. Reboots into the new system

## Performance

| Scenario | Time | What happens |
|---|---|---|
| First deploy | ~26s | eval (17s) + sync (0.4s) + build (8s) |
| Repeat deploy, config unchanged | ~8s | eval cached + sync + rebuild (no-op) |
| With `--cache` flag | ~9s | mount + build with substituter + push diff |
| After `--skip-eval` | ~8s | sync + build only |

### Optimizations

- **Eval caching**: hashes all `*.nix` + `flake.lock` files. If unchanged since last successful eval, skips the 17s `nix eval` step entirely. Use `--force-eval` to override.
- **SSH ControlMaster**: opens one TCP connection, reuses it for all subsequent SSH calls (sync, build, cache, verify). Saves ~0.5s per call.
- **SSH keepalive**: `ServerAliveInterval=15` prevents connection drops during long builds.
- **Git-aware sync**: uses `git ls-files` to determine what to transfer. Respects `.gitignore` automatically.
- **Commit-based sync skip**: after deploy, writes the git commit hash to the server. On next deploy, if the working tree is clean and the commit matches, skips sync.
- **Build output filtering**: suppresses the hundreds of `/nix/store/...` derivation paths that `nixos-rebuild` dumps. Shows only actionable build progress.
- **TTY-aware build output**: interactive terminals get the live rebuild stream, while non-interactive sessions stay quiet unless there is a warning, failure, or activation milestone worth surfacing.
- **Error tail**: on build failure, shows the last 50 lines of output for diagnosis.
- **Closure-diff cache push**: after activation, compares the new system closure with the pre-build one and only pushes paths that are genuinely new.

## Binary cache

The `--cache` flag provides a persistent Nix binary cache backed by any SFTP-accessible storage (e.g., Hetzner Storage Box). **No NixOS config changes needed** — everything is handled on-demand by the tool.

### Architecture

```
┌────────────┐     SSHFS mount      ┌──────────────────┐
│   Server   │ ──── port 23 ──────→ │  Storage Box     │
│            │                       │  (1TB, €3.20/mo) │
│  nix copy  │ ← file:///mnt/cache  │  /nix-cache/     │
│  --from/to │                       │   *.narinfo      │
│            │                       │   nar/*.nar.xz   │
└────────────┘                       └──────────────────┘
```

### How it works

1. **Mount**: SSHFS-mounts the Storage Box to `/mnt/nix-cache` using the server's own SSH key. The tool retries transient mount failures automatically and prints the last SSHFS error if all attempts fail.
2. **Substituter**: passes `--option extra-substituters 'file:///mnt/nix-cache'` to `nixos-rebuild` — nix fetches from cache only what it needs
3. **Build**: anything not in cache is built normally (and pulled from `cache.nixos.org`)
4. **Push**: diffs the system closure before/after build, pushes only new paths via `nix copy --to file:///mnt/nix-cache`
5. **Unmount**: cleans up the SSHFS mount

### Setup (Hetzner Storage Box)

```bash
# 1. Create a Storage Box (1TB, Helsinki, SSH enabled)
hcloud storage-box create --name nix-cache --type bx11 --location hel1 \
  --password "$(openssl rand -base64 24)" --enable-ssh

# 2. Upload the server's SSH public key to the Storage Box
ssh root@your-server "cat /etc/ssh/ssh_host_ed25519_key.pub" > /tmp/server.pub
# Upload via SFTP to .ssh/authorized_keys on the Storage Box

# 3. Create the cache directory
ssh root@your-server \
  "echo 'mkdir nix-cache' | sftp -P 23 -i /etc/ssh/ssh_host_ed25519_key user@user.your-storagebox.de"

# 4. Deploy with cache
nix-remote-delivery my-server --flake . --cache user@user.your-storagebox.de
```

### Works with any SFTP storage

The `--cache` flag accepts any `user@host` that supports SFTP. Port 23 is the default for Hetzner Storage Boxes; override it with `--cache-port` for other SSHFS/SFTP endpoints.

## Options

```
nix-remote-delivery — deploy or install NixOS, building on the server

USAGE
    nix-remote-delivery [NODE] [OPTIONS]            Deploy
    nix-remote-delivery install [NODE] [OPTIONS]    Initial install

ARGS
    NODE                  NixOS flake node name  [default: stuffix-one]

OPTIONS
    --host <HOST>         SSH host               [default: same as NODE]
    --flake <PATH>        Path to flake          [default: .]
    --remote-path <PATH>  Remote source path     [default: /etc/nixos/stuffix]
    --skip-eval           Skip local nix eval entirely
    --force-eval          Force eval even if .nix files unchanged
    --cache <USER@HOST>   SFTP cache via SSHFS (e.g. u123@u123.storagebox.de)
    --cache-port <PORT>   SSH port for --cache      [default: 23]
    --verbose, -v         Show commands and extra details
    --kexec-url <URL>     Custom kexec tarball URL (install mode only)
    -h, --help            Show this help

ENVIRONMENT
    NIX_REMOTE_DELIVERY_SSH_CONTROL_DIR  Directory for SSH ControlPath sockets [default: system temp dir]
    TMPDIR                              Base temp dir for other local temp files
```

## Requirements

**Local machine** (where you run the tool):
- `nix` (for eval)
- `git` (for file listing)
- `ssh` (for everything else)

**Remote server**:
- NixOS (for deploy mode)
- Any Linux with SSH (for install mode — it installs NixOS)
- `sshfs` in system packages (only if using `--cache`)

## Comparison with other tools

| | nix-remote-delivery | deploy-rs | colmena | nixos-rebuild |
|---|---|---|---|---|
| Builds on | Server | Local | Local | Local or server |
| Transfers | Source (~KB) | Closure (~GB) | Closure (~GB) | Closure (~GB) |
| Eval location | Local | Local | Local | Local |
| Binary cache | SSHFS (no config) | Manual | Manual | Manual |
| Rollback | Via NixOS generations | Magic rollback | Via generations | Via generations |
| Install mode | kexec + disko | No | No | No |
| Bad internet friendly | Yes | No | No | Partially |

## License

MIT
