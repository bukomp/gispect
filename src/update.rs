//! Self-update: check the upstream repo for a newer commit and reinstall
//! via `cargo install --git`. No network calls beyond `git ls-remote`.

use std::process::Command;

use anyhow::{bail, Context, Result};

/// Git commit hash this binary was built from, embedded by `build.rs`.
/// `"unknown"` when the build tree wasn't a git checkout.
pub fn current_commit() -> &'static str {
    env!("GISPECT_BUILD_COMMIT")
}

/// Upstream repository URL used for update checks. A `GISPECT_REPO_URL`
/// environment variable set at runtime takes precedence over the URL
/// embedded at build time by `build.rs`, so installs can be pointed at a
/// fork or mirror without rebuilding.
pub fn repo_url() -> String {
    std::env::var("GISPECT_REPO_URL").unwrap_or_else(|_| env!("GISPECT_REPO_URL").to_string())
}

/// Query the upstream repo's `HEAD` and compare it against the commit this
/// binary was built from.
///
/// Returns `Ok(Some(remote_hash))` when an update is available (including
/// when the local commit is `"unknown"`, so the user can still force an
/// update), and `Ok(None)` when already up to date.
pub fn check_for_update() -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["ls-remote", repo_url().as_str(), "HEAD"])
        .output()
        .context("failed to run `git ls-remote` — is git installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git ls-remote {} HEAD failed: {stderr}", repo_url());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let remote_hash = stdout
        .split_whitespace()
        .next()
        .with_context(|| format!("unexpected `git ls-remote` output: {stdout:?}"))?
        .to_string();

    let current = current_commit();
    if current != "unknown" && current == remote_hash {
        Ok(None)
    } else {
        Ok(Some(remote_hash))
    }
}

/// Reinstall gispect from upstream via `cargo install --git ... --force`,
/// inheriting stdio so the user sees cargo's progress. Works for binaries
/// originally installed with `cargo install`, since cargo replaces the
/// existing binary in `~/.cargo/bin`.
pub fn perform_update() -> Result<()> {
    // net.git-fetch-with-cli makes cargo shell out to the system `git`, so
    // user gitconfig (insteadOf rewrites, ssh agent, credential helpers)
    // applies; cargo's built-in git client often can't authenticate.
    let status = Command::new("cargo")
        .args([
            "install",
            "--config",
            "net.git-fetch-with-cli=true",
            "--git",
            repo_url().as_str(),
            "--force",
            "gispect",
        ])
        .status()
        .context("failed to run `cargo install` — is cargo installed?")?;

    if !status.success() {
        bail!("cargo install exited with {status}");
    }

    Ok(())
}

/// Replace the current process with a fresh invocation of the gispect
/// binary, preserving the original CLI arguments. Called after a
/// successful self-update, once the terminal has been restored, so the
/// user lands back in the TUI running the new version.
///
/// On Unix this uses `exec` and only returns on failure. On other
/// platforms it spawns a child, waits for it, and exits with its status
/// code, so it also never returns `Ok` in practice.
pub fn restart() -> Result<()> {
    // `cargo install --force` overwrites the binary at this same path, so
    // re-executing it picks up the freshly installed version.
    let exe = std::env::current_exe().context("failed to resolve current executable path")?;
    let args = std::env::args_os().skip(1);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let err = Command::new(&exe).args(args).exec();
        // `exec` only returns on failure — if we get here, the exec failed.
        Err(err).with_context(|| format!("failed to exec {}", exe.display()))
    }

    #[cfg(not(unix))]
    {
        let status = Command::new(&exe)
            .args(args)
            .status()
            .with_context(|| format!("failed to spawn {}", exe.display()))?;
        std::process::exit(status.code().unwrap_or(0));
    }
}

/// Short (7-char) form of a commit hash, for readable status messages.
fn short(hash: &str) -> &str {
    &hash[..hash.len().min(7)]
}

/// CLI entry point for `gispect update`: check upstream, report the
/// outcome, and reinstall if a newer commit is available.
pub fn run_cli() -> Result<()> {
    match check_for_update()? {
        None => {
            println!("already up to date ({})", short(current_commit()));
            return Ok(());
        }
        Some(remote) => {
            println!(
                "updating {} -> {}",
                short(current_commit()),
                short(&remote)
            );
        }
    }

    perform_update()?;
    println!("updated — restart gispect");
    Ok(())
}
