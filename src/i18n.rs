// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 消息目录 —— 对应 PHP 版 `translations/{zh,en}/messages.php`。
//!
//! 文案逐条照搬（含「错误：/ Error: 」前缀），因为前端会把这些字符串原样显示给用户，
//! 而返回给客户端的 `error` 字段就是这里的译文。语种由请求参数 `locale` 决定
//! （PHP 版由适配器把 locale 透传给框架翻译器，未命中回落 `en`）。

/// 默认语种（与 PHP 版 `$locale = Runtime::request()->input('locale', 'en')` 一致）。
pub const DEFAULT_LOCALE: &str = "en";

/// key → (zh, en)。键与 PHP 版翻译文件的键一一对应。
pub const MESSAGES: &[(&str, &str, &str)] = &[
    (
        "upload_error",
        "错误: 上传发生错误",
        "Error: error occurs during upload",
    ),
    (
        "invalid_resource_params",
        "错误：缺少必要的文件参数",
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
        "invalid_chunk_params",
        "错误：缺少必要的文件块参数",
        "Error: chunk parameters are invalid",
    ),
    (
        "invalid_operation",
        "错误：非法操作",
        "Error: operation is forbidden",
    ),
    (
        "http_post_only",
        "错误：文件必须通过HTTP POST上传",
        "Error: upload must through HTTP POST",
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
        "create_header_fail",
        "错误：创建头文件失败",
        "Error: fail to create header",
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

/// 语种归一：`zh` / `zh-CN` / `zh_TW` 都算中文，其余一律回落英文
/// （PHP 版把 locale 交给框架翻译器，查不到就回落到默认语种）。
pub fn normalize_locale(locale: &str) -> &'static str {
    let head = locale
        .trim()
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();

    if head == "zh" { "zh" } else { "en" }
}

/// 查表取译文；未知 key 原样返回（与 PHP 翻译器的「找不到返回 key」行为一致）。
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
    fn wording_matches_the_php_catalog() {
        assert_eq!(translate("upload_error", "zh"), "错误: 上传发生错误");
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

    /// 目录与 PHP 翻译文件逐键对账：16 个键一个都不能少。
    #[test]
    fn catalog_is_complete() {
        let keys: Vec<&str> = MESSAGES.iter().map(|(key, _, _)| *key).collect();
        for expected in [
            "upload_error",
            "invalid_resource_params",
            "invalid_resource_size",
            "invalid_resource_type",
            "create_subfolder_fail",
            "invalid_chunk_params",
            "invalid_operation",
            "http_post_only",
            "create_resource_fail",
            "write_resource_fail",
            "delete_resource_fail",
            "rename_resource_fail",
            "create_header_fail",
            "write_header_fail",
            "read_header_fail",
            "delete_header_fail",
            "missing_mimetype",
        ] {
            assert!(keys.contains(&expected), "缺少消息键 {expected}");
        }
    }
}
