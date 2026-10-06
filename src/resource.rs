// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 成品文件对象 —— 对应 PHP 版 `Resource.php`。
//!
//! 也是「上传完成」事件拿到的那个参数：改名落盘之后，它就是最终地址，
//! 宿主可以据此入库、生成缩略图、推送通知。

use std::path::PathBuf;

use crate::error::Result;
use crate::storage::Storage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub name: String,
    pub group: String,
    pub group_dir: String,
    pub group_sub_dir: String,
    /// 绝对路径：`<upload_root>/<group_dir>/<subdir>/<name>`。
    /// s3 驱动下这个路径不存在于本地 —— 判断存在性请用 [`Resource::exists`]。
    pub path: PathBuf,
}

impl Resource {
    pub fn new(
        upload_root: &std::path::Path,
        group: &str,
        group_dir: &str,
        group_sub_dir: &str,
        name: &str,
    ) -> Self {
        Self {
            name: name.to_string(),
            group: group.to_string(),
            group_dir: group_dir.to_string(),
            group_sub_dir: group_sub_dir.to_string(),
            path: upload_root.join(group_dir).join(group_sub_dir).join(name),
        }
    }

    /// 存在性由存储驱动回答（local 看磁盘，s3 发 HEAD）。
    pub fn exists(&self, storage: &dyn Storage) -> Result<bool> {
        storage.exists(&self.group_dir, &self.group_sub_dir, &self.name)
    }

    pub fn delete(&self, storage: &dyn Storage) -> Result<()> {
        storage.delete(&self.group_dir, &self.group_sub_dir, &self.name)
    }

    /// 可直接下发的 URL（s3 预签名；local 为 `None`）。
    pub fn url(
        &self,
        storage: &dyn Storage,
        response_params: &[(String, String)],
    ) -> Result<Option<String>> {
        storage.url(
            &self.group_dir,
            &self.group_sub_dir,
            &self.name,
            response_params,
        )
    }

    /// 资源扩展名（小写）。
    pub fn extension(&self) -> String {
        crate::util::extension_of(&self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::LocalStorage;

    #[test]
    fn exists_and_delete_go_through_storage() {
        let root = std::env::temp_dir().join(format!("aetherupload-res-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("file/202610")).unwrap();
        std::fs::write(root.join("file/202610/abc.jpg"), b"x").unwrap();

        let storage = LocalStorage::new(&root);
        let resource = Resource::new(&root, "file", "file", "202610", "abc.jpg");

        assert!(resource.exists(&storage).unwrap());
        assert_eq!(resource.extension(), "jpg");
        resource.delete(&storage).unwrap();
        assert!(!resource.exists(&storage).unwrap());

        let _ = std::fs::remove_dir_all(&root);
    }
}
