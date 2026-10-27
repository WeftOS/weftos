//! Walk docs trees, chunk markdown/MDX, and derive tags / URLs.

use std::path::{Path, PathBuf};

use regex::Regex;
use walkdir::WalkDir;

const MAX_CHUNK_CHARS: usize = 2000;
const MAX_FILE_CHARS: usize = 400_000;
const FUMADOCS_BASE_URL: &str = "https://weftos.weavelogic.ai";
const GITHUB_BLOB_BASE: &str = "https://github.com/WeftOS/weftos/blob/HEAD";

#[derive(Debug, Default)]
pub struct Frontmatter {
    pub title: String,
    #[allow(dead_code)]
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct Chunk {
    pub heading: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct DocChunk {
    pub slug: String,
    pub source: String,
    pub doc_url: String,
    pub title: String,
    pub section: String,
    pub tags: Vec<String>,
    pub category: String,
    pub text: String,
    pub has_code: bool,
    pub chunk_index: usize,
    pub total_chunks: usize,
}

pub fn parse_frontmatter(content: &str) -> (Frontmatter, &str) {
    let mut fm = Frontmatter::default();

    if !content.starts_with("---") {
        return (fm, content);
    }

    let after_open = &content[3..];
    let Some(close) = after_open.find("\n---") else {
        return (fm, content);
    };

    let yaml_block = &after_open[..close];
    let body_start = 3 + close + 4;
    let body = if body_start < content.len() {
        &content[body_start..]
    } else {
        ""
    };

    for line in yaml_block.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("title:") {
            fm.title = unquote(rest.trim());
        } else if let Some(rest) = line.strip_prefix("description:") {
            fm.description = unquote(rest.trim());
        }
    }

    (fm, body)
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

pub fn strip_mdx(text: &str) -> String {
    let import_re = Regex::new(r"(?m)^import\s+.*$").unwrap();
    let jsx_re = Regex::new(r"</?[A-Z][A-Za-z0-9]*[^>]*/?>\s*").unwrap();
    let text = import_re.replace_all(text, "");
    let text = jsx_re.replace_all(&text, "");
    text.to_string()
}

pub fn chunk_by_headings(body: &str) -> Vec<Chunk> {
    let heading_re = Regex::new(r"(?m)^(#{2,3})\s+(.+)$").unwrap();

    let mut chunks: Vec<Chunk> = Vec::new();
    let mut last_heading = String::new();
    let mut last_start: usize = 0;
    let mut first = true;

    for m in heading_re.find_iter(body) {
        let caps = heading_re.captures(&body[m.start()..]).unwrap();
        let heading_text = caps.get(2).unwrap().as_str().trim().to_string();

        if first {
            let intro = body[..m.start()].trim();
            if !intro.is_empty() {
                chunks.push(Chunk {
                    heading: String::new(),
                    text: intro.to_string(),
                });
            }
            first = false;
        } else {
            let section_text = body[last_start..m.start()].trim();
            if !section_text.is_empty() {
                chunks.push(Chunk {
                    heading: last_heading.clone(),
                    text: section_text.to_string(),
                });
            }
        }

        last_heading = heading_text;
        last_start = m.start();
    }

    let tail = body[last_start..].trim();
    if !tail.is_empty() {
        chunks.push(Chunk {
            heading: last_heading,
            text: tail.to_string(),
        });
    }

    if chunks.is_empty() && !body.trim().is_empty() {
        chunks.push(Chunk {
            heading: String::new(),
            text: body.trim().to_string(),
        });
    }

    chunks
}

pub fn split_large_chunks(chunks: Vec<Chunk>) -> Vec<Chunk> {
    let mut result = Vec::new();
    for chunk in chunks {
        if chunk.text.len() <= MAX_CHUNK_CHARS {
            result.push(chunk);
            continue;
        }
        let paragraphs: Vec<&str> = chunk.text.split("\n\n").collect();
        let mut current = String::new();
        let mut sub_idx = 0;
        for para in &paragraphs {
            if !current.is_empty() && current.len() + para.len() + 2 > MAX_CHUNK_CHARS {
                result.push(Chunk {
                    heading: if sub_idx == 0 {
                        chunk.heading.clone()
                    } else {
                        format!("{} (cont.)", chunk.heading)
                    },
                    text: current.clone(),
                });
                current.clear();
                sub_idx += 1;
            }
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(para);
        }
        if !current.is_empty() {
            result.push(Chunk {
                heading: if sub_idx == 0 {
                    chunk.heading.clone()
                } else {
                    format!("{} (cont.)", chunk.heading)
                },
                text: current,
            });
        }
    }
    result
}

pub fn is_fumadocs_dir(docs_dir: &Path) -> bool {
    docs_dir
        .components()
        .rev()
        .take(2)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        == ["docs", "content"]
        || docs_dir.ends_with("content/docs")
}

fn rel_posix(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub fn source_and_url(file_path: &Path, docs_dir: &Path, repo_root: &Path) -> (String, String) {
    if is_fumadocs_dir(docs_dir) {
        let rel = file_path.strip_prefix(docs_dir).unwrap_or(file_path);
        let without_ext = rel.with_extension("");
        let mut path_str = without_ext.to_string_lossy().replace('\\', "/");
        if path_str.ends_with("/index") {
            path_str = path_str[..path_str.len() - 6].to_string();
        }
        let src = format!("/docs/{path_str}");
        let url = format!("{FUMADOCS_BASE_URL}{src}");
        (src, url)
    } else {
        let src = rel_posix(file_path, repo_root);
        let url = format!("{GITHUB_BLOB_BASE}/{src}");
        (src, url)
    }
}

pub fn page_slug(file_path: &Path, docs_dir: &Path, repo_root: &Path) -> String {
    let rel = if is_fumadocs_dir(docs_dir) {
        file_path.strip_prefix(docs_dir).unwrap_or(file_path)
    } else {
        file_path.strip_prefix(repo_root).unwrap_or(file_path)
    };
    let without_ext = rel.with_extension("");
    let mut slug = without_ext
        .to_string_lossy()
        .replace('\\', "/")
        .replace('/', "-");
    if slug.ends_with("-index") {
        slug = slug[..slug.len() - 6].to_string();
    }
    slug
}

pub fn tags_from_path(file_path: &Path, docs_dir: &Path, repo_root: &Path) -> Vec<String> {
    let rel = if is_fumadocs_dir(docs_dir) {
        file_path.strip_prefix(docs_dir).unwrap_or(file_path)
    } else {
        file_path.strip_prefix(repo_root).unwrap_or(file_path)
    };
    rel.with_extension("")
        .iter()
        .map(|c| c.to_string_lossy().to_string())
        .filter(|s| s != "index" && s != "docs" && s != "src" && s != "content")
        .collect()
}

pub fn category_from_path(file_path: &Path, docs_dir: &Path, repo_root: &Path) -> String {
    tags_from_path(file_path, docs_dir, repo_root)
        .first()
        .cloned()
        .unwrap_or_else(|| "general".to_string())
}

fn is_doc_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("mdx"))
}

fn should_skip(path: &Path, docs_dir: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.starts_with('.') {
        return true;
    }
    if name.contains("Zone.Identifier") {
        return true;
    }
    let rel = path.strip_prefix(docs_dir).unwrap_or(path);
    let s = rel.to_string_lossy().replace('\\', "/");
    // Relative to this walk root: skip ticket dumps, generated site trees,
    // and image mockups. Fumadocs MDX is ingested via its own --docs-dir.
    let first = s.split('/').next().unwrap_or("");
    first == "plans"
        || first == "src"
        || s.contains("/mockups/")
        || s.contains("/public/api/")
}

/// Collect heading-chunked documents from one docs root.
pub fn collect_from_dir(docs_dir: &Path, repo_root: &Path) -> (usize, Vec<DocChunk>) {
    let mut file_count = 0;
    let mut out = Vec::new();
    let code_fence_re = Regex::new(r"```").unwrap();

    for entry in WalkDir::new(docs_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file() && is_doc_file(e.path()) && !should_skip(e.path(), docs_dir)
        })
    {
        let path = entry.path();
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Warning: could not read {}: {e}", path.display());
                continue;
            }
        };
        if content.len() > MAX_FILE_CHARS {
            eprintln!(
                "Warning: skipping oversized {} ({} chars)",
                path.display(),
                content.len()
            );
            continue;
        }

        file_count += 1;
        let (fm, body) = parse_frontmatter(&content);
        let cleaned = strip_mdx(body);
        let chunks = split_large_chunks(chunk_by_headings(&cleaned));
        let slug = page_slug(path, docs_dir, repo_root);
        let (src, doc_url) = source_and_url(path, docs_dir, repo_root);
        let tags = tags_from_path(path, docs_dir, repo_root);
        let category = category_from_path(path, docs_dir, repo_root);
        let total_chunks = chunks.len();
        let title = if fm.title.is_empty() {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| slug.clone())
        } else {
            fm.title
        };

        for (i, chunk) in chunks.into_iter().enumerate() {
            let has_code = code_fence_re.find_iter(&chunk.text).count() >= 2;
            let section = if chunk.heading.is_empty() {
                title.clone()
            } else {
                chunk.heading
            };
            out.push(DocChunk {
                slug: slug.clone(),
                source: src.clone(),
                doc_url: doc_url.clone(),
                title: title.clone(),
                section,
                tags: tags.clone(),
                category: category.clone(),
                text: chunk.text,
                has_code,
                chunk_index: i,
                total_chunks,
            });
        }
    }

    (file_count, out)
}

