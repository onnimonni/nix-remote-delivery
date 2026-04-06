# nix-remote-delivery

Deploy NixOS by syncing source to remote servers and building there — designed for slow/bad local internet.

Instead of building locally and copying closures (slow with bad upload), this tool:
1. **Evaluates** your NixOS config locally (catches errors without downloading)
2. **Syncs** only your source files via tar-over-ssh (~400KB)
3. **Builds** directly on the server using its fast CPU and datacenter internet
4. **Caches** built paths to a Hetzner Storage Box via SSHFS for instant rebuilds

## Install

```bash
cargo install --git https://github.com/onnimonni/nix-remote-delivery
```

Or in a Nix flake:
```nix
nixos-deploy = pkgs.rustPlatform.buildRustPackage {
  pname = "nix-remote-delivery";
  version = "0.1.0";
  src = ./tools/nix-remote-delivery;
  cargoLock.lockFile = ./tools/nix-remote-delivery/Cargo.lock;
  doCheck = false; # tests need git
};
```

## Usage

```bash
# Deploy (eval → sync → build on server → activate)
nix-remote-delivery stuffix-one --flake .

# With binary cache on Hetzner Storage Box
nix-remote-delivery stuffix-one --flake . --cache u123@u123.your-storagebox.de

# Skip eval (faster, less safe)
nix-remote-delivery stuffix-one --flake . --skip-eval

# Force eval even if .nix files unchanged
nix-remote-delivery stuffix-one --flake . --force-eval

# Initial install (kexec → disko → nixos-install)
nix-remote-delivery install stuffix-one --host 1.2.3.4 --flake .
```

## Performance

| Scenario | Time |
|---|---|
| First deploy | ~26s |
| Repeat deploy (no changes) | ~8s |
| With cache push/pull | ~9s |

### What makes it fast

- **Eval caching**: hashes `*.nix` + `flake.lock` — skips eval when unchanged (~17s saved)
- **SSH ControlMaster**: reuses TCP connections across SSH calls (~0.5s saved per call)
- **Git-aware sync**: only transfers git-tracked files, always sends everything (~400KB compressed)
- **Commit-based sync skip**: stores deploy marker on server, skips sync when same commit
- **Build output filtering**: suppresses `/nix/store/...` derivation list noise
- **Closure-diff cache push**: only pushes newly-built paths, not the entire closure

## Cache

The `--cache` flag mounts a Hetzner Storage Box via SSHFS and uses it as a `file://` substituter during `nixos-rebuild`. No NixOS config changes needed.

**Setup:**
1. Create a Hetzner Storage Box (`hcloud storage-box create --name nix-cache --type bx11 --location hel1 --enable-ssh`)
2. Add the server's SSH public key to the Storage Box
3. Create the cache directory: `sftp -P 23 user@host` → `mkdir nix-cache`
4. Use `--cache user@host.your-storagebox.de`

**How it works:**
- Before build: mounts Storage Box, adds `file:///mnt/nix-cache` as `extra-substituters`
- During build: nix fetches from cache instead of rebuilding
- After build: diffs closures, pushes only new paths to cache
- After push: unmounts Storage Box

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
    --cache <USER@HOST>   SFTP cache via SSHFS
    --verbose, -v         Show commands and extra details
    --kexec-url <URL>     Custom kexec tarball URL (install mode only)
    -h, --help            Show this help
```

## License

MIT
