//! Bulk filesystem primitives shared by the archivers.
//!
//! Archiving used to be "walk to size it, copy it file by file, then walk it twice more to verify".
//! For a `~/.claude/projects` holding thousands of session files that is four full traversals plus a
//! serial byte-for-byte copy — the reason snapshots crawl once a user has real history.
//!
//! Two things fix it:
//!
//! 1. **Move instead of copy.** Snapshot creation *takes* the data out of the live directory (the
//!    old code copied and then deleted the source), and every archive root lives on the same volume
//!    as the data it archives. `fs::rename` on a directory is a metadata operation, so a multi-GB
//!    `projects/` tree archives in microseconds — and atomically, which is strictly safer than
//!    copy-then-verify-then-delete. Cross-volume renames fail cleanly, so we fall back to copying.
//! 2. **Parallel + single pass when we must copy.** The copy walks the tree once on a dedicated IO
//!    pool and returns the exact bytes/files it wrote, so the size accounting and the completeness
//!    check come out of the copy itself instead of two extra traversals.
//!
//! Everything here treats `preserve` the same way the snapshot specs do: a relative path is skipped
//! if it equals, or lives under, one of the preserved paths.

use crate::error::AppError;
use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// What a move/copy actually transferred. Also doubles as the completeness check: a copy is
/// complete iff what it wrote matches what a stat of the source said was there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Transfer {
    pub bytes: u64,
    pub files: u64,
}

impl Transfer {
    fn file(bytes: u64) -> Self {
        Self { bytes, files: 1 }
    }

    fn merge(self, other: Self) -> Self {
        Self {
            bytes: self.bytes + other.bytes,
            files: self.files + other.files,
        }
    }

    fn sum<I: IntoIterator<Item = Self>>(items: I) -> Self {
        items.into_iter().fold(Self::default(), Self::merge)
    }
}

/// Archiving is IO-bound, not CPU-bound: with many small files the cost is per-file metadata work
/// (and, on Windows, the AV filter driver), which overlaps well beyond the CPU count. The global
/// Rayon pool is deliberately small and reserved for indexing, so bulk file work gets its own pool
/// instead of queueing behind — or starving — session parsing.
fn io_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(4)
            .clamp(4, 8);
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|index| format!("dejavu-io-{index}"))
            .build()
            .expect("build io thread pool")
    })
}

pub fn should_preserve(rel: &Path, preserve: &[PathBuf]) -> bool {
    preserve
        .iter()
        .any(|preserved| rel == preserved || rel.starts_with(preserved))
}

/// True if any preserved path lives strictly *below* `rel`, i.e. the directory must be walked into
/// rather than removed wholesale.
pub fn has_preserved_descendant(rel: &Path, preserve: &[PathBuf]) -> bool {
    preserve
        .iter()
        .any(|preserved| preserved.starts_with(rel) && preserved != rel)
}

fn children(dir: &Path) -> Vec<(PathBuf, std::ffi::OsString)> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| (entry.path(), entry.file_name()))
        .collect()
}

/// Bytes and file count under `path`, skipping preserved paths. Parallel, one traversal.
pub fn dir_stats(path: &Path, preserve: &[PathBuf]) -> Transfer {
    io_pool().install(|| dir_stats_rec(path, preserve, Path::new("")))
}

fn dir_stats_rec(path: &Path, preserve: &[PathBuf], rel: &Path) -> Transfer {
    if should_preserve(rel, preserve) || !path.exists() {
        return Transfer::default();
    }
    if path.is_file() {
        return Transfer::file(fs::metadata(path).map(|meta| meta.len()).unwrap_or(0));
    }
    if !path.is_dir() {
        return Transfer::default();
    }
    Transfer::sum(
        children(path)
            .into_par_iter()
            .map(|(child, name)| dir_stats_rec(&child, preserve, &rel.join(name)))
            .collect::<Vec<_>>(),
    )
}

/// True as soon as one non-preserved file is found. Stops early instead of walking the whole tree.
pub fn has_data(path: &Path, preserve: &[PathBuf]) -> bool {
    has_data_rec(path, preserve, Path::new(""))
}

fn has_data_rec(path: &Path, preserve: &[PathBuf], rel: &Path) -> bool {
    if should_preserve(rel, preserve) || !path.exists() {
        return false;
    }
    if path.is_file() {
        return true;
    }
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| has_data_rec(&entry.path(), preserve, &rel.join(entry.file_name())))
}

/// Copy `src` onto `dst`, skipping preserved paths, returning exactly what was written.
pub fn copy_tree(src: &Path, dst: &Path, preserve: &[PathBuf]) -> Result<Transfer, AppError> {
    io_pool().install(|| copy_rec(src, dst, preserve, Path::new("")))
}

fn copy_rec(
    src: &Path,
    dst: &Path,
    preserve: &[PathBuf],
    rel: &Path,
) -> Result<Transfer, AppError> {
    if should_preserve(rel, preserve) {
        return Ok(Transfer::default());
    }
    if src.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        return Ok(Transfer::file(fs::copy(src, dst)?));
    }
    if !src.is_dir() {
        return Ok(Transfer::default());
    }
    fs::create_dir_all(dst)?;
    let transferred = children(src)
        .into_par_iter()
        .map(|(child, name)| copy_rec(&child, &dst.join(&name), preserve, &rel.join(&name)))
        .collect::<Result<Vec<_>, AppError>>()?;
    Ok(Transfer::sum(transferred))
}

