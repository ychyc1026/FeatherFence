use std::path::{Path, PathBuf};

/// 跨卷移动文件：先复制，再删除源文件。
///
/// 删除源文件失败时，不能把操作报告为成功，否则调用方会误以为 MOVE 已完成。
/// 此时尽量删除刚创建的目标副本，把操作回滚到“源文件仍在、目标文件不存在”。
fn copy_then_remove_file(
    src: &Path,
    dest: &Path,
    rename_error: &std::io::Error,
) -> Result<(), String> {
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
    let mut source = std::fs::File::open(src).map_err(|e| e.to_string())?;
    let mut target = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|e| e.to_string())?;
    std::io::copy(&mut source, &mut target).map_err(|e| e.to_string())?;
    target.sync_all().map_err(|e| e.to_string())?;
    if let Ok(metadata) = source.metadata() {
        std::fs::set_permissions(dest, metadata.permissions()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn copy_dir_tree(src: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir(dest).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let source = entry.path();
        let target = dest.join(entry.file_name());
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() {
            copy_dir_tree(&source, &target)?;
        } else {
            copy_file_new(&source, &target)?;
        }
    }
    if let Ok(metadata) = std::fs::metadata(src) {
        std::fs::set_permissions(dest, metadata.permissions()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 复制项目到目标目录，保留源项目，并像 Explorer 一样为重名目标生成新名称。
pub fn copy_to_dir(src: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    if !dest_dir.exists() {
        std::fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;
    }
    let name = src.file_name().ok_or("no file name")?.to_os_string();
    let dest = unique_dest(dest_dir, &name);
    let result = if src.is_dir() {
        copy_dir_tree(src, &dest)
    } else {
        copy_file_new(src, &dest)
    };
    if let Err(error) = result {
        // `dest` was chosen as a unique name and created only by this copy attempt.
        if dest.is_dir() {
            let _ = std::fs::remove_dir_all(&dest);
        } else {
            let _ = std::fs::remove_file(&dest);
        }
        return Err(error);
    }
    Ok(dest)
}

/// 移动项目到目标目录（同卷 rename，跨卷文件 copy+delete），自动避免重名。
pub fn move_to_dir(src: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    if !dest_dir.exists() {
        std::fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;
    }
    let name = src.file_name().ok_or("no file name")?.to_os_string();
    let dest = unique_dest(dest_dir, &name);
    match std::fs::rename(src, &dest) {
        Ok(()) => Ok(dest),
        Err(rename_error) => {
            // std::fs::copy 不支持目录。目录同卷可由 rename 完成；跨卷失败时明确报错，
            // 交给上层提示用户，不做可能产生半成品的递归复制。
            if src.is_dir() {
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
    if !cand.exists() {
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
        if !c.exists() {
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
    use super::{copy_then_remove_file, copy_to_dir};
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::windows::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

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
}
