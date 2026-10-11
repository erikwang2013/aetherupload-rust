// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 上传接口的响应体 —— 成功体是 snake_case 的「上传结果」，失败体是 RFC 9457 问题响应。
//!
//! 手写 JSON 而非引 `serde_json`：字段集固定，且本 crate 默认 feature 下要求零依赖。
//! 转义规则：**非 ASCII 原样输出、斜杠不转义**，只有引号、反斜杠与控制字符走转义。
//!
//! 这里只产出**响应体**；状态码与响应头由载体处理：原生宿主用 [`crate::guard::JsonBody`]，
//! 框架适配器用各自的响应对象（见 `integrations/`）。

use crate::controller::upload::{PreprocessOk, SaveChunkOk};
use crate::i18n;

/// 问题响应（`application/problem+json`）—— 2.0 起失败一律用它。
///
/// `code` 是稳定的机器码（状态码 × code 映射表见 README），`title` 是按请求 `locale`
/// 本地化的人类可读消息；`extra` 是扩展成员（目前只有 `chunk_out_of_order` 的
/// `expected_index`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub status: u16,
    pub code: &'static str,
    pub title: String,
    pub extra: Vec<(&'static str, String)>,
}

impl Problem {
    /// 显式构造：`title` 由调用方给。内核代码走 [`Problem::localized`] 一族，
    /// 文案只在查 i18n 表的那一处产生。
    pub fn new(
        status: u16,
        code: &'static str,
        title: impl Into<String>,
        extra: Vec<(&'static str, String)>,
    ) -> Self {
        Self {
            status,
            code,
            title: title.into(),
            extra,
        }
    }

    /// 按 `code` 查 i18n 表构造（`locale` 为 `None` 时用默认语种）——
    /// 本地化集中在这里，适配层与 [`crate::guard::Guard`] 不许把 `title` 换成自己的文案。
    pub(crate) fn localized(
        status: u16,
        code: &'static str,
        locale: Option<&str>,
        extra: Vec<(&'static str, String)>,
    ) -> Self {
        let locale = locale.unwrap_or(i18n::DEFAULT_LOCALE);

        Self::new(status, code, i18n::translate(code, locale), extra)
    }

    /// 400 `invalid_request_body`：请求体无法解析 / 缺 `Content-Type` ——
    /// **适配层**判定「这条请求本身不合法」时用它；内核的参数错误是 `invalid_parameter`。
    pub fn invalid_request_body(locale: Option<&str>) -> Self {
        Self::localized(400, "invalid_request_body", locale, Vec::new())
    }

    /// 404 `resource_not_found`：文件端点（display / download）找不到资源。
    pub fn resource_not_found(locale: Option<&str>) -> Self {
        Self::localized(404, "resource_not_found", locale, Vec::new())
    }

    /// 413 `payload_too_large`：**适配层**处理框架自己的 body 上限时用它 ——
    /// 与内核分块写入超限共用同一个 code，不许把框架 413 降级成 400。
    pub fn payload_too_large(locale: Option<&str>) -> Self {
        Self::localized(413, "payload_too_large", locale, Vec::new())
    }

    /// 400 `invalid_parameter`：内核参数校验失败（适配层自校参数时同用）——
    /// code 与文案都取自同一张 i18n 表，适配层不许自造文案。
    pub fn invalid_parameter(locale: Option<&str>) -> Self {
        Self::localized(400, "invalid_parameter", locale, Vec::new())
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    /// 问题响应的固定 `Content-Type`（不带 charset）。
    pub fn content_type(&self) -> &'static str {
        "application/problem+json"
    }

    /// RFC 9457 形态：`type` / `title` / `status` / `code`，其后跟扩展成员。
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(160);

        out.push('{');
        push_key(&mut out, "type");
        push_string(&mut out, "about:blank");
        out.push(',');
        push_key(&mut out, "title");
        push_string(&mut out, &self.title);
        out.push(',');
        push_key(&mut out, "status");
        out.push_str(&self.status.to_string());
        out.push(',');
        push_key(&mut out, "code");
        push_string(&mut out, self.code);

        for (key, value) in &self.extra {
            out.push(',');
            push_key(&mut out, key);
            push_extra_value(&mut out, value);
        }

        out.push('}');

        out
    }
}

/// JSON 对象键（含引号与冒号）。
pub(crate) fn push_key(out: &mut String, key: &str) {
    push_string(out, key);
    out.push(':');
}

/// JSON 字符串（含引号），转义规则见模块文档。
pub(crate) fn push_string(out: &mut String, value: &str) {
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

/// 扩展成员的值：纯十进制数字按 JSON 数值输出（`expected_index` 要与客户端手里的
/// 序号做数值比较），其余按字符串转义输出。
fn push_extra_value(out: &mut String, value: &str) {
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        out.push_str(value);
    } else {
        push_string(out, value);
    }
}

/// 拼 `preprocess` 的 200 响应体（字段名与前端读取约定一致）。
pub fn upload_ok_to_json(ok: &PreprocessOk) -> String {
    let mut out = String::with_capacity(160);

    out.push('{');
    push_key(&mut out, "chunk_size");
    out.push_str(&ok.chunk_size.to_string());
    out.push(',');
    push_key(&mut out, "resource_temp_base_name");
    push_string(&mut out, &ok.resource_temp_base_name);
    out.push(',');
    push_key(&mut out, "group_sub_dir");
    push_string(&mut out, &ok.group_sub_dir);
    out.push(',');
    push_key(&mut out, "resource_ext");
    push_string(&mut out, &ok.resource_ext);
    out.push(',');
    push_key(&mut out, "saved_path");
    push_string(&mut out, &ok.saved_path);
    out.push('}');

    out
}

/// 拼 `saveChunk` 的 200 响应体。
pub fn save_chunk_ok_to_json(ok: &SaveChunkOk) -> String {
    let mut out = String::with_capacity(40);

    out.push('{');
    push_key(&mut out, "saved_path");
    push_string(&mut out, &ok.saved_path);
    out.push('}');

    out
}

#[cfg(test)]
mod tests {
    use crate::controller::upload::{PreprocessOk, SaveChunkOk};
    use super::*;