/// Delete a whole tree. `fs::remove_dir_all` unlinks serially, which is the slow half of clearing a
/// large `projects/` directory; fanning the children out cuts it down the same way the copy does.
pub fn remove_tree(path: &Path) -> Result<(), AppError> {
    if !path.exists() {
        return Ok(());
    }
    if !path.is_dir() {
        return Ok(fs::remove_file(path)?);
    }
    io_pool().install(|| remove_dir_rec(path))
}

fn remove_dir_rec(dir: &Path) -> Result<(), AppError> {
    children(dir)
        .into_par_iter()
        .map(|(child, _)| {
            if child.is_dir() {
                remove_dir_rec(&child)
            } else {
                fs::remove_file(&child).map_err(AppError::from)
            }
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    fs::remove_dir(dir)?;
    Ok(())
}

/// Delete everything under `root` except preserved paths, pruning directories that end up empty.
pub fn remove_tree_except(root: &Path, preserve: &[PathBuf]) -> Result<(), AppError> {
    if preserve.is_empty() {
        return remove_tree(root);
    }
    io_pool().install(|| remove_except_rec(root, preserve, Path::new("")))
}

fn remove_except_rec(root: &Path, preserve: &[PathBuf], rel: &Path) -> Result<(), AppError> {
    if !root.exists() {
        return Ok(());
    }
    if !root.is_dir() {
        if !should_preserve(rel, preserve) {
            fs::remove_file(root)?;
        }
        return Ok(());
    }

    children(root)
        .into_par_iter()
        .map(|(child, name)| {
            let child_rel = rel.join(&name);
            if should_preserve(&child_rel, preserve) {
                return Ok(());
            }
            if child.is_dir() && has_preserved_descendant(&child_rel, preserve) {
                remove_except_rec(&child, preserve, &child_rel)?;
                // Only prune the directory if nothing preserved survived inside it.
                if fs::read_dir(&child)
                    .map(|mut entries| entries.next().is_none())
                    .unwrap_or(false)
                {
                    fs::remove_dir(&child)?;
                }
                return Ok(());
            }
            remove_tree(&child)
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    Ok(())
}

/// Move `src` to `dst`, skipping preserved paths (which stay behind in `src`).
///
/// Takes the `fs::rename` fast path whenever the two live on the same volume — the common case,
/// since every archive root sits beside the data it archives. Falls back to copy-then-delete
/// across volumes, verifying the copy against a stat of the source before anything is removed.
///
/// The returned [`Transfer`] describes what ended up at `dst`, i.e. preserved paths are excluded.
pub fn move_tree(src: &Path, dst: &Path, preserve: &[PathBuf]) -> Result<Transfer, AppError> {
    if !src.exists() {
        return Ok(Transfer::default());
    }
    // Sizing has to happen before the move, while the source still exists. One parallel traversal
    // is noise next to the copy it replaces.
    let expected = dir_stats(src, preserve);

    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    if !dst.exists() && try_rename_move(src, dst, preserve)? {
        return Ok(expected);
    }

    let copied = copy_tree(src, dst, preserve)?;
    if copied != expected {
        let _ = remove_tree(dst);
        return Err(AppError::Archive(format!(
            "复制 {} 后校验失败（源 {} 个文件/{} 字节，目标 {} 个文件/{} 字节），已回滚",
            src.to_string_lossy(),
            expected.files,
            expected.bytes,
            copied.files,
            copied.bytes
        )));
    }
    remove_tree_except(src, preserve)?;
    Ok(copied)
}

/// The rename fast path. Returns `Ok(false)` when rename is not usable here (cross-volume, or the
/// OS refused it) so the caller can copy instead; the filesystem is untouched in that case.
fn try_rename_move(src: &Path, dst: &Path, preserve: &[PathBuf]) -> Result<bool, AppError> {
    if preserve.is_empty() {
        return Ok(fs::rename(src, dst).is_ok());
    }

    // Preserved entries (auth tokens, tiny) must survive in the live directory, so stash copies
    // beside the source first. They are copied — never moved — so a failure anywhere below leaves
    // the originals in place and the fallback copy path still sees an intact source.
    let Some(parent) = src.parent() else {
        return Ok(false);
    };
    let stash = parent.join(format!(
        ".dejavu-keep-{}-{}",
        std::process::id(),
        dst.file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "snapshot".to_string())
    ));
    let _ = remove_tree(&stash);
    let mut stashed = Vec::new();
    for rel in preserve {
        let from = src.join(rel);
        if !from.exists() {
            continue;
        }
        let to = stash.join(rel);
        if copy_tree(&from, &to, &[]).is_err() {
            let _ = remove_tree(&stash);
            return Ok(false);
        }
        stashed.push(rel.clone());
    }

    if fs::rename(src, dst).is_err() {
        let _ = remove_tree(&stash);
        return Ok(false);
    }

    // Past this point the move already happened, so failures are real errors rather than a reason
    // to fall back: the stash is the only copy of the preserved files and must be restored.
    fs::create_dir_all(src)?;
    for rel in &stashed {
        let from = stash.join(rel);
        let to = src.join(rel);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        if fs::rename(&from, &to).is_err() {
            copy_tree(&from, &to, &[]).map_err(|err| {
                AppError::Archive(format!(
                    "快照已生成，但保留文件 {} 未能放回原位（暂存于 {}）: {}",
                    rel.to_string_lossy(),
                    stash.to_string_lossy(),
                    err
                ))
            })?;
        }
        // The preserved copy that rode along inside the archive is not part of the snapshot.
        let _ = remove_tree(&dst.join(rel));
    }
    let _ = remove_tree(&stash);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempTree {
        path: PathBuf,
    }

    impl TempTree {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "dejavu-fsops-{}-{}-{}",
                name,
                std::process::id(),
                nonce
            ));
            fs::create_dir_all(&path).expect("temp dir");
            Self { path }
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, body).expect("write");
    }

    #[test]
    fn move_tree_relocates_everything_and_reports_exact_totals() {
        let temp = TempTree::new("move");
        let src = temp.path.join("live");
        let dst = temp.path.join("archive").join("snapshot");
        write(&src.join("a.txt"), "12345");
        write(&src.join("nested").join("b.txt"), "678");

        let moved = move_tree(&src, &dst, &[]).expect("move");

        assert_eq!(moved, Transfer { bytes: 8, files: 2 });
        assert!(!src.exists());
        assert_eq!(
            fs::read_to_string(dst.join("nested").join("b.txt")).expect("read"),
            "678"
        );
    }

    #[test]
    fn move_tree_leaves_preserved_paths_behind_and_out_of_the_archive() {
        let temp = TempTree::new("preserve");
        let src = temp.path.join("live");
        let dst = temp.path.join("archive").join("snapshot");
        write(&src.join("auth.json"), "secret");
        write(&src.join("nested").join("token.json"), "tok");
        write(&src.join("nested").join("state.json"), "state");
        write(&src.join("sessions").join("one.jsonl"), "line");
        let preserve = vec![
            PathBuf::from("auth.json"),
            PathBuf::from("nested/token.json"),
        ];

        let moved = move_tree(&src, &dst, &preserve).expect("move");

        assert_eq!(moved.files, 2);
        assert_eq!(
            fs::read_to_string(src.join("auth.json")).expect("auth stays"),
            "secret"
        );
        assert_eq!(
            fs::read_to_string(src.join("nested").join("token.json")).expect("token stays"),
            "tok"
        );
        assert!(!src.join("sessions").exists());
        assert!(!src.join("nested").join("state.json").exists());
        assert!(!dst.join("auth.json").exists());
        assert!(!dst.join("nested").join("token.json").exists());
        assert!(dst.join("nested").join("state.json").exists());
        assert!(dst.join("sessions").join("one.jsonl").exists());
        // No stash directory may survive a successful move.
        let leftovers: Vec<_> = children(&temp.path)
            .into_iter()
            .filter(|(_, name)| name.to_string_lossy().starts_with(".dejavu-keep-"))
            .collect();
        assert!(leftovers.is_empty(), "stash left behind: {:?}", leftovers);
    }

    #[test]
    fn copy_tree_skips_preserved_and_dir_stats_agrees_with_it() {
        let temp = TempTree::new("copy");
        let src = temp.path.join("live");
        let dst = temp.path.join("copy");
        write(&src.join("keep.json"), "aaaa");
        write(&src.join("data").join("one.txt"), "bb");
        let preserve = vec![PathBuf::from("keep.json")];

        let copied = copy_tree(&src, &dst, &preserve).expect("copy");

        assert_eq!(copied, Transfer { bytes: 2, files: 1 });
        assert_eq!(copied, dir_stats(&src, &preserve));
        assert!(src.join("keep.json").exists());
        assert!(!dst.join("keep.json").exists());
    }

    #[test]
    fn remove_tree_except_keeps_preserved_and_prunes_emptied_dirs() {
        let temp = TempTree::new("clear");
        let root = temp.path.join("live");
        write(&root.join("nested").join("token.json"), "tok");
        write(&root.join("nested").join("state.json"), "state");
        write(&root.join("sessions").join("one.jsonl"), "line");
        let preserve = vec![PathBuf::from("nested/token.json")];

        remove_tree_except(&root, &preserve).expect("clear");

        assert!(root.join("nested").join("token.json").exists());
        assert!(!root.join("nested").join("state.json").exists());
        assert!(!root.join("sessions").exists());
    }

    #[test]
    fn has_data_ignores_trees_that_only_hold_preserved_files() {
        let temp = TempTree::new("hasdata");
        let root = temp.path.join("live");
        write(&root.join("auth.json"), "secret");
        let preserve = vec![PathBuf::from("auth.json")];

        assert!(!has_data(&root, &preserve));
        write(&root.join("config.toml"), "x");
        assert!(has_data(&root, &preserve));
    }
}
