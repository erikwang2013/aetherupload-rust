// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! Salvo 适配层的端到端测试：预处理 → 分两块上传（续传协议）→ 展示 / 下载。
//!
//! 走 [`salvo::test::TestClient`]，不起真实端口。TestClient 只提供字节体，
//! multipart 表单在这里手拼（含 boundary 与文件字段的 filename）。

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use aetherupload::integrations::salvo::routes;
use aetherupload::md5::md5_hex;
use aetherupload::{Config, Runtime};
use salvo::Router;
use salvo::http::StatusCode;
use salvo::http::header::CONTENT_TYPE;
use salvo::test::{ResponseExt, TestClient};

/// 测试用的真实地址（TestClient 需要一个可解析的 URI）。
const ORIGIN: &str = "http://127.0.0.1";

/// 建临时根目录并把**分组目录**建好 —— 内核建子目录用的是非递归 `mkdir`，
/// 分组目录缺失时第一个分块就会以「建子目录失败」收场。真实部署里这步由
/// `aetherupload groups` 完成，这里直接调同一个函数。
fn setup(tag: &str) -> (PathBuf, Arc<Runtime>) {
    let root =
        std::env::temp_dir().join(format!("aetherupload-salvo-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    let runtime = Arc::new(Runtime::new(Config::default(), &root).unwrap());

    let mut lines = Vec::new();
    assert_eq!(
        aetherupload::console::list_groups(&runtime, &mut |line| lines.push(line.to_string())),
        0,
        "{lines:?}"
    );

    (root, runtime)
}

/// 取内核 JSON 里某个字段的原始值（字符串去掉引号，数字原样）—— 测试不引 JSON 库，
/// 内核输出的这几个字段都是安全字符集，切片足够。
fn json_field(json: &str, key: &str) -> String {
    let needle = format!("\"{key}\":");
    let start = json.find(&needle).expect("字段存在") + needle.len();
    let rest = &json[start..];

    match rest.strip_prefix('"') {
        Some(body) => body[..body.find('"').expect("字符串闭合")].to_string(),
        None => rest[..rest.find([',', '}']).expect("值结束")].to_string(),
    }
}

/// 手拼 multipart/form-data 请求体。
struct Multipart {
    boundary: String,
    body: Vec<u8>,
}

impl Multipart {
    fn new() -> Self {
        Self {
            boundary: format!("----aetherupload-{}", std::process::id()),
            body: Vec::new(),
        }
    }

    fn text(mut self, name: &str, value: &str) -> Self {
        write!(
            self.body,
            "--{0}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n",
            self.boundary
        )
        .unwrap();
        self
    }

    fn file(mut self, name: &str, filename: &str, bytes: &[u8]) -> Self {
        write!(
            self.body,
            "--{0}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n",
            self.boundary
        )
        .unwrap();
        self.body.extend_from_slice(bytes);
        self.body.extend_from_slice(b"\r\n");
        self
    }

    /// 收尾并给出 Content-Type 的值与完整请求体。
    fn build(mut self) -> (String, Vec<u8>) {
        write!(self.body, "--{}--\r\n", self.boundary).unwrap();

        (
            format!("multipart/form-data; boundary={}", self.boundary),
            self.body,
        )
    }
}

async fn post(router: &Arc<Router>, url: &str, form: Multipart) -> salvo::Response {
    let (content_type, body) = form.build();

    TestClient::post(url)
        .add_header(CONTENT_TYPE, content_type, true)
        .body(body)
        .send(router.clone())
        .await
}

async fn get(router: &Arc<Router>, url: &str) -> salvo::Response {
    TestClient::get(url).send(router.clone()).await
}

#[tokio::test]
async fn upload_display_download_roundtrip() {
    let (root, runtime) = setup("roundtrip");
    let router = Arc::new(routes(runtime.clone()));

    let preprocess_path = runtime.config().route_preprocess.clone();
    let uploading_path = runtime.config().route_uploading.clone();

    // 分两块传，覆盖「中间块返回空 savedPath + 末块落盘」的分支
    let content = b"hello aetherupload, chunked world!".to_vec();
    let (first, second) = content.split_at(11);
    let hash = md5_hex(&content);

    // —— 1) 预处理 ——
    let mut response = post(
        &router,
        &format!("{ORIGIN}{preprocess_path}"),
        Multipart::new()
            .text("resource_name", "a.txt")
            .text("resource_size", &content.len().to_string())
            .text("group", "file")
            .text("resource_hash", &hash),
    )
    .await;
    assert_eq!(response.status_code, Some(StatusCode::OK));

    let json = response.take_string().await.unwrap();
    assert_eq!(json_field(&json, "error"), "0", "预处理应当成功：{json}");

    let chunk_size: u64 = json_field(&json, "chunkSize").parse().expect("chunkSize");
    let temp_base_name = json_field(&json, "resourceTempBaseName");
    let group_sub_dir = json_field(&json, "groupSubDir");
    let resource_ext = json_field(&json, "resourceExt");

    assert_eq!(chunk_size, 1_000_000);
    assert!(!temp_base_name.is_empty());
    assert_eq!(group_sub_dir.len(), 6, "默认按月：YYYYMM");
    assert_eq!(resource_ext, "txt");

    // —— 2) 分块上传 ——
    let chunk_body = |index: &str, bytes: &[u8]| {
        Multipart::new()
            .text("chunk_total", "2")
            .text("chunk_index", index)
            .text("resource_temp_basename", &temp_base_name)
            .text("resource_ext", &resource_ext)
            .text("group_subdir", &group_sub_dir)
            .text("group", "file")
            .text("resource_hash", &hash)
            .file("resource_chunk", "chunk", bytes)
    };

    let mut response = post(
        &router,
        &format!("{ORIGIN}{uploading_path}"),
        chunk_body("1", first),
    )
    .await;
    assert_eq!(response.status_code, Some(StatusCode::OK));

    let json = response.take_string().await.unwrap();
    assert_eq!(json_field(&json, "error"), "0", "首块应当成功：{json}");
    assert_eq!(json_field(&json, "savedPath"), "", "非末块不返回 savedPath");

    let mut response = post(
        &router,
        &format!("{ORIGIN}{uploading_path}"),
        chunk_body("2", second),
    )
    .await;
    assert_eq!(response.status_code, Some(StatusCode::OK));

    let json = response.take_string().await.unwrap();
    assert_eq!(json_field(&json, "error"), "0", "末块应当成功：{json}");

    let saved_path = json_field(&json, "savedPath");
    assert!(!saved_path.is_empty(), "末块必须返回 savedPath");
    assert_eq!(saved_path, format!("file_{group_sub_dir}_{hash}.txt"));

    // 成品落在 <root>/storage/app/aetherupload/file/<subdir>/<md5>.txt，内容一字不差
    let landed = root
        .join("storage/app/aetherupload/file")
        .join(&group_sub_dir)
        .join(format!("{hash}.txt"));
    assert_eq!(std::fs::read(&landed).unwrap(), content, "落盘内容应当一致");

    // 断点文件应已清理
    assert_eq!(
        std::fs::read_dir(root.join("storage/app/aetherupload/_header"))
            .unwrap()
            .count(),
        0
    );

    // —— 3) 展示 ——
    let mut response = get(
        &router,
        &format!("{ORIGIN}{}/{saved_path}", runtime.config().route_display),
    )
    .await;
    assert_eq!(response.status_code, Some(StatusCode::OK));
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(response.headers().get(CONTENT_TYPE).unwrap(), "text/plain");

    let served = response.take_bytes(None).await.unwrap();
    assert_eq!(served.as_ref(), content.as_slice());

    // —— 4) 下载（改文件名，扩展名仍是原资源的）——
    let mut response = get(
        &router,
        &format!(
            "{ORIGIN}{}/{saved_path}/report",
            runtime.config().route_download
        ),
    )
    .await;
    assert_eq!(response.status_code, Some(StatusCode::OK));

    let disposition = response
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(disposition.starts_with("attachment;"), "{disposition}");
    assert!(disposition.contains("report.txt"), "{disposition}");

    let served = response.take_bytes(None).await.unwrap();
    assert_eq!(served.as_ref(), content.as_slice());

    // —— 5) 参数缺失 / 资源不存在 ——
    let mut response = post(
        &router,
        &format!("{ORIGIN}{preprocess_path}"),
        Multipart::new().text("group", "file"),
    )
    .await;
    assert_eq!(response.status_code, Some(StatusCode::OK));

    let json = response.take_string().await.unwrap();
    assert_ne!(
        json_field(&json, "error"),
        "0",
        "缺必填字段应当报错：{json}"
    );

    let response = get(
        &router,
        &format!(
            "{ORIGIN}{}/file_202610_missing.txt",
            runtime.config().route_display
        ),
    )
    .await;
    assert_eq!(response.status_code, Some(StatusCode::NOT_FOUND));

    let _ = std::fs::remove_dir_all(&root);
}
