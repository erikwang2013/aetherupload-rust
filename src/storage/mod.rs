// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 成品文件的存储驱动 —— 对应 PHP 版 `Storage/` 目录。
//!
//! 边界与 PHP 逐字一致：**分块暂存与临时文件不经过这里**（那是本地磁盘的活，
//! `_header/` 与 `*.part` 永远落在上传根目录），驱动只管「成品」的落地与读取。
//!
//! 驱动语义三条（照抄 `StorageInterface`）：
//! - [`Storage::publish`]：目标已存在（同 hash 去重）时删掉本地临时文件、不重复写；
//! - [`Storage::delete`]：local 缺失报错，s3 幂等（缺失也算成功）；
//! - [`Storage::url`]：能直发就给出可直发的 URL（s3 预签名），否则 `None`。

use std::path::{Path, PathBuf};

use crate::error::Result;

pub mod local;
#[cfg(feature = "s3")]
pub mod s3;
#[cfg(feature = "s3")]
pub mod sigv4;

pub use local::LocalStorage;

/// 存储驱动的统一接口。
pub trait Storage: Send + Sync {
    /// 把本地成品文件落到存储。
    fn publish(
        &self,
        local_path: &Path,
        group_dir: &str,
        group_sub_dir: &str,
        name: &str,
    ) -> Result<()>;

    /// 目标是否存在。
    fn exists(&self, group_dir: &str, group_sub_dir: &str, name: &str) -> Result<bool>;

    /// 删除目标。
    fn delete(&self, group_dir: &str, group_sub_dir: &str, name: &str) -> Result<()>;

    /// 可直接下发的 URL（s3 预签名 GET；`response_params` 合并为 `response-content-*`）。
    /// local 恒为 `None` —— 文件由宿主进程或前置服务器直接读盘下发。
    fn url(
        &self,
        group_dir: &str,
        group_sub_dir: &str,
        name: &str,
        response_params: &[(String, String)],
    ) -> Result<Option<String>>;
}

/// 上传根目录下的目标绝对路径（local 驱动用）。
pub(crate) fn local_target(
    upload_root: &Path,
    group_dir: &str,
    group_sub_dir: &str,
    name: &str,
) -> PathBuf {
    upload_root.join(group_dir).join(group_sub_dir).join(name)
}
