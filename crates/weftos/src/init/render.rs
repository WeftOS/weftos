//! Host-neutral render types plus the pieces shared across hosts: the
//! `AGENTS.md` pointer block and the project-context template.
//!
//! Rendering is pure: packages in, sorted `[Rendered]` out, no clock or I/O.

use serde::{Deserialize, Serialize};

use super::package::Package;
use super::{render_claude, render_codex, render_grok};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Host {
    Claude,
    Grok,
    Codex,
}

impl Host {
    pub fn as_str(self) -> &'static str {
        match self {
            Host::Claude => "claude",
            Host::Grok => "grok",
            Host::Codex => "codex",
        }
    }
}

/// How a rendered file lands on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Merge {
    /// WeftOS owns the whole file.
    File,
    /// WeftOS owns only the marked block ([`BLOCK_BEGIN`]..[`BLOCK_END`]).
    Block,
}

pub const BLOCK_BEGIN: &str = "<!-- weftos:agents:begin -->";
pub const BLOCK_END: &str = "<!-- weftos:agents:end -->";

/// Owner id used in the lock for the shared `AGENTS.md` block.
pub const INDEX_OWNER: &str = "_agents_md";

#[derive(Clone, Debug)]
pub struct Rendered {
    /// Path relative to the target root, forward slashes.
    pub path: String,
    pub bytes: Vec<u8>,
    pub host: Host,
    /// Package id, or [`INDEX_OWNER`].
    pub owner: String,
    pub merge: Merge,
}

/// Target-root layout differences between a project and `--global` ($HOME).
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub global: bool,
}

impl Layout {
    /// Codex skills: `.agents/skills` in a repo, `$CODEX_HOME/skills` globally.
    pub fn codex_skills_dir(self) -> &'static str {
        if self.global {
            ".codex/skills"
        } else {
            ".agents/skills"
        }
    }

    pub fn agents_md(self) -> &'static str {
        if self.global {
            ".codex/AGENTS.md"
        } else {
            "AGENTS.md"
        }
    }

    /// How a rendered path is written in pointer text.
    pub fn display(self, rel: &str) -> String {
        if self.global {
            format!("~/{rel}")
        } else {
            format!("`{rel}`")
        }
    }
}

