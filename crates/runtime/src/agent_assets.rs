//! Discovery of agent assets: rule files and skill definitions.
//!
//! Layers are passed in priority order (user home first, project second);
//! project-level entries win over user-level ones with the same identity.

use std::fs;
use std::path::{Path, PathBuf};

const MAX_RULE_FILES: usize = 32;
const MAX_RULE_FILE_BYTES: usize = 32 * 1024;
const MAX_SKILLS: usize = 64;

/// One rule file ready for full injection into the system prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleFile {
    pub path: PathBuf,
    pub content: String,
}

/// One-level skill metadata; full text stays on disk until requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// Collect `.agent/rules/*.md` from every root, sorted by file name per layer.
#[must_use]
pub fn discover_rules(roots: &[&Path]) -> Vec<RuleFile> {
    let mut rules = Vec::new();
    for root in roots {
        let dir = root.join(".agent").join("rules");
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        files.sort();
        for path in files {
            if rules.len() >= MAX_RULE_FILES {
                return rules;
            }
            if let Ok(content) = fs::read_to_string(&path) {
                if content.trim().is_empty() {
                    continue;
                }
                rules.push(RuleFile {
                    path,
                    content: truncate_utf8(&content, MAX_RULE_FILE_BYTES).to_string(),
                });
            }
        }
    }
    rules
}

/// Collect `.agent/skills/*/SKILL.md` metadata from every root. Later roots
/// override earlier ones by skill name (project beats user).
#[must_use]
pub fn discover_skills(roots: &[&Path]) -> Vec<SkillSummary> {
    let mut skills: Vec<SkillSummary> = Vec::new();
    for root in roots {
        let dir = root.join(".agent").join("skills");
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut skill_dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        skill_dirs.sort();
        for skill_dir in skill_dirs {
            let skill_file = skill_dir.join("SKILL.md");
            let Ok(content) = fs::read_to_string(&skill_file) else {
                continue;
            };
            let (front_name, description) = parse_frontmatter(&content);
            let name = front_name.unwrap_or_else(|| {
                skill_dir
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
            });
            if name.is_empty() {
                continue;
            }
            let summary = SkillSummary {
                name,
                description,
                path: skill_file,
            };
            match skills
                .iter_mut()
                .find(|existing| existing.name == summary.name)
            {
                Some(existing) => *existing = summary,
                None => {
                    if skills.len() < MAX_SKILLS {
                        skills.push(summary);
                    }
                }
            }
        }
    }
    skills
}

/// Parse `name:` / `description:` keys from a minimal YAML frontmatter block.
fn parse_frontmatter(content: &str) -> (Option<String>, String) {
    let mut lines = content.lines();
    if lines.next().is_none_or(|first| first.trim() != "---") {
        return (None, String::new());
    }

    let mut name = None;
    let mut description = String::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("name:") {
            name = Some(unquote(value));
        } else if let Some(value) = trimmed.strip_prefix("description:") {
            description = unquote(value);
        }
    }
    (name, description)
}

fn unquote(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_string()
}

fn truncate_utf8(content: &str, max_bytes: usize) -> &str {
    if content.len() <= max_bytes {
        return content;
    }
    let mut end = max_bytes;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    &content[..end]
}

#[cfg(test)]
mod tests {
    use super::{discover_rules, discover_skills, parse_frontmatter, truncate_utf8};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-agent-{label}-{nanos}"))
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent should exist");
        }
        fs::write(path, content).expect("file should write");
    }

    #[test]
    fn discovers_rules_sorted_per_layer() {
        let root = temp_dir("rules");
        write(&root.join(".agent/rules/b-second.md"), "second");
        write(&root.join(".agent/rules/a-first.md"), "first");
        write(&root.join(".agent/rules/notes.txt"), "ignored");

        let rules = discover_rules(&[&root]);
        let names: Vec<&str> = rules
            .iter()
            .map(|rule| rule.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["a-first.md", "b-second.md"]);
        assert_eq!(rules[0].content, "first");

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn empty_rules_dirs_are_tolerated() {
        let root = temp_dir("rules-empty");
        fs::create_dir_all(root.join(".agent").join("rules")).expect("empty rules dir");
        assert_eq!(discover_rules(&[&root]), []);
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn discovers_skills_with_frontmatter_and_directory_fallback() {
        let root = temp_dir("skills");
        write(
            &root.join(".agent/skills/commit/SKILL.md"),
            "---\nname: commit\ndescription: \"Write commit messages\"\n---\n\nbody",
        );
        write(&root.join(".agent/skills/ghost/SKILL.md"), "no frontmatter");

        let skills = discover_skills(&[&root]);
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "commit");
        assert_eq!(skills[0].description, "Write commit messages");
        assert_eq!(skills[1].name, "ghost");
        assert_eq!(skills[1].description, "");

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn project_layer_overrides_user_layer_by_name() {
        let user = temp_dir("skills-user");
        let project = temp_dir("skills-project");
        write(
            &user.join(".agent/skills/review/SKILL.md"),
            "---\nname: review\ndescription: user version\n---",
        );
        write(
            &project.join(".agent/skills/review/SKILL.md"),
            "---\nname: review\ndescription: project version\n---",
        );

        let skills = discover_skills(&[&user, &project]);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "project version");

        fs::remove_dir_all(user).expect("cleanup");
        fs::remove_dir_all(project).expect("cleanup");
    }

    #[test]
    fn frontmatter_parser_handles_missing_block() {
        let (name, description) = parse_frontmatter("plain text only");
        assert_eq!(name, None);
        assert_eq!(description, "");
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_utf8("hello", 32), "hello");
        let truncated = truncate_utf8("你好世界", 7);
        assert!(truncated.len() <= 7);
        assert_eq!(truncated, "你好");
    }
}
