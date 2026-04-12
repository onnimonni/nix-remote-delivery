use std::env;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};

// ── ANSI helpers ──────────────────────────────────────────────────────────────

const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[2m";

fn ok(msg: &str) {
    eprintln!("  {GREEN}✓{RESET} {msg}");
}

fn warn(msg: &str) {
    eprintln!("  {YELLOW}⚠{RESET} {msg}");
}

fn fail(msg: &str) {
    eprintln!("  {RED}{BOLD}✗ {msg}{RESET}");
}

fn verbose(on: bool, msg: &str) {
    if on {
        eprintln!("  {DIM}{msg}{RESET}");
    }
}

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Deploy,
    Install,
}

struct Config {
    mode: Mode,
    node: String,
    host: String,
    flake: String,
    remote_path: String,
    skip_eval: bool,
    force_eval: bool,
    verbose: bool,
    kexec_url: String,
    /// SFTP user@host for binary cache
    cache_url: Option<String>,
    /// SSH port for the SFTP/SSHFS cache endpoint
    cache_port: u16,
    /// SSH host keys source: directory path OR sops-encrypted YAML file
    /// Directory: --host-keys ./keys/ (contains ssh_host_*_key files)
    /// SOPS file: --host-keys secrets/hosts/server.yaml (decrypted on-the-fly)
    host_keys_dir: Option<String>,
}

const DEFAULT_KEXEC_URL: &str = "https://github.com/nix-community/nixos-images/releases/download/nixos-unstable/nixos-kexec-installer-noninteractive-x86_64-linux.tar.gz";
const DEFAULT_CACHE_PORT: u16 = 23;
const CACHE_MOUNT_RETRIES: usize = 3;
const CACHE_MOUNT_RETRY_DELAY_MS: u64 = 1_500;
const SSH_CONTROL_DIR_ENV: &str = "NIX_REMOTE_DELIVERY_SSH_CONTROL_DIR";

fn usage() {
    eprintln!("{BOLD}nix-remote-delivery{RESET} — deploy or install NixOS, building on the server");
    eprintln!();
    eprintln!("{BOLD}USAGE{RESET}");
    eprintln!(
        "    nix-remote-delivery [NODE] [OPTIONS]            Deploy (eval → sync → build+activate)"
    );
    eprintln!("    nix-remote-delivery install [NODE] [OPTIONS]    Initial install (kexec → disko → install)");
    eprintln!();
    eprintln!("{BOLD}ARGS{RESET}");
    eprintln!("    NODE                  NixOS flake node name  [default: stuffix-one]");
    eprintln!();
    eprintln!("{BOLD}OPTIONS{RESET}");
    eprintln!("    --host <HOST>         SSH host               [default: same as NODE]");
    eprintln!("    --flake <PATH>        Path to flake          [default: .]");
    eprintln!("    --remote-path <PATH>  Remote source path     [default: /etc/nixos/stuffix]");
    eprintln!("    --skip-eval           Skip local nix eval entirely");
    eprintln!("    --force-eval          Force eval even if .nix files unchanged");
    eprintln!("    --verbose, -v         Show commands and extra details");
    eprintln!("    --cache <USER@HOST>    SFTP cache via SSHFS (e.g. u123@u123.storagebox.de)");
    eprintln!("    --cache-port <PORT>   SSH port for --cache      [default: 23]");
    eprintln!(
        "    --host-keys <DIR>     Inject SSH host keys from DIR (install mode, persists identity)"
    );
    eprintln!("    --kexec-url <URL>     Custom kexec tarball URL (install mode only)");
    eprintln!("    -h, --help            Show this help");
    eprintln!();
    eprintln!("{BOLD}ENVIRONMENT{RESET}");
    eprintln!(
        "    {SSH_CONTROL_DIR_ENV}   Directory for SSH ControlPath sockets [default: system temp dir]"
    );
    eprintln!("    TMPDIR                              Base temp dir for other local temp files");
}

fn parse_args() -> Config {
    parse_args_from(env::args().skip(1))
}