/// Render every package for one host, sorted by path.
pub fn render_host(host: Host, packages: &[Package], layout: Layout) -> Vec<Rendered> {
    let mut out: Vec<Rendered> = packages
        .iter()
        .flat_map(|p| match host {
            Host::Claude => render_claude::render(p),
            Host::Grok => render_grok::render(p),
            Host::Codex => render_codex::render(p, layout),
        })
        .collect();
    if host == Host::Codex && !packages.is_empty() {
        out.push(Rendered {
            path: layout.agents_md().to_string(),
            bytes: agents_md_block(packages, layout).into_bytes(),
            host,
            owner: INDEX_OWNER.into(),
            merge: Merge::Block,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Skill files copied verbatim under `<skills_dir>/<skill>/`.
pub fn skill_files(p: &Package, host: Host, skills_dir: &str) -> Vec<Rendered> {
    p.skills
        .iter()
        .flat_map(|s| {
            s.files.iter().map(move |(rel, bytes)| Rendered {
                path: format!("{skills_dir}/{}/{rel}", s.name),
                bytes: bytes.clone(),
                host,
                owner: p.id.clone(),
                merge: Merge::File,
            })
        })
        .collect()
}

/// Collapse a multi-line description to one line.
pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A YAML double-quoted scalar (JSON strings are valid YAML).
pub fn yaml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// The first sentence of a description, capped for pointer lines.
pub fn summary(desc: &str) -> String {
    let flat = one_line(desc);
    // End of the first sentence: ". " before a capital, not after "vs." / "e.g.".
    let bytes = flat.as_bytes();
    let sentence_end = (1..bytes.len().saturating_sub(2)).find(|&i| {
        bytes[i] == b'.'
            && bytes[i + 1] == b' '
            && bytes[i + 2].is_ascii_uppercase()
            && !flat[..i].ends_with("vs")
            && !flat[..i].ends_with("e.g")
            && !flat[..i].ends_with("i.e")
    });
    let cut = sentence_end
        .map(|i| i + 1)
        .or_else(|| flat.find(" Use when:"))
        .unwrap_or(flat.len());
    let mut s: String = flat[..cut].trim().to_string();
    if s.chars().count() > 200 {
        s = s.chars().take(197).collect::<String>() + "...";
    }
    s
}

/// The marked `AGENTS.md` block pointing Codex at the installed agents and
/// skills (Codex has no subagent auto-discovery from AGENTS.md alone).
pub fn agents_md_block(packages: &[Package], layout: Layout) -> String {
    let skills_dir = layout.codex_skills_dir();
    let mut s = format!(
        "{BLOCK_BEGIN}\n## WeftOS agents\n\n\
         Managed by `weftos init --codex`; edit the source packages in weftos `agents/`, \
         not this block. Read {} first; it holds the project facts these agents expect.\n\n",
        if layout.global {
            "the project's `.agents/project-context.md`".to_string()
        } else {
            "`.agents/project-context.md`".to_string()
        }
    );
    for p in packages {
        let skills: Vec<String> = p
            .skills
            .iter()
            .map(|sk| layout.display(&format!("{skills_dir}/{}/SKILL.md", sk.name)))
            .collect();
        match &p.persona {
            Some(persona) => {
                let nick = persona
                    .nickname
                    .as_deref()
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default();
                s.push_str(&format!(
                    "- **{}**{nick}: {} Agent: {}.",
                    p.id,
                    summary(&persona.description),
                    layout.display(&format!(".codex/agents/{}.toml", p.id))
                ));
            }
            None => {
                let desc = p
                    .skills
                    .first()
                    .map(|sk| summary(&sk.description))
                    .unwrap_or_default();
                s.push_str(&format!(
                    "- **{}** (skill, not a spawnable agent): {desc}",
                    p.id
                ));
            }
        }
        if !skills.is_empty() {
            s.push_str(&format!(" Skills: {}.", skills.join(", ")));
        }
        s.push('\n');
    }
    s.push_str(BLOCK_END);
    s.push('\n');
    s
}

/// Template for `.agents/project-context.md`, listing every key the
/// installed packages expect. Written only when the file is absent.
///
/// A key in `detected` is filled with its value; the rest stay open questions.
pub fn project_context_template<'a>(
    packages: impl IntoIterator<Item = &'a Package>,
    detected: &std::collections::BTreeMap<String, String>,
) -> String {
    let mut s = String::from(
        "# Project context\n\n\
         Read first by every WeftOS agent installed here (`weftos init`). Fill in each value;\n\
         agents ask only for what is still missing. Client and project facts live here, never\n\
         in an agent file. WeftOS never overwrites this file.\n",
    );
    for p in packages.into_iter().filter(|p| !p.context.is_empty()) {
        s.push_str(&format!("\n## {}\n\n", p.id));
        for k in &p.context {
            let opt = if k.required { "" } else { " (optional)" };
            match detected.get(&k.key) {
                Some(v) => s.push_str(&format!("- `{}`{opt}: {v} (detected)\n", k.key)),
                None => s.push_str(&format!(
                    "- `{}`{opt}: TODO ({})\n",
                    k.key,
                    one_line(&k.description)
                )),
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_stops_at_first_real_sentence() {
        assert_eq!(summary("A lane. It builds things."), "A lane.");
        assert_eq!(
            summary("Compare x vs. y here. Next."),
            "Compare x vs. y here."
        );
        assert_eq!(summary("no period Use when: x"), "no period");
    }

    #[test]
    fn yaml_str_quotes_and_escapes() {
        assert_eq!(yaml_str("say \"hi\": now"), "\"say \\\"hi\\\": now\"");
    }
}
