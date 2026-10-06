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

    pub fn push_text(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.fields
            .push((name.into(), FieldValue::Text(value.into())));
    }

    pub fn push_file(&mut self, name: impl Into<String>, body: ChunkBody) {
        self.fields.push((name.into(), FieldValue::File(body)));
    }

    /// 取文本字段。**重名取最后一个** —— 与 PHP 解析表单的语义一致
    /// （后出现的同名值覆盖先出现的）。
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

    /// 重名取最后一个：PHP 解析表单就是这个语义，别让 Rust 端更「严格」而产生分歧。
    #[test]
    fn duplicate_names_take_the_last() {
        let mut form = FormData::new();
        form.push_text("group", "file");
        form.push_text("group", "other");

        assert_eq!(form.text("group"), Some("other"));
    }
}
