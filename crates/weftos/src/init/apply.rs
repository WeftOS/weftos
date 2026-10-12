//! Write a [`Plan`] to disk, and format it for the terminal. Only items whose
//! action writes are touched; the lock is written only when it changed, so a
//! repeat `--apply` is a true no-op.

use std::fs;
use std::path::Path;

use super::InitError;
use super::lock::LOCK_PATH;
use super::plan::{Action, Plan};
use super::render::{BLOCK_BEGIN, BLOCK_END, Merge};

pub fn read_text(path: &Path) -> Result<Option<String>, InitError> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(fs::read_to_string(path)?))
}

/// Byte range of the marked block, including one trailing newline.
fn block_range(text: &str) -> Option<(usize, usize)> {
    let start = text.find(BLOCK_BEGIN)?;
    let end = start + text[start..].find(BLOCK_END)? + BLOCK_END.len();
    let end = if text[end..].starts_with('\n') {
        end + 1
    } else {
        end
    };
    Some((start, end))
}

pub fn extract_block(text: &str) -> Option<&str> {
    block_range(text).map(|(s, e)| &text[s..e])
}

/// Replace the marked block, or append it (creating the file if needed).
pub fn splice_block(existing: Option<&str>, block: &str) -> String {
    match existing {
        Some(text) => match block_range(text) {
            Some((s, e)) => format!("{}{block}{}", &text[..s], &text[e..]),
            None if text.trim().is_empty() => block.to_string(),
            None => format!("{}\n\n{block}", text.trim_end()),
        },
        None => block.to_string(),
    }
}

fn remove_block(text: &str) -> String {
    match block_range(text) {
        Some((s, e)) => {
            format!("{}{}", text[..s].trim_end_matches('\n'), &text[e..])
                .trim_end()
                .to_string()
                + "\n"
        }
        None => text.to_string(),
    }
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), InitError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)?;
    Ok(())
}

/// Remove now-empty directories between `path` and `root`.
fn prune_empty_parents(root: &Path, path: &Path) {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

/// Write every writing item of `plan`, then the lock if it changed.
/// Returns the number of paths written or deleted.
pub fn apply(plan: &Plan) -> Result<usize, InitError> {
    let root = &plan.root;
    let mut touched = 0;
    for it in plan.items.iter().filter(|i| i.action.writes()) {
        let path = root.join(&it.path);
        match (it.action, it.merge) {
            (Action::Delete, Merge::File) => {
                fs::remove_file(&path)?;
                prune_empty_parents(root, &path);
            }
            (Action::Delete, Merge::Block) => {
                if let Some(text) = read_text(&path)? {
                    let rest = remove_block(&text);
                    if rest.trim().is_empty() {
                        // The block was all the file held: WeftOS created it.
                        fs::remove_file(&path)?;
                        prune_empty_parents(root, &path);
                    } else {
                        fs::write(&path, rest)?;
                    }
                }
            }
            (_, Merge::Block) => {
                let block = String::from_utf8_lossy(&it.bytes);
                let text = splice_block(read_text(&path)?.as_deref(), &block);
                write_file(&path, text.as_bytes())?;
            }
            (_, Merge::File) => write_file(&path, &it.bytes)?,
        }
        touched += 1;
    }
    if plan.lock_remove {
        let lock = root.join(LOCK_PATH);
        fs::remove_file(&lock)?;
        prune_empty_parents(root, &lock);
        touched += 1;
    } else if plan.lock_changed {
        write_file(&root.join(LOCK_PATH), plan.lock_json.as_bytes())?;
        touched += 1;
    }
    Ok(touched)
}

/// Human-readable plan (and, after apply, result) for the terminal.
pub fn format_plan(plan: &Plan, applied: bool) -> String {
    let short = if plan.commit.len() > 12 {
        &plan.commit[..12]
    } else {
        &plan.commit
    };
    let mut s = format!(
        "weftos init {} — target {}\n  source: {} (weftos {} @ {short})\n  team: {}\n  agents: {}\n",
        if applied { "apply" } else { "plan" },
        plan.root.display(),
        plan.source,
        crate::VERSION,
        plan.team.as_deref().unwrap_or("-"),
        plan.selected.join(", "),
    );
    for w in &plan.warnings {
        s.push_str(&format!("  warning: {w}\n"));
    }
    s.push('\n');
    let width = plan.items.iter().map(|i| i.path.len()).max().unwrap_or(0);
    for it in &plan.items {
        let tag = it.host.map(|h| h.as_str()).unwrap_or("-");
        s.push_str(&format!(
            "  {:<9} {:<width$}  [{tag}/{}]\n",
            it.action.label(),
            it.path,
            it.owner
        ));
    }
    s.push_str(&format!(
        "  {:<9} {LOCK_PATH}\n\n",
        if plan.lock_changed {
            "write"
        } else {
            "unchanged"
        }
    ));
    let summary: Vec<String> = [
        Action::Create,
        Action::Seed,
        Action::Update,
        Action::Unchanged,
        Action::Drifted,
        Action::Unmanaged,
        Action::Overwrite,
        Action::Delete,
        Action::Release,
        Action::Keep,
    ]
    .iter()
    .filter_map(|&a| {
        let n = plan.count(a);
        (n > 0).then(|| format!("{n} {}", if a == Action::Seed { "seed" } else { a.label() }))
    })
    .collect();
    s.push_str(&format!("Summary: {}\n", summary.join(", ")));
    if plan.count(Action::Drifted) + plan.count(Action::Unmanaged) > 0 {
        s.push_str("Drifted/unmanaged files were left as they are; re-run with --force to overwrite them.\n");
    }
    if applied {
        if plan.is_noop() {
            s.push_str("No changes: everything is up to date.\n");
        }
    } else if !plan.is_noop() {
        s.push_str("Plan only. Re-run with --apply to write these changes (weftos never commits or pushes).\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "<!-- weftos:agents:begin -->\nnew\n<!-- weftos:agents:end -->\n";

    #[test]
    fn splice_creates_appends_and_replaces() {
        assert_eq!(splice_block(None, BLOCK), BLOCK);
        let appended = splice_block(Some("# Notes\n\nkeep me\n"), BLOCK);
        assert_eq!(appended, format!("# Notes\n\nkeep me\n\n{BLOCK}"));
        let old = "top\n<!-- weftos:agents:begin -->\nold\n<!-- weftos:agents:end -->\nbottom\n";
        assert_eq!(
            splice_block(Some(old), BLOCK),
            format!("top\n{BLOCK}bottom\n")
        );
    }

    #[test]
    fn extract_block_matches_spliced_block() {
        let text = splice_block(Some("intro\n"), BLOCK);
        assert_eq!(extract_block(&text), Some(BLOCK));
        assert_eq!(extract_block("no markers"), None);
    }

    #[test]
    fn remove_block_keeps_surrounding_text() {
        let text = format!("intro\n\n{BLOCK}");
        assert_eq!(remove_block(&text), "intro\n");
    }
}
