//! workdir が linked worktree か通常リポジトリかの判別。
//! VM はホストの .git をそのまま rw マウントで共有するため（VM内コミット＝ホストに直接反映）、
//! ここで得た情報はマウント構成・メタデータ・`rm --with-worktree` にだけ使う。
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum RepoKind {
    Worktree,
    Normal,
}

#[derive(Debug, Clone)]
pub struct RepoInfo {
    pub kind: RepoKind,
    pub host_git: PathBuf,  // ホスト側の .git 実体（VM内でも同じパスに見える）
    pub host_repo: PathBuf, // リポジトリルート（worktreeならメインリポジトリ）
    pub branch: String,
}

fn head_branch(git_dir: &Path) -> String {
    std::fs::read_to_string(git_dir.join("HEAD"))
        .map(|s| s.trim().trim_start_matches("ref: refs/heads/").to_string())
        .unwrap_or_default()
}

/// workdir が linked worktree か通常リポジトリかを判別する。gitリポジトリでなければ None。
pub fn inspect_repo(workdir: &Path) -> Result<Option<RepoInfo>> {
    let dot = workdir.join(".git");
    let md = match std::fs::metadata(&dot) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if md.is_dir() {
        return Ok(Some(RepoInfo {
            kind: RepoKind::Normal,
            branch: head_branch(&dot),
            host_git: dot,
            host_repo: workdir.to_path_buf(),
        }));
    }
    let content = std::fs::read_to_string(&dot)?;
    let gd = content
        .trim()
        .strip_prefix("gitdir: ")
        .ok_or_else(|| anyhow!("unrecognized .git file in {}", workdir.display()))?;
    let gd = if Path::new(gd).is_absolute() {
        PathBuf::from(gd)
    } else {
        workdir.join(gd)
    };
    let host_git = gd
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow!("bad gitdir pointer"))?
        .to_path_buf(); // .git/worktrees/<name> → .git
    Ok(Some(RepoInfo {
        kind: RepoKind::Worktree,
        branch: head_branch(&gd),
        host_repo: host_git.parent().unwrap_or(Path::new("/")).to_path_buf(),
        host_git,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspects_normal_repository() {
        let root = tempfile::tempdir().unwrap();
        let git = root.path().join(".git");
        std::fs::create_dir(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();

        let info = inspect_repo(root.path()).unwrap().unwrap();
        assert_eq!(info.kind, RepoKind::Normal);
        assert_eq!(info.host_repo, root.path());
        assert_eq!(info.host_git, git);
        assert_eq!(info.branch, "main");
    }

    #[test]
    fn inspects_linked_worktree_with_absolute_gitdir() {
        let root = tempfile::tempdir().unwrap();
        let main = root.path().join("main");
        let worktree = root.path().join("feature");
        let gitdir = main.join(".git/worktrees/feature");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::create_dir(&worktree).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();

        let info = inspect_repo(&worktree).unwrap().unwrap();
        assert_eq!(info.kind, RepoKind::Worktree);
        assert_eq!(info.host_repo, main);
        assert_eq!(info.host_git, main.join(".git"));
        assert_eq!(info.branch, "feature");
    }

    #[test]
    fn rejects_unrecognized_git_pointer() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".git"), "not a gitdir\n").unwrap();
        assert!(inspect_repo(root.path()).is_err());
    }
}
