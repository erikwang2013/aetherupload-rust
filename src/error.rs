// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 内核错误 —— 对应 PHP 版 `UploadController::KNOWN_ERROR_MESSAGES` 的白名单语义。
//!
//! PHP 版把异常消息与已知错误键比对，**不在白名单里的一律对客户端报 `upload_error`**
//! （不泄漏内部异常细节）。Rust 版用枚举把「已知错误」变成类型：每个变体就是一条对外的
//! 错误键，而 [`Error::Backend`] / [`Error::Io`] 这类内部故障统一翻译成 `upload_error`
//! —— 与 PHP 的兜底行为一致。
//!
//! 面向客户端的文案由 [`Error::localized`] 按请求语种渲染；`Display` 固定用中文，
//! 供日志与 `?` 传播时阅读。

use std::fmt;

use crate::i18n;

/// 已知错误键（与 PHP 白名单同名）。
///
/// 不派生 `Clone` / `PartialEq`：变体里带着 `std::io::Error` 与后端消息字符串。
/// 需要比较种类时用 [`Error::key`]。
#[derive(Debug)]
pub enum Error {
    InvalidResourceParams,
    InvalidResourceSize,
    InvalidResourceType,
    MissingMimetype,
    InvalidOperation,
    UploadError,
    CreateSubfolderFail,
    CreateResourceFail,
    WriteResourceFail,
    RenameResourceFail,
    DeleteResourceFail,
    CreateHeaderFail,
    WriteHeaderFail,
    ReadHeaderFail,
    DeleteHeaderFail,
    /// 秒传（Redis）后端故障：消息原样透传给调用方日志，对外报 `upload_error`
    Backend(String),
    /// 文件系统错误：对外报 `upload_error`（与 PHP 里未列入白名单的异常同待遇）
    Io(std::io::Error),
}

impl Error {
    /// 对应的翻译键。
    pub fn key(&self) -> &'static str {
        match self {
            Self::InvalidResourceParams => "invalid_resource_params",
            Self::InvalidResourceSize => "invalid_resource_size",
            Self::InvalidResourceType => "invalid_resource_type",
            Self::MissingMimetype => "missing_mimetype",
            Self::InvalidOperation => "invalid_operation",
            Self::CreateSubfolderFail => "create_subfolder_fail",
            Self::CreateResourceFail => "create_resource_fail",
            Self::WriteResourceFail => "write_resource_fail",
            Self::RenameResourceFail => "rename_resource_fail",
            Self::DeleteResourceFail => "delete_resource_fail",
            Self::CreateHeaderFail => "create_header_fail",
            Self::WriteHeaderFail => "write_header_fail",
            Self::ReadHeaderFail => "read_header_fail",
            Self::DeleteHeaderFail => "delete_header_fail",
            // 内部故障：对外只说「上传出错」，细节不进响应体
            Self::UploadError | Self::Backend(_) | Self::Io(_) => "upload_error",
        }
    }

    /// 是否是可以原样告知客户端的「已知错误」。
    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Backend(_) | Self::Io(_) | Self::UploadError)
    }

    /// 按语种渲染给客户端的文案。
    pub fn localized(&self, locale: &str) -> String {
        i18n::translate(self.key(), locale).to_string()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let generic = i18n::translate("upload_error", "zh");

        match self {
            Self::Backend(message) => write!(f, "{generic}（{message}）"),
            Self::Io(err) => write!(f, "{generic}（{err}）"),
            other => f.write_str(i18n::translate(other.key(), "zh")),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_errors_reach_the_client() {
        assert_eq!(Error::InvalidOperation.localized("zh"), "错误：非法操作");
        assert_eq!(
            Error::MissingMimetype.localized("en"),
            "Error: missing mime-type mapping"
        );
        assert!(Error::DeleteHeaderFail.is_known());
    }

    /// 内部故障不泄漏细节：对外一律 upload_error。
    #[test]
    fn internal_errors_collapse_to_upload_error() {
        let backend = Error::Backend("redis: connection refused".into());
        assert!(!backend.is_known());
        assert_eq!(backend.localized("zh"), "错误: 上传发生错误");
        assert_eq!(backend.localized("en"), "Error: error occurs during upload");

        let io = Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"));
        assert_eq!(io.localized("en"), "Error: error occurs during upload");
        // 但日志里要能看到真因
        assert!(format!("{io}").contains("nope"));
    }
}
