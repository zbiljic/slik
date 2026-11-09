//! Launcher for self-contained `slik` packages.
//!
//! `GStreamer` reads its plugin, scanner, and registry locations from the
//! environment during initialization. Setting those variables in-process would
//! need `unsafe` code, so this standard-library-only launcher locates the
//! package relative to its own executable, sets the variables, and replaces
//! itself with the real `slik` binary.

use std::env;
use std::ffi::OsString;
use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const BUNDLE_ID: &str = "com.github.zbiljic.slik";

/// Locations inside a self-contained package.
#[derive(Debug, PartialEq, Eq)]
struct Layout {
    executable: PathBuf,
    plugins: PathBuf,
    scanner: PathBuf,
    resources: PathBuf,
    /// Private GIO module directory, so the host's GIO modules (linked against
    /// the host's `GLib`) are never loaded into the bundled `GLib`.
    gio_modules: PathBuf,
}

impl Layout {
    /// macOS application bundle: `Slik.app/Contents/MacOS/slik`.
    #[cfg(target_os = "macos")]
    fn from_launcher_dir(macos_dir: &Path) -> io::Result<Self> {
        let contents_dir = parent(macos_dir, "application Contents directory")?;
        Ok(Self {
            executable: macos_dir.join("slik-real"),
            plugins: contents_dir.join("Frameworks"),
            scanner: contents_dir.join("Helpers/gst-plugin-scanner"),
            resources: contents_dir.join("Resources"),
            gio_modules: contents_dir.join("Resources/gio/modules"),
        })
    }

    /// Prefix-style tree: `<root>/bin/slik` with libraries under `<root>/lib`.
    #[cfg(not(target_os = "macos"))]
    fn from_launcher_dir(bin_dir: &Path) -> io::Result<Self> {
        let root = parent(bin_dir, "package root directory")?;
        Ok(Self {
            executable: root.join("libexec/slik/slik"),
            plugins: root.join("lib/gstreamer-1.0"),
            scanner: root.join("libexec/gstreamer-1.0/gst-plugin-scanner"),
            resources: root.join("share/slik"),
            gio_modules: root.join("lib/gio/modules"),
        })
    }

    fn check(&self) -> io::Result<()> {
        require_file(&self.executable, "application executable")?;
        require_dir(&self.plugins, "GStreamer plugin directory")?;
        require_file(&self.scanner, "GStreamer plugin scanner")?;
        require_dir(&self.resources, "application resource directory")
    }
}

fn main() -> ExitCode {
    match launch() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("failed to launch slik: {err}");
            ExitCode::FAILURE
        }
    }
}

fn launch() -> io::Result<()> {
    // Resolve symlinks: on macOS `current_exe` returns the invoked path, so a
    // symlink such as /usr/local/bin/slik would otherwise hide the bundle.
    let launcher = std::fs::canonicalize(env::current_exe()?)?;
    let launcher_dir = parent(&launcher, "launcher directory")?;
    let layout = Layout::from_launcher_dir(launcher_dir)?;
    layout.check()?;

    let registry = registry_path(
        env::var_os("XDG_CACHE_HOME"),
        env::var_os("HOME"),
        env::temp_dir(),
    );
    if let Some(cache_dir) = registry.parent() {
        std::fs::create_dir_all(cache_dir)?;
    }

    let mut command = Command::new(&layout.executable);
    command
        .args(env::args_os().skip(1))
        .env("GST_PLUGIN_PATH_1_0", "")
        .env("GST_PLUGIN_SYSTEM_PATH_1_0", &layout.plugins)
        .env("GST_PLUGIN_SCANNER_1_0", &layout.scanner)
        .env("GST_REGISTRY_1_0", registry)
        .env("SLIK_RESOURCE_DIR", &layout.resources)
        .env("GIO_MODULE_DIR", &layout.gio_modules);
    Err(command.exec())
}

fn registry_path(
    xdg_cache_home: Option<OsString>,
    home: Option<OsString>,
    temp_dir: PathBuf,
) -> PathBuf {
    let cache_root = if let Some(path) = xdg_cache_home.filter(|path| !path.is_empty()) {
        PathBuf::from(path)
    } else if let Some(path) = home.filter(|path| !path.is_empty()) {
        let home = PathBuf::from(path);
        if cfg!(target_os = "macos") {
            home.join("Library/Caches")
        } else {
            home.join(".cache")
        }
    } else {
        temp_dir
    };

    cache_root.join(BUNDLE_ID).join(format!(
        "gstreamer/registry-{}-{}.bin",
        env!("CARGO_PKG_VERSION"),
        env::consts::ARCH
    ))
}

fn parent<'a>(path: &'a Path, description: &str) -> io::Result<&'a Path> {
    path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cannot determine {description} from {}", path.display()),
        )
    })
}

fn require_file(path: &Path, description: &str) -> io::Result<()> {
    if path.is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{description} not found: {}", path.display()),
        ))
    }
}

fn require_dir(path: &Path, description: &str) -> io::Result<()> {
    if path.is_dir() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{description} not found: {}", path.display()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_reports_the_requested_context() {
        let err = parent(Path::new("/"), "test parent").expect_err("root has no parent");
        assert!(err.to_string().contains("test parent"));
    }

    #[test]
    fn registry_prefers_xdg_cache_home() {
        let path = registry_path(
            Some("/xdg".into()),
            Some("/home/user".into()),
            PathBuf::from("/tmp"),
        );
        assert!(path.starts_with(Path::new("/xdg").join(BUNDLE_ID)));
    }

    #[test]
    fn registry_ignores_empty_environment_values() {
        let path = registry_path(Some(OsString::new()), None, PathBuf::from("/tmp"));
        assert!(path.starts_with(Path::new("/tmp").join(BUNDLE_ID)));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_layout_resolves_bundle_directories() {
        let layout = Layout::from_launcher_dir(Path::new("/Apps/Slik.app/Contents/MacOS"))
            .expect("bundle layout");
        assert_eq!(
            layout.executable,
            Path::new("/Apps/Slik.app/Contents/MacOS/slik-real")
        );
        assert_eq!(
            layout.plugins,
            Path::new("/Apps/Slik.app/Contents/Frameworks")
        );
        assert_eq!(
            layout.resources,
            Path::new("/Apps/Slik.app/Contents/Resources")
        );
        assert_eq!(
            layout.gio_modules,
            Path::new("/Apps/Slik.app/Contents/Resources/gio/modules")
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn prefix_layout_resolves_package_directories() {
        let layout = Layout::from_launcher_dir(Path::new("/opt/slik/bin")).expect("prefix layout");
        assert_eq!(layout.executable, Path::new("/opt/slik/libexec/slik/slik"));
        assert_eq!(layout.plugins, Path::new("/opt/slik/lib/gstreamer-1.0"));
        assert_eq!(
            layout.scanner,
            Path::new("/opt/slik/libexec/gstreamer-1.0/gst-plugin-scanner")
        );
        assert_eq!(layout.resources, Path::new("/opt/slik/share/slik"));
        assert_eq!(layout.gio_modules, Path::new("/opt/slik/lib/gio/modules"));
    }
}