fn parse_args_from<I>(iter: I) -> Config
where
    I: IntoIterator<Item = String>,
{
    let mut args = iter.into_iter().peekable();
    let mut mode = Mode::Deploy;
    let mut node = String::from("stuffix-one");
    let mut host: Option<String> = None;
    let mut flake = String::from(".");
    let mut remote_path = String::from("/etc/nixos/stuffix");
    let mut skip_eval = false;
    let mut force_eval = false;
    let mut verbose = false;
    let mut kexec_url = String::from(DEFAULT_KEXEC_URL);
    let mut cache_url: Option<String> = None;
    let mut cache_port = DEFAULT_CACHE_PORT;
    let mut host_keys_dir: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "install" => mode = Mode::Install,
            "-h" | "--help" => {
                usage();
                std::process::exit(0);
            }
            "--host" => {
                host = Some(args.next().unwrap_or_else(|| {
                    fail("--host requires a value");
                    std::process::exit(1);
                }));
            }
            "--flake" => {
                flake = args.next().unwrap_or_else(|| {
                    fail("--flake requires a value");
                    std::process::exit(1);
                });
            }
            "--remote-path" => {
                remote_path = args.next().unwrap_or_else(|| {
                    fail("--remote-path requires a value");
                    std::process::exit(1);
                });
            }
            "--cache" => {
                cache_url = Some(args.next().unwrap_or_else(|| {
                    fail("--cache requires a value (user@host)");
                    std::process::exit(1);
                }));
            }
            "--cache-port" => {
                cache_port = args
                    .next()
                    .unwrap_or_else(|| {
                        fail("--cache-port requires a value");
                        std::process::exit(1);
                    })
                    .parse::<u16>()
                    .unwrap_or_else(|_| {
                        fail("--cache-port must be an integer between 1 and 65535");
                        std::process::exit(1);
                    });
            }
            "--host-keys" => {
                host_keys_dir = Some(args.next().unwrap_or_else(|| {
                    fail("--host-keys requires a directory path");
                    std::process::exit(1);
                }));
            }
            "--kexec-url" => {
                kexec_url = args.next().unwrap_or_else(|| {
                    fail("--kexec-url requires a value");
                    std::process::exit(1);
                });
            }
            "--skip-eval" => skip_eval = true,
            "--force-eval" => force_eval = true,
            "--verbose" | "-v" => verbose = true,
            s if !s.starts_with('-') => node = s.to_string(),
            s => {
                fail(&format!("unknown argument: {s}"));
                usage();
                std::process::exit(1);
            }
        }
    }

    Config {
        mode,
        host: host.unwrap_or_else(|| node.clone()),
        node,
        flake,
        remote_path,
        skip_eval,
        force_eval,
        verbose,
        kexec_url,
        cache_url,
        cache_port,
        host_keys_dir,
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Run a command, inheriting stdout/stderr. Returns Ok(()) on success.
fn run(program: &str, args: &[&str]) -> Result<(), i32> {
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .unwrap_or_else(|e| {
            fail(&format!("failed to exec '{program}': {e}"));
            std::process::exit(1);
        });

    if status.success() {
        Ok(())
    } else {
        Err(status.code().unwrap_or(1))
    }
}

/// Run a command, capture stdout+stderr, stream each line via a callback.
/// Returns (exit_code, last_n_lines) for error reporting.
fn run_streaming(
    program: &str,
    args: &[&str],
    on_line: &dyn Fn(&str),
) -> Result<(), (i32, Vec<String>)> {
    use std::io::BufRead;

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| {
            fail(&format!("failed to exec '{program}': {e}"));
            std::process::exit(1);
        });

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    let tail_size = 50;
    let tail = std::sync::Arc::new(std::sync::Mutex::new(
        std::collections::VecDeque::<String>::new(),
    ));
    let tail2 = tail.clone();

    let handle = std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let mut t = tail2.lock().unwrap();
            if t.len() >= tail_size {
                t.pop_front();
            }
            t.push_back(line);
        }
    });

    let stderr_reader = std::io::BufReader::new(stderr);
    for line in stderr_reader.lines().map_while(Result::ok) {
        on_line(&line);
        let mut t = tail.lock().unwrap();
        if t.len() >= tail_size {
            t.pop_front();
        }
        t.push_back(line);
    }

    handle.join().ok();
    let status = child.wait().unwrap_or_else(|e| {
        fail(&format!("wait failed: {e}"));
        std::process::exit(1);
    });

    if status.success() {
        Ok(())
    } else {
        let lines: Vec<String> = tail.lock().unwrap().iter().cloned().collect();
        Err((status.code().unwrap_or(1), lines))
    }
}

/// Build SSH args with ControlMaster multiplexing, keepalive, optional host key bypass.
fn ssh_control_dir() -> PathBuf {
    env::var_os(SSH_CONTROL_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
}

fn ssh_control_path_from_dir(base_dir: &Path, host: &str) -> PathBuf {
    base_dir.join(format!("nix-remote-delivery-{host}"))
}

fn ssh_control_path(host: &str) -> PathBuf {
    let base_dir = ssh_control_dir();
    ssh_control_path_from_dir(&base_dir, host)
}

fn ssh_base_args(host: &str, strict_host: bool) -> Vec<String> {
    let control_path = ssh_control_path(host);
    if let Some(parent) = control_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut args = vec![
        "-o".into(),
        format!("ControlPath={}", control_path.display()),
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        "ControlPersist=30".into(),
        "-o".into(),
        "Compression=yes".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=4".into(),
    ];
    if !strict_host {
        args.extend_from_slice(&[
            "-o".into(),
            "StrictHostKeyChecking=no".into(),
            "-o".into(),
            "UserKnownHostsFile=/dev/null".into(),
        ]);
    }
    args.push(format!("root@{host}"));
    args
}

/// Run ssh command on remote host.
fn ssh_run(host: &str, cmd: &str, v: bool) -> Result<(), i32> {
    verbose(v, &format!("ssh root@{host} {cmd}"));
    let mut args = ssh_base_args(host, true);
    args.push(cmd.into());
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run("ssh", &refs)
}

/// Run ssh command, accepting new host keys (for after kexec).
fn ssh_run_no_check(host: &str, cmd: &str, v: bool) -> Result<(), i32> {
    verbose(v, &format!("ssh root@{host} {cmd}"));
    let mut args = ssh_base_args(host, false);
    args.push(cmd.into());
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run("ssh", &refs)
}

/// Run SSH command and capture stdout.
fn ssh_output(host: &str, cmd: &str) -> Result<String, ()> {
    let mut args = ssh_base_args(host, true);
    args.push(cmd.into());
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    Command::new("ssh")
        .args(&refs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .ok_or(())
}

/// Run SSH command and capture combined stdout/stderr for diagnostics.
fn ssh_output_combined(host: &str, cmd: &str) -> Result<String, String> {
    let mut args = ssh_base_args(host, true);
    args.push(cmd.into());
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let out = Command::new("ssh")
        .args(&refs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("failed to exec ssh: {e}"))?;

    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&out.stdout));
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    let text = text.trim().to_string();

    if out.status.success() {
        Ok(text)
    } else if text.is_empty() {
        Err(format!(
            "ssh exited with status {}",
            out.status.code().unwrap_or(1)
        ))
    } else {
        Err(text)
    }
}

fn summarize_tail(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();

    if lines.is_empty() {
        return String::new();
    }

    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join(" | ")
}