pub fn collect_from_dirs(docs_dirs: &[PathBuf], repo_root: &Path) -> (usize, Vec<DocChunk>) {
    let mut files = 0;
    let mut chunks = Vec::new();
    for dir in docs_dirs {
        let (n, mut part) = collect_from_dir(dir, repo_root);
        files += n;
        chunks.append(&mut part);
    }
    (files, chunks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn headings_split_intro_and_sections() {
        let body = "Intro para.\n\n## First\n\nAlpha.\n\n### Nested\n\nBeta.\n";
        let chunks = chunk_by_headings(body);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].heading, "");
        assert!(chunks[0].text.contains("Intro"));
        assert_eq!(chunks[1].heading, "First");
        assert_eq!(chunks[2].heading, "Nested");
    }

    #[test]
    fn fumadocs_url_uses_site_path() {
        let docs_dir = Path::new("/repo/docs/src/content/docs");
        let file = docs_dir.join("clawft/providers.mdx");
        let (src, url) = source_and_url(&file, docs_dir, Path::new("/repo"));
        assert_eq!(src, "/docs/clawft/providers");
        assert!(url.contains("weftos.weavelogic.ai/docs/clawft/providers"));
    }

    #[test]
    fn markdown_url_uses_github_blob() {
        let docs_dir = Path::new("/repo/docs/adr");
        let file = docs_dir.join("adr-096-metaharness-foundation.md");
        let (src, url) = source_and_url(&file, docs_dir, Path::new("/repo"));
        assert_eq!(src, "docs/adr/adr-096-metaharness-foundation.md");
        assert!(url.contains("github.com/WeftOS/weftos/blob/HEAD/docs/adr/"));
    }

    #[test]
    fn tags_drop_fumadocs_prefix_and_keep_adr() {
        let docs_dir = Path::new("/repo/docs/src/content/docs");
        let file = docs_dir.join("weftos/guides/agent-harness.mdx");
        let tags = tags_from_path(&file, docs_dir, Path::new("/repo"));
        assert_eq!(tags, vec!["weftos", "guides", "agent-harness"]);

        let adr_dir = Path::new("/repo/docs/adr");
        let adr = adr_dir.join("adr-096-metaharness-foundation.md");
        let tags = tags_from_path(&adr, adr_dir, Path::new("/repo"));
        assert!(tags.contains(&"adr".to_string()));
        assert!(tags.iter().any(|t| t.contains("metaharness")));
    }

    #[test]
    fn walk_includes_md_and_mdx() {
        let tmp = std::env::temp_dir().join(format!("build-kb-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("adr")).unwrap();
        fs::write(
            tmp.join("adr/adr-096-metaharness-foundation.md"),
            "# MetaHarness\n\n## Doctrine\n\nFreeze the model.\n",
        )
        .unwrap();
        fs::write(
            tmp.join("page.mdx"),
            "---\ntitle: Hello\n---\n\n## Intro\n\nWorld.\n",
        )
        .unwrap();

        let (n, chunks) = collect_from_dir(&tmp, &tmp);
        let _ = fs::remove_dir_all(&tmp);
        assert_eq!(n, 2);
        assert!(chunks.iter().any(|c| c.text.contains("Freeze the model")));
        assert!(chunks.iter().any(|c| c.title == "Hello"));
    }
}
