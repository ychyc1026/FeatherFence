use std::fs::Metadata;
use std::path::{Path, PathBuf};

fn source_metadata_without_link(path: &Path) -> Result<Metadata, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "检测到文件系统链接（符号链接、目录联接等），为避免跟随到源目录树之外，已停止：{}",
            path.display()
        ));
    }
    Ok(metadata)
}

fn destination_name_is_available(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => false,
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// 跨卷移动文件：先复制，再删除源文件。
///
/// 删除源文件失败时，不能把操作报告为成功，否则调用方会误以为 MOVE 已完成。
/// 此时尽量删除刚创建的目标副本，把操作回滚到“源文件仍在、目标文件不存在”。
fn copy_then_remove_file(
    src: &Path,
    dest: &Path,
    rename_error: &std::io::Error,
) -> Result<(), String> {
    source_metadata_without_link(src)
        .map_err(|error| format!("重命名失败：{rename_error}；无法执行跨磁盘复制：{error}"))?;
    let mut destination_created = false;
    let copy_result = (|| -> std::io::Result<()> {
        let mut source = std::fs::File::open(src)?;
        // create_new 保证即使目标名称在检查后被其他程序抢先创建，也不会覆盖它。
        let mut target = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)?;
        destination_created = true;
        std::io::copy(&mut source, &mut target)?;
        target.sync_all()?;
        if let Ok(metadata) = source.metadata() {
            std::fs::set_permissions(dest, metadata.permissions())?;
        }
        Ok(())
    })();
    if let Err(copy_error) = copy_result {
        // 只有本次 create_new 确实创建了 dest 才清理，绝不能删除竞态中由别人创建的文件。
        if destination_created {
            let _ = std::fs::remove_file(dest);
        }
        return Err(format!(
            "重命名失败：{rename_error}；复制失败：{copy_error}"
        ));
    }

    if let Err(remove_error) = std::fs::remove_file(src) {
        return match std::fs::remove_file(dest) {
            Ok(()) => Err(format!(
                "重命名失败：{rename_error}；复制完成但无法删除源文件：{remove_error}；已撤销目标副本"
            )),
            Err(rollback_error) => Err(format!(
                "重命名失败：{rename_error}；复制完成但无法删除源文件：{remove_error}；目标副本也无法回滚：{rollback_error}"
            )),
        };
    }

    Ok(())
}

fn copy_file_new(src: &Path, dest: &Path) -> Result<(), String> {
    source_metadata_without_link(src)?;
    let mut source = std::fs::File::open(src).map_err(|e| e.to_string())?;
    let mut target = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|e| e.to_string())?;
    let result = (|| -> std::io::Result<()> {
        std::io::copy(&mut source, &mut target)?;
        target.sync_all()?;
        if let Ok(metadata) = source.metadata() {
            std::fs::set_permissions(dest, metadata.permissions())?;
        }
        Ok(())
    })();
    drop(target);
    if let Err(error) = result {
        return match std::fs::remove_file(dest) {
            Ok(()) => Err(error.to_string()),
            Err(cleanup_error) => Err(format!(
                "{error}；无法清理本次创建的目标文件：{cleanup_error}"
            )),
        };
    }
    Ok(())
}

fn copy_dir_tree(src: &Path, dest: &Path) -> Result<(), String> {
    let source_metadata = source_metadata_without_link(src)?;
    std::fs::create_dir(dest).map_err(|e| e.to_string())?;
    let result = (|| -> Result<(), String> {
        for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let source = entry.path();
            let target = dest.join(entry.file_name());
            let metadata = source_metadata_without_link(&source)?;
            if metadata.is_dir() {
                copy_dir_tree(&source, &target)?;
            } else {
                copy_file_new(&source, &target)?;
            }
        }
        std::fs::set_permissions(dest, source_metadata.permissions()).map_err(|e| e.to_string())?;
        Ok(())
    })();
    if let Err(error) = result {
        return match std::fs::remove_dir_all(dest) {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(format!(
                "{error}；无法清理本次创建的目标目录：{cleanup_error}"
            )),
        };
    }
    Ok(())
}

