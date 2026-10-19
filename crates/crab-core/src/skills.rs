//! Skills discovery: Agent Skills spec directories → system-prompt advertisement.
//!
//! Spec: https://agentskills.io/specification (我们只消费 name/description)。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A discovered skill: a directory containing SKILL.md.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub skill_md: PathBuf,
}

/// frontmatter → (name, description)；None 表示没有 frontmatter 块。
fn parse_frontmatter(content: &str) -> Option<(Option<String>, Option<String>)> {
    let mut lines = content.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let mut name = None;
    let mut description = None;
    for line in lines {
        let line = line.trim_end();
        if line == "---" {
            return Some((name, description));
        }
        if let Some((k, v)) = line.split_once(':') {
            let v = v.trim();
            match k.trim() {
                "name" => name = Some(v.to_string()),
                "description" => description = Some(v.to_string()),
                _ => {} // license/metadata/allowed-tools 等一律忽略（YAGNI）
            }
        }
    }
    None // 没有闭合 ---：视为无 frontmatter
}

/// 发现 skill：项目级 `.agents/skills`（cwd 向上，停在 git repo 根，含根本身；
/// 无 repo 则走到文件系统根）+ 用户级 `home/.agents/skills`。
/// 同名先发现者赢（项目 > 用户，近 > 远）；返回按 name 排序。
pub fn discover(cwd: &Path, home: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();
    let mut seen_dirs: HashSet<PathBuf> = HashSet::new();
    let mut seen_names: HashSet<String> = HashSet::new();

    // 项目级：cwd 向上逐层，含 git repo 根本身后停止。
    let mut dir = cwd.to_path_buf();
    loop {
        scan_dir(
            &dir.join(".agents/skills"),
            &mut skills,
            &mut seen_dirs,
            &mut seen_names,
        );
        if dir.join(".git").exists() {
            break;
        }
        match dir.parent() {
            Some(p) if p != dir => dir = p.to_path_buf(),
            _ => break,
        }
    }
    // 用户级最后：项目级（近 > 远）优先于用户级。
    scan_dir(
        &home.join(".agents/skills"),
        &mut skills,
        &mut seen_dirs,
        &mut seen_names,
    );

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

fn scan_dir(
    skills_dir: &Path,
    skills: &mut Vec<Skill>,
    seen_dirs: &mut HashSet<PathBuf>,
    seen_names: &mut HashSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return; // 目录不存在：正常，不是错误
    };
    // 排序保证发现顺序（同名冲突谁赢）确定。
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let skill_md = dir.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        let canon = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if !seen_dirs.insert(canon) {
            continue;
        }
        let label = dir.display();
        let Ok(content) = std::fs::read_to_string(&skill_md) else {
            eprintln!("[crab] skill '{label}': SKILL.md not valid UTF-8 (skipped)");
            continue;
        };
        let Some((fm_name, fm_desc)) = parse_frontmatter(&content) else {
            eprintln!("[crab] skill '{label}': no frontmatter (skipped)");
            continue;
        };
        let Some(description) = fm_desc.filter(|d| !d.is_empty()) else {
            eprintln!("[crab] skill '{label}': no description (skipped)");
            continue;
        };
        let name = fm_name.filter(|n| !n.is_empty()).unwrap_or_else(|| {
            dir.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
        if !seen_names.insert(name.clone()) {
            eprintln!("[crab] skill '{name}' in {label}: duplicate name (skipped, first found wins)");
            continue;
        }
        skills.push(Skill {
            name,
            description,
            dir,
            skill_md,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在 root 下建 skill 目录并写 SKILL.md，返回目录路径。
    fn write_skill(root: &Path, rel: &str, body: &str) -> PathBuf {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), body).unwrap();
        dir
    }

    const TDD: &str = "---\nname: tdd\ndescription: Test-driven development. Use when test-first.\n---\n\n# TDD\n";
    const REVIEW: &str = "---\nname: code-review\ndescription: Review changes. Use when asked to review.\n---\n\n# Review\n";

    #[test]
    fn frontmatter_parses_name_and_description() {
        let (name, desc) = parse_frontmatter(TDD).unwrap();
        assert_eq!(name.as_deref(), Some("tdd"));
        assert_eq!(desc.as_deref(), Some("Test-driven development. Use when test-first."));
    }

    #[test]
    fn frontmatter_value_may_contain_colons() {
        let (_, desc) = parse_frontmatter("---\ndescription: Use when: review, audit\n---\n").unwrap();
        assert_eq!(desc.as_deref(), Some("Use when: review, audit"));
    }

    #[test]
    fn frontmatter_tolerates_crlf() {
        let (name, desc) = parse_frontmatter("---\r\nname: tdd\r\ndescription: d\r\n---\r\nbody").unwrap();
        assert_eq!(name.as_deref(), Some("tdd"));
        assert_eq!(desc.as_deref(), Some("d"));
    }

    #[test]
    fn no_frontmatter_is_none() {
        assert!(parse_frontmatter("# Just markdown\nno frontmatter here").is_none());
        assert!(parse_frontmatter("---\nname: x\n").is_none()); // 无闭合 ---
    }

    #[test]
    fn discover_finds_user_and_project_skills() {
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        write_skill(home.path(), ".agents/skills/tdd", TDD);
        write_skill(proj.path(), ".agents/skills/code-review", REVIEW);

        let skills = discover(proj.path(), home.path());
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "code-review"); // 按 name 排序
        assert_eq!(skills[1].name, "tdd");
        assert_eq!(skills[1].description, "Test-driven development. Use when test-first.");
        assert_eq!(skills[1].skill_md, home.path().join(".agents/skills/tdd/SKILL.md"));
    }

    #[test]
    fn walk_up_stops_at_repo_root() {
        let root = tempfile::tempdir().unwrap(); // 充当 /
        let repo = root.path().join("repo");
        let cwd = repo.join("sub");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir(repo.join(".git")).unwrap();
        // repo 根之上还有一层 skill——不应被发现
        write_skill(root.path(), ".agents/skills/outer", TDD);
        // 不写 name：默认取目录名 "inner"
        write_skill(&repo, ".agents/skills/inner", "---\ndescription: inner skill\n---\n");

        let home = root.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let skills = discover(&cwd, &home);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "inner");
    }

    #[test]
    fn no_repo_walks_past_to_filesystem_root() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("a/b/c");
        std::fs::create_dir_all(&cwd).unwrap();
        write_skill(root.path(), ".agents/skills/tdd", TDD);

        let home = root.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let skills = discover(&cwd, &home);
        assert_eq!(skills.len(), 1, "no .git anywhere: must walk all the way up");
    }

    #[test]
    fn project_skill_beats_user_skill_on_name_collision() {
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        write_skill(home.path(), ".agents/skills/tdd", TDD);
        let body = "---\nname: tdd\ndescription: project-local override\n---\n";
        let dir = write_skill(proj.path(), ".agents/skills/local-name-tdd", body);

        let skills = discover(proj.path(), home.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "project-local override");
        assert_eq!(skills[0].dir, dir);
    }

    #[test]
    fn missing_description_is_skipped() {
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        write_skill(proj.path(), ".agents/skills/nodef", "---\nname: nodef\n---\n");
        write_skill(proj.path(), ".agents/skills/emptydef", "---\ndescription:\n---\n");
        write_skill(proj.path(), ".agents/skills/ok", REVIEW);
        assert_eq!(discover(proj.path(), home.path()).len(), 1);
    }

    #[test]
    fn name_defaults_to_directory_name() {
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        write_skill(proj.path(), ".agents/skills/my-skill", "---\ndescription: d\n---\n");
        let skills = discover(proj.path(), home.path());
        assert_eq!(skills[0].name, "my-skill");
    }

    #[test]
    fn non_utf8_skill_md_is_skipped() {
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let dir = proj.path().join(".agents/skills/bin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), [0xff, 0xfe, 0xfd]).unwrap();
        assert!(discover(proj.path(), home.path()).is_empty());
    }

    #[test]
    fn duplicate_dirs_via_canonicalize_are_scanned_once() {
        let home = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        write_skill(proj.path(), ".agents/skills/tdd", TDD);
        write_skill(proj.path(), ".agents/skills/tdd2", TDD); // 同名，不同目录
        let skills = discover(proj.path(), home.path());
        assert_eq!(skills.len(), 1, "same name twice: first (sorted dir order) wins");
    }
}
