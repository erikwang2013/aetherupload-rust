// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! Axum 端到端：真起 `Router`，走完整链路（预处理 → 分块 → 末块 → 展示 → 下载）。
//!
//! 断言的都是协议级事实：成功体是 snake_case 的 `application/json`、失败体是
//! `application/problem+json`（状态码 + 稳定 code）、分块按序追加、成品落在
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

/// 拼一条预处理请求（multipart）。
fn preprocess_request(name: &str, size: usize, hash: &str) -> Request<Body> {
    post(
        "/aetherupload/preprocess",
        multipart(
            &[
                ("resource_name", name),
                ("resource_size", &size.to_string()),
                ("group", "file"),
                ("resource_hash", hash),
            ],
            None,
        ),
    )
}

/// 拼一条分块上传请求（字段与完整链路一致）。
fn chunk_request(
    index: usize,
    total: usize,
    temp_base: &str,
    sub_dir: &str,
    hash: &str,
    chunk: &[u8],
) -> Request<Body> {
    post(
        "/aetherupload/uploading",
        multipart(
            &[
                ("chunk_total", &total.to_string()),
                ("chunk_index", &index.to_string()),
                ("resource_temp_basename", temp_base),
                ("resource_ext", "txt"),
                ("group_subdir", sub_dir),
                ("group", "file"),
                ("resource_hash", hash),
            ],
            Some(("resource_chunk", chunk)),
        ),
    )
}

/// 断言一条问题响应：状态码原样 + problem+json + 稳定 code + 非空 title。
/// 返回响应体，方便再断言 `expected_index` 之类的附加成员。
async fn assert_problem(
    response: axum::response::Response,
    status: StatusCode,
    code: &str,
) -> String {
    assert_eq!(response.status(), status);

    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        content_type.starts_with("application/problem+json"),
        "问题响应必须带 problem+json：{content_type}"
    );

    let body = body_text(response).await;
    assert_eq!(json_value(&body, "type"), Some("about:blank"), "{body}");
    assert_eq!(
        json_value(&body, "status"),
        Some(status.as_u16().to_string().as_str()),
        "{body}"
    );
    assert_eq!(json_value(&body, "code"), Some(code), "{body}");
    assert!(
        !json_value(&body, "title").unwrap_or("").is_empty(),
        "title 必须非空：{body}"
    );

    body
}

/// 409 chunk_out_of_order：在问题响应基础上再断言 expected_index 指向缺口。
async fn assert_out_of_order(response: axum::response::Response, expected_index: u64) {
    let body = assert_problem(response, StatusCode::CONFLICT, "chunk_out_of_order").await;
    assert_eq!(
        json_value(&body, "expected_index"),
        Some(expected_index.to_string().as_str()),
        "{body}"
    );
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
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/json"),
        "成功体必须是 application/json"
    );

    let body = body_text(response).await;
    assert_eq!(json_value(&body, "chunk_size"), Some("1000000"), "{body}");

    let temp_base = json_value(&body, "resource_temp_base_name")
        .unwrap()
        .to_string();
    let sub_dir = json_value(&body, "group_sub_dir").unwrap().to_string();
    let ext = json_value(&body, "resource_ext").unwrap().to_string();
    assert_eq!(ext, "txt");
    assert_eq!(
        json_value(&body, "saved_path"),
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

        if index + 1 == total {
            assert!(
                !json_value(&last_body, "saved_path").unwrap_or("").is_empty(),
                "末块必须给出 saved_path：{last_body}"
            );
        } else {
            assert_eq!(
                json_value(&last_body, "saved_path"),
                Some(""),
                "第 {} 块还没完成：{last_body}",
                index + 1
            );
        }
    }

    let saved_path = json_value(&last_body, "saved_path").unwrap().to_string();
    assert!(!saved_path.is_empty(), "{last_body}");

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

    // `filename=` 那一份保留原始 UTF-8（属 obs-text，浏览器按字节原样解），
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