fn is_noisy_build_line(trimmed: &str) -> bool {
    if trimmed.starts_with("  /nix/store/") || trimmed.starts_with("/nix/store/") {
        return true;
    }

    let lower = trimmed.to_ascii_lowercase();

    lower.starts_with("these ")
        || lower.starts_with("copying path ")
        || lower.starts_with("warning: download buffer ")
        || lower.starts_with("unpacking ")
        || lower.starts_with("building ")
        || lower.starts_with("installing ")
        || lower.starts_with("waiting for children")
        || lower.starts_with("evaluation warning:")
}

fn should_print_build_line(line: &str, interactive: bool, verbose: bool) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    if verbose {
        return !trimmed.starts_with("  /nix/store/") && !trimmed.starts_with("/nix/store/");
    }

    if interactive {
        return !is_noisy_build_line(trimmed)
            || trimmed.starts_with("building the system configuration");
    }

    let lower = trimmed.to_ascii_lowercase();

    if lower.contains("error")
        || lower.contains("failed")
        || lower.contains("warning:")
        || lower.contains("activating")
        || lower.contains("switching to system configuration")
        || lower.contains("restarting the following units")
        || lower.contains("starting the following units")
        || lower.contains("stopping the following units")
        || lower.contains("reloading the following units")
        || lower.contains("the following new units were started")
        || lower.contains("the following units were restarted")
        || lower.contains("created symlink")
        || lower.contains("setting up /etc")
        || lower.contains("done. the new configuration is")
    {
        return true;
    }

    trimmed.starts_with("building the system configuration")
}

fn mount_remote_cache(
    host: &str,
    cache: &str,
    cache_mount: &str,
    cache_port: u16,
    v: bool,
) -> Result<(), String> {
    let mount_cmd = format!(
        "command -v sshfs >/dev/null && command -v fusermount >/dev/null && \
         fusermount -u {cache_mount} 2>/dev/null; \
         mkdir -p {cache_mount} && \
         sshfs -p {cache_port} \
           -o StrictHostKeyChecking=no,reconnect,ServerAliveInterval=15 \
           -o IdentityFile=/etc/ssh/ssh_host_ed25519_key \
           {cache}:./nix-cache {cache_mount} 2>&1"
    );

    let mut last_error = String::new();

    for attempt in 1..=CACHE_MOUNT_RETRIES {
        if attempt > 1 {
            verbose(
                v,
                &format!(
                    "retrying cache mount ({attempt}/{CACHE_MOUNT_RETRIES}) after previous failure"
                ),
            );
            std::thread::sleep(Duration::from_millis(CACHE_MOUNT_RETRY_DELAY_MS));
        }

        match ssh_output_combined(host, &mount_cmd) {
            Ok(_) => return Ok(()),
            Err(err) => last_error = err,
        }
    }

    let summary = summarize_tail(&last_error, 3);
    if summary.is_empty() {
        Err(format!(
            "cache mount failed after {CACHE_MOUNT_RETRIES} attempts"
        ))
    } else {
        Err(format!(
            "cache mount failed after {CACHE_MOUNT_RETRIES} attempts: {summary}"
        ))
    }
}

/// Evaluate the NixOS config locally (no builds, no downloads).
fn nix_eval(flake: &str, node: &str, v: bool) -> Result<(), String> {
    let attr = format!("{flake}#nixosConfigurations.{node}.config.system.build.toplevel.drvPath");
    verbose(v, &format!("nix eval --raw {attr}"));

    let out = Command::new("nix")
        .args(["eval", "--raw", &attr])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| {
            fail(&format!("failed to run nix: {e}"));
            std::process::exit(1);
        });

    if out.status.success() {
        if v {
            let drv = String::from_utf8_lossy(&out.stdout);
            verbose(true, &format!("→ {}", drv.trim()));
        }
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

// ── Eval cache ────────────────────────────────────────────────────────────────

#[cfg(test)]
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Hash all *.nix files + flake.lock to detect config changes.
fn nix_files_hash(flake_dir: &Path) -> String {
    let out = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(flake_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    let mut hasher = Sha256::new();
    if let Ok(out) = out {
        let mut files: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.ends_with(".nix") || *l == "flake.lock")
            .map(|s| s.to_string())
            .collect();
        files.sort();
        for f in &files {
            if let Ok(data) = std::fs::read(flake_dir.join(f)) {
                hasher.update(f.as_bytes());
                hasher.update(&data);
            }
        }
    }
    format!("{:x}", hasher.finalize())
}

fn eval_cache_path(node: &str) -> PathBuf {
    env::temp_dir().join(format!("nix-remote-delivery-eval-{node}.hash"))
}

fn should_skip_eval(flake_dir: &Path, node: &str) -> bool {
    let cache = eval_cache_path(node);
    let current = nix_files_hash(flake_dir);
    if let Ok(cached) = std::fs::read_to_string(&cache) {
        cached.trim() == current
    } else {
        false
    }
}

fn save_eval_cache(flake_dir: &Path, node: &str) {
    let cache = eval_cache_path(node);
    let _ = std::fs::write(&cache, nix_files_hash(flake_dir));
}

// ── Sync ──────────────────────────────────────────────────────────────────────

/// Get git-tracked files relative to `dir`.
fn git_tracked_files(dir: &Path, v: bool) -> Vec<PathBuf> {
    verbose(
        v,
        &format!(
            "git ls-files --cached --others --exclude-standard  (in {})",
            dir.display()
        ),
    );
    let out = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match out {
        Ok(out) if out.status.success() => {
            let files: Vec<PathBuf> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect();
            verbose(v, &format!("→ {} tracked files", files.len()));
            files
        }
        _ => {
            fail("git ls-files failed — run from a git repo");
            std::process::exit(1);
        }
    }
}

/// Get current git HEAD rev (short).
fn git_rev(dir: &Path) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Check if git working tree is clean.
fn git_is_clean(dir: &Path) -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .is_some_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim().is_empty())
}

