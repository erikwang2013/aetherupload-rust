// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 消息目录 —— 问题响应的 `title` 与内部错误的日志文案都在这里查表。
//!
//! 键分两类：
//!
//! - **2.0 的 code**（`invalid_parameter`、`chunk_out_of_order` 等）—— 就是 §4 表里的
//!   机器码，[`crate::json::Problem::localized`] 拿它当 `title` 的查表键，每个 code 在
//!   zh/en 各有一条人类可读消息；
//! - **1.x 遗留的错误键**（`write_header_fail` 等）—— 只给日志与 `Display` 用，
//!   不再进响应体；保留是为了日志仍然能区分「写断点失败」和「读断点失败」。
//!
//! 语种由请求参数 `locale` 决定（缺省 `en`），查不到的实现未命中回落 `en`。

/// 默认语种（请求没带 `locale` 时用它）。
pub const DEFAULT_LOCALE: &str = "en";

/// code / key → (zh, en)。
pub const MESSAGES: &[(&str, &str, &str)] = &[
    // —— 2.0 问题响应的 code ——
    (
        "invalid_parameter",
        "错误：请求参数非法",
        "Error: invalid request parameter",
    ),
    (
        "invalid_request_body",
        "错误：请求体无法解析",
        "Error: request body cannot be parsed",
    ),
    (
        "invalid_group",
        "错误：无效的上传分组",
        "Error: invalid upload group",
    ),
    (
        "upload_not_prepared",
        "错误：上传未预处理或临时资源已失效",
        "Error: upload not prepared or temporary resource is gone",
    ),
    (
        "chunk_out_of_order",
        "错误：分块序号不连续",
        "Error: chunk index out of order",
    ),
    (
        "payload_too_large",
        "错误：上传数据超过大小限制",
        "Error: payload exceeds the size limit",
    ),
    (
        "hash_mismatch",
        "错误：文件完整性校验不通过",
        "Error: file integrity check failed",
    ),
    (
        "unsupported_media_type",
        "错误：不支持的媒体类型",
        "Error: unsupported media type",
    ),
    (
        "internal_error",
        "错误：服务器内部错误",
        "Error: internal server error",
    ),
    (
        "resource_not_found",
        "错误：资源不存在",
        "Error: resource not found",
    ),
    (
        "method_not_allowed",
        "错误：请求方法不被允许",
        "Error: method not allowed",
    ),
    // —— 1.x 遗留键：只进日志 ——
    (
        "upload_error",
        "错误：上传发生错误",
        "Error: error occurs during upload",
    ),
    (
        "invalid_resource_params",
        "错误：无效的文件参数",
        "Error: invalid resource parameters",
    ),
    (
        "invalid_resource_size",
        "错误：无效的文件大小",
        "Error: invalid resource size",
    ),
    (
        "invalid_resource_type",
        "错误：无效的文件类型",
        "Error: invalid resource type",
    ),
    (
        "create_subfolder_fail",
        "错误：创建子文件夹失败",
        "Error: fail to create subfolder",
    ),
    (
        "invalid_operation",
        "错误：非法操作",
        "Error: operation is forbidden",
    ),
    (
        "create_resource_fail",
        "错误：创建文件失败",
        "Error: fail to create resource",
    ),
    (
        "write_resource_fail",
        "错误：写文件失败",
        "Error: fail to write resource",
    ),
    (
        "delete_resource_fail",
        "错误：删除文件失败",
        "Error: fail to delete resource",
    ),
    (
        "rename_resource_fail",
        "错误：重命名文件失败",
        "Error: fail to rename resource",
    ),
    (
        "write_header_fail",
        "错误：写头文件失败",
        "Error: fail to write head resource",
    ),
    (
        "read_header_fail",
        "错误：读头文件失败",
        "Error: fail to read head resource",
    ),
    (
        "delete_header_fail",
        "错误：删除头文件失败",
        "Error: fail to delete head resource",
    ),
    (
        "missing_mimetype",
        "错误：缺少此类文件的mime-type映射",
        "Error: missing mime-type mapping",
    ),
];

