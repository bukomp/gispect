use std::process::Command;

fn main() {
    // Commit hash of the source tree this binary was built from. `cargo install
    // --git` builds from a real checkout, so this works for installed binaries too.
    let commit = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GISPECT_BUILD_COMMIT={commit}");

    // Upstream repo used for update checks; overridable at build time.
    let repo_url = std::env::var("GISPECT_REPO_URL")
        .unwrap_or_else(|_| "https://github.com/bukomp/gispect".to_string());
    println!("cargo:rustc-env=GISPECT_REPO_URL={repo_url}");

    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-env-changed=GISPECT_REPO_URL");
}
