//! Read-only discovery of Git worktrees for saved checkouts.

use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{Result, config::Repository};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub prunable: bool,
    #[serde(default)]
    pub changes: Option<Changes>,
}

/// Uncommitted tracked-line totals across the index and working tree.
/// Untracked files are reported separately; binary and mode changes still mark dirty.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Changes {
    pub dirty: bool,
    pub added: u64,
    pub removed: u64,
    pub untracked: usize,
}

/// Discovery errors belong to one repository, not the whole workspace.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RepositoryWorktrees {
    pub url: String,
    pub entries: Vec<Worktree>,
    pub error: Option<String>,
}

pub fn discover(repositories: &[Repository]) -> Vec<RepositoryWorktrees> {
    // Bound the entire snapshot, rather than multiplying a timeout by repo count.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut remaining = 256 * 1024;
    repositories
        .iter()
        .filter_map(|repo| {
            let path = repo.checkout_path.as_ref()?;
            let mut group = RepositoryWorktrees {
                url: repo.url.clone(),
                ..Default::default()
            };
            match list(path, deadline) {
                Ok(mut entries) => {
                    // Preserve the configured spelling for a symlinked checkout.
                    if let Ok(canonical) = path.canonicalize() {
                        for entry in &mut entries {
                            if entry.path == canonical {
                                entry.path = path.clone();
                            }
                        }
                    }
                    for entry in &mut entries {
                        if !entry.bare && !entry.prunable {
                            entry.changes = changes(&entry.path, deadline).ok();
                        }
                    }
                    let size = serde_json::to_vec(&entries)
                        .map(|bytes| bytes.len())
                        .unwrap_or(usize::MAX);
                    if size <= remaining {
                        remaining -= size;
                        group.entries = entries;
                    } else {
                        group.error =
                            Some("Worktree snapshot exceeds the discovery size limit".into());
                    }
                }
                Err(error) => group.error = Some(error.to_string()),
            }
            Some(group)
        })
        .collect()
}

pub(crate) fn list(path: &Path, deadline: Instant) -> Result<Vec<Worktree>> {
    parse(&git_output(
        path,
        &["worktree", "list", "--porcelain", "-z"],
        deadline,
    )?)
}

