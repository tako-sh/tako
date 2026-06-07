use super::*;

#[test]
fn extract_repo_slug_ssh_url() {
    assert_eq!(
        extract_repo_slug("git@github.com:user/repo.git"),
        "user/repo"
    );
    assert_eq!(
        extract_repo_slug("git@gitlab.com:org/project"),
        "org/project"
    );
}

#[test]
fn extract_repo_slug_https_url() {
    assert_eq!(
        extract_repo_slug("https://github.com/user/repo.git"),
        "user/repo"
    );
    assert_eq!(
        extract_repo_slug("https://github.com/user/repo"),
        "user/repo"
    );
    assert_eq!(
        extract_repo_slug("https://github.com/user/repo/"),
        "user/repo"
    );
}
