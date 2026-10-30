use std::{
    env, fs, io,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Component, Path, PathBuf},
    process,
};

use fs_extra::{dir, file};

const COLD: &str = "/mnt/data/cold";

fn error(msg: impl std::fmt::Display) -> ! {
    eprintln!("lnfs: {msg}");
    process::exit(1);
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn ensure_absent(path: &Path) -> io::Result<()> {
    if exists(path) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("already exists: {}", path.display()),
        ));
    }
    Ok(())
}

// Resolve the parent, but never follow the final component.
fn resolve(path: &Path) -> io::Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };

    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid path"))?;

    Ok(fs::canonicalize(path.parent().unwrap())?.join(name))
}

fn relative(path: &Path) -> io::Result<PathBuf> {
    let home =
        fs::canonicalize(env::var_os("HOME").ok_or_else(|| io::Error::other("HOME not set"))?)?;

    let path = resolve(path)?;
    let rel = path
        .strip_prefix(&home)
        .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "path must be inside HOME"))?;

    if rel.as_os_str().is_empty() {
        return Err(io::Error::other("cannot operate on HOME itself"));
    }

    // Avoid unusual path components.
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(io::Error::other("invalid relative path"));
    }

    Ok(rel.to_path_buf())
}

fn cold_path(rel: &Path) -> PathBuf {
    Path::new(COLD).join(rel)
}

fn check_tree(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;

    if meta.is_file() {
        return Ok(());
    }

    if !meta.is_dir() {
        return Err(io::Error::other(format!(
            "unsupported file type: {}",
            path.display()
        )));
    }

    for entry in fs::read_dir(path)? {
        check_tree(&entry?.path())?;
    }

    Ok(())
}

fn copy_entry(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;

    if meta.is_file() {
        let mut options = file::CopyOptions::new();
        options.overwrite = false;

        file::copy(src, dst, &options).map_err(io::Error::other)?;
    } else if meta.is_dir() {
        fs::create_dir(dst)?;

        let mut options = dir::CopyOptions::new();
        options.overwrite = false;
        options.copy_inside = true;
        options.content_only = true;

        dir::copy(src, dst, &options).map_err(io::Error::other)?;
    } else {
        return Err(io::Error::other("unsupported file type"));
    }

    fs::set_permissions(dst, fs::Permissions::from_mode(meta.permissions().mode()))?;

    Ok(())
}

fn remove_entry(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;

    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn staging_path(target: &Path) -> PathBuf {
    let name = target.file_name().unwrap_or_default();

    target.with_file_name(format!(
        ".lnfs-{}-{}",
        name.to_string_lossy(),
        process::id()
    ))
}

// HOME -> cold
fn move_to_cold(input: &Path) -> io::Result<()> {
    let rel = relative(input)?;
    let src = resolve(input)?;
    let target = cold_path(&rel);

    let meta = fs::symlink_metadata(&src)?;

    if meta.file_type().is_symlink() {
        return Err(io::Error::other("source is already a symlink"));
    }

    check_tree(&src)?;
    fs::create_dir_all(target.parent().unwrap())?;
    ensure_absent(&target)?;

    let stage = staging_path(&target);
    ensure_absent(&stage)?;

    println!("Copying: {}", src.display());
    copy_entry(&src, &stage)?;

    // Stage and target are on the same filesystem.
    fs::rename(&stage, &target)?;

    // Original remains intact if removal fails.
    if let Err(e) = remove_entry(&src) {
        return Err(io::Error::other(format!(
            "copy is safe at {}; failed to remove original: {e}",
            target.display()
        )));
    }

    if let Err(e) = symlink(&target, &src) {
        return Err(io::Error::other(format!(
            "data is safe at {}; failed to create symlink: {e}",
            target.display()
        )));
    }

    println!("{} -> {}", src.display(), target.display());
    Ok(())
}

// cold -> HOME
fn restore(input: &Path) -> io::Result<()> {
    let rel = relative(input)?;
    let link = resolve(input)?;
    let target = cold_path(&rel);

    let meta = fs::symlink_metadata(&link)?;

    if !meta.file_type().is_symlink() {
        return Err(io::Error::other("source is not a symlink"));
    }

    let actual = fs::read_link(&link)?;

    if actual != target {
        return Err(io::Error::other(format!(
            "symlink mismatch: expected {}, got {}",
            target.display(),
            actual.display()
        )));
    }

    check_tree(&target)?;

    let stage = staging_path(&link);
    ensure_absent(&stage)?;

    println!("Restoring: {}", target.display());
    copy_entry(&target, &stage)?;

    // Only remove link after local copy is ready.
    fs::remove_file(&link)?;

    if let Err(e) = fs::rename(&stage, &link) {
        let _ = symlink(&target, &link);
        return Err(e);
    }

    // Local copy is safe now.
    if let Err(e) = remove_entry(&target) {
        return Err(io::Error::other(format!(
            "restored successfully, but cold data remains: {e}"
        )));
    }

    println!("{} <- {}", link.display(), target.display());
    Ok(())
}

fn main() {
    let args: Vec<_> = env::args_os().skip(1).collect();

    let (restore_mode, path) = match args.as_slice() {
        [path] => (false, PathBuf::from(path)),
        [flag, path] if flag == "-D" => (true, PathBuf::from(path)),
        _ => error("usage: lnfs [-D] <path>"),
    };

    let result = if restore_mode {
        restore(&path)
    } else {
        move_to_cold(&path)
    };

    if let Err(e) = result {
        error(e);
    }
}
