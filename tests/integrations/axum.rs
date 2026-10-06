// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! Axum 端到端：真起 `Router`，走完整链路（预处理 → 分块 → 末块 → 展示 → 下载）。
//!
//! 断言的都是协议级事实：JSON 字段名与 PHP 版一致、分块按序追加、成品落在
//! `<root>/storage/app/aetherupload/file/<子目录>/<md5>.<ext>`、下载带头。

use std::sync::Arc;

use aetherupload::md5::md5_hex;
use aetherupload::{Config, Runtime};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

const BOUNDARY: &str = "aetherupload-test-boundary";

fn app(tag: &str) -> (std::path::PathBuf, Router) {
    let base = std::env::temp_dir().join(format!("aetherupload-axum-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);

    let runtime = Arc::new(Runtime::new(Config::default(), &base).expect("运行时装配"));

    // 先建目录（真实部署里是 `aetherupload groups`）
    let mut lines = Vec::new();
    assert_eq!(
        aetherupload::console::list_groups(&runtime, &mut |line| lines.push(line.to_string())),
        0,
        "{lines:?}"
    );

    let router = aetherupload::integrations::axum::routes(runtime);

    (base, router)
}

/// 拼一个 multipart 表单体（文本字段 + 可选的二进制文件字段）。
fn multipart(fields: &[(&str, &str)], file: Option<(&str, &[u8])>) -> Vec<u8> {
    let mut body = Vec::new();

    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }

    if let Some((name, bytes)) = file {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"blob\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }

    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

    body
}

fn post(uri: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap()
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 从响应体里取 `"key":0` 或 `"key":"值"`（不引 JSON 解析器，字段集是固定的）。
fn json_value<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":");
    let rest = &body[body.find(&needle)? + needle.len()..];

    if let Some(quoted) = rest.strip_prefix('"') {
        return Some(&quoted[..quoted.find('"')?]);
    }

    rest.split([',', '}']).next()
}

#[tokio::test]
async fn full_upload_flow_through_axum() {
    let (base, router) = app("flow");
    let content = b"axum end to end payload".repeat(37);
    let hash = md5_hex(&content);

    // 1) 预处理
    let response = router
        .clone()
        .oneshot(post(
            "/aetherupload/preprocess",
            multipart(
                &[
                    ("resource_name", "报告.txt"),
                    ("resource_size", &content.len().to_string()),
                    ("group", "file"),
                    ("resource_hash", &hash),
                    ("locale", "zh"),
                ],
                None,
            ),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert_eq!(json_value(&body, "error"), Some("0"), "预处理失败：{body}");

    let temp_base = json_value(&body, "resourceTempBaseName")
        .unwrap()
        .to_string();
    let sub_dir = json_value(&body, "groupSubDir").unwrap().to_string();
    let ext = json_value(&body, "resourceExt").unwrap().to_string();
    assert_eq!(ext, "txt");
    assert_eq!(
        json_value(&body, "savedPath"),
        Some(""),
        "首次上传不该有秒传命中"
    );

    // 2) 分块（按 11 字节切，故意不整除）
    let chunks: Vec<&[u8]> = content.chunks(11).collect();
    let total = chunks.len();
    let mut last_body = String::new();

    for (index, chunk) in chunks.iter().enumerate() {
        let response = router
            .clone()
            .oneshot(post(
                "/aetherupload/uploading",
                multipart(
                    &[
                        ("chunk_total", &total.to_string()),
                        ("chunk_index", &(index + 1).to_string()),
                        ("resource_temp_basename", &temp_base),
                        ("resource_ext", &ext),
                        ("group_subdir", &sub_dir),
                        ("group", "file"),
                        ("resource_hash", &hash),
                        ("locale", "zh"),
                    ],
                    Some(("resource_chunk", chunk)),
                ),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        last_body = body_text(response).await;
        assert_eq!(
            json_value(&last_body, "error"),
            Some("0"),
            "第 {} 块失败：{last_body}",
            index + 1
        );
    }

    let saved_path = json_value(&last_body, "savedPath").unwrap().to_string();
    assert!(
        !saved_path.is_empty(),
        "末块必须给出 savedPath：{last_body}"
    );

    // 3) 成品落盘：<root>/storage/app/aetherupload/file/<子目录>/<md5>.txt
    let landed = base
        .join("storage/app/aetherupload/file")
        .join(&sub_dir)
        .join(format!("{hash}.txt"));
    assert!(landed.is_file(), "成品未落盘：{}", landed.display());
    assert_eq!(
        std::fs::read(&landed).unwrap(),
        content,
        "内容必须逐字节一致"
    );

    // 断点文件应已清理
    assert_eq!(
        std::fs::read_dir(base.join("storage/app/aetherupload/_header"))
            .unwrap()
            .count(),
        0
    );

    // 4) 展示
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/aetherupload/display/{saved_path}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-content-type-options")
            .unwrap()
            .to_str()
            .unwrap(),
        "nosniff"
    );
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/plain"
    );
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        content
    );

    // 5) 下载（改文件名，扩展名保持）
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/aetherupload/download/{saved_path}/新名字"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // `filename=` 那一份保留原始 UTF-8（与 PHP 逐字节一致，属 obs-text），
    // 所以按字节比对；`filename*=UTF-8''…` 那份是纯 ASCII 的百分号编码
    let disposition = response.headers().get(header::CONTENT_DISPOSITION).unwrap();
    let disposition = String::from_utf8_lossy(disposition.as_bytes());

    assert!(
        disposition.contains("新名字.txt"),
        "ASCII 回退应保留原名：{disposition}"
    );
    assert!(
        disposition.contains("%E6%96%B0%E5%90%8D%E5%AD%97.txt"),
        "下载名应带 RFC 5987 编码：{disposition}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn bad_requests_keep_the_http_contract() {
    let (base, router) = app("errors");

    // 参数缺失：HTTP 200 + JSON 里带错误消息（与 PHP 一致，客户端按 error 字段判成败）
    let response = router
        .clone()
        .oneshot(post(
            "/aetherupload/preprocess",
            multipart(&[("resource_name", "x.txt")], None),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert!(
        json_value(&body, "error").unwrap().starts_with("Error"),
        "默认语种是 en：{body}"
    );

    // 不存在的资源：404 文本
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/aetherupload/display/file_202610_missing.txt")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_text(response).await, "display fail");

    // 目录穿越：savedPath 解码时就被拒
    let response = router
        .oneshot(
            Request::builder()
                .uri("/aetherupload/display/file_..%2F..%2Fetc%2Fpasswd")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn instant_completion_short_circuits_through_http() {
    let base =
        std::env::temp_dir().join(format!("aetherupload-axum-instant-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);

    let config = Config {
        instant_completion: true,
        ..Config::default()
    };

    let runtime = Arc::new(
        Runtime::new(config, &base)
            .unwrap()
            .with_instant(Arc::new(aetherupload::instant::MemoryInstantStore::new())),
    );

    let mut lines = Vec::new();
    aetherupload::console::list_groups(&runtime, &mut |line| lines.push(line.to_string()));

    let router = aetherupload::integrations::axum::routes(runtime);
    let content = b"instant through http".repeat(3);
    let hash = md5_hex(&content);

    // 第一次：走完整链路
    let first = upload_via_router(&router, &content, &hash, "a.txt").await;
    assert!(!first.is_empty());

    // 第二次：预处理阶段就命中，savedPath 非空且无分块上传
    let response = router
        .clone()
        .oneshot(post(
            "/aetherupload/preprocess",
            multipart(
                &[
                    ("resource_name", "a.txt"),
                    ("resource_size", &content.len().to_string()),
                    ("group", "file"),
                    ("resource_hash", &hash),
                    ("locale", "zh"),
                ],
                None,
            ),
        ))
        .await
        .unwrap();

    let body = body_text(response).await;
    assert_eq!(json_value(&body, "error"), Some("0"));
    assert_eq!(
        json_value(&body, "savedPath"),
        Some(first.as_str()),
        "{body}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

async fn upload_via_router(router: &Router, content: &[u8], hash: &str, name: &str) -> String {
    let response = router
        .clone()
        .oneshot(post(
            "/aetherupload/preprocess",
            multipart(
                &[
                    ("resource_name", name),
                    ("resource_size", &content.len().to_string()),
                    ("group", "file"),
                    ("resource_hash", hash),
                    ("locale", "zh"),
                ],
                None,
            ),
        ))
        .await
        .unwrap();

    let body = body_text(response).await;
    assert_eq!(json_value(&body, "error"), Some("0"), "{body}");

    let temp_base = json_value(&body, "resourceTempBaseName")
        .unwrap()
        .to_string();
    let sub_dir = json_value(&body, "groupSubDir").unwrap().to_string();

    let response = router
        .clone()
        .oneshot(post(
            "/aetherupload/uploading",
            multipart(
                &[
                    ("chunk_total", "1"),
                    ("chunk_index", "1"),
                    ("resource_temp_basename", &temp_base),
                    ("resource_ext", "txt"),
                    ("group_subdir", &sub_dir),
                    ("group", "file"),
                    ("resource_hash", hash),
                    ("locale", "zh"),
                ],
                Some(("resource_chunk", content)),
            ),
        ))
        .await
        .unwrap();

    let body = body_text(response).await;
    assert_eq!(json_value(&body, "error"), Some("0"), "{body}");

    json_value(&body, "savedPath").unwrap().to_string()
}
