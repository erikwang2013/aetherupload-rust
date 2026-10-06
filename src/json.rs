// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 上传接口的 JSON 响应 —— 对应 PHP 版 `Responser.php` + 前端的读取约定。
//!
//! 手写而非引 `serde_json`：字段集是固定的六个，且本 crate 默认 feature 下要求零依赖。
//! 输出与 PHP 的 `json($result)`（`JSON_UNESCAPED_UNICODE | JSON_UNESCAPED_SLASHES`）
//! 对齐 —— **非 ASCII 原样输出、斜杠不转义**，只有引号、反斜杠与控制字符走转义。
//!
//! 字段名严格照抄 PHP 的 camelCase：前端 `aetherupload-core.js` 直接读
//! `rst.chunkSize` / `rst.resourceTempBaseName` / `rst.savedPath` / `rst.error`。
//!
//! 这里只产出**响应体**。状态码与响应头（`X-Content-Type-Options` 等）由适配器加
//! —— PHP 版里它们挂在 `Runtime::response()` 返回的框架响应对象上，Rust 版则作为
//! 数据交给各框架适配器（见 `integrations/`）。

/// `preprocess` 的响应体。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreprocessResult {
    /// `None` = 成功（JSON 里输出 `0`，与 PHP 的 `'error' => 0` 一致）；
    /// `Some(消息)` = 失败（JSON 里输出该消息字符串）。
    pub error: Option<String>,
    pub chunk_size: u64,
    pub group_sub_dir: String,
    pub resource_temp_base_name: String,
    pub resource_ext: String,
    pub saved_path: String,
}

/// `saveChunk` 的响应体。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SaveChunkResult {
    pub error: Option<String>,
    pub saved_path: String,
}

impl PreprocessResult {
    /// 失败响应：字段保持默认值，只带错误消息（与 PHP `reportError()` 对 `$result` 的
    /// 处理一致 —— 未赋值的项仍是预处理里初始化的 0 / 空串）。
    pub fn fail(error: Option<String>) -> Self {
        Self {
            error,
            ..Self::default()
        }
    }

    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(160);

        out.push('{');
        push_key(&mut out, "error");
        match &self.error {
            Some(message) => push_string(&mut out, message),
            None => out.push('0'),
        }
        out.push(',');
        push_key(&mut out, "chunkSize");
        out.push_str(&self.chunk_size.to_string());
        out.push(',');
        push_key(&mut out, "groupSubDir");
        push_string(&mut out, &self.group_sub_dir);
        out.push(',');
        push_key(&mut out, "resourceTempBaseName");
        push_string(&mut out, &self.resource_temp_base_name);
        out.push(',');
        push_key(&mut out, "resourceExt");
        push_string(&mut out, &self.resource_ext);
        out.push(',');
        push_key(&mut out, "savedPath");
        push_string(&mut out, &self.saved_path);
        out.push('}');

        out
    }
}

impl SaveChunkResult {
    pub fn fail(error: Option<String>) -> Self {
        Self {
            error,
            saved_path: String::new(),
        }
    }

    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(96);

        out.push('{');
        push_key(&mut out, "error");
        match &self.error {
            Some(message) => push_string(&mut out, message),
            None => out.push('0'),
        }
        out.push(',');
        push_key(&mut out, "savedPath");
        push_string(&mut out, &self.saved_path);
        out.push('}');

        out
    }
}

fn push_key(out: &mut String, key: &str) {
    push_string(out, key);
    out.push(':');
}

/// JSON 字符串（含引号），转义规则见模块文档。
fn push_string(out: &mut String, value: &str) {
    out.push('"');

    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }

    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preprocess_failure_matches_php_shape() {
        let result = PreprocessResult::fail(Some("错误：缺少必要的文件参数".into()));
        assert_eq!(
            result.to_json(),
            r#"{"error":"错误：缺少必要的文件参数","chunkSize":0,"groupSubDir":"","resourceTempBaseName":"","resourceExt":"","savedPath":""}"#
        );
    }

    #[test]
    fn preprocess_success_has_zero_error() {
        let result = PreprocessResult {
            error: None,
            chunk_size: 1_000_000,
            group_sub_dir: "202610".into(),
            resource_temp_base_name: "0123456789abcdef".into(),
            resource_ext: "jpg".into(),
            saved_path: String::new(),
        };

        assert_eq!(
            result.to_json(),
            r#"{"error":0,"chunkSize":1000000,"groupSubDir":"202610","resourceTempBaseName":"0123456789abcdef","resourceExt":"jpg","savedPath":""}"#
        );
    }

    /// 前端直接读 savedPath：秒传命中与末块完成都靠它。
    #[test]
    fn save_chunk_success_and_failure() {
        let ok = SaveChunkResult {
            error: None,
            saved_path: "file_202610_d41d8cd9.jpg".into(),
        };
        assert_eq!(
            ok.to_json(),
            r#"{"error":0,"savedPath":"file_202610_d41d8cd9.jpg"}"#
        );

        let failed = SaveChunkResult::fail(Some("Error: error occurs during upload".into()));
        assert_eq!(
            failed.to_json(),
            r#"{"error":"Error: error occurs during upload","savedPath":""}"#
        );
    }

    #[test]
    fn escaping_follows_json_rules() {
        // 非 ASCII 不转义（PHP 用 JSON_UNESCAPED_UNICODE）；斜杠不转义
        let result = SaveChunkResult {
            error: Some("路径/错误 \"引号\"\\反斜杠\n换行".into()),
            saved_path: String::new(),
        };
        let json = result.to_json();
        assert!(
            json.contains(r#""路径/错误 \"引号\"\\反斜杠\n换行""#),
            "实际: {json}"
        );

        // 控制字符走 \u 转义
        let result = SaveChunkResult {
            error: Some("\u{01}".into()),
            saved_path: String::new(),
        };
        assert!(result.to_json().contains(r#""\u0001""#));
    }
}
