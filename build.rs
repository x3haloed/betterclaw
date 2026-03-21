use std::process::Command;

fn main() {
    // Set GIT_COMMIT env for embedding in the binary
    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_COMMIT={commit}");
    // Re-run if git HEAD changes
    println!("cargo:rerun-if-changed=.git/HEAD");
}
