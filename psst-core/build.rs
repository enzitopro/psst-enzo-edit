use std::{env, fs, io::Write, process::Command};
use time::OffsetDateTime;

fn main() {
    let outdir = env::var("OUT_DIR").unwrap();
    let outfile = format!("{outdir}/build-time.txt");

    let mut fh = fs::File::create(outfile).unwrap();
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    write!(fh, r#""{now}""#).ok();

    // Asking git also works in a worktree, where `.git` is a file.  The
    // `git_version!` macro needs git at build time anyway.
    let output = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .expect("git not found");
    let mut remote_url = String::from_utf8(output.stdout)
        .expect("Couldn't extract origin url!")
        .trim()
        .to_string();

    // Check whether origin is accessed via ssh
    if remote_url.contains('@') {
        // If yes, strip the `git@` prefix and split the domain and path
        let mut split = remote_url
            .strip_prefix("git@")
            .unwrap_or(&remote_url)
            .split(':');
        let domain = split
            .next()
            .expect("Couldn't extract domain from ssh-style origin");
        let path = split
            .next()
            .expect("Couldn't expect path from ssh-style origin");

        // And construct the http-style url
        remote_url = format!("https://{domain}/{path}");
    }
    let trimmed_url = remote_url.trim_end_matches(".git");
    remote_url.clone_from(&String::from(trimmed_url));

    let outfile = format!("{outdir}/remote-url.txt");
    let mut file = fs::File::create(outfile).unwrap();
    write!(file, r#""{remote_url}""#).ok();
}
