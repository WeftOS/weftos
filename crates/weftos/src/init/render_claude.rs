//! Claude Code: `.claude/agents/<id>.md` and `.claude/skills/<skill>/`.
//!
//! Frontmatter follows the subagent format Claude Code loads: `name`,
//! `description`, `tools` (comma-separated), `model`. `skills` preloads the
//! package's skills into the subagent.

use super::package::Package;
use super::render::{Host, Merge, Rendered, one_line, skill_files, yaml_str};

pub fn render(p: &Package) -> Vec<Rendered> {
    let mut out = skill_files(p, Host::Claude, ".claude/skills");
    if let Some(persona) = &p.persona {
        let mut fm = format!(
            "---\nname: {}\ndescription: {}\n",
            p.id,
            yaml_str(&one_line(&persona.description))
        );
        if !persona.tools.is_empty() {
            fm.push_str(&format!("tools: {}\n", persona.tools.join(", ")));
        }
        if !p.skills.is_empty() {
            let names: Vec<&str> = p.skills.iter().map(|s| s.name.as_str()).collect();
            fm.push_str(&format!("skills: {}\n", names.join(", ")));
        }
        fm.push_str(&format!("model: {}\n---\n\n", persona.model.as_deref().unwrap_or("inherit")));
        out.push(Rendered {
            path: format!(".claude/agents/{}.md", p.id),
            bytes: (fm + &persona.body).into_bytes(),
            host: Host::Claude,
            owner: p.id.clone(),
            merge: Merge::File,
        });
    }
    out
}
