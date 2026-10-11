// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 内核错误与「错误 → 问题响应」的映射 —— 状态码 × code 表的唯一出处。
//!
//! 1.x 把已知异常统一折叠成一个模糊文案；2.0 起每个变体对应一个**稳定的机器码**，
//! 语义唯一的变体直接在 [`Error::outcome`] 里给出 `(状态码, code)`。
//!
//! 一码多义的变体（例如 [`Error::InvalidOperation`] 能表示分组非法、`savedPath` 解码
//! 失败、临时资源缺失、hash 格式非法四种语义）`outcome()` 返回 `None`，**逼调用点按
//! 语义拆开**：要么换成语义唯一的变体，要么直接构造对应的 [`Problem`] —— 不许在映射
//! 层用一个 code 糊过去。真漏了映射也只给 500 `internal_error`，绝不猜成 4xx。
//!
//! 面向客户端的文案由 [`Problem::localized`] 按请求语种渲染；`Display` 固定中文，
//! 供日志与 `?` 传播时阅读。

use std::fmt;

use crate::i18n;
use crate::json::Problem;

/// 内核错误。变体名沿用 1.x 的白名单键，语义唯一的那些与 §4 的 code 一一对应。
#[derive(Debug)]
pub enum Error {
    InvalidResourceParams,
    InvalidResourceSize,
    InvalidResourceType,
    MissingMimetype,
    /// 分组未配置或名字含下划线 —— preprocess 与 display/download 共用同一判定
    InvalidGroup,
    /// 末块整份 md5 与客户端声明不符（`lax_mode = false`）
    HashMismatch,
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
    /// 秒传（Redis）后端故障：消息原样透传给调用方日志，对外报 `internal_error`
    Backend(String),
    /// 文件系统错误：对外报 `internal_error`，细节不进响应体
    Io(std::io::Error),
}

impl Error {
    /// §4 映射：语义唯一的变体给出 `(状态码, 机器码)`；一码多义的变体返回 `None`。
    pub fn outcome(&self) -> Option<(u16, &'static str)> {
        Some(match self {
            Self::InvalidResourceParams => (400, "invalid_parameter"),
            Self::InvalidGroup => (400, "invalid_group"),
            Self::InvalidResourceSize => (413, "payload_too_large"),
            Self::InvalidResourceType | Self::MissingMimetype => (415, "unsupported_media_type"),
            Self::HashMismatch => (422, "hash_mismatch"),
            // 一码多义：调用点必须按语义拆开（分组 / 解码 / 临时资源 / hash 格式各不相同）
            Self::InvalidOperation | Self::ReadHeaderFail => return None,
            // 磁盘 / IO / 索引等内部失败；真因只进日志
            Self::UploadError
            | Self::CreateSubfolderFail
            | Self::CreateResourceFail
            | Self::WriteResourceFail
            | Self::RenameResourceFail
            | Self::DeleteResourceFail
            | Self::CreateHeaderFail
            | Self::WriteHeaderFail
            | Self::DeleteHeaderFail
            | Self::Backend(_)
            | Self::Io(_) => (500, "internal_error"),
        })
    }

    /// 日志文案的翻译键 —— 比机器码更具体（`create_header_fail` vs `internal_error`）。
    pub fn key(&self) -> &'static str {
        match self {
            Self::InvalidResourceParams => "invalid_resource_params",
            Self::InvalidResourceSize => "invalid_resource_size",
            Self::InvalidResourceType => "invalid_resource_type",
            Self::MissingMimetype => "missing_mimetype",
            Self::InvalidGroup => "invalid_group",
            Self::HashMismatch => "hash_mismatch",
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
            // 内部故障：日志里也只说「上传出错」，细节另附
            Self::UploadError | Self::Backend(_) | Self::Io(_) => "upload_error",
        }
    }
}

/// 内部错误 → 问题响应（`locale` 为 `None` 时用默认语种）。
pub fn error_problem(err: &Error, locale: Option<&str>) -> Problem {
    let (status, code) = err.outcome().unwrap_or((500, "internal_error"));

    Problem::localized(status, code, locale, Vec::new())
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
    fn outcome_covers_the_frozen_table() {
        for (err, expected) in [
            (Error::InvalidResourceParams, (400, "invalid_parameter")),
            (Error::InvalidGroup, (400, "invalid_group")),
            (Error::InvalidResourceSize, (413, "payload_too_large")),
            (Error::InvalidResourceType, (415, "unsupported_media_type")),
            (Error::MissingMimetype, (415, "unsupported_media_type")),
            (Error::HashMismatch, (422, "hash_mismatch")),
            (Error::CreateSubfolderFail, (500, "internal_error")),
            (Error::WriteHeaderFail, (500, "internal_error")),
            (Error::UploadError, (500, "internal_error")),
            (Error::Backend("redis down".into()), (500, "internal_error")),
        ] {
            assert_eq!(err.outcome(), Some(expected), "{err:?}");
        }
    }

    /// 一码多义的变体不给映射，逼调用点自己拆（分组/解码/临时资源/hash 格式）。
    #[test]
    fn ambiguous_variants_have_no_outcome() {
        assert_eq!(Error::InvalidOperation.outcome(), None);
        assert_eq!(Error::ReadHeaderFail.outcome(), None);

        // 真漏了映射也只给 500，不猜 4xx，也不把内部细节带进响应体
        let problem = error_problem(&Error::InvalidOperation, Some("zh"));
        assert_eq!((problem.status(), problem.code), (500, "internal_error"));
        assert_eq!(problem.title, "错误：服务器内部错误");
    }

    #[test]
    fn problem_titles_follow_the_request_locale() {
        let zh = error_problem(&Error::HashMismatch, Some("zh-CN"));
        assert_eq!(
            zh.to_json(),
            r#"{"type":"about:blank","title":"错误：文件完整性校验不通过","status":422,"code":"hash_mismatch"}"#
        );

        let en = error_problem(&Error::InvalidGroup, None);
        assert_eq!(en.title, "Error: invalid upload group");
    }

    /// 内部故障不泄漏细节：对外 internal_error，日志里能看到真因。
    #[test]
    fn internal_details_stay_in_the_log() {
        let io = Error::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"));
        assert_eq!(error_problem(&io, None).code, "internal_error");
        assert!(format!("{io}").contains("nope"));
        assert!(format!("{}", Error::Backend("conn refused".into())).contains("conn refused"));
    }
}
