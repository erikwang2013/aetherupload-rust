// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 本地磁盘驱动（默认）—— 对应 PHP 版 `Storage/LocalStorage.php`。
//!
//! 行为与历史版本逐字节相同：`publish` 就是旧的「目标存在则删临时、否则 rename」。

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::storage::{Storage, local_target};

#[derive(Debug, Clone)]
pub struct LocalStorage {
    /// 上传根目录的绝对路径（`base_path/root_dir`）。
    upload_root: PathBuf,
}

impl LocalStorage {
    pub fn new(upload_root: impl Into<PathBuf>) -> Self {
        Self {
            upload_root: upload_root.into(),
        }
    }

    pub fn full_path(&self, group_dir: &str, group_sub_dir: &str, name: &str) -> PathBuf {
        local_target(&self.upload_root, group_dir, group_sub_dir, name)
    }
}

impl Storage for LocalStorage {
    fn publish(
        &self,
        local_path: &Path,
        group_dir: &str,
        group_sub_dir: &str,
        name: &str,
    ) -> Result<()> {
        let dest = self.full_path(group_dir, group_sub_dir, name);

        if dest.exists() {
            // 去重语义（旧 rename() 的行为原样保留）：目标已存在即丢弃本次临时文件
            std::fs::remove_file(local_path).map_err(|_| Error::DeleteResourceFail)?;
            return Ok(());
        }

        std::fs::rename(local_path, &dest).map_err(|_| Error::RenameResourceFail)
    }

    fn exists(&self, group_dir: &str, group_sub_dir: &str, name: &str) -> Result<bool> {
        Ok(self.full_path(group_dir, group_sub_dir, name).exists())
    }

    fn delete(&self, group_dir: &str, group_sub_dir: &str, name: &str) -> Result<()> {
        std::fs::remove_file(self.full_path(group_dir, group_sub_dir, name))
            .map_err(|_| Error::DeleteResourceFail)
    }

    fn url(
        &self,
        _group_dir: &str,
        _group_sub_dir: &str,
        _name: &str,
        _response_params: &[(String, String)],
    ) -> Result<Option<String>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aetherupload-local-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("file/202610")).unwrap();
        dir
    }

    #[test]
    fn publish_moves_and_dedupes() {
        let root = temp_root("publish");
        let storage = LocalStorage::new(&root);

        // 正常落地：临时文件被移走
        let part = root.join("tmp.part");
        std::fs::write(&part, b"payload").unwrap();
        storage.publish(&part, "file", "202610", "abc.jpg").unwrap();
        assert!(!part.exists());
        assert_eq!(
            std::fs::read(storage.full_path("file", "202610", "abc.jpg")).unwrap(),
            b"payload"
        );

        // 同 hash 去重：目标已存在 → 删临时文件，不覆盖已落地的成品
        let part2 = root.join("tmp2.part");
        std::fs::write(&part2, b"other").unwrap();
        storage
            .publish(&part2, "file", "202610", "abc.jpg")
            .unwrap();
        assert!(!part2.exists());
        assert_eq!(
            std::fs::read(storage.full_path("file", "202610", "abc.jpg")).unwrap(),
            b"payload",
            "已有成品不应被覆盖"
        );

        // exists / delete
        assert!(storage.exists("file", "202610", "abc.jpg").unwrap());
        storage.delete("file", "202610", "abc.jpg").unwrap();
        assert!(!storage.exists("file", "202610", "abc.jpg").unwrap());
        // local 删除缺失文件报 delete_resource_fail（与 PHP 一致；s3 才幂等）
        assert!(matches!(
            storage.delete("file", "202610", "abc.jpg"),
            Err(Error::DeleteResourceFail)
        ));

        assert_eq!(storage.url("file", "202610", "abc.jpg", &[]).unwrap(), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn publish_rename_failure_is_reported() {
        let root = temp_root("rename");
        let storage = LocalStorage::new(&root);

        // 临时文件不存在 → rename 失败 → rename_resource_fail
        let missing = root.join("nope.part");
        assert!(matches!(
            storage.publish(&missing, "file", "202610", "x.jpg"),
            Err(Error::RenameResourceFail)
        ));

        let _ = std::fs::remove_dir_all(&root);
    }
}
