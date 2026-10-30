//! Making an install easy to reach: `home-runner` on the shell's `PATH`, and on macOS the desktop
//! app in `/Applications`.
//!
//! Both point back at the binary that ran `setup` (or `install`, which calls it), so rebuilding or
//! replacing that binary updates the command and the app without running anything again.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::paths::home_dir;

const BLOCK_START: &str = "# >>> home-runner >>>";
const BLOCK_END: &str = "# <<< home-runner <<<";

/// Links the command, makes sure the shell finds it and, on a Mac, installs the app. Every step is
/// attempted; the first failure is returned once the others have run.
pub fn run(exe: &Path) -> Result<()> {
    let mut first_error = None;
    let mut note = |step: &str, result: Result<()>| {
        if let Err(e) = result {
            eprintln!("warning: {step}: {e:#}");
            first_error.get_or_insert(e);
        }
    };

    let bin_dir = home_dir().join(".local/bin");
    let launcher = match link_command(exe, &bin_dir, std::env::var_os("PATH").as_deref()) {
        Ok(link) => {
            println!("{}", link.describe());
            let launcher = link.path().to_path_buf();
            note("shell PATH", ensure_shell_path(&bin_dir));
            launcher
        }
        Err(e) => {
            note("command link", Err(e));
            exe.to_path_buf()
        }
    };
    #[cfg(all(target_os = "macos", feature = "ui"))]
    note("desktop app", app::install(&launcher));
    #[cfg(not(all(target_os = "macos", feature = "ui")))]
    let _ = launcher;

    first_error.map_or(Ok(()), Err)
}

/// Removes what `run` added, except the shell's `PATH` line: it is harmless and other tools in
/// `~/.local/bin` may rely on it by now.
pub fn remove() -> Result<()> {
    let link = home_dir().join(".local/bin").join(COMMAND);
    if std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()) {
        std::fs::remove_file(&link).with_context(|| format!("removing {}", link.display()))?;
        println!("removed {}", link.display());
    }
    #[cfg(all(target_os = "macos", feature = "ui"))]
    app::remove()?;
    Ok(())
}

const COMMAND: &str = "home-runner";

/// How `home-runner` can be reached from the shell.
#[derive(Debug, PartialEq, Eq)]
enum Link {
    /// A directory on `PATH` already resolves `home-runner` to this binary.
    OnPath(PathBuf),
    /// A symlink was created in `bin_dir`. `shadowed_by` is another `home-runner` earlier on
    /// `PATH`, which the shell will run instead.
    Linked {
        path: PathBuf,
        shadowed_by: Option<PathBuf>,
    },
}

impl Link {
    fn path(&self) -> &Path {
        match self {
            Link::OnPath(path) | Link::Linked { path, .. } => path,
        }
    }

    fn describe(&self) -> String {
        match self {
            Link::OnPath(path) => {
                format!("home-runner is already on your PATH ({})", path.display())
            }
            Link::Linked {
                path,
                shadowed_by: None,
            } => {
                format!(
                    "linked {} (home-runner is now on your PATH)",
                    path.display()
                )
            }
            Link::Linked {
                path,
                shadowed_by: Some(other),
            } => format!(
                "linked {}, but {} is earlier on your PATH and runs instead; remove it or put {} first",
                path.display(),
                other.display(),
                path.parent().unwrap_or(Path::new("")).display(),
            ),
        }
    }
}

/// Makes `bin_dir/home-runner` a symlink to `exe`, unless `path_var` already finds this binary.
/// Only a symlink is ever replaced; a real file there is somebody's copy and is left alone.
fn link_command(exe: &Path, bin_dir: &Path, path_var: Option<&std::ffi::OsStr>) -> Result<Link> {
    let exe = exe
        .canonicalize()
        .with_context(|| format!("resolving {}", exe.display()))?;
    let found = find_on_path(path_var);
    if let Some(path) = &found
        && path.canonicalize().is_ok_and(|p| p == exe)
    {
        return Ok(Link::OnPath(path.clone()));
    }

    let target = bin_dir.join(COMMAND);
    std::fs::create_dir_all(bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;
    match std::fs::symlink_metadata(&target) {
        Ok(m) if m.file_type().is_symlink() => std::fs::remove_file(&target)?,
        Ok(_) => bail!(
            "{} exists and isn't a symlink; delete it (or copy the new binary over it) and run `home-runner setup` again",
            target.display()
        ),
        Err(_) => {}
    }
    std::os::unix::fs::symlink(&exe, &target)
        .with_context(|| format!("linking {}", target.display()))?;

    let shadowed_by = path_var.and_then(|var| {
        let dirs: Vec<PathBuf> = std::env::split_paths(var).collect();
        let at = dirs.iter().position(|d| d == bin_dir)?;
        dirs[..at]
            .iter()
            .map(|d| d.join(COMMAND))
            .find(|p| p.is_file())
    });
    Ok(Link::Linked {
        path: target,
        shadowed_by,
    })
}

fn find_on_path(path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path_var?)
        .map(|dir| dir.join(COMMAND))
        .find(|p| p.is_file())
}

