// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! bee-rust 适配层端到端测试：用 bee 自己的构建方式（`bee_router::Router` + `build()`）组装应用，
//! 经 tower `oneshot` 驱动（bee_router 自己的 HTTP 测试也是这个打法），跑完
//! 预处理 → 分块 → 落盘 → 展示 / 下载 的完整链路。

use std::path::PathBuf;
use std::sync::Arc;

use aetherupload::{Config, Runtime};
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use tower::ServiceExt;

const BOUNDARY: &str = "aetherupload-bee-rust-boundary";

/// 建临时项目根（目录由 `aetherupload::console::list_groups` 建，等价部署时的 `aetherupload groups`）。
fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("aetherupload-bee-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

/// 建上传根目录、`_header/` 与分组目录；不建的话预处理写 header 就会失败。
fn prepare(runtime: &Arc<Runtime>) {
    let mut lines = Vec::new();
    assert_eq!(
        aetherupload::console::list_groups(runtime, &mut |line| lines.push(line.to_string())),
        0,
        "{lines:?}"
    );
}

/// bee 应用 = bee 自己的路由 + aetherupload 的 axum 路由（`merge`）。
fn app(runtime: Arc<Runtime>) -> axum::Router {
    async fn health() -> &'static str {
        "OK"
    }

    bee_router::Router::new()
        .ns("/api/v1", |ns| ns.get("/health", health))
        .build()
        .merge(aetherupload::integrations::bee_rust::routes(runtime))
}

/// 手搓 multipart 请求体（文本字段，外加可选的 `resource_chunk` 文件字段）。
fn multipart_body(fields: &[(&str, &str)], chunk: Option<&[u8]>) -> Vec<u8> {
    let mut body = Vec::new();

    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }

    if let Some(bytes) = chunk {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"resource_chunk\"; \
                 filename=\"blob\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }

    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// 从内核手写的 JSON 里取 `"key":值`（测试不引 serde_json）。
fn json_field<'a>(body: &'a str, key: &str) -> &'a str {
    let needle = format!("\"{key}\":");
    let start = body.find(&needle).expect("字段存在") + needle.len();
    let rest = &body[start..];

    match rest.strip_prefix('"') {
        Some(stripped) => &stripped[..stripped.find('"').expect("字符串闭合")],
        None => &rest[..rest.find([',', '}']).expect("值结束")],
    }
}

async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, String, HeaderMap) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();

    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        headers,
    )
}

async fn post_multipart(
    app: &axum::Router,
    path: &str,
    fields: &[(&str, &str)],
    chunk: Option<&[u8]>,
) -> String {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(multipart_body(fields, chunk)))
        .unwrap();

    let (status, body, _) = send(app, request).await;
    assert_eq!(status, StatusCode::OK, "响应体: {body}");

    body
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, String, HeaderMap) {
    send(
        app,
        Request::builder().uri(path).body(Body::empty()).unwrap(),
    )
    .await
}

#[tokio::test]
async fn full_chain_through_bee_router() {
    let root = temp_root("chain");
    let runtime = Arc::new(Runtime::new(Config::default(), &root).unwrap());
    prepare(&runtime);
    let app = app(runtime);

    // bee 自己的路由没被上传路由挤掉
    let (status, body, _) = get(&app, "/api/v1/health").await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "OK"));

    let content = b"aetherupload over bee (axum under the hood)";
    let md5 = aetherupload::md5::md5_hex(content);
    let size = content.len().to_string();

    // 1) 预处理
    let preprocess = post_multipart(
        &app,
        "/aetherupload/preprocess",
        &[
            ("resource_name", "a.txt"),
            ("resource_size", &size),
            ("group", "file"),
            ("resource_hash", &md5),
        ],
        None,
    )
    .await;

    assert_eq!(
        json_field(&preprocess, "error"),
        "0",
        "预处理失败: {preprocess}"
    );
    assert_eq!(json_field(&preprocess, "chunkSize"), "1000000");
    assert_eq!(json_field(&preprocess, "resourceExt"), "txt");

    let sub_dir = json_field(&preprocess, "groupSubDir").to_string();
    let temp_base_name = json_field(&preprocess, "resourceTempBaseName").to_string();
    assert!(!sub_dir.is_empty() && !temp_base_name.is_empty());

    // 2) 分块上传（内容不足一个 chunk：单块即末块）
    let chunk = post_multipart(
        &app,
        "/aetherupload/uploading",
        &[
            ("chunk_total", "1"),
            ("chunk_index", "1"),
            ("resource_temp_basename", &temp_base_name),
            ("resource_ext", "txt"),
            ("group_subdir", &sub_dir),
            ("group", "file"),
            ("resource_hash", &md5),
        ],
        Some(content),
    )
    .await;

    assert_eq!(json_field(&chunk, "error"), "0", "分块失败: {chunk}");

    let saved_path = json_field(&chunk, "savedPath").to_string();
    assert!(!saved_path.is_empty(), "末块应当返回 savedPath: {chunk}");

    // 3) 落盘路径：<root>/storage/app/aetherupload/file/<subdir>/<md5>.txt
    let landed = root
        .join("storage/app/aetherupload/file")
        .join(&sub_dir)
        .join(format!("{md5}.txt"));
    assert_eq!(std::fs::read(&landed).unwrap(), content);

    // 4) 展示：内联下发，字节一致，带 nosniff
    let (status, body, headers) = get(&app, &format!("/aetherupload/display/{saved_path}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), content);
    assert_eq!(headers["x-content-type-options"], "nosniff");

    // 5) 下载：改名 + 附件头
    let (status, body, headers) = get(
        &app,
        &format!("/aetherupload/download/{saved_path}/renamed.txt"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), content);

    let disposition = headers["content-disposition"].to_str().unwrap();
    assert!(
        disposition.starts_with("attachment;"),
        "实际: {disposition}"
    );
    assert!(disposition.contains("renamed.txt"), "实际: {disposition}");

    let _ = std::fs::remove_dir_all(&root);
}