/// 复制项目到目标目录，保留源项目，并像 Explorer 一样为重名目标生成新名称。
/// 文件系统链接和目录联接不会被跟随复制；遇到后回滚本次创建的目标树并明确报错。
pub fn copy_to_dir(src: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    if !dest_dir.exists() {
        std::fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;
    }
    let name = src.file_name().ok_or("no file name")?.to_os_string();
    let dest = unique_dest(dest_dir, &name);
    let metadata = source_metadata_without_link(src)?;
    let result = if metadata.is_dir() {
        copy_dir_tree(src, &dest)
    } else {
        copy_file_new(src, &dest)
    };
    result?;
    Ok(dest)
}

/// 移动项目到目标目录（同卷 rename，跨卷文件 copy+delete），自动避免重名。
/// 同卷 rename 可原样移动链接；跨卷回退不会把链接转换成其目标内容。
pub fn move_to_dir(src: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    if !dest_dir.exists() {
        std::fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;
    }
    let name = src.file_name().ok_or("no file name")?.to_os_string();
    let dest = unique_dest(dest_dir, &name);
    match std::fs::rename(src, &dest) {
        Ok(()) => Ok(dest),
        Err(rename_error) => {
            let metadata = source_metadata_without_link(src).map_err(|error| {
                format!("重命名失败：{rename_error}；无法执行跨磁盘移动：{error}")
            })?;
            // std::fs::copy 不支持目录。目录同卷可由 rename 完成；跨卷失败时明确报错，
            // 交给上层提示用户，不做可能产生半成品的递归复制。
            if metadata.is_dir() {
                return Err(format!(
                    "无法移动文件夹：{rename_error}；暂不支持跨磁盘移动文件夹"
                ));
            }
            copy_then_remove_file(src, &dest, &rename_error)?;
            Ok(dest)
        }
    }
}