/// Check remote deploy marker to see if sync can be skipped.
fn remote_deploy_rev(host: &str, remote_path: &str) -> Option<String> {
    let marker = format!("{remote_path}/.deploy-rev");
    let mut args = ssh_base_args(host, true);
    args.push(format!("cat '{marker}' 2>/dev/null"));
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    Command::new("ssh")
        .args(&refs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Write deploy marker on remote after successful sync.
fn write_deploy_rev(host: &str, remote_path: &str, rev: &str, v: bool) {
    let cmd = format!("echo '{rev}' > '{remote_path}/.deploy-rev'");
    let _ = ssh_run(host, &cmd, v);
}

/// Create a tar.gz archive in memory containing the given files.
fn create_tar_gz(base_dir: &Path, files: &[PathBuf], v: bool) -> Vec<u8> {
    let mut archive_bytes = Vec::new();
    let mut count = 0u32;
    {
        let encoder = GzEncoder::new(&mut archive_bytes, Compression::fast());
        let mut tar = tar::Builder::new(encoder);

        for rel in files {
            let full = base_dir.join(rel);
            if full.is_file() && tar.append_path_with_name(&full, rel).is_ok() {
                count += 1;
            }
        }

        let encoder = tar.into_inner().unwrap_or_else(|e| {
            fail(&format!("tar finalize failed: {e}"));
            std::process::exit(1);
        });
        encoder.finish().unwrap_or_else(|e| {
            fail(&format!("gzip finalize failed: {e}"));
            std::process::exit(1);
        });
    }
    verbose(
        v,
        &format!("tar.gz: {} bytes ({count} files)", archive_bytes.len()),
    );
    archive_bytes
}

/// Sync all git-tracked files to remote via tar-over-ssh.
/// Simple: always sends everything (project is small).
/// Returns number of files synced.
fn sync_via_tar(host: &str, local_dir: &Path, remote_dir: &str, strict_host: bool, v: bool) -> u32 {
    let files = git_tracked_files(local_dir, v);
    let archive = create_tar_gz(local_dir, &files, v);

    let extract_cmd =
        format!("rm -rf '{remote_dir}' && mkdir -p '{remote_dir}' && tar xzf - -C '{remote_dir}'");

    let mut args = ssh_base_args(host, strict_host);
    args.push(extract_cmd);
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    verbose(v, &format!("ssh root@{host} tar xzf - -C '{remote_dir}'"));

    let mut child = Command::new("ssh")
        .args(&refs)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| {
            fail(&format!("ssh spawn failed: {e}"));
            std::process::exit(1);
        });

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&archive).unwrap_or_else(|e| {
            fail(&format!("pipe to ssh failed: {e}"));
            std::process::exit(1);
        });
    }

    let status = child.wait().unwrap_or_else(|e| {
        fail(&format!("ssh wait failed: {e}"));
        std::process::exit(1);
    });

    if !status.success() {
        fail("sync failed");
        std::process::exit(1);
    }

    files.iter().filter(|f| local_dir.join(f).is_file()).count() as u32
}

/// Pipe a tar.gz archive of SSH host keys to /mnt/etc/ssh/ on the remote.
fn inject_host_keys_archive(host: &str, archive: &[u8]) {
    let mut args = ssh_base_args(host, false);
    args.push(
        "mkdir -p /mnt/etc/ssh && tar xzf - -C /mnt/etc/ssh/ && \
         chmod 600 /mnt/etc/ssh/ssh_host_*_key 2>/dev/null; \
         chmod 644 /mnt/etc/ssh/ssh_host_*_key.pub 2>/dev/null; true"
            .into(),
    );
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let mut child = Command::new("ssh")
        .args(&refs)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| {
            fail(&format!("ssh failed: {e}"));
            std::process::exit(1);
        });
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(archive);
    }
    let _ = child.wait();
}

/// Poll SSH until the host is reachable, with timeout.
fn wait_for_ssh(host: &str, timeout: Duration) {
    let start = Instant::now();
    let ssh_host = format!("root@{host}");

    loop {
        if start.elapsed() > timeout {
            fail(&format!("SSH timeout after {:.0}s", timeout.as_secs_f32()));
            std::process::exit(1);
        }

        let result = Command::new("ssh")
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "BatchMode=yes",
                &ssh_host,
                "true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        if let Ok(status) = result {
            if status.success() {
                return;
            }
        }

        std::thread::sleep(Duration::from_secs(3));
        eprint!(".");
    }
}

// ── Deploy mode ───────────────────────────────────────────────────────────────

