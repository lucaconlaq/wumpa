//! GitHub shorthand for the dashboard; the server still receives SSH URLs.

use crate::Result;

/// Convert owner/repo (optionally ending in .git) into a GitHub SSH URL.
pub(super) fn url(value: &str) -> Result<String> {
    let invalid = "Enter a GitHub repository as owner/repo (not a URL).";
    if value.chars().any(char::is_control) {
        return Err(invalid.into());
    }
    let (owner, repository) = value.trim().split_once('/').ok_or(invalid)?;
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    if owner.is_empty()
        || owner.len() > 39
        || owner.starts_with('-')
        || owner.ends_with('-')
        || owner.contains("--")
        || !owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || repository.is_empty()
        || repository.len() > 100
        || !repository
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(invalid.into());
    }
    let url = format!("ssh://git@github.com/{owner}/{repository}.git");
    // Validate the un-suffixed path too: appending .git must not hide traversal.
    if matches!(repository, "." | "..") {
        return Err(invalid.into());
    }
    crate::repository::folder_name(&url, Some("checkout")).map_err(|_| invalid)?;
    Ok(url)
}

/// Recover shorthand only from supported saved GitHub URLs, never another host.
pub(super) fn name(saved_url: &str) -> Result<String> {
    let value = saved_url
        .trim()
        .strip_prefix("ssh://git@github.com/")
        .ok_or(
            "The TUI clones GitHub repositories only; use the plain client for other SSH URLs.",
        )?;
    let canonical = url(value)?;
    Ok(canonical
        .trim_start_matches("ssh://git@github.com/")
        .strip_suffix(".git")
        .ok_or("missing GitHub repository name")?
        .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_github_shorthand_and_round_trips_saved_urls() {
        for value in ["owner/repo", " owner/repo ", "owner/repo.git"] {
            assert_eq!(url(value).unwrap(), "ssh://git@github.com/owner/repo.git");
        }
        assert!(url("some-org/repo_name.js").is_ok());
        assert!(url("owner/.github").is_ok());
        for saved in [
            "ssh://git@github.com/owner/repo",
            "ssh://git@github.com/owner/repo.git",
        ] {
            assert_eq!(name(saved).unwrap(), "owner/repo");
        }
    }

    #[test]
    fn rejects_urls_foreign_hosts_and_invalid_shorthand() {
        for value in [
            "",
            "repo",
            "owner/",
            "/repo",
            "owner/repo/extra",
            "owner/../repo",
            "owner/..",
            "owner/.",
            "owner/-option",
            "-owner/repo",
            "owner-/repo",
            "own_er/repo",
            "own--er/repo",
            "owner/repo?token=value",
            "owner/repo#ref",
            "owner/repo%2fname",
            "owner/re po",
            "owner/repo\n",
            "owner/$(command)",
            "owner/café",
            "https://github.com/owner/repo",
            "ssh://git@github.com/owner/repo.git",
            "git@github.com:owner/repo.git",
        ] {
            assert!(url(value).is_err(), "accepted {value:?}");
        }
        for saved in [
            "ssh://git@elsewhere/owner/repo.git",
            "ssh://git@github.com.evil/owner/repo.git",
            "https://github.com/owner/repo",
            "ssh://git@github.com/owner/repo/extra",
        ] {
            assert!(name(saved).is_err(), "accepted {saved:?}");
        }
        assert!(url(&format!("{}/repo", "o".repeat(40))).is_err());
        assert!(url(&format!("owner/{}", "r".repeat(101))).is_err());
    }
}