/// 语种归一：`zh` / `zh-CN` / `zh_TW` 都算中文，其余一律回落英文。
pub fn normalize_locale(locale: &str) -> &'static str {
    let head = locale
        .trim()
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();

    if head == "zh" { "zh" } else { "en" }
}

/// 查表取译文；未知 key 原样返回（借用入参的生命周期，未知键也能原样传出）。
pub fn translate<'a>(key: &'a str, locale: &str) -> &'a str {
    let zh = normalize_locale(locale) == "zh";

    for (k, zh_text, en_text) in MESSAGES {
        if *k == key {
            return if zh { zh_text } else { en_text };
        }
    }

    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wording_is_frozen() {
        assert_eq!(translate("upload_error", "zh"), "错误：上传发生错误");
        assert_eq!(
            translate("invalid_resource_params", "zh"),
            "错误：无效的文件参数"
        );
        assert_eq!(
            translate("upload_error", "en"),
            "Error: error occurs during upload"
        );
        assert_eq!(translate("invalid_operation", "zh-CN"), "错误：非法操作");
        assert_eq!(
            translate("missing_mimetype", "en-US"),
            "Error: missing mime-type mapping"
        );
    }

    /// §4 表的每个 code 都有两条人类可读消息（问题响应的 title 就取它）。
    #[test]
    fn every_problem_code_has_a_message() {
        for code in [
            "invalid_parameter",
            "invalid_request_body",
            "invalid_group",
            "upload_not_prepared",
            "chunk_out_of_order",
            "payload_too_large",
            "hash_mismatch",
            "unsupported_media_type",
            "internal_error",
            "resource_not_found",
            "method_not_allowed",
        ] {
            let zh = translate(code, "zh");
            let en = translate(code, "en");
            assert_ne!(zh, code, "zh 缺少 {code}");
            assert_ne!(en, code, "en 缺少 {code}");
            assert!(zh.starts_with("错误："), "{code} 的 zh 消息格式不对");
            assert!(en.starts_with("Error: "), "{code} 的 en 消息格式不对");
        }
    }

    #[test]
    fn unknown_locale_falls_back_to_english() {
        assert_eq!(
            translate("invalid_resource_size", "fr"),
            "Error: invalid resource size"
        );
        assert_eq!(normalize_locale("zh-Hans"), "zh");
        assert_eq!(normalize_locale("ZH_TW"), "zh");
    }

    #[test]
    fn unknown_key_returns_itself() {
        assert_eq!(translate("no_such_key", "zh"), "no_such_key");
    }

    /// 日志键完整性：`Error::key()` 能返回的每个键都必须有译文 —— 查不到 translate
    /// 会原样吐键名，日志里就成了裸键。
    #[test]
    fn catalog_covers_every_error_key() {
        use crate::error::Error;

        for variant in [
            Error::InvalidResourceParams,
            Error::InvalidResourceSize,
            Error::InvalidResourceType,
            Error::MissingMimetype,
            Error::InvalidGroup,
            Error::HashMismatch,
            Error::InvalidOperation,
            Error::UploadError,
            Error::CreateSubfolderFail,
            Error::CreateResourceFail,
            Error::WriteResourceFail,
            Error::RenameResourceFail,
            Error::DeleteResourceFail,
            Error::WriteHeaderFail,
            Error::ReadHeaderFail,
            Error::DeleteHeaderFail,
            Error::Backend("x".into()),
            Error::Io(std::io::Error::other("x")),
        ] {
            let key = variant.key();
            assert_ne!(translate(key, "zh"), key, "zh 缺少日志键 {key}");
            assert_ne!(translate(key, "en"), key, "en 缺少日志键 {key}");
        }
    }
}
