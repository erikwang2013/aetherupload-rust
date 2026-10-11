// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! e-cat 适配层端到端测试：e-cat 的 HTTP 传输层就是 axum `Router`
//! （`HttpServer::new(addr).router(app)`），因此路由以 `axum::Router<()>` 的形态交付，
//! 用 tower `oneshot` 跑完 预处理 → 分块 → 落盘 → 展示 / 下载 的完整链路。

use std::path::PathBuf;
use std::sync::Arc;

use aetherupload::{Config, Runtime};
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use tower::ServiceExt;

const BOUNDARY: &str = "aetherupload-ecat-boundary";

/// 建临时项目根（目录由 `aetherupload::console::list_groups` 建，等价部署时的 `aetherupload groups`）。
fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("aetherupload-ecat-{tag}-{}", std::process::id()));
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

/// e-cat 应用：交给 `HttpServer::router(...)` 的就是这个 `axum::Router<()>`。
fn app(runtime: Arc<Runtime>) -> axum::Router {
    // 类型即契约：e-cat 的 HttpServer::router 接收 axum 0.8 的 Router
    let upload: axum::Router = aetherupload::integrations::ecat::routes(runtime);

    upload
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
) -> (StatusCode, String, HeaderMap) {
    send(app, multipart_request(path, fields, chunk)).await
}

/// 组一条 multipart POST 请求（不发送）。
fn multipart_request(path: &str, fields: &[(&str, &str)], chunk: Option<&[u8]>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(multipart_body(fields, chunk)))
        .unwrap()
}

/// 组一条分块上传请求（字段与完整链路一致）。
fn chunk_request(
    index: usize,
    total: usize,
    temp_base_name: &str,
    sub_dir: &str,
    hash: &str,
    chunk: &[u8],
) -> Request<Body> {
    multipart_request(
        "/aetherupload/uploading",
        &[
            ("chunk_total", &total.to_string()),
            ("chunk_index", &index.to_string()),
            ("resource_temp_basename", temp_base_name),
            ("resource_ext", "txt"),
            ("group_subdir", sub_dir),
            ("group", "file"),
            ("resource_hash", hash),
        ],
        Some(chunk),
    )
}

/// 断言一条问题响应：状态码原样 + problem+json + 稳定 code + 非空 title。
/// 返回响应体，方便再断言 `expected_index` 之类的附加成员。
async fn assert_problem(
    app: &axum::Router,
    request: Request<Body>,
    status: StatusCode,
    code: &str,
) -> String {
    let (actual, body, headers) = send(app, request).await;
    assert_eq!(actual, status, "{body}");

    let content_type = headers["content-type"].to_str().unwrap();
    assert!(
        content_type.starts_with("application/problem+json"),
        "问题响应必须带 problem+json：{content_type}；{body}"
    );
    assert_eq!(json_field(&body, "type"), "about:blank", "{body}");
    assert_eq!(
        json_field(&body, "status"),
        status.as_u16().to_string().as_str(),
        "{body}"
    );
    assert_eq!(json_field(&body, "code"), code, "{body}");
    assert!(
        !json_field(&body, "title").is_empty(),
        "title 必须非空：{body}"
    );

    body
}

/// 409 chunk_out_of_order：在问题响应基础上再断言 expected_index 指向缺口。
async fn assert_out_of_order(app: &axum::Router, request: Request<Body>, expected_index: usize) {
    let body = assert_problem(app, request, StatusCode::CONFLICT, "chunk_out_of_order").await;
    assert_eq!(
        json_field(&body, "expected_index"),
        expected_index.to_string().as_str(),
        "{body}"
    );
}

/// urlencoded 表单请求（前端脚本发 `preprocess` 的形态）。
async fn post_urlencoded(app: &axum::Router, path: &str, form: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header(
            "content-type",
            "application/x-www-form-urlencoded; charset=UTF-8",
        )
        .body(Body::from(form.to_string()))
        .unwrap();

    let (status, body, _) = send(app, request).await;

    (status, body)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, String, HeaderMap) {
    send(
        app,
        Request::builder().uri(path).body(Body::empty()).unwrap(),
    )
    .await
}