/// 目标已存在则加 "(1)"/"(2)" 后缀
pub fn unique_dest(dir: &Path, name: &std::ffi::OsStr) -> PathBuf {
    let cand = dir.join(name);
    if destination_name_is_available(&cand) {
        return cand;
    }
    let stem = Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let ext = Path::new(name)
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    for i in 1..1000 {
        let c = dir.join(format!("{stem} ({i}){ext}"));
        if destination_name_is_available(&c) {
            return c;
        }
    }
    dir.join(format!(
        "{stem} ({}){ext}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        copy_dir_tree, copy_file_new, copy_then_remove_file, copy_to_dir, move_to_dir, unique_dest,
    };
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt, symlink_dir};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    fn test_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "feather-fences-move-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn create_directory_reparse_point(target: &Path, link: &Path) {
        match symlink_dir(target, link) {
            Ok(()) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                let output = Command::new("cmd")
                    .args(["/d", "/c", "mklink", "/J"])
                    .arg(link)
                    .arg(target)
                    .output()
                    .expect("cmd should create the test junction");
                assert!(
                    output.status.success(),
                    "failed to create test junction: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(error) => panic!("failed to create test symlink: {error}"),
        }
    }

    #[test]
    fn copy_then_remove_reports_a_completed_move() {
        let dir = test_dir();
        let src = dir.join("source.txt");
        let dest = dir.join("dest.txt");
        std::fs::write(&src, b"content").unwrap();
        let rename_error = std::io::Error::other("forced cross-volume fallback");

        copy_then_remove_file(&src, &dest, &rename_error).unwrap();

        assert!(!src.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"content");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn copy_then_remove_rolls_back_when_the_source_cannot_be_deleted() {
        let dir = test_dir();
        let src = dir.join("source.txt");
        let dest = dir.join("dest.txt");
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .open(&src)
            .unwrap();
        file.write_all(b"content").unwrap();
        file.flush().unwrap();
        let rename_error = std::io::Error::other("forced cross-volume fallback");

        let error = copy_then_remove_file(&src, &dest, &rename_error).unwrap_err();

        assert!(error.contains("已撤销目标副本"));
        assert!(src.exists());
        assert!(!dest.exists());
        drop(file);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn copy_then_remove_never_overwrites_a_raced_destination() {
        let dir = test_dir();
        let src = dir.join("source.txt");
        let dest = dir.join("dest.txt");
        std::fs::write(&src, b"source content").unwrap();
        std::fs::write(&dest, b"existing content").unwrap();
        let rename_error = std::io::Error::other("forced cross-volume fallback");

        let error = copy_then_remove_file(&src, &dest, &rename_error).unwrap_err();

        assert!(error.contains("复制失败"));
        assert!(src.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"existing content");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn copy_to_dir_keeps_source_and_copies_nested_folders() {
        let dir = test_dir();
        let source_root = dir.join("source-root");
        let source = source_root.join("folder");
        let destination = dir.join("destination");
        std::fs::create_dir_all(source.join("nested")).unwrap();
        std::fs::write(source.join("nested").join("file.txt"), b"content").unwrap();

        let copied = copy_to_dir(&source, &destination).unwrap();

        assert!(source.join("nested").join("file.txt").exists());
        assert_eq!(
            std::fs::read(copied.join("nested").join("file.txt")).unwrap(),
            b"content"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn copy_file_never_deletes_a_destination_it_did_not_create() {
        let dir = test_dir();
        let src = dir.join("source.txt");
        let dest = dir.join("raced.txt");
        std::fs::write(&src, b"source content").unwrap();
        std::fs::write(&dest, b"other process content").unwrap();

        let error = copy_file_new(&src, &dest).unwrap_err();

        assert!(!error.is_empty());
        assert_eq!(std::fs::read(&dest).unwrap(), b"other process content");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn copy_directory_never_deletes_a_destination_it_did_not_create() {
        let dir = test_dir();
        let src = dir.join("source");
        let dest = dir.join("raced");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("source.txt"), b"source content").unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("other.txt"), b"other process content").unwrap();

        let error = copy_dir_tree(&src, &dest).unwrap_err();

        assert!(!error.is_empty());
        assert_eq!(
            std::fs::read(dest.join("other.txt")).unwrap(),
            b"other process content"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn copy_rejects_a_nested_directory_reparse_point_without_following_it() {
        let dir = test_dir();
        let source = dir.join("source");
        let outside = dir.join("outside");
        let destination = dir.join("destination");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("outside.txt"), b"must not be copied").unwrap();
        let linked = source.join("linked");
        create_directory_reparse_point(&outside, &linked);

        let error = copy_to_dir(&source, &destination).unwrap_err();

        assert!(error.contains("文件系统链接"));
        assert!(error.contains("linked"));
        assert!(!destination.join("source").exists());
        assert_eq!(
            std::fs::read(outside.join("outside.txt")).unwrap(),
            b"must not be copied"
        );
        std::fs::remove_dir(linked).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn same_volume_move_preserves_a_directory_reparse_point() {
        let dir = test_dir();
        let target = dir.join("target");
        let source = dir.join("source-link");
        let destination = dir.join("destination");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("target.txt"), b"target content").unwrap();
        create_directory_reparse_point(&target, &source);

        let moved = move_to_dir(&source, &destination).unwrap();

        assert!(!source.exists());
        assert_ne!(
            std::fs::symlink_metadata(&moved).unwrap().file_attributes()
                & FILE_ATTRIBUTE_REPARSE_POINT.0,
            0
        );
        assert_eq!(
            std::fs::read(target.join("target.txt")).unwrap(),
            b"target content"
        );
        std::fs::remove_dir(moved).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cross_volume_copy_fallback_rejects_a_reparse_point() {
        let dir = test_dir();
        let target = dir.join("target");
        let source = dir.join("source-link");
        let destination = dir.join("copied-link");
        std::fs::create_dir(&target).unwrap();
        create_directory_reparse_point(&target, &source);
        let rename_error = std::io::Error::other("forced cross-volume fallback");

        let error = copy_then_remove_file(&source, &destination, &rename_error).unwrap_err();

        assert!(error.contains("跨磁盘复制"));
        assert!(error.contains("文件系统链接"));
        assert!(std::fs::symlink_metadata(&source).is_ok());
        assert!(std::fs::symlink_metadata(&destination).is_err());
        std::fs::remove_dir(source).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn broken_reparse_point_name_is_treated_as_occupied() {
        let dir = test_dir();
        let target = dir.join("target");
        let link = dir.join("item");
        std::fs::create_dir(&target).unwrap();
        create_directory_reparse_point(&target, &link);
        std::fs::remove_dir(&target).unwrap();

        assert_eq!(
            unique_dest(&dir, std::ffi::OsStr::new("item")),
            dir.join("item (1)")
        );
        std::fs::remove_dir(link).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
