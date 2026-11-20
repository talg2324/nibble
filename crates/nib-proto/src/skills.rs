//! Skills on disk: `<dir>/<name>/SKILL.md`, with `name` / `description` in YAML frontmatter.
//! Shared by nib (its own and the user's skills) and nibd (the assistant's and the project's).

use crate::{AiSkill, SkillGroup};
use std::fs;
use std::path::Path;

/// The skills in `dir`, by name. A folder without a readable `SKILL.md` is skipped.
pub fn list(dir: &Path, group: SkillGroup) -> Vec<AiSkill> {
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<AiSkill> = read
        .flatten()
        .filter_map(|e| {
            let path = e.path().join("SKILL.md");
            let text = fs::read_to_string(&path).ok()?;
            let (meta, _) = split(&text);
            let folder = e.file_name().to_string_lossy().into_owned();
            Some(AiSkill {
                name: field(meta, "name").filter(|n| !n.contains(char::is_whitespace)).unwrap_or(folder),
                description: field(meta, "description").unwrap_or_default(),
                group,
                path: path.to_string_lossy().into_owned(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// (frontmatter, body). No frontmatter: ("", all of it).
pub fn split(text: &str) -> (&str, &str) {
    let Some(rest) = text.strip_prefix("---").and_then(|r| r.strip_prefix('\n').or_else(|| r.strip_prefix("\r\n"))) else {
        return ("", text);
    };
    let mut at = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return (&rest[..at], rest[at + line.len()..].trim_start_matches(['\r', '\n']));
        }
        at += line.len();
    }
    ("", text)
}

/// A top-level frontmatter field: on its line, quoted or not, or folded over the indented lines
/// after it (`description: >`).
fn field(meta: &str, key: &str) -> Option<String> {
    let mut lines = meta.lines();
    while let Some(line) = lines.next() {
        let Some(v) = line.strip_prefix(key).and_then(|r| r.strip_prefix(':')) else { continue };
        let v = v.trim();
        let v = if v.is_empty() || matches!(v, ">" | "|" | ">-" | "|-") {
            let more: Vec<&str> = lines.clone().take_while(|l| l.starts_with([' ', '\t']) || l.is_empty()).map(str::trim).collect();
            more.join(" ").trim().to_string()
        } else {
            v.trim_matches(|c| c == '"' || c == '\'').to_string()
        };
        return Some(v).filter(|v| !v.is_empty());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_fields_and_body() {
        let t = "---\nname: nib-ai\ndescription: >\n  Run local models\n  and sub-agents.\nother: x\n---\n\n# Body\n";
        let (meta, body) = split(t);
        assert_eq!(field(meta, "name").as_deref(), Some("nib-ai"));
        assert_eq!(field(meta, "description").as_deref(), Some("Run local models and sub-agents."));
        assert_eq!(body, "# Body\n");
        assert_eq!(field(split("---\ndescription: \"Quoted: yes\"\n---\nb").0, "description").as_deref(), Some("Quoted: yes"));
        assert_eq!(split("no frontmatter"), ("", "no frontmatter"));
    }
}
