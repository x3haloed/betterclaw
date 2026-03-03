//! Filesystem-backed workspace files.
//!
//! BetterClaw historically stored "workspace files" (AGENTS.md, SOUL.md, HEARTBEAT.md, etc.)
//! inside the database. Our new framework treats workspace files as real files on disk,
//! and "memory" as an append-only ledger + deterministic indexes.
//!
//! This module is intentionally small: it supports only what the runtime needs to
//! build system prompts and run heartbeat from a real workspace directory.

use std::path::{Path, PathBuf};
use std::path::Component;

use tokio::fs;

use crate::bootstrap::betterclaw_base_dir;
use crate::error::WorkspaceError;
use crate::workspace::paths;

/// Default template seeded into HEARTBEAT.md on first access.
///
/// Intentionally comment-only so the heartbeat runner treats it as
/// "effectively empty" and skips the LLM call until the user adds real tasks.
const HEARTBEAT_SEED: &str = "\
# Heartbeat Checklist

<!-- Keep this file empty to skip heartbeat API calls.
     Add tasks below when you want the agent to check something periodically.

     Rotate through these checks 2-4 times per day:
     - [ ] Check for urgent messages
     - [ ] Review upcoming calendar events
     - [ ] Check project status or CI builds

     Stay quiet during 23:00-08:00 user-local time unless urgent.
     If nothing needs attention, reply HEARTBEAT_OK.

     Proactive work you can do without asking:
     - Organize and curate documents (remove stale, consolidate dupes)
     - Update daily logs with session summaries
     - Clean up context/documents that are outdated
-->";

/// Filesystem workspace.
///
/// Layout (default):
/// `~/.betterclaw/workspaces/<user_id>/files/{AGENTS.md,SOUL.md,HEARTBEAT.md,...}`
#[derive(Debug, Clone)]
pub struct FsWorkspace {
    user_id: String,
    root: PathBuf,
}

impl FsWorkspace {
    /// Create a new filesystem workspace rooted under `~/.betterclaw/workspaces/<user_id>/`.
    pub fn new(user_id: impl Into<String>) -> Self {
        let user_id = user_id.into();
        let root = betterclaw_base_dir().join("workspaces").join(&user_id);
        Self { user_id, root }
    }

    /// Create a new filesystem workspace rooted under `base_dir/workspaces/<user_id>/`.
    ///
    /// Useful for unit tests so they don't touch the real `~/.betterclaw` directory.
    pub fn new_in_base(user_id: impl Into<String>, base_dir: impl Into<PathBuf>) -> Self {
        let user_id = user_id.into();
        let root = base_dir.into().join("workspaces").join(&user_id);
        Self { user_id, root }
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn root_dir(&self) -> &Path {
        &self.root
    }

    pub fn files_dir(&self) -> PathBuf {
        self.root.join("files")
    }

    fn resolve_file(&self, file: &str) -> PathBuf {
        self.files_dir().join(file)
    }

    fn resolve_rel_path(&self, rel: &str) -> Result<PathBuf, WorkspaceError> {
        let rel_path = Path::new(rel);
        if rel_path.is_absolute() {
            return Err(WorkspaceError::Io {
                reason: format!("Refusing to resolve absolute workspace path: {}", rel),
            });
        }

        // Prevent `..` traversal outside the workspace root.
        let mut clean = PathBuf::new();
        for c in rel_path.components() {
            match c {
                Component::Normal(part) => clean.push(part),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(WorkspaceError::Io {
                        reason: format!("Refusing to resolve parent traversal in path: {}", rel),
                    });
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(WorkspaceError::Io {
                        reason: format!("Refusing to resolve non-relative workspace path: {}", rel),
                    });
                }
            }
        }

        Ok(self.files_dir().join(clean))
    }

    async fn ensure_dirs(&self) -> Result<(), WorkspaceError> {
        fs::create_dir_all(self.files_dir())
            .await
            .map_err(|e| WorkspaceError::Io {
                reason: format!("Failed to create workspace dirs: {}", e),
            })
    }

    /// Read a workspace file (relative to `files/`) if it exists.
    pub async fn read_optional_rel(&self, rel: &str) -> Result<Option<String>, WorkspaceError> {
        self.ensure_dirs().await?;
        let path = self.resolve_rel_path(rel)?;
        match fs::read_to_string(&path).await {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(WorkspaceError::Io {
                reason: format!("Failed to read {}: {}", path.display(), e),
            }),
        }
    }

    /// Read a workspace file (relative to `files/`), erroring if missing.
    pub async fn read_text_rel(&self, rel: &str) -> Result<String, WorkspaceError> {
        self.read_optional_rel(rel).await?.ok_or_else(|| {
            WorkspaceError::DocumentNotFound {
                doc_type: rel.to_string(),
                user_id: self.user_id.clone(),
            }
        })
    }

    /// Write a workspace file (relative to `files/`), creating parent directories if needed.
    pub async fn write_text_rel(&self, rel: &str, content: &str) -> Result<(), WorkspaceError> {
        self.ensure_dirs().await?;
        let path = self.resolve_rel_path(rel)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| WorkspaceError::Io {
                    reason: format!(
                        "Failed to create parent dirs for {}: {}",
                        path.display(),
                        e
                    ),
                })?;
        }
        fs::write(&path, content)
            .await
            .map_err(|e| WorkspaceError::Io {
                reason: format!("Failed to write {}: {}", path.display(), e),
            })
    }

    async fn read_optional(&self, file: &str) -> Result<Option<String>, WorkspaceError> {
        self.ensure_dirs().await?;
        let path = self.resolve_file(file);
        match fs::read_to_string(&path).await {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(WorkspaceError::Io {
                reason: format!("Failed to read {}: {}", path.display(), e),
            }),
        }
    }

    async fn write_if_missing(&self, file: &str, content: &str) -> Result<(), WorkspaceError> {
        self.ensure_dirs().await?;
        let path = self.resolve_file(file);
        if fs::metadata(&path).await.is_ok() {
            return Ok(());
        }
        fs::write(&path, content)
            .await
            .map_err(|e| WorkspaceError::Io {
                reason: format!("Failed to seed {}: {}", path.display(), e),
            })
    }

    /// Build the system prompt from identity files.
    ///
    /// Unlike the legacy DB-backed Workspace, this does NOT inject MEMORY.md or
    /// daily logs. Those are handled via ledger recall instead.
    pub async fn system_prompt_for_context(
        &self,
        _is_group_chat: bool,
    ) -> Result<String, WorkspaceError> {
        let mut parts = Vec::new();

        let identity_files = [
            (paths::AGENTS, "## Agent Instructions"),
            (paths::SOUL, "## Core Values"),
            (paths::USER, "## User Context"),
            (paths::IDENTITY, "## Identity"),
        ];

        for (file, header) in identity_files {
            if let Some(content) = self.read_optional(file).await?
                && !content.trim().is_empty()
            {
                parts.push(format!("{}\n\n{}", header, content.trim_end()));
            }
        }

        Ok(parts.join("\n\n---\n\n"))
    }

    /// Convenience wrapper used by subsystems that don't need per-context behavior.
    pub async fn system_prompt(&self) -> Result<String, WorkspaceError> {
        self.system_prompt_for_context(false).await
    }

    /// Load (and seed if missing) HEARTBEAT.md checklist.
    pub async fn heartbeat_checklist(&self) -> Result<Option<String>, WorkspaceError> {
        self.write_if_missing(paths::HEARTBEAT, HEARTBEAT_SEED).await?;
        self.read_optional(paths::HEARTBEAT).await
    }
}
