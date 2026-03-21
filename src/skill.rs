use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::fs;

/// A discovered skill from the workspace `skills/` directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Directory name (e.g., "weather")
    pub name: String,
    /// Absolute path to the skill directory
    pub path: PathBuf,
    /// Short description extracted from SKILL.md frontmatter
    pub description: String,
    /// Full SKILL.md content (instructions for the agent)
    pub instructions: String,
    /// Optional scripts directory path if it exists
    pub scripts_dir: Option<PathBuf>,
}

/// Frontmatter parsed from SKILL.md (optional YAML block between --- fences).
#[derive(Debug, Clone, Default)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
}

/// Discover all skills in the workspace's `skills/` directory.
/// Each skill is a subdirectory containing a `SKILL.md` file.
pub async fn discover_skills(workspace_root: &Path) -> Vec<Skill> {
    let skills_dir = workspace_root.join("skills");
    let mut skills = Vec::new();

    let mut entries = match fs::read_dir(&skills_dir).await {
        Ok(entries) => entries,
        Err(_) => return skills, // No skills directory — that's fine
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }
        let content = match fs::read_to_string(&skill_md).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        let (frontmatter, instructions) = parse_skill_md(&content);
        let dir_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let scripts_dir = path.join("scripts");
        let scripts_dir = if scripts_dir.is_dir() {
            Some(scripts_dir)
        } else {
            None
        };

        skills.push(Skill {
            name: frontmatter.name.unwrap_or_else(|| dir_name.clone()),
            path: path.clone(),
            description: frontmatter
                .description
                .unwrap_or_else(|| format!("Skill: {dir_name}")),
            instructions,
            scripts_dir,
        });
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Parse SKILL.md into optional frontmatter and body instructions.
fn parse_skill_md(content: &str) -> (SkillFrontmatter, String) {
    let trimmed = content.trim();

    // Look for YAML frontmatter between --- fences
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let yaml_block = &rest[..end];
            let body = rest[end + 4..].trim().to_string();
            let frontmatter = parse_frontmatter(yaml_block);
            return (frontmatter, body);
        }
    }

    (SkillFrontmatter::default(), trimmed.to_string())
}

/// Simple frontmatter parser — extracts `name:` and `description:` fields.
fn parse_frontmatter(yaml: &str) -> SkillFrontmatter {
    let mut name = None;
    let mut description = None;

    for line in yaml.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            name = Some(val.trim().trim_matches('"').to_string());
        } else if let Some(val) = line.strip_prefix("description:") {
            description = Some(val.trim().trim_matches('"').to_string());
        }
    }

    SkillFrontmatter { name, description }
}

/// Build a skills overview block for injection into the system prompt.
pub fn build_skills_block(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }

    let mut block = String::from("<skills>\nAvailable skills loaded from workspace:\n\n");
    for skill in skills {
        block.push_str(&format!("## {}\n", skill.name));
        block.push_str(&format!("{}\n\n", skill.description));
        if skill.scripts_dir.is_some() {
            block.push_str(&format!(
                "Scripts directory: skills/{}/scripts/\n\n",
                skill.name
            ));
        }
    }
    block.push_str(
        "To read a skill's full instructions, use the `read_skill` tool with the skill name.\n",
    );
    block.push_str("</skills>");
    Some(block)
}

/// Read a specific skill's full instructions by name.
pub async fn read_skill_by_name(workspace_root: &Path, skill_name: &str) -> Option<Skill> {
    let skills = discover_skills(workspace_root).await;
    skills.into_iter().find(|s| s.name == skill_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skill_md_with_frontmatter() {
        let content = r#"---
name: weather
description: Get weather forecasts
---

# Weather Skill

Use wttr.in or Open-Meteo to fetch weather data."#;

        let (fm, body) = parse_skill_md(content);
        assert_eq!(fm.name.as_deref(), Some("weather"));
        assert_eq!(fm.description.as_deref(), Some("Get weather forecasts"));
        assert!(body.contains("Weather Skill"));
    }

    #[test]
    fn parse_skill_md_without_frontmatter() {
        let content = "# My Skill\n\nSome instructions here.";
        let (fm, body) = parse_skill_md(content);
        assert!(fm.name.is_none());
        assert!(fm.description.is_none());
        assert_eq!(body, content);
    }

    #[test]
    fn build_skills_block_empty() {
        assert!(build_skills_block(&[]).is_none());
    }

    #[test]
    fn build_skills_block_with_skills() {
        let skills = vec![Skill {
            name: "weather".to_string(),
            path: PathBuf::from("/tmp/skills/weather"),
            description: "Get weather forecasts".to_string(),
            instructions: "Use wttr.in".to_string(),
            scripts_dir: None,
        }];
        let block = build_skills_block(&skills).unwrap();
        assert!(block.contains("<skills>"));
        assert!(block.contains("weather"));
        assert!(block.contains("Get weather forecasts"));
        assert!(block.contains("read_skill"));
    }

    #[test]
    fn parse_frontmatter_quoted_values() {
        let yaml = r#"
name: "my-skill"
description: "A test skill"
"#;
        let fm = parse_frontmatter(yaml);
        assert_eq!(fm.name.as_deref(), Some("my-skill"));
        assert_eq!(fm.description.as_deref(), Some("A test skill"));
    }
}
