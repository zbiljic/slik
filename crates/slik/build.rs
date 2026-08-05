use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");

    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|hash| hash.trim().to_owned())
        .filter(|hash| !hash.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env=SLIK_GIT_HASH={hash}");
}