/// Adds `bin_dir` to the zsh `PATH` unless it is already there. `~/.zshenv` is read by every zsh,
/// interactive or not, login or not, so scripts and editors that spawn `zsh -c` see it too.
fn ensure_shell_path(bin_dir: &Path) -> Result<()> {
    if std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|d| d == bin_dir) {
        return Ok(());
    }
    let zdotdir = std::env::var_os("ZDOTDIR")
        .map(PathBuf::from)
        .unwrap_or_else(home_dir);
    let zshenv = zdotdir.join(".zshenv");
    if add_path_block(&zshenv, bin_dir)? {
        println!(
            "added {} to your PATH in {}; open a new terminal for it to take effect",
            bin_dir.display(),
            zshenv.display()
        );
    }
    Ok(())
}

/// Appends a marked `export PATH` block to `file`. Returns false when the block is already there.
fn add_path_block(file: &Path, bin_dir: &Path) -> Result<bool> {
    let existing = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
    };
    if existing.contains(BLOCK_START) {
        return Ok(false);
    }
    let dir = match bin_dir.strip_prefix(home_dir()) {
        Ok(rest) => format!("$HOME/{}", rest.display()),
        Err(_) => bin_dir.display().to_string(),
    };
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!(
        "{BLOCK_START}\nexport PATH=\"{dir}:$PATH\"\n{BLOCK_END}\n"
    ));
    std::fs::write(file, text).with_context(|| format!("writing {}", file.display()))?;
    Ok(true)
}

#[cfg(all(target_os = "macos", feature = "ui"))]
mod app {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, bail};

    use crate::paths::home_dir;
    use crate::prereq::probe::sh_quote;

    const BUNDLE: &str = "Home Runner.app";
    const IDENTIFIER: &str = "dev.home-runner.app";
    const EXECUTABLE: &str = "home-runner-app";

    /// Creates `Home Runner.app` in `/Applications`, or `~/Applications` when that isn't writable.
    /// It is a launcher: Finder starts it, and it runs `<command> ui` with the PATH a terminal
    /// would have, since an app started from Finder doesn't get one (`container` and `tart` live
    /// in `/usr/local/bin` and `/opt/homebrew/bin`).
    pub fn install(command: &Path) -> Result<()> {
        let mut last_error = None;
        for dir in [
            PathBuf::from("/Applications"),
            home_dir().join("Applications"),
        ] {
            match write_bundle(&dir, command) {
                Ok(path) => {
                    println!("installed {}", path.display());
                    return Ok(());
                }
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.expect("two directories were tried"))
    }

    pub fn remove() -> Result<()> {
        for dir in [
            PathBuf::from("/Applications"),
            home_dir().join("Applications"),
        ] {
            let bundle = dir.join(BUNDLE);
            if is_ours(&bundle) {
                std::fs::remove_dir_all(&bundle)
                    .with_context(|| format!("removing {}", bundle.display()))?;
                println!("removed {}", bundle.display());
            }
        }
        Ok(())
    }

    fn is_ours(bundle: &Path) -> bool {
        std::fs::read_to_string(bundle.join("Contents/Info.plist"))
            .is_ok_and(|plist| plist.contains(IDENTIFIER))
    }

    fn write_bundle(dir: &Path, command: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let bundle = dir.join(BUNDLE);
        if bundle.exists() {
            if !is_ours(&bundle) {
                bail!(
                    "{} exists and wasn't created by home-runner",
                    bundle.display()
                );
            }
            std::fs::remove_dir_all(&bundle)
                .with_context(|| format!("replacing {}", bundle.display()))?;
        }
        let macos = bundle.join("Contents/MacOS");
        std::fs::create_dir_all(&macos).with_context(|| format!("creating {}", macos.display()))?;
        std::fs::write(bundle.join("Contents/Info.plist"), info_plist())?;
        let launcher = macos.join(EXECUTABLE);
        std::fs::write(&launcher, launcher_script(command))?;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))?;
        Ok(bundle)
    }

    pub(super) fn info_plist() -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Home Runner</string>
  <key>CFBundleDisplayName</key><string>Home Runner</string>
  <key>CFBundleIdentifier</key><string>{IDENTIFIER}</string>
  <key>CFBundleExecutable</key><string>{EXECUTABLE}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