/// 前端脚本发的是 urlencoded —— zepto 对普通对象 `data` 的默认编码，不是 multipart。
/// 请求体按 `aetherupload-all.js` 里 `preprocess` 的原样拼（含中文名的 percent 编码、
/// `; charset=UTF-8` 参数）。这条链路断了，示例页第一步就会 400「上传失败」。
#[tokio::test]
async fn urlencoded_preprocess_matches_the_bundled_frontend() {
    let (base, router) = app("urlencoded");

    let form = "resource_name=%E4%B8%AD%E6%96%87.txt&resource_size=12&group=file\
                &resource_hash=d41d8cd98f00b204e9800998ecf8427e&locale=zh";

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/aetherupload/preprocess")
                .header(
                    header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded; charset=UTF-8",
                )
                .body(Body::from(form))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "urlencoded 的预处理必须被接受"
    );

    let body = body_text(response).await;
    assert_eq!(json_value(&body, "chunk_size"), Some("1000000"), "{body}");
    assert_eq!(json_value(&body, "resource_ext"), Some("txt"));
    assert!(
        !json_value(&body, "resource_temp_base_name")
            .unwrap_or_default()
            .is_empty(),
        "应当下发临时名：{body}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// 失败路径矩阵：每个失败都必须是「正确状态码 + problem+json + 稳定 code」。
#[tokio::test]
async fn problem_responses_carry_the_2_0_shape() {
    let (base, router) = app("problems");

    // 参数缺失 → 400 invalid_parameter（1.x 是 200 + error 字段）
    let response = router
        .clone()
        .oneshot(post(
            "/aetherupload/preprocess",
            multipart(&[("resource_name", "x.txt")], None),
        ))
        .await
        .unwrap();
    assert_problem(response, StatusCode::BAD_REQUEST, "invalid_parameter").await;

    // 未配置的分组 → 400 invalid_group
    let response = router
        .clone()
        .oneshot(post(
            "/aetherupload/preprocess",
            multipart(
                &[
                    ("resource_name", "x.txt"),
                    ("resource_size", "3"),
                    ("group", "nope"),
                ],
                None,
            ),
        ))
        .await
        .unwrap();
    assert_problem(response, StatusCode::BAD_REQUEST, "invalid_group").await;

    // 没预处理直接打分块 → 409 upload_not_prepared
    let response = router
        .clone()
        .oneshot(chunk_request(
            1,
            1,
            "0123456789abcdef",
            "202610",
            "",
            b"orphan",
        ))
        .await
        .unwrap();
    assert_problem(response, StatusCode::CONFLICT, "upload_not_prepared").await;

    // 预处理后：跳号与重发已落块都是 409 chunk_out_of_order + expected_index
    let content = b"0123456789abcdefghij"; // 20 字节 → 按 7 切成 3 块
    let hash = md5_hex(content);
    let response = router
        .clone()
        .oneshot(preprocess_request("seq.txt", content.len(), &hash))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = body_text(response).await;
    let temp_base = json_value(&body, "resource_temp_base_name")
        .unwrap()
        .to_string();
    let sub_dir = json_value(&body, "group_sub_dir").unwrap().to_string();

    let chunks: Vec<&[u8]> = content.chunks(7).collect();
    let total = chunks.len();

    let response = router
        .clone()
        .oneshot(chunk_request(1, total, &temp_base, &sub_dir, &hash, chunks[0]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // 跳号（跳过第 2 块直接发第 3 块）→ expected_index 指回缺口
    let response = router
        .clone()
        .oneshot(chunk_request(3, total, &temp_base, &sub_dir, &hash, chunks[2]))
        .await
        .unwrap();
    assert_out_of_order(response, 2).await;

    // 重发已落块（第 1 块）→ 同样 409，expected_index 不变
    let response = router
        .clone()
        .oneshot(chunk_request(1, total, &temp_base, &sub_dir, &hash, chunks[0]))
        .await
        .unwrap();
    assert_out_of_order(response, 2).await;

    // 补齐缺口后照常完成：拒绝没有破坏已落进度
    for index in 2..=total {
        let response = router
            .clone()
            .oneshot(chunk_request(
                index,
                total,
                &temp_base,
                &sub_dir,
                &hash,
                chunks[index - 1],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "第 {index} 块");

        if index == total {
            let body = body_text(response).await;
            assert!(!json_value(&body, "saved_path").unwrap_or("").is_empty(), "{body}");
        }
    }

    // 单块超过配置的 chunk_size → 413 payload_too_large（1.x 无此校验，2.0 起拒）
    let small_root =
        std::env::temp_dir().join(format!("aetherupload-axum-small-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&small_root);

    let small = Arc::new(
        Runtime::new(
            Config {
                chunk_size: 1024,
                ..Config::default()
            },
            &small_root,
        )
        .unwrap(),
    );

    let mut lines = Vec::new();
    aetherupload::console::list_groups(&small, &mut |line| lines.push(line.to_string()));
    let small_router = aetherupload::integrations::axum::routes(small);

    let big_chunk = vec![b'x'; 2048];
    let big_hash = md5_hex(&big_chunk);
    let response = small_router
        .clone()
        .oneshot(preprocess_request("big.txt", big_chunk.len(), &big_hash))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = body_text(response).await;
    assert_eq!(json_value(&body, "chunk_size"), Some("1024"), "{body}");
    let temp_base = json_value(&body, "resource_temp_base_name")
        .unwrap()
        .to_string();
    let sub_dir = json_value(&body, "group_sub_dir").unwrap().to_string();

    // 框架层 body 上限（= chunk_size + 1MB）由 axum 的 `DefaultBodyLimit` 先把关：
    // 超限请求到不了内核，适配层 catcher 也必须统一成 413 + payload_too_large（§11.4）
    let junk = "a".repeat(1_100_000);
    let response = small_router
        .clone()
        .oneshot(post(
            "/aetherupload/preprocess",
            multipart(&[("resource_name", "x.txt"), ("junk", junk.as_str())], None),
        ))
        .await
        .unwrap();
    assert_problem(response, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large").await;

    let response = small_router
        .oneshot(chunk_request(
            1,
            1,
            &temp_base,
            &sub_dir,
            &big_hash,
            &big_chunk,
        ))
        .await
        .unwrap();
    assert_problem(
        response,
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
    )
    .await;

    let _ = std::fs::remove_dir_all(&small_root);

    // 不存在的资源 → 404 resource_not_found（problem+json，不再是纯文本 "display fail"）
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
    assert_problem(response, StatusCode::NOT_FOUND, "resource_not_found").await;

    // 目录穿越：saved_path 解码时就被拒 → 400 invalid_parameter
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/aetherupload/display/file_..%2F..%2Fetc%2Fpasswd")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_problem(response, StatusCode::BAD_REQUEST, "invalid_parameter").await;

    // 两种编码都不认的 content-type：适配层自己的解析失败 → 400 invalid_request_body
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/aetherupload/preprocess")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_problem(response, StatusCode::BAD_REQUEST, "invalid_request_body").await;

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

    // 第二次：预处理阶段就命中，saved_path 非空且无分块上传
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

    assert_eq!(response.status(), StatusCode::OK);

    let body = body_text(response).await;
    assert_eq!(
        json_value(&body, "saved_path"),
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

    assert_eq!(response.status(), StatusCode::OK);

    let body = body_text(response).await;
    let temp_base = json_value(&body, "resource_temp_base_name")
        .unwrap()
        .to_string();
    let sub_dir = json_value(&body, "group_sub_dir").unwrap().to_string();

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

    assert_eq!(response.status(), StatusCode::OK);

    let body = body_text(response).await;
    json_value(&body, "saved_path").unwrap().to_string()
}