#[tokio::test]
async fn full_chain_through_ecat_transport() {
    let root = temp_root("chain");
    let runtime = Arc::new(Runtime::new(Config::default(), &root).unwrap());
    prepare(&runtime);
    let app = app(runtime);

    let content = b"aetherupload over e-cat (axum transport)";
    let md5 = aetherupload::md5::md5_hex(content);
    let size = content.len().to_string();

    // 1) 预处理
    let (status, preprocess, headers) = post_multipart(
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

    assert_eq!(status, StatusCode::OK, "{preprocess}");
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json"),
        "成功体必须是 application/json"
    );
    assert_eq!(json_field(&preprocess, "chunk_size"), "1000000");
    assert_eq!(json_field(&preprocess, "resource_ext"), "txt");
    assert_eq!(json_field(&preprocess, "saved_path"), "", "{preprocess}");

    let sub_dir = json_field(&preprocess, "group_sub_dir").to_string();
    let temp_base_name = json_field(&preprocess, "resource_temp_base_name").to_string();
    assert!(!sub_dir.is_empty() && !temp_base_name.is_empty());

    // 2) 分块上传（内容不足一个 chunk：单块即末块）
    let (status, chunk, _) = post_multipart(
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

    assert_eq!(status, StatusCode::OK, "{chunk}");

    let saved_path = json_field(&chunk, "saved_path").to_string();
    assert!(!saved_path.is_empty(), "末块应当返回 saved_path: {chunk}");

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

/// 路由挂载面：返回值就是 `axum::Router<()>`，e-cat `HttpServer::router` 直接接收。
#[tokio::test]
async fn routes_return_an_axum_router_for_ecat_http_server() {
    let root = temp_root("mount");
    let runtime = Arc::new(Runtime::new(Config::default(), &root).unwrap());

    let mounted: axum::Router = aetherupload::integrations::ecat::routes(runtime);

    // 展示一条不存在的资源 → 404 resource_not_found（路由确实挂上了，而不是空 Router，
    // 且失败已是 problem+json）
    assert_problem(
        &mounted,
        Request::builder()
            .uri("/aetherupload/display/nope_202610_x.txt")
            .body(Body::empty())
            .unwrap(),
        StatusCode::NOT_FOUND,
        "resource_not_found",
    )
    .await;

    let _ = std::fs::remove_dir_all(&root);
}

/// 前端脚本的 `preprocess` 发的是 urlencoded（zepto 的默认编码），不是 multipart。
/// 请求体按 `aetherupload-all.js` 的原样拼（含中文名的 percent 编码）；这条链路断了，
/// e-cat 下的示例页第一步就会 400「上传失败」。
#[tokio::test]
async fn urlencoded_preprocess_matches_the_bundled_frontend() {
    let root = temp_root("urlencoded");
    let runtime = Arc::new(Runtime::new(Config::default(), &root).unwrap());
    prepare(&runtime);
    let app = app(runtime);

    let (status, body) = post_urlencoded(
        &app,
        "/aetherupload/preprocess",
        "resource_name=%E4%B8%AD%E6%96%87.txt&resource_size=12&group=file\
         &resource_hash=d41d8cd98f00b204e9800998ecf8427e&locale=zh",
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "urlencoded 的预处理必须被接受: {body}"
    );
    assert_eq!(json_field(&body, "chunk_size"), "1000000");
    assert_eq!(json_field(&body, "resource_ext"), "txt");

    let _ = std::fs::remove_dir_all(&root);
}

/// 失败路径矩阵：每个失败都必须是「正确状态码 + problem+json + 稳定 code」。
#[tokio::test]
async fn problem_responses_carry_the_2_0_shape() {
    let root = temp_root("problems");
    let runtime = Arc::new(Runtime::new(Config::default(), &root).unwrap());
    prepare(&runtime);
    let app = app(runtime);

    // 参数缺失 → 400 invalid_parameter（1.x 是 200 + error 字段）
    assert_problem(
        &app,
        multipart_request(
            "/aetherupload/preprocess",
            &[("resource_name", "x.txt")],
            None,
        ),
        StatusCode::BAD_REQUEST,
        "invalid_parameter",
    )
    .await;

    // 未配置的分组 → 400 invalid_group
    assert_problem(
        &app,
        multipart_request(
            "/aetherupload/preprocess",
            &[
                ("resource_name", "x.txt"),
                ("resource_size", "3"),
                ("group", "nope"),
            ],
            None,
        ),
        StatusCode::BAD_REQUEST,
        "invalid_group",
    )
    .await;

    // 没预处理直接打分块 → 409 upload_not_prepared
    assert_problem(
        &app,
        chunk_request(1, 1, "0123456789abcdef", "202610", "", b"orphan"),
        StatusCode::CONFLICT,
        "upload_not_prepared",
    )
    .await;

    // 预处理后：跳号与重发已落块都是 409 chunk_out_of_order + expected_index
    let content = b"0123456789abcdefghij"; // 20 字节 → 按 7 切成 3 块
    let md5 = aetherupload::md5::md5_hex(content);
    let (status, preprocess, _) = post_multipart(
        &app,
        "/aetherupload/preprocess",
        &[
            ("resource_name", "seq.txt"),
            ("resource_size", &content.len().to_string()),
            ("group", "file"),
            ("resource_hash", &md5),
        ],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preprocess}");

    let temp_base_name = json_field(&preprocess, "resource_temp_base_name").to_string();
    let sub_dir = json_field(&preprocess, "group_sub_dir").to_string();
    let chunks: Vec<&[u8]> = content.chunks(7).collect();
    let total = chunks.len();

    let (status, body, _) = post_multipart(
        &app,
        "/aetherupload/uploading",
        &[
            ("chunk_total", &total.to_string()),
            ("chunk_index", "1"),
            ("resource_temp_basename", &temp_base_name),
            ("resource_ext", "txt"),
            ("group_subdir", &sub_dir),
            ("group", "file"),
            ("resource_hash", &md5),
        ],
        Some(chunks[0]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // 跳号（跳过第 2 块直接发第 3 块）→ expected_index 指回缺口
    assert_out_of_order(
        &app,
        chunk_request(3, total, &temp_base_name, &sub_dir, &md5, chunks[2]),
        2,
    )
    .await;

    // 重发已落块（第 1 块）→ 同样 409，expected_index 不变
    assert_out_of_order(
        &app,
        chunk_request(1, total, &temp_base_name, &sub_dir, &md5, chunks[0]),
        2,
    )
    .await;

    // 补齐缺口后照常完成：拒绝没有破坏已落进度
    for index in 2..=total {
        let (status, body, _) = post_multipart(
            &app,
            "/aetherupload/uploading",
            &[
                ("chunk_total", &total.to_string()),
                ("chunk_index", &index.to_string()),
                ("resource_temp_basename", &temp_base_name),
                ("resource_ext", "txt"),
                ("group_subdir", &sub_dir),
                ("group", "file"),
                ("resource_hash", &md5),
            ],
            Some(chunks[index - 1]),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "第 {index} 块: {body}");

        if index == total {
            assert!(!json_field(&body, "saved_path").is_empty(), "{body}");
        }
    }

    // 不存在的资源 → 404 resource_not_found（problem+json，不再是纯文本）
    assert_problem(
        &app,
        Request::builder()
            .uri("/aetherupload/display/file_202610_missing.txt")
            .body(Body::empty())
            .unwrap(),
        StatusCode::NOT_FOUND,
        "resource_not_found",
    )
    .await;

    // 两种编码都不认的 content-type：适配层自己的解析失败 → 400 invalid_request_body
    assert_problem(
        &app,
        Request::builder()
            .method("POST")
            .uri("/aetherupload/preprocess")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap(),
        StatusCode::BAD_REQUEST,
        "invalid_request_body",
    )
    .await;

    // 框架层 body 上限（= chunk_size + 1MB）的 413 payload_too_large 由 axum 适配层的
    // catcher 统一，本层复用同一份路由 —— 已由 `integration_axum` 钉住（§11.4）。

    let _ = std::fs::remove_dir_all(&root);
}
