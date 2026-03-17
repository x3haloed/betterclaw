//! Self-update mechanism for BetterClaw.
//!
//! Provides the ability to pull latest code, rebuild, and exec into the new binary.
//! Designed to be triggered via HTTP endpoint or Tidepool command.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Serialize;

/// Status returned after attempting a self-update.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatus {
    pub success: bool,
    pub git_output: String,
    pub build_output: String,
    pub elapsed_ms: u64,
    pub will_exec: bool,
    pub message: String,
}

/// Detect the BetterClaw project root directory.
///
/// Checks in order:
/// 1. BETTERCLAW_PROJECT_ROOT env var (explicit override)
/// 2. Parent directory of the current executable (if it's inside a target/ dir)
/// 3. Default: /Users/chad/Repos/betterclaw
pub fn detect_project_root() -> PathBuf {
    if let Ok(root) = std::env::var("BETTERCLAW_PROJECT_ROOT") {
        return PathBuf::from(root);
    }

    // Try to detect from current exe path
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            if parent.ends_with("target/debug") || parent.ends_with("target/release") {
                if let Some(root) = parent.parent().and_then(|p| p.parent()) {
                    if root.join("Cargo.toml").exists() {
                        tracing::info!(root = %root.display(), "Auto-detected project root from exe path");
                        return root.to_path_buf();
                    }
                }
            }
        }
    }

    let default = PathBuf::from("/Users/chad/Repos/betterclaw");
    tracing::warn!(root = %default.display(), "Using default project root");
    default
}

/// Pull latest code from git.
fn git_pull(project_root: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["pull", "--ff-only"])
        .current_dir(project_root)
        .output()
        .context("failed to run git pull")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = if stderr.is_empty() {
        stdout
    } else {
        format!("{stdout}\n{stderr}")
    };

    if !output.status.success() {
        anyhow::bail!("git pull failed: {}", combined.trim());
    }

    Ok(combined.trim().to_string())
}

/// Build the project with cargo.
fn cargo_build(project_root: &Path) -> Result<String> {
    let output = Command::new("cargo")
        .args(["build"])
        .current_dir(project_root)
        .env("RUSTFLAGS", "-Awarnings") // suppress warnings during update builds
        .output()
        .context("failed to run cargo build")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let combined = format!("{stdout}\n{stderr}");

    if !output.status.success() {
        anyhow::bail!("cargo build failed:\n{}", combined);
    }

    Ok(combined.trim().to_string())
}

/// Check if there are new commits available without pulling.
pub fn check_for_updates(project_root: Option<&Path>) -> Result<String> {
    let root = project_root
        .map(|p| p.to_path_buf())
        .unwrap_or_else(detect_project_root);

    let output = Command::new("git")
        .args(["fetch", "origin"])
        .current_dir(&root)
        .output()
        .context("failed to run git fetch")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git fetch failed: {}", stderr);
    }

    let output = Command::new("git")
        .args(["rev-list", "--count", "HEAD..origin/main"])
        .current_dir(&root)
        .output()
        .context("failed to check for new commits")?;

    let count = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .unwrap_or(0);

    if count > 0 {
        Ok(format!("{count} new commit(s) available"))
    } else {
        Ok("up to date".to_string())
    }
}

/// Perform a self-update: pull → build → return status.
///
/// If `do_exec` is true, the process will exec into the new binary after
/// returning the status (the caller should send the response before the exec happens).
pub fn perform_update(do_exec: bool) -> Result<UpdateStatus> {
    let start = Instant::now();
    let project_root = detect_project_root();

    tracing::info!(root = %project_root.display(), exec = do_exec, "Starting self-update");

    // Step 1: git pull
    let git_output = match git_pull(&project_root) {
        Ok(output) => output,
        Err(e) => {
            return Ok(UpdateStatus {
                success: false,
                git_output: e.to_string(),
                build_output: String::new(),
                elapsed_ms: start.elapsed().as_millis() as u64,
                will_exec: false,
                message: format!("git pull failed: {e}"),
            });
        }
    };

    // Check if we actually got new code
    if git_output.contains("Already up to date") && !do_exec {
        return Ok(UpdateStatus {
            success: true,
            git_output,
            build_output: "skipped (already up to date)".to_string(),
            elapsed_ms: start.elapsed().as_millis() as u64,
            will_exec: false,
            message: "Already up to date, no rebuild needed".to_string(),
        });
    }

    // Step 2: cargo build
    let _build_output = match cargo_build(&project_root) {
        Ok(output) => output,
        Err(e) => {
            return Ok(UpdateStatus {
                success: false,
                git_output,
                build_output: e.to_string(),
                elapsed_ms: start.elapsed().as_millis() as u64,
                will_exec: false,
                message: format!("cargo build failed: {e}"),
            });
        }
    };

    let elapsed = start.elapsed().as_millis() as u64;

    if do_exec {
        tracing::info!(elapsed_ms = elapsed, "Build succeeded, preparing to exec");

        // Spawn a background process that waits then execs
        // This allows the HTTP response to be sent first
        let new_binary = project_root.join("target/debug/betterclaw");
        let current_exe = std::env::current_exe().unwrap_or_else(|_| new_binary.clone());

        // Use the newly built binary path
        let exec_path = if new_binary.exists() {
            new_binary
        } else {
            current_exe.clone()
        };

        // Collect current args and env for re-execution
        let args: Vec<String> = std::env::args().skip(1).collect();

        // Spawn a detached child that waits then replaces us
        // We use a small delay to let the HTTP response flush
        let _ = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "sleep 2 && exec \"{}\" {}",
                exec_path.display(),
                args.iter()
                    .map(|a| format!("\"{a}\""))
                    .collect::<Vec<_>>()
                    .join(" ")
            ))
            .current_dir(&project_root)
            .spawn();

        Ok(UpdateStatus {
            success: true,
            git_output,
            build_output: "(see build logs)".to_string(),
            elapsed_ms: elapsed,
            will_exec: true,
            message: format!("Build succeeded in {elapsed}ms. Process will exec into new binary in 2 seconds."),
        })
    } else {
        Ok(UpdateStatus {
            success: true,
            git_output,
            build_output: "(dry run, no exec)".to_string(),
            elapsed_ms: elapsed,
            will_exec: false,
            message: format!("Build succeeded in {elapsed}ms. Ready for exec."),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_project_root_from_env() {
        // Should find Cargo.toml in the default location
        let root = detect_project_root();
        assert!(root.join("Cargo.toml").exists(), "project root must contain Cargo.toml");
    }
}