"#,
            version = env!("CARGO_PKG_VERSION"),
        )
    }

    pub(super) fn launcher_script(command: &Path) -> String {
        format!(
            "#!/bin/sh\n\
             # Written by `home-runner setup`. Runs the desktop app of the binary it points at.\n\
             export PATH=\"$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH\"\n\
             exec {} ui \"$@\"\n",
            sh_quote(&command.display().to_string())
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn bundle_is_written_and_replaced_only_when_ours() {
            let dir = tempfile_dir("bundle");
            let path = write_bundle(&dir, Path::new("/opt/it's/home-runner")).unwrap();
            assert_eq!(path, dir.join(BUNDLE));
            let launcher =
                std::fs::read_to_string(path.join("Contents/MacOS").join(EXECUTABLE)).unwrap();
            assert!(launcher.contains("exec '/opt/it'\\''s/home-runner' ui"));
            let mode = std::fs::metadata(path.join("Contents/MacOS").join(EXECUTABLE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);

            // A second run replaces it.
            write_bundle(&dir, Path::new("/new/home-runner")).unwrap();
            let launcher =
                std::fs::read_to_string(path.join("Contents/MacOS").join(EXECUTABLE)).unwrap();
            assert!(launcher.contains("'/new/home-runner'"));

            // Somebody else's app of the same name is never touched.
            std::fs::write(path.join("Contents/Info.plist"), "<plist>other</plist>").unwrap();
            assert!(write_bundle(&dir, Path::new("/new/home-runner")).is_err());
            assert!(path.join("Contents/Info.plist").exists());
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn plist_names_the_app_and_launcher() {
            let plist = info_plist();
            assert!(plist.contains(IDENTIFIER) && plist.contains(EXECUTABLE));
        }

        fn tempfile_dir(tag: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("hr-setup-{tag}-{}", std::process::id()));
            std::fs::remove_dir_all(&dir).ok();
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hr-setup-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn binary(dir: &Path) -> PathBuf {
        let exe = dir.join(COMMAND);
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        exe
    }

    fn path_of(dirs: &[&Path]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    #[test]
    fn links_into_bin_dir_and_relinks_a_stale_link() {
        let root = dir("link");
        let build = root.join("build");
        std::fs::create_dir_all(&build).unwrap();
        let exe = binary(&build);
        let bin = root.join("bin");

        let link = link_command(&exe, &bin, Some(&path_of(&[&bin]))).unwrap();
        assert_eq!(
            link,
            Link::Linked {
                path: bin.join(COMMAND),
                shadowed_by: None
            }
        );
        assert_eq!(std::fs::read_link(bin.join(COMMAND)).unwrap(), exe);

        // The link is on PATH now and resolves to this binary: nothing to do.
        let again = link_command(&exe, &bin, Some(&path_of(&[&bin]))).unwrap();
        assert_eq!(again, Link::OnPath(bin.join(COMMAND)));

        // A new build elsewhere replaces the old link.
        let other = root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let newer = binary(&other);
        link_command(&newer, &bin, Some(&path_of(&[&bin]))).unwrap();
        assert_eq!(std::fs::read_link(bin.join(COMMAND)).unwrap(), newer);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_real_file_is_left_alone() {
        let root = dir("file");
        let build = root.join("build");
        std::fs::create_dir_all(&build).unwrap();
        let exe = binary(&build);
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        binary(&bin);
        assert!(link_command(&exe, &bin, Some(&path_of(&[&bin]))).is_err());
        assert!(
            !std::fs::symlink_metadata(bin.join(COMMAND))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn reports_a_different_binary_earlier_on_path() {
        let root = dir("shadow");
        let build = root.join("build");
        let old = root.join("old");
        for d in [&build, &old] {
            std::fs::create_dir_all(d).unwrap();
        }
        let exe = binary(&build);
        binary(&old);
        let bin = root.join("bin");
        let link = link_command(&exe, &bin, Some(&path_of(&[&old, &bin]))).unwrap();
        assert_eq!(
            link,
            Link::Linked {
                path: bin.join(COMMAND),
                shadowed_by: Some(old.join(COMMAND))
            }
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn path_block_is_added_once_and_keeps_existing_lines() {
        let root = dir("zshenv");
        let file = root.join(".zshenv");
        std::fs::write(&file, ". \"$HOME/.cargo/env\"").unwrap();
        let bin = home_dir().join(".local/bin");

        assert!(add_path_block(&file, &bin).unwrap());
        assert!(!add_path_block(&file, &bin).unwrap());
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            text,
            format!(
                ". \"$HOME/.cargo/env\"\n{BLOCK_START}\nexport PATH=\"$HOME/.local/bin:$PATH\"\n{BLOCK_END}\n"
            )
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