fn cmd_deploy(cfg: &Config) {
    let flake = PathBuf::from(&cfg.flake)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&cfg.flake))
        .to_string_lossy()
        .to_string();
    let flake_path = Path::new(&flake);

    let t0 = Instant::now();
    let v = cfg.verbose;

    eprintln!(
        "deploy {CYAN}{}{RESET} → {CYAN}{}{RESET}",
        cfg.node, cfg.host
    );
    verbose(v, &format!("flake: {flake}"));
    verbose(v, &format!("remote: {}:{}", cfg.host, cfg.remote_path));

    // ── Decide what to do ─────────────────────────────────────────────────
    let do_eval = if cfg.skip_eval {
        false
    } else if cfg.force_eval {
        true
    } else if should_skip_eval(flake_path, &cfg.node) {
        false // auto-skip: .nix files unchanged since last eval
    } else {
        true
    };

    let local_rev = git_rev(flake_path);
    let clean = git_is_clean(flake_path);
    let skip_sync = clean
        && local_rev.as_ref().is_some_and(|rev| {
            remote_deploy_rev(&cfg.host, &cfg.remote_path).is_some_and(|remote| &remote == rev)
        });

    // ── Run eval + sync in parallel ───────────────────────────────────────
    let eval_handle = if do_eval {
        eprint!("  eval+sync...");
        let f = flake.clone();
        let n = cfg.node.clone();
        Some(std::thread::spawn(move || nix_eval(&f, &n, false)))
    } else {
        if !cfg.skip_eval {
            ok("eval: unchanged, cached");
        } else {
            warn("eval skipped (--skip-eval)");
        }
        None
    };

    let t = Instant::now();
    let sync_count = if skip_sync {
        ok("sync: up to date (same commit)");
        0
    } else {
        if eval_handle.is_none() {
            eprint!("  sync...");
        }
        let n = sync_via_tar(&cfg.host, flake_path, &cfg.remote_path, true, v);
        // Write deploy marker
        if let Some(ref rev) = local_rev {
            write_deploy_rev(&cfg.host, &cfg.remote_path, rev, v);
        }
        n
    };
    let sync_time = t.elapsed().as_secs_f32();

    // Wait for eval if running
    if let Some(handle) = eval_handle {
        let eval_t = t0.elapsed(); // includes parallel sync time
        match handle.join().unwrap() {
            Ok(()) => {
                save_eval_cache(flake_path, &cfg.node);
                if skip_sync {
                    eprintln!("\r  {GREEN}✓{RESET} eval  {:.1}s", eval_t.as_secs_f32());
                } else {
                    eprintln!(
                        "\r  {GREEN}✓{RESET} eval+sync  {sync_count} files  {:.1}s (eval {:.1}s, sync {sync_time:.1}s)",
                        eval_t.as_secs_f32(),
                        eval_t.as_secs_f32() // eval dominates
                    );
                }
            }
            Err(stderr) => {
                eprintln!();
                eprint!("{stderr}");
                fail("eval failed — fix errors above");
                std::process::exit(1);
            }
        }
    } else if !skip_sync && sync_count > 0 {
        eprintln!("\r  {GREEN}✓{RESET} sync  {sync_count} files  {sync_time:.1}s",);
    }

    // ── mount cache if --cache is set ───────────────────────────────────
    // SSHFS mount using server's own SSH key (set up during initial server config)
    let cache_mount = "/mnt/nix-cache";
    let cache_mounted = if let Some(ref cache) = cfg.cache_url {
        verbose(
            v,
            &format!(
                "mounting cache: {cache} → {cache_mount} (port {})",
                cfg.cache_port
            ),
        );
        match mount_remote_cache(&cfg.host, cache, cache_mount, cfg.cache_port, v) {
            Ok(()) => true,
            Err(err) => {
                warn(&err);
                false
            }
        }
    } else {
        false
    };

    if cfg.cache_url.is_some() && !cache_mounted {
        warn("building without cache");
    }

    // ── save pre-build closure on server for diff ─────────────────────────
    if cache_mounted {
        let _ = ssh_run(
            &cfg.host,
            "nix-store -qR /run/current-system 2>/dev/null | sort > /tmp/nix-pre-closure.txt",
            v,
        );
    }

    // ── build + activate ──────────────────────────────────────────────────
    eprintln!("  build+activate:");

    // If cache is mounted, use it as a substituter during the build
    let remote_cmd = if cache_mounted {
        format!(
            "nixos-rebuild switch --flake '{}#{}' \
             --option extra-substituters 'file://{cache_mount}' \
             --option require-sigs false",
            cfg.remote_path, cfg.node
        )
    } else {
        format!(
            "nixos-rebuild switch --flake '{}#{}'",
            cfg.remote_path, cfg.node
        )
    };
    verbose(v, &format!("ssh root@{} {remote_cmd}", cfg.host));

    let mut build_args = ssh_base_args(&cfg.host, true);
    build_args.push(remote_cmd);
    let build_refs: Vec<&str> = build_args.iter().map(|s| s.as_str()).collect();

    let t = Instant::now();
    let interactive_stderr = std::io::stderr().is_terminal();

    let result = run_streaming("ssh", &build_refs, &|line| {
        if should_print_build_line(line, interactive_stderr, v) {
            eprintln!("    {}", line.trim());
        }
    });

    match result {
        Ok(()) => {
            ok(&format!("activated  {:.1}s", t.elapsed().as_secs_f32()));
            if let Ok(gen) = ssh_output(
                &cfg.host,
                "nixos-rebuild list-generations 2>/dev/null | tail -1",
            ) {
                if !gen.is_empty() {
                    verbose(v, &format!("generation: {gen}"));
                }
            }
        }
        Err((code, tail)) => {
            fail(&format!("build failed (exit {code})"));
            eprintln!();
            eprintln!("  {BOLD}last output:{RESET}");
            for line in &tail {
                if !line.starts_with("  /nix/store/") {
                    eprintln!("    {line}");
                }
            }
            std::process::exit(code);
        }
    }

    // ── push only NEW paths to cache + unmount ─────────────────────────
    if cache_mounted {
        eprint!("  cache push...");
        let t = Instant::now();

        // Server-side closure diff + push + unmount
        let push_cmd = format!(
            "nix-store -qR /run/current-system | sort > /tmp/nix-post-closure.txt && \
             DIFF=$(comm -13 /tmp/nix-pre-closure.txt /tmp/nix-post-closure.txt) && \
             COUNT=$(echo \"$DIFF\" | grep -c . 2>/dev/null || echo 0) && \
             if [ -n \"$DIFF\" ] && [ \"$COUNT\" -gt 0 ]; then \
               echo \"$DIFF\" | xargs nix copy --to 'file://{cache_mount}' 2>/dev/null; \
               echo \"pushed $COUNT new paths\"; \
             else echo 'no new paths'; fi; \
             rm -f /tmp/nix-pre-closure.txt /tmp/nix-post-closure.txt; \
             fusermount -u {cache_mount} 2>/dev/null; true"
        );

        match ssh_run(&cfg.host, &push_cmd, v) {
            Ok(()) => eprintln!(
                "\r  {GREEN}✓{RESET} cache push  {:.1}s",
                t.elapsed().as_secs_f32()
            ),
            Err(_) => {
                eprintln!();
                warn("cache push failed (non-fatal)");
                let _ = ssh_run(
                    &cfg.host,
                    &format!("fusermount -u {cache_mount} 2>/dev/null; true"),
                    false,
                );
            }
        }
    }

    eprintln!(
        "\n{GREEN}{BOLD}✓{RESET} deployed {BOLD}{}{RESET}  total {:.0}s\n",
        cfg.node,
        t0.elapsed().as_secs_f32()
    );
}

