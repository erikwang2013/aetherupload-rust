// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! `savedPath` 的三段式编解码 —— 对应 PHP 版 `SavedPathResolver.php`。
//!
//! 形如 `group_groupSubDir_<md5>.<ext>`：客户端拿到什么、原样回传什么，服务端无状态
//! 地拆回三段即可寻址，不需要为「文件在哪」维护一张表。三段各自的合法性在 decode
//! 时逐段校验（客户端可控值，不能直接拼进路径）。

use crate::error::{Error, Result};
use crate::util::is_safe_path_component;

/// 一段在磁盘上的资源：分组 / 分组子目录 / 文件名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedPath {
    pub group: String,
    pub group_sub_dir: String,
    pub resource_name: String,
}

impl SavedPath {
    /// 与 PHP 完全一致的拼接：`group . '_' . groupSubDir . '_' . name`。
    ///
    /// 这里不做校验 —— group 与 groupSubDir 由服务端自己生成（分组名不含下划线是配置
    /// 约束，子目录名由 `generate_sub_dir_name()` 产出），name 是 `<md5>.<ext>`。
    pub fn encode(group: &str, group_sub_dir: &str, name: &str) -> String {
        format!("{group}_{group_sub_dir}_{name}")
    }

    /// 拆解客户端回传的 `savedPath`。
    ///
    /// 与 PHP `explode('_', $savedPath, 3)` 等价：**只拆前两处下划线**，第三段里的
    /// 下划线属于文件名。段数不足或任一段不满足安全字符集 → [`Error::InvalidOperation`]。
    pub fn decode(saved_path: &str) -> Result<Self> {
        let mut parts = saved_path.splitn(3, '_');

        let (Some(group), Some(group_sub_dir), Some(resource_name)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(Error::InvalidOperation);
        };

        // 三段都要过白名单：group 与 group_sub_dir 参与寻址，resource_name 参与拼路径
        for field in [group, group_sub_dir, resource_name] {
            if !is_safe_path_component(field, true, None) {
                return Err(Error::InvalidOperation);
            }
        }

        Ok(Self {
            group: group.to_string(),
            group_sub_dir: group_sub_dir.to_string(),
            resource_name: resource_name.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let saved = SavedPath::encode("file", "202610", "d41d8cd98f00b204e9800998ecf8427e.jpg");
        assert_eq!(saved, "file_202610_d41d8cd98f00b204e9800998ecf8427e.jpg");

        let decoded = SavedPath::decode(&saved).unwrap();
        assert_eq!(decoded.group, "file");
        assert_eq!(decoded.group_sub_dir, "202610");
        assert_eq!(
            decoded.resource_name,
            "d41d8cd98f00b204e9800998ecf8427e.jpg"
        );
    }

    /// 只拆前两处下划线：文件名里带下划线是合法的（三段制寻址的边界）。
    #[test]
    fn extra_underscores_stay_in_the_name() {
        let decoded = SavedPath::decode("file_202610_my_file_name.pdf").unwrap();
        assert_eq!(decoded.resource_name, "my_file_name.pdf");
    }

    #[test]
    fn malformed_paths_are_rejected() {
        // 段数不足
        assert!(matches!(
            SavedPath::decode("file_202610"),
            Err(Error::InvalidOperation)
        ));
        assert!(matches!(
            SavedPath::decode(""),
            Err(Error::InvalidOperation)
        ));
        // 目录穿越：段内出现 `/`、`..` 之外的路径语义字符一律拒
        assert!(matches!(
            SavedPath::decode("file_../../etc/passwd"),
            Err(Error::InvalidOperation)
        ));
        assert!(matches!(
            SavedPath::decode("file_202610_a/b.png"),
            Err(Error::InvalidOperation)
        ));
        assert!(matches!(
            SavedPath::decode("file_202610_.."),
            Err(Error::InvalidOperation)
        ));
        // 空段
        assert!(matches!(
            SavedPath::decode("file__a.png"),
            Err(Error::InvalidOperation)
        ));
    }
}