/// Run isolated read-only Git with a shared deadline and per-command output limit.
pub(crate) fn git_output(path: &Path, args: &[&str], deadline: Instant) -> Result<Vec<u8>> {
    if Instant::now() >= deadline {
        return Err("Worktree discovery timed out; refresh to retry".into());
    }
    let mut output = tempfile::tempfile()?;
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(Stdio::null());
    struct Process(Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Process(command.spawn()?);
    const MAX_OUTPUT: u64 = 256 * 1024;
    loop {
        if output.metadata()?.len() > MAX_OUTPUT {
            return Err("Worktree list exceeds the discovery size limit".into());
        }
        if let Some(status) = child.0.try_wait()? {
            if !status.success() {
                return Err(format!(
                    "Git worktree discovery failed ({status}); check the saved checkout path"
                )
                .into());
            }
            break;
        }
        if Instant::now() >= deadline {
            return Err("Worktree discovery timed out; refresh to retry".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    output.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    output.take(MAX_OUTPUT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_OUTPUT {
        return Err("Worktree list exceeds the discovery size limit".into());
    }
    Ok(bytes)
}

fn changes(path: &Path, deadline: Instant) -> Result<Changes> {
    let status = git_output(
        path,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
        deadline,
    )?;
    let mut changes = Changes {
        dirty: !status.is_empty(),
        ..Default::default()
    };
    let mut records = status.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.starts_with(b"?? ") {
            changes.untracked += 1;
        }
        // Renames/copies have a second NUL-delimited pathname.
        if record
            .get(..2)
            .is_some_and(|xy| xy.iter().any(|b| matches!(b, b'R' | b'C')))
        {
            records.next();
        }
    }
    for cached in [false, true] {
        let mut args = vec![
            "diff",
            "--numstat",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=none",
        ];
        if cached {
            args.push("--cached");
        }
        args.push("--");
        let output = git_output(path, &args, deadline)?;
        add_numstat(&mut changes, &output)?;
    }
    Ok(changes)
}

fn add_numstat(changes: &mut Changes, bytes: &[u8]) -> Result<()> {
    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let mut fields = record.splitn(3, |byte| *byte == b'\t');
        let added = fields.next().ok_or("Missing added line count")?;
        let removed = fields.next().ok_or("Missing removed line count")?;
        fields.next().ok_or("Missing diff pathname")?;
        if added == b"-" && removed == b"-" {
            continue;
        }
        changes.added += std::str::from_utf8(added)?.parse::<u64>()?;
        changes.removed += std::str::from_utf8(removed)?.parse::<u64>()?;
    }
    Ok(())
}

fn parse(bytes: &[u8]) -> Result<Vec<Worktree>> {
    let text = std::str::from_utf8(bytes).map_err(|_| "Worktree metadata is not UTF-8")?;
    let mut entries = Vec::new();
    let mut current: Option<Worktree> = None;
    for field in text.split('\0') {
        if let Some(path) = field.strip_prefix("worktree ") {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err("Git returned a non-absolute worktree path".into());
            }
            current = Some(Worktree {
                path,
                ..Default::default()
            });
        } else if let Some(entry) = &mut current {
            if let Some(branch) = field.strip_prefix("branch ") {
                entry.branch = Some(branch.strip_prefix("refs/heads/").unwrap_or(branch).into());
            } else if field == "detached" {
                entry.detached = true;
            } else if field == "bare" {
                entry.bare = true;
            } else if field == "prunable" || field.starts_with("prunable ") {
                entry.prunable = true;
            }
        }
    }
    if let Some(entry) = current {
        entries.push(entry);
    }
    if entries.is_empty() {
        return Err("Git returned no worktrees".into());
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_text_but_not_binary_numstat_records() {
        let mut counts = Changes::default();
        add_numstat(
            &mut counts,
            b"12\t3\tfile\twith\nwhitespace\0-\t-\tbinary\0",
        )
        .unwrap();
        assert_eq!((counts.added, counts.removed), (12, 3));
        assert!(add_numstat(&mut counts, b"invalid\0").is_err());
    }

    #[test]
    fn observes_index_working_tree_untracked_and_unborn_changes() {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir.path())
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        };
        let inspect = || changes(dir.path(), Instant::now() + Duration::from_secs(5)).unwrap();
        run(&["init"]);
        assert!(!inspect().dirty);
        std::fs::write(dir.path().join("file"), "one\ntwo\n").unwrap();
        assert_eq!(inspect().untracked, 1);
        run(&["add", "file"]);
        assert_eq!(inspect().added, 2);
        run(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "initial",
        ]);
        assert!(!inspect().dirty);
        std::fs::write(dir.path().join("file"), "one\nthree\n").unwrap();
        run(&["add", "file"]);
        std::fs::write(dir.path().join("file"), "one\nthree\nfour\n").unwrap();
        std::fs::write(dir.path().join("new\nfile"), "untracked\n").unwrap();
        let counts = inspect();
        assert!(counts.dirty);
        assert_eq!((counts.added, counts.removed, counts.untracked), (2, 1, 1));
        assert!(changes(dir.path(), Instant::now()).is_err());
    }

    #[test]
    fn parses_nul_paths_and_worktree_states() {
        let entries = parse(b"worktree /repo\0HEAD abc\0branch refs/heads/main\0\0worktree /a space\nand newline\0HEAD def\0detached\0prunable missing\0\0worktree /bare\0bare\0\0").unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(entries[1].path, Path::new("/a space\nand newline"));
        assert!(entries[1].detached && entries[1].prunable);
        assert!(entries[2].bare);
        assert!(parse(b"worktree relative\0").is_err());
        assert!(parse(b"").is_err());
    }

    #[test]
    fn discovers_external_worktrees_and_reports_missing_checkouts() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let linked = dir.path().join("linked space");
        let run = |args: &[&std::ffi::OsStr]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        };
        run(&["init".as_ref(), repo.as_os_str()]);
        run(&[
            "-C".as_ref(),
            repo.as_os_str(),
            "-c".as_ref(),
            "user.name=Test".as_ref(),
            "-c".as_ref(),
            "user.email=test@example.com".as_ref(),
            "commit".as_ref(),
            "--allow-empty".as_ref(),
            "-m".as_ref(),
            "initial".as_ref(),
        ]);
        run(&[
            "-C".as_ref(),
            repo.as_os_str(),
            "worktree".as_ref(),
            "add".as_ref(),
            "-b".as_ref(),
            "feature".as_ref(),
            linked.as_os_str(),
        ]);
        let entries = list(&repo, Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .any(|entry| entry.branch.as_deref() == Some("feature"))
        );
        assert!(
            list(
                &dir.path().join("missing"),
                Instant::now() + Duration::from_secs(5)
            )
            .is_err()
        );
        assert!(list(&repo, Instant::now()).is_err());
    }
}