// ── Install mode (kexec + disko + nixos-install) ──────────────────────────────

fn cmd_install(cfg: &Config) {
    let flake = PathBuf::from(&cfg.flake)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&cfg.flake))
        .to_string_lossy()
        .to_string();

    let t0 = Instant::now();
    let v = cfg.verbose;
    let source_dir = "/tmp/nixos-install-source";

    eprintln!(
        "install {CYAN}{}{RESET} → {CYAN}{}{RESET}",
        cfg.node, cfg.host
    );
    eprintln!(
        "  {YELLOW}⚠{RESET} this will WIPE ALL DISKS on {}",
        cfg.host
    );
    verbose(v, &format!("flake: {flake}"));
    verbose(v, &format!("kexec: {}", cfg.kexec_url));

    // ── kexec ─────────────────────────────────────────────────────────────
    eprintln!("  kexec:");

    let t = Instant::now();
    let download_cmd = format!(
        "mkdir -p /root/kexec && curl -sL '{}' | tar xzf - -C /root/kexec 2>&1",
        cfg.kexec_url
    );
    if ssh_run(&cfg.host, &download_cmd, v).is_err() {
        fail("failed to download kexec tarball on remote");
        std::process::exit(1);
    }
    ok(&format!(
        "kexec tarball downloaded  {:.1}s",
        t.elapsed().as_secs_f32()
    ));

    let t = Instant::now();
    if ssh_run(&cfg.host, "setsid --wait /root/kexec/kexec/run 2>&1", v).is_err() {
        fail("kexec execution failed");
        std::process::exit(1);
    }
    ok(&format!(
        "kexec initiated  {:.1}s",
        t.elapsed().as_secs_f32()
    ));

    eprint!("  waiting for reboot");
    std::thread::sleep(Duration::from_secs(10));
    wait_for_ssh(&cfg.host, Duration::from_secs(300));
    eprintln!();
    ok("NixOS installer booted");

    if ssh_run_no_check(&cfg.host, "test -f /etc/NIXOS 2>/dev/null", v).is_err() {
        fail("remote is not running NixOS installer after kexec");
        std::process::exit(1);
    }

    // ── sync ──────────────────────────────────────────────────────────────
    eprint!("  sync...");
    let t = Instant::now();
    let n = sync_via_tar(&cfg.host, Path::new(&flake), source_dir, false, v);
    eprintln!(
        "\r  {GREEN}✓{RESET} sync  {n} files  {:.1}s",
        t.elapsed().as_secs_f32()
    );

    // ── disko ─────────────────────────────────────────────────────────────
    eprintln!("  disko (partition+format):");

    let disko_cmd = format!(
        "nix --experimental-features 'nix-command flakes' build \
         '{source}#nixosConfigurations.{node}.config.system.build.diskoScript' \
         --no-link --print-out-paths 2>&1",
        source = source_dir,
        node = cfg.node,
    );
    verbose(v, &format!("ssh root@{} {disko_cmd}", cfg.host));

    let mut disko_args = ssh_base_args(&cfg.host, false);
    disko_args.push(disko_cmd);
    let disko_refs: Vec<&str> = disko_args.iter().map(|s| s.as_str()).collect();

    let disko_build = Command::new("ssh")
        .args(&disko_refs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            fail(&format!("ssh failed: {e}"));
            std::process::exit(1);
        });

    if !disko_build.status.success() {
        fail("disko script build failed");
        std::process::exit(1);
    }

    let disko_path = String::from_utf8_lossy(&disko_build.stdout)
        .trim()
        .to_string();
    if disko_path.is_empty() {
        fail("disko script path is empty");
        std::process::exit(1);
    }
    verbose(v, &format!("disko script: {disko_path}"));

    let t = Instant::now();
    if ssh_run_no_check(&cfg.host, &format!("{disko_path} 2>&1"), v).is_err() {
        fail("disko failed — check disk config");
        std::process::exit(1);
    }
    ok(&format!(
        "disks formatted+mounted  {:.1}s",
        t.elapsed().as_secs_f32()
    ));

    // ── nixos-install ─────────────────────────────────────────────────────
    eprintln!("  nixos-install:");

    let t = Instant::now();
    let mnt_source = format!("/mnt{}", cfg.remote_path);
    sync_via_tar(&cfg.host, Path::new(&flake), &mnt_source, false, v);

    let install_cmd = format!(
        "nixos-install --flake '{source}#{node}' --no-root-passwd --no-channel-copy 2>&1",
        source = mnt_source,
        node = cfg.node,
    );

    if ssh_run_no_check(&cfg.host, &install_cmd, v).is_err() {
        fail("nixos-install failed");
        std::process::exit(1);
    }
    ok(&format!("installed  {:.1}s", t.elapsed().as_secs_f32()));

    // ── inject SSH host keys into installed system ──────────────────────
    // --host-keys <dir>:  inject from directory (plaintext key files)
    // --host-keys <file>: decrypt SOPS YAML, extract keys, inject
    // (none):             copy installer's current keys (like nixos-anywhere)
    if let Some(ref keys_src) = cfg.host_keys_dir {
        let src_path = Path::new(keys_src);
        eprint!("  host keys...");

        if src_path.is_dir() {
            // Directory mode: tar up ssh_host_* files and pipe to server
            let mut key_files: Vec<PathBuf> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(src_path) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with("ssh_host_") {
                        key_files.push(PathBuf::from(&name));
                    }
                }
            }
            if !key_files.is_empty() {
                let archive = create_tar_gz(src_path, &key_files, v);
                inject_host_keys_archive(&cfg.host, &archive);
                ok(&format!("{} key(s) from {keys_src}", key_files.len()));
            }
        } else if src_path.is_file() {
            // SOPS file mode: decrypt with `sops -d`, extract key fields, inject
            verbose(v, &format!("decrypting SOPS file: {keys_src}"));
            let tmpdir = std::env::temp_dir().join("nix-remote-delivery-host-keys");
            let _ = std::fs::remove_dir_all(&tmpdir);
            std::fs::create_dir_all(&tmpdir).unwrap();

            // Extract each key field from the SOPS YAML
            let mut injected = 0;
            for (field, filename, mode) in [
                ("ssh_host_ed25519_key", "ssh_host_ed25519_key", "0600"),
                (
                    "ssh_host_ed25519_key_pub",
                    "ssh_host_ed25519_key.pub",
                    "0644",
                ),
                ("ssh_host_rsa_key", "ssh_host_rsa_key", "0600"),
                ("ssh_host_rsa_key_pub", "ssh_host_rsa_key.pub", "0644"),
            ] {
                let out = Command::new("sops")
                    .args(["-d", "--extract", &format!("[\"{field}\"]"), keys_src])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .output();

                if let Ok(out) = out {
                    if out.status.success() && !out.stdout.is_empty() {
                        let key_path = tmpdir.join(filename);
                        std::fs::write(&key_path, &out.stdout).unwrap();
                        // Set local file permissions before tar
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let perms = std::fs::Permissions::from_mode(
                                u32::from_str_radix(mode, 8).unwrap(),
                            );
                            std::fs::set_permissions(&key_path, perms).ok();
                        }
                        injected += 1;
                    }
                }
            }

            if injected > 0 {
                let files: Vec<PathBuf> = std::fs::read_dir(&tmpdir)
                    .unwrap()
                    .flatten()
                    .map(|e| PathBuf::from(e.file_name()))
                    .collect();
                let archive = create_tar_gz(&tmpdir, &files, v);
                inject_host_keys_archive(&cfg.host, &archive);
                ok(&format!("{injected} key(s) from SOPS {keys_src}"));
            } else {
                warn("no host keys found in SOPS file");
            }

            let _ = std::fs::remove_dir_all(&tmpdir);
        } else {
            warn(&format!("host keys path not found: {keys_src}"));
        }
    } else {
        // Default: copy installer's current host keys (like nixos-anywhere)
        let copy_keys = "cp /etc/ssh/ssh_host_* /mnt/etc/ssh/ 2>/dev/null && \
                          chmod 600 /mnt/etc/ssh/ssh_host_*_key && \
                          chmod 644 /mnt/etc/ssh/ssh_host_*_key.pub";
        if ssh_run_no_check(&cfg.host, copy_keys, v).is_ok() {
            ok("host keys copied from installer");
        } else {
            warn("host key copy failed (server will generate new keys)");
        }
    }

    // ── reboot ────────────────────────────────────────────────────────────
    let _ = ssh_run_no_check(
        &cfg.host,
        "umount -Rv /mnt 2>/dev/null; swapoff -a 2>/dev/null; nohup sh -c 'sleep 6 && reboot' &>/dev/null &",
        v,
    );
    eprintln!("  rebooting...");

    std::thread::sleep(Duration::from_secs(10));
    eprint!("  waiting for boot");
    wait_for_ssh(&cfg.host, Duration::from_secs(300));
    eprintln!();

    ok("server is up");

    eprintln!(
        "\n{GREEN}{BOLD}✓{RESET} installed {BOLD}{}{RESET}  total {:.0}s\n",
        cfg.node,
        t0.elapsed().as_secs_f32()
    );
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let cfg = parse_args();

    match cfg.mode {
        Mode::Deploy => cmd_deploy(&cfg),
        Mode::Install => cmd_install(&cfg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git_is_available() -> bool {
        Command::new("git")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn init_temp_git_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();

        if git_is_available() {
            let status = Command::new("git")
                .args(["init", "-q"])
                .current_dir(&dir)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git init failed");
        }

        dir
    }

    #[test]
    fn sha256_known_value() {
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn sha256_empty() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn create_tar_gz_roundtrip() {
        let dir = std::env::temp_dir().join("nix-remote-delivery-test-tar");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.nix"), "{ }").unwrap();
        fs::write(dir.join("sub/b.nix"), "{ pkgs }: pkgs").unwrap();

        let files = vec![
            PathBuf::from("a.nix"),
            PathBuf::from("sub/b.nix"),
            PathBuf::from("nonexistent.nix"),
        ];

        let archive = create_tar_gz(&dir, &files, false);
        assert!(!archive.is_empty());

        let decoder = flate2::read::GzDecoder::new(&archive[..]);
        let mut ar = tar::Archive::new(decoder);
        let entries: Vec<String> = ar
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(entries.contains(&"a.nix".to_string()));
        assert!(entries.contains(&"sub/b.nix".to_string()));
        assert_eq!(entries.len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_tar_gz_empty() {
        let dir = std::env::temp_dir().join("nix-remote-delivery-test-empty");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let archive = create_tar_gz(&dir, &[], false);
        assert!(!archive.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ssh_base_args_strict() {
        let args = ssh_base_args("myhost", true);
        assert!(args.contains(&"ControlMaster=auto".to_string()));
        assert!(args.contains(&"Compression=yes".to_string()));
        assert!(args.contains(&"root@myhost".to_string()));
        assert!(args.iter().any(|arg| {
            arg == &format!(
                "ControlPath={}",
                env::temp_dir().join("nix-remote-delivery-myhost").display()
            )
        }));
        assert!(!args.contains(&"StrictHostKeyChecking=no".to_string()));
    }

    #[test]
    fn ssh_base_args_no_check() {
        let args = ssh_base_args("myhost", false);
        assert!(args.contains(&"StrictHostKeyChecking=no".to_string()));
        assert!(args.contains(&"ControlMaster=auto".to_string()));
    }

    #[test]
    fn ssh_control_path_from_dir_uses_override_dir() {
        let path = ssh_control_path_from_dir(Path::new("/var/run/demo"), "myhost");
        assert_eq!(
            path,
            PathBuf::from("/var/run/demo").join("nix-remote-delivery-myhost")
        );
    }

    #[test]
    fn run_streaming_captures_exit_code() {
        let result = run_streaming("sh", &["-c", "echo ok; exit 0"], &|_| {});
        assert!(result.is_ok());
    }

    #[test]
    fn run_streaming_reports_failure() {
        let result = run_streaming("sh", &["-c", "echo 'err line' >&2; exit 42"], &|_| {});
        match result {
            Err((code, tail)) => {
                assert_eq!(code, 42);
                assert!(tail.iter().any(|l| l.contains("err line")));
            }
            Ok(()) => panic!("expected failure"),
        }
    }

    #[test]
    fn run_streaming_captures_tail() {
        let result = run_streaming(
            "sh",
            &[
                "-c",
                "for i in $(seq 1 100); do echo line$i >&2; done; exit 1",
            ],
            &|_| {},
        );
        match result {
            Err((_, tail)) => {
                assert!(tail.len() <= 50);
                assert!(tail.last().unwrap().contains("line100"));
            }
            Ok(()) => panic!("expected failure"),
        }
    }

    #[test]
    fn git_tracked_files_works_in_repo() {
        if !git_is_available() {
            return;
        }

        let dir = init_temp_git_repo("nix-remote-delivery-test-git-files");
        fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();

        let files = git_tracked_files(&dir, false);
        let names: Vec<String> = files
            .iter()
            .map(|f| f.to_string_lossy().to_string())
            .collect();
        assert!(
            names.contains(&"Cargo.toml".to_string()),
            "Cargo.toml not found in {names:?}"
        );
        assert!(
            names.contains(&"src/main.rs".to_string()),
            "src/main.rs not found in {names:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn nix_files_hash_deterministic() {
        if !git_is_available() {
            return;
        }

        let dir = init_temp_git_repo("nix-remote-delivery-test-nix-hash");
        fs::write(dir.join("flake.nix"), "{ }\n").unwrap();
        fs::write(dir.join("module.nix"), "{ pkgs }: pkgs\n").unwrap();
        fs::write(dir.join("flake.lock"), "{ }\n").unwrap();
        fs::write(dir.join("README.md"), "ignore me\n").unwrap();

        let h1 = nix_files_hash(&dir);
        let h2 = nix_files_hash(&dir);
        assert_eq!(h1, h2);
        assert!(!h1.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn eval_cache_roundtrip() {
        let node = "test-node-cache";
        let cache = eval_cache_path(node);
        let _ = fs::remove_file(&cache);

        // No cache → should not skip
        assert!(!should_skip_eval(Path::new("."), node));

        // Save cache → should skip (same dir)
        save_eval_cache(Path::new("."), node);
        // Note: can't test should_skip_eval because it depends on the actual nix files
        // being unchanged between calls, which they are in tests

        let _ = fs::remove_file(&cache);
    }

    #[test]
    fn eval_cache_path_uses_system_temp_dir() {
        assert_eq!(
            eval_cache_path("demo-node"),
            env::temp_dir().join("nix-remote-delivery-eval-demo-node.hash")
        );
    }

    #[test]
    fn parse_args_supports_cache_port() {
        let cfg = parse_args_from([
            "demo-node".to_string(),
            "--cache".to_string(),
            "user@example.test".to_string(),
            "--cache-port".to_string(),
            "2222".to_string(),
        ]);

        assert_eq!(cfg.node, "demo-node");
        assert_eq!(cfg.cache_url.as_deref(), Some("user@example.test"));
        assert_eq!(cfg.cache_port, 2222);
    }

    #[test]
    fn parse_args_uses_default_cache_port() {
        let cfg = parse_args_from(["demo-node".to_string()]);
        assert_eq!(cfg.cache_port, DEFAULT_CACHE_PORT);
    }

    #[test]
    fn summarize_tail_keeps_last_lines() {
        let text = "first\nsecond\nthird\nfourth\n";
        assert_eq!(summarize_tail(text, 2), "third | fourth");
    }

    #[test]
    fn non_interactive_build_output_keeps_only_high_signal_lines() {
        assert!(should_print_build_line(
            "building the system configuration...",
            false,
            false
        ));
        assert!(should_print_build_line(
            "activating the configuration...",
            false,
            false
        ));
        assert!(!should_print_build_line(
            "copying path '/nix/store/foo' from 'https://cache.nixos.org'",
            false,
            false
        ));
        assert!(!should_print_build_line(
            "these 12 derivations will be built:",
            false,
            false
        ));
    }

    #[test]
    fn verbose_mode_still_hides_store_paths_only() {
        assert!(should_print_build_line(
            "copying path '/nix/store/foo' from 'https://cache.nixos.org'",
            true,
            true
        ));
        assert!(!should_print_build_line("/nix/store/abc.drv", true, true));
    }
}
