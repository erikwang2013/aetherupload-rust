// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 上传请求的表单载体 —— multipart 解析的产物，也是内核入参的来源。
//!
//! 放在内核（而不是 `integrations/`）里，是因为**原生 Rust 宿主**（[`crate::guard::Guard`]）
//! 也要用它：不依赖任何框架的服务器只需把表单字段塞进来，剩下的翻译只有这一处。
//! 八个框架适配器则把自己的 multipart 提取器喂给 [`FormData`]，字段名与前端一致
//! （`resource_name` / `resource_chunk` / `chunk_index` …）。

use crate::controller::{ChunkBody, PreprocessRequest, SaveChunkRequest};

/// multipart 表单解析后的字段集合。
///
/// 各框架的 multipart API 差别很大（有的给流、有的给内存字节、有的落临时文件），
/// 适配器把它们统一成 `FormData`，再由两个 `to_*_request()` 翻译成内核入参。
#[derive(Debug, Default)]
pub struct FormData {
    fields: Vec<(String, FieldValue)>,
}

#[derive(Debug)]
enum FieldValue {
    Text(String),
    File(ChunkBody),
}

impl FormData {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从任意「键值对」构造（手写服务器解析完 query / urlencoded / multipart 文本字段后直接用）。
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            fields: pairs
                .into_iter()
                .map(|(name, value)| (name.into(), FieldValue::Text(value.into())))
                .collect(),
        }
    }

    /// 从 urlencoded 表单串构造 —— `preprocess` 端点两种编码都收，这是 urlencoded
    /// 那一路（multipart 由适配器经 [`FormData::push_file`] 装进来）。
    ///
    /// 语义遵循 `application/x-www-form-urlencoded` 的通行解析：`+` 解成空格、
    /// `%XX` 解成字节、非法百分号原样保留、没有 `=` 的键当空串、重名同样取最后一个。
    pub fn from_urlencoded(body: &str) -> Self {
        let mut form = Self::new();

        for pair in body.split('&') {
            if pair.is_empty() {
                continue;
            }

            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            form.push_text(percent_decode(name), percent_decode(value));
        }

        form
    }

    pub fn push_text(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.fields
            .push((name.into(), FieldValue::Text(value.into())));
    }

    pub fn push_file(&mut self, name: impl Into<String>, body: ChunkBody) {
        self.fields.push((name.into(), FieldValue::File(body)));
    }

    /// 取文本字段。**重名取最后一个**（后出现的同名值覆盖先出现的）—— 表单解析的
    /// 通行语义，顺带让两端对重名输入不会得出不同答案。
    pub fn text(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .rev()
            .find_map(|(key, value)| match value {
                FieldValue::Text(text) if key == name => Some(text.as_str()),
                _ => None,
            })
    }

    /// 取文件字段（分块本体）。重名同样取最后一个。
    pub fn file(&self, name: &str) -> Option<&ChunkBody> {
        self.fields
            .iter()
            .rev()
            .find_map(|(key, value)| match value {
                FieldValue::File(body) if key == name => Some(body),
                _ => None,
            })
    }

    /// 翻译成 `preprocess` 的入参（字段名与前端一致）。
    pub fn to_preprocess_request(&self) -> PreprocessRequest {
        PreprocessRequest {
            resource_name: self.text("resource_name").map(str::to_string),
            resource_size: self.text("resource_size").map(str::to_string),
            group: self.text("group").map(str::to_string),
            // 宽松模式下前端可能不发这个字段
            resource_hash: self.text("resource_hash").map(str::to_string),
            locale: self.text("locale").map(str::to_string),
        }
    }

    /// 翻译成 `saveChunk` 的入参；分块本体取自表单文件字段 `resource_chunk`。
    pub fn to_save_chunk_request(&self) -> SaveChunkRequest {
        SaveChunkRequest {
            chunk_total: self.text("chunk_total").map(str::to_string),
            chunk_index: self.text("chunk_index").map(str::to_string),
            resource_temp_basename: self.text("resource_temp_basename").map(str::to_string),
            resource_ext: self.text("resource_ext").map(str::to_string),
            group_subdir: self.text("group_subdir").map(str::to_string),
            group: self.text("group").map(str::to_string),
            resource_hash: self.text("resource_hash").map(str::to_string),
            locale: self.text("locale").map(str::to_string),
            chunk: self.file("resource_chunk").cloned(),
        }
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                match (hex_digit(bytes[index + 1]), hex_digit(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        out.push(high * 16 + low);
                        index += 3;
                    }
                    // 非法转义原样留下（解码器不因单个坏转义而整体失败）
                    _ => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_data_translates_frontend_field_names() {
        let mut form = FormData::new();
        form.push_text("resource_name", "photo.jpg");
        form.push_text("resource_size", "2048");
        form.push_text("group", "file");
        form.push_text("resource_hash", "d41d8cd98f00b204e9800998ecf8427e");
        form.push_text("locale", "zh");
        form.push_file("resource_chunk", ChunkBody::Bytes(b"chunk".to_vec()));

        let preprocess = form.to_preprocess_request();
        assert_eq!(preprocess.resource_name.as_deref(), Some("photo.jpg"));
        assert_eq!(preprocess.resource_size.as_deref(), Some("2048"));
        assert_eq!(preprocess.group.as_deref(), Some("file"));
        assert_eq!(preprocess.locale.as_deref(), Some("zh"));

        let save = form.to_save_chunk_request();
        assert!(save.chunk.is_some());
        // 未提供的字段保持 None，由内核按「必填缺失」处理
        assert_eq!(save.chunk_index, None);
        assert_eq!(save.group_subdir, None);
    }

    #[test]
    fn from_pairs_builds_the_same_thing() {
        let form = FormData::from_pairs([
            ("resource_name", "a.txt"),
            ("resource_size", "12"),
            ("group", "file"),
        ]);

        assert_eq!(form.text("resource_name"), Some("a.txt"));
        assert_eq!(form.text("group"), Some("file"));
        assert_eq!(form.text("resource_hash"), None);
    }

    /// 重名取最后一个：表单解析的通行语义，别让两端对同一份输入得出不同答案。
    #[test]
    fn duplicate_names_take_the_last() {
        let mut form = FormData::new();
        form.push_text("group", "file");
        form.push_text("group", "other");

        assert_eq!(form.text("group"), Some("other"));
    }

    /// urlencoded 这一路：`+` 与多字节 `%XX` 的还原。
    #[test]
    fn urlencoded_form_matches_the_frontend_payload() {
        let form = FormData::from_urlencoded(
            "resource_name=%E4%B8%AD%E6%96%87+2.txt&resource_size=2048&group=file\
             &resource_hash=d41d8cd98f00b204e9800998ecf8427e&locale=zh",
        );

        // `+` 是空格，`%XX` 按 UTF-8 逐字节还原
        assert_eq!(form.text("resource_name"), Some("中文 2.txt"));
        assert_eq!(form.text("resource_size"), Some("2048"));
        assert_eq!(form.text("group"), Some("file"));
        assert_eq!(form.text("locale"), Some("zh"));

        let preprocess = form.to_preprocess_request();
        assert_eq!(preprocess.resource_name.as_deref(), Some("中文 2.txt"));
        assert_eq!(preprocess.group.as_deref(), Some("file"));
    }

    /// 宽松模式不发 `resource_hash`；空段、无 `=`、非法转义按表单通行的解码规则走。
    #[test]
    fn urlencoded_edge_cases_follow_form_decoding() {
        let form = FormData::from_urlencoded("a=1&&b&c=%zz&d=%E4%B8%AD");

        assert_eq!(form.text("a"), Some("1"));
        assert_eq!(form.text("b"), Some("")); // 无 `=` → 空串
        assert_eq!(form.text("c"), Some("%zz")); // 非法转义原样保留
        assert_eq!(form.text("d"), Some("中")); // 多字节字符按字节还原（UTF-8 逐字节百分号编码）
        assert_eq!(form.text("resource_hash"), None);

        // 重名同样取最后一个（与 multipart 解析一致）
        assert_eq!(
            FormData::from_urlencoded("group=file&group=other").text("group"),
            Some("other")
        );
        // `%` 收尾不越界
        assert_eq!(FormData::from_urlencoded("e=%").text("e"), Some("%"));
    }
}
