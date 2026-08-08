use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let workspace_root = workspace_root();

    if let Some(root) = workspace_root.as_deref() {
        watch_git_path(root, &["rev-parse", "--git-path", "HEAD"]);

        if let Some(reference) = git_output(root, &["symbolic-ref", "-q", "HEAD"]) {
            watch_git_path(root, &["rev-parse", "--git-path", &reference]);
        }
    }

    let hash = workspace_root
        .as_deref()
        .and_then(|root| git_output(root, &["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env=SLIK_GIT_HASH={hash}");
}

fn workspace_root() -> Option<PathBuf> {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")?;
    let slik_dir = Path::new(&manifest_dir);
    if slik_dir.file_name() != Some(std::ffi::OsStr::new("slik")) {
        return None;
    }

    Some(slik_dir.parent()?.parent()?.to_path_buf())
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success())?;

    String::from_utf8(output.stdout)
        .ok()
        .map(|output| output.trim().to_owned())
        .filter(|output| !output.is_empty())
}

fn watch_git_path(root: &Path, args: &[&str]) {
    let Some(path) = git_output(root, args).and_then(|path| canonical_path(root, Path::new(&path)))
    else {
        return;
    };

    println!("cargo:rerun-if-changed={}", path.display());
}

fn canonical_path(root: &Path, path: &Path) -> Option<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    path.canonicalize().ok()
}