    #[test]
    fn problem_body_has_the_frozen_fields() {
        let problem = Problem::new(
            409,
            "chunk_out_of_order",
            "错误：分块序号不连续",
            vec![("expected_index", "3".into())],
        );

        assert_eq!(problem.status(), 409);
        assert_eq!(problem.content_type(), "application/problem+json");
        assert_eq!(
            problem.to_json(),
            r#"{"type":"about:blank","title":"错误：分块序号不连续","status":409,"code":"chunk_out_of_order","expected_index":3}"#
        );
    }

    #[test]
    fn titles_come_from_the_code_and_locale() {
        let zh = Problem::resource_not_found(Some("zh-CN"));
        assert_eq!(
            zh.to_json(),
            r#"{"type":"about:blank","title":"错误：资源不存在","status":404,"code":"resource_not_found"}"#
        );

        // 缺省语种是 en
        assert!(Problem::resource_not_found(None).title.starts_with("Error:"));

        let body = Problem::invalid_request_body(Some("zh"));
        assert_eq!(body.status(), 400);
        assert_eq!(body.code, "invalid_request_body");
        assert_eq!(body.title, "错误：请求体无法解析");

        // 框架层 body 上限用的便利构造，与内核 413 同码
        let body = Problem::payload_too_large(None);
        assert_eq!(body.status(), 413);
        assert_eq!(body.code, "payload_too_large");
        assert!(body.title.starts_with("Error:"));

        // 内核参数校验与适配层自校参数共用（§11.7），文案同源
        let body = Problem::invalid_parameter(Some("zh"));
        assert_eq!(body.status(), 400);
        assert_eq!(body.code, "invalid_parameter");
        assert_eq!(body.title, "错误：请求参数非法");
    }

    /// 成功体是 snake_case 的上传结果，没有 error 字段。
    #[test]
    fn upload_ok_bodies_are_snake_case() {
        let pre = PreprocessOk {
            chunk_size: 1_000_000,
            resource_temp_base_name: "34c0fbcc4b7b1258".into(),
            group_sub_dir: "202610".into(),
            resource_ext: "txt".into(),
            saved_path: String::new(),
        };

        assert_eq!(
            upload_ok_to_json(&pre),
            r#"{"chunk_size":1000000,"resource_temp_base_name":"34c0fbcc4b7b1258","group_sub_dir":"202610","resource_ext":"txt","saved_path":""}"#
        );
        assert_eq!(
            save_chunk_ok_to_json(&SaveChunkOk {
                saved_path: "file_202610_x.txt".into()
            }),
            r#"{"saved_path":"file_202610_x.txt"}"#
        );
    }

    #[test]
    fn escaping_follows_json_rules() {
        // 非 ASCII 不转义、斜杠不转义
        let problem = Problem::new(400, "invalid_parameter", "路径/错误 \"引号\"\\反斜杠\n换行", vec![]);
        let json = problem.to_json();
        assert!(
            json.contains(r#""title":"路径/错误 \"引号\"\\反斜杠\n换行""#),
            "实际: {json}"
        );

        // 控制字符走 \u 转义；扩展成员里的非数字串同样走字符串转义
        let problem = Problem::new(400, "x", "\u{01}", vec![("detail", "a\"b".into())]);
        let json = problem.to_json();
        assert!(json.contains(r#""title":"\u0001""#), "实际: {json}");
        assert!(json.contains(r#""detail":"a\"b""#), "实际: {json}");
    }
}
