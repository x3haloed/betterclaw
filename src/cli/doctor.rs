//! `ironclaw doctor` - active health diagnostics.
//!
//! Probes external dependencies and validates configuration to surface
//! problems before they bite during normal operation. Each check reports
//! pass/fail with actionable guidance on failures.

use std::path::PathBuf;

use crate::bootstrap::ironclaw_base_dir;

/// Run all diagnostic checks and print results.
pub async fn run_doctor_command() -> anyhow::Result<()> {
    println!("IronClaw Doctor");
    println!("===============\n");

    let mut passed = 0u32;
    let mut failed = 0u32;

    // ── Configuration checks ──────────────────────────────────

    check(
        "Database backend",
        check_database().await,
        &mut passed,
        &mut failed,
    );

    check(
        "Workspace directory",
        check_workspace_dir(),
        &mut passed,
        &mut failed,
    );

    // ── External binary checks ────────────────────────────────

    check(
        "Docker",
        check_binary("docker", &["--version"]),
        &mut passed,
        &mut failed,
    );

    check(
        "cloudflared",
        check_binary("cloudflared", &["--version"]),
        &mut passed,
        &mut failed,
    );

    check(
        "ngrok",
        check_binary("ngrok", &["version"]),
        &mut passed,
        &mut failed,
    );

    check(
        "tailscale",
        check_binary("tailscale", &["version"]),
        &mut passed,
        &mut failed,
    );

    // ── Summary ───────────────────────────────────────────────

    println!();
    println!("  {passed} passed, {failed} failed");

    if failed > 0 {
        println!("\n  Some checks failed. This is normal if you don't use those features.");
    }

    Ok(())
}

// ── Individual checks ───────────────────────────────────────

fn check(name: &str, result: CheckResult, passed: &mut u32, failed: &mut u32) {
    match result {
        CheckResult::Pass(detail) => {
            *passed += 1;
            println!("  [pass] {name}: {detail}");
        }
        CheckResult::Fail(detail) => {
            *failed += 1;
            println!("  [FAIL] {name}: {detail}");
        }
        CheckResult::Skip(reason) => {
            println!("  [skip] {name}: {reason}");
        }
    }
}

enum CheckResult {
    Pass(String),
    Fail(String),
    Skip(String),
}

async fn check_database() -> CheckResult {
    let backend = std::env::var("DATABASE_BACKEND")
        .ok()
        .unwrap_or_else(|| "libsql".into());

    match backend.as_str() {
        "libsql" | "turso" | "sqlite" => {
            let path = std::env::var("LIBSQL_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|_| crate::config::default_libsql_path());

            if path.exists() {
                CheckResult::Pass(format!("libSQL database exists ({})", path.display()))
            } else {
                CheckResult::Pass(format!(
                    "libSQL database not found at {} (will be created on first run)",
                    path.display()
                ))
            }
        }
        other => CheckResult::Fail(format!(
            "unsupported DATABASE_BACKEND '{other}' (BetterClaw is libsql-only)"
        )),
    }
}

fn check_workspace_dir() -> CheckResult {
    let dir = ironclaw_base_dir();

    if dir.exists() {
        if dir.is_dir() {
            CheckResult::Pass(format!("{}", dir.display()))
        } else {
            CheckResult::Fail(format!("{} exists but is not a directory", dir.display()))
        }
    } else {
        CheckResult::Pass(format!("{} will be created on first run", dir.display()))
    }
}

fn check_binary(name: &str, args: &[&str]) -> CheckResult {
    match std::process::Command::new(name)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(output) => {
            let version = String::from_utf8_lossy(&output.stdout);
            let version = version.trim();
            // Some tools print version to stderr
            let version = if version.is_empty() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                stderr.trim().lines().next().unwrap_or("").to_string()
            } else {
                version.lines().next().unwrap_or("").to_string()
            };

            if output.status.success() {
                CheckResult::Pass(version)
            } else {
                CheckResult::Fail(format!("exited with {}", output.status))
            }
        }
        Err(_) => CheckResult::Skip(format!("{name} not found in PATH")),
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::doctor::*;

    #[test]
    fn check_binary_finds_sh() {
        match check_binary("sh", &["-c", "echo ok"]) {
            CheckResult::Pass(_) => {}
            other => panic!("expected Pass for sh, got: {}", format_result(&other)),
        }
    }

    #[test]
    fn check_binary_skips_nonexistent() {
        match check_binary("__ironclaw_nonexistent_binary__", &["--version"]) {
            CheckResult::Skip(_) => {}
            other => panic!(
                "expected Skip for nonexistent binary, got: {}",
                format_result(&other)
            ),
        }
    }

    #[test]
    fn check_workspace_dir_does_not_panic() {
        let result = check_workspace_dir();
        match result {
            CheckResult::Pass(_) | CheckResult::Fail(_) | CheckResult::Skip(_) => {}
        }
    }

    fn format_result(r: &CheckResult) -> String {
        match r {
            CheckResult::Pass(s) => format!("Pass({s})"),
            CheckResult::Fail(s) => format!("Fail({s})"),
            CheckResult::Skip(s) => format!("Skip({s})"),
        }
    }
}
