use std::path::PathBuf;

pub struct Config {
    pub repository_url: String,
    pub ssh_key_path: String,
    pub github_username: String,
    pub github_key: String,
    pub repo_tmpdir: PathBuf,
    pub secret: String,
}
