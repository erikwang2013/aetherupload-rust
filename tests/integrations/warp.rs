// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! warp 适配层端到端测试：`warp::test::request()` 把请求直接喂给过滤器链，
//! 跑完 预处理 → 分块 → 落盘 → 展示 / 下载 的完整链路。

use std::path::PathBuf;
use std::sync::Arc;

use aetherupload::{Config, Runtime};
use warp::http::StatusCode;

const BOUNDARY: &str = "aetherupload-warp-boundary";

/// 建临时项目根（目录由 `aetherupload::console::list_groups` 建，等价部署时的 `aetherupload groups`）。
fn temp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("aetherupload-warp-{tag}-{}", std::process::id()));
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

/// 拼一条分块上传的请求体（字段与完整链路一致）。
fn chunk_body(
    total: usize,
    index: usize,
    temp_base_name: &str,
    sub_dir: &str,
    hash: &str,
    bytes: &[u8],
) -> Vec<u8> {
    multipart_body(
        &[
            ("chunk_total", &total.to_string()),
            ("chunk_index", &index.to_string()),
            ("resource_temp_basename", temp_base_name),
            ("resource_ext", "txt"),
            ("group_subdir", sub_dir),
            ("group", "file"),
            ("resource_hash", hash),
        ],
        Some(bytes),
    )
}

/// 测试响应的类型别名（`warp::test` 的 `reply()` 经 hyper 转出 `Bytes`）。
type Reply = warp::http::Response<warp::hyper::body::Bytes>;

/// 断言一条问题响应：状态码原样 + problem+json 体 + 稳定 code + 非空 title。
fn assert_problem(response: &Reply, status: StatusCode, code: &str) {
    let body = String::from_utf8(response.body().to_vec()).unwrap();
    assert_eq!(response.status(), status, "{body}");

    let content_type = response.headers()["content-type"].to_str().unwrap();
    assert!(
        content_type.starts_with("application/problem+json"),
        "问题响应必须带 problem+json：{content_type}；{body}"
    );
    assert_eq!(json_field(&body, "code"), code, "{body}");
    assert_eq!(
        json_field(&body, "status"),
        status.as_u16().to_string().as_str(),
        "{body}"
    );
    assert!(!json_field(&body, "title").is_empty(), "{body}");
}

/// 409 chunk_out_of_order：在问题响应基础上再断言 expected_index 指向缺口。
fn assert_out_of_order(response: &Reply, expected_index: u64) {
    assert_problem(response, StatusCode::CONFLICT, "chunk_out_of_order");

    let body = String::from_utf8(response.body().to_vec()).unwrap();
    assert_eq!(
        json_field(&body, "expected_index").parse::<u64>().unwrap(),
        expected_index,
        "{body}"
    );
}

#[tokio::test]
async fn full_chain_through_warp_filters() {
    let root = temp_root("chain");
    let runtime = Arc::new(Runtime::new(Config::default(), &root).unwrap());
    prepare(&runtime);
    let routes = aetherupload::integrations::warp::routes(runtime);

    let content = b"aetherupload over warp filters";
    let md5 = aetherupload::md5::md5_hex(content);
    let size = content.len().to_string();

    // 1) 预处理
    let response = warp::test::request()
        .method("POST")
        .path("/aetherupload/preprocess")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(multipart_body(
            &[
                ("resource_name", "a.txt"),
                ("resource_size", &size),
                ("group", "file"),
                ("resource_hash", &md5),
            ],
            None,
        ))
        .reply(&routes)
        .await;

    assert_eq!(response.status(), StatusCode::OK);

    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json"),
        "成功体必须是 application/json"
    );

    let preprocess = String::from_utf8(response.body().to_vec()).unwrap();
    assert_eq!(json_field(&preprocess, "chunk_size"), "1000000");
    assert_eq!(json_field(&preprocess, "resource_ext"), "txt");
    assert_eq!(json_field(&preprocess, "saved_path"), "", "{preprocess}");

    let sub_dir = json_field(&preprocess, "group_sub_dir").to_string();
    let temp_base_name = json_field(&preprocess, "resource_temp_base_name").to_string();
    assert!(!sub_dir.is_empty() && !temp_base_name.is_empty());

    // 2) 分块上传（内容不足一个 chunk：单块即末块）
    let response = warp::test::request()
        .method("POST")
        .path("/aetherupload/uploading")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(multipart_body(
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
        ))
        .reply(&routes)
        .await;

    assert_eq!(response.status(), StatusCode::OK);

    let chunk = String::from_utf8(response.body().to_vec()).unwrap();
    let saved_path = json_field(&chunk, "saved_path").to_string();
    assert!(!saved_path.is_empty(), "末块应当返回 saved_path: {chunk}");

    // 3) 落盘路径：<root>/storage/app/aetherupload/file/<subdir>/<md5>.txt
    let landed = root
        .join("storage/app/aetherupload/file")
        .join(&sub_dir)
        .join(format!("{md5}.txt"));
    assert_eq!(std::fs::read(&landed).unwrap(), content);

    // 4) 展示：内联下发，字节一致，带 nosniff
    let response = warp::test::request()
        .path(&format!("/aetherupload/display/{saved_path}"))
        .reply(&routes)
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.body().as_ref(), content);
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");

    // 5) 下载：改名 + 附件头
    let response = warp::test::request()
        .path(&format!("/aetherupload/download/{saved_path}/renamed.txt"))
        .reply(&routes)
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.body().as_ref(), content);

    let disposition = response.headers()["content-disposition"].to_str().unwrap();
    assert!(
        disposition.starts_with("attachment;"),
        "实际: {disposition}"
    );
    assert!(disposition.contains("renamed.txt"), "实际: {disposition}");

    let _ = std::fs::remove_dir_all(&root);
}

/// 未上传过的路径 404；`saved_path` 缺失（路径段不够）同样 404。
#[tokio::test]
async fn missing_resources_are_404() {
    let root = temp_root("missing");
    let runtime = Arc::new(Runtime::new(Config::default(), &root).unwrap());
    let routes = aetherupload::integrations::warp::routes(runtime);

    let response = warp::test::request()
        .path("/aetherupload/display/nope_202610_x.txt")
        .reply(&routes)
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // 404 也必须是问题响应体（1.x 这里是纯文本）
    assert_problem(&response, StatusCode::NOT_FOUND, "resource_not_found");

    // 路径前缀在、缺 `{saved_path}` 段：四个分支全部 reject，warp 的
    // 「NOT_FOUND 优先级最低、METHOD_NOT_ALLOWED 次之」规则把 POST 兄弟路由的
    // 方法不匹配抬了上来 → 405（axum 适配层同一路径是 404）。
    let response = warp::test::request()
        .path("/aetherupload/display")
        .reply(&routes)
        .await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);

    let _ = std::fs::remove_dir_all(&root);
}

/// 前端脚本发的是 urlencoded —— zepto 对普通对象 `data` 的默认编码，不是 multipart。
/// 这条分支走 `.or()` 的另一侧：content-type 不是 urlencoded 时它拒绝，落到 multipart。
#[tokio::test]
async fn urlencoded_preprocess_matches_the_bundled_frontend() {
    let root = temp_root("urlencoded");
    let runtime = Arc::new(Runtime::new(Config::default(), &root).unwrap());
    prepare(&runtime);
    let routes = aetherupload::integrations::warp::routes(runtime);

    let response = warp::test::request()
        .method("POST")
        .path("/aetherupload/preprocess")
        .header(
            "content-type",
            "application/x-www-form-urlencoded; charset=UTF-8",
        )
        .body(
            "resource_name=%E4%B8%AD%E6%96%87.txt&resource_size=12&group=file\
             &resource_hash=d41d8cd98f00b204e9800998ecf8427e&locale=zh",
        )
        .reply(&routes)
        .await;

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "urlencoded 的预处理必须被接受"
    );

    let body = String::from_utf8(response.body().to_vec()).unwrap();
    assert_eq!(json_field(&body, "resource_ext"), "txt");

    // multipart 那条分支仍然照旧（同一个 runtime，换个编码再打一次）
    let response = warp::test::request()
        .method("POST")
        .path("/aetherupload/preprocess")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(multipart_body(
            &[
                ("resource_name", "a.txt"),
                ("resource_size", "12"),
                ("group", "file"),
                ("resource_hash", "d41d8cd98f00b204e9800998ecf8427e"),
            ],
            None,
        ))
        .reply(&routes)
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(response.body().to_vec()).unwrap();
    assert_eq!(json_field(&body, "chunk_size"), "1000000");

    // 两种都不认的 content-type：适配层自己的解析失败 → 400 invalid_request_body
    // （§11.5.2，1.x 是框架原生的纯文本 400）
    let response = warp::test::request()
        .method("POST")
        .path("/aetherupload/preprocess")
        .header("content-type", "application/json")
        .body("{}")
        .reply(&routes)
        .await;

    assert_problem(&response, StatusCode::BAD_REQUEST, "invalid_request_body");

    let _ = std::fs::remove_dir_all(&root);
}

/// 失败路径矩阵：每个失败都必须是「正确状态码 + problem+json + 稳定 code」。
#[tokio::test]
async fn problem_responses_carry_the_2_0_shape() {
    let root = temp_root("problems");
    let runtime = Arc::new(Runtime::new(Config::default(), &root).unwrap());
    prepare(&runtime);
    let routes = aetherupload::integrations::warp::routes(runtime);

    // 1) 缺 resource_size → 400 invalid_parameter（1.x 是 200 + error 字段）
    let response = warp::test::request()
        .method("POST")
        .path("/aetherupload/preprocess")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(multipart_body(
            &[("resource_name", "a.txt"), ("group", "file")],
            None,
        ))
        .reply(&routes)
        .await;
    assert_problem(&response, StatusCode::BAD_REQUEST, "invalid_parameter");

    // 2) 没预处理直接打分块 → 409 upload_not_prepared
    let response = warp::test::request()
        .method("POST")
        .path("/aetherupload/uploading")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(chunk_body(1, 1, "0123456789abcdef", "202610", "", b"orphan"))
        .reply(&routes)
        .await;
    assert_problem(&response, StatusCode::CONFLICT, "upload_not_prepared");

    // 3) 预处理后：跳号与重发已落块都是 409 chunk_out_of_order
    let content = b"0123456789abcdefghij"; // 20 字节 → 按 7 切成 3 块
    let md5 = aetherupload::md5::md5_hex(content);
    let response = warp::test::request()
        .method("POST")
        .path("/aetherupload/preprocess")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(multipart_body(
            &[
                ("resource_name", "seq.txt"),
                ("resource_size", &content.len().to_string()),
                ("group", "file"),
                ("resource_hash", &md5),
            ],
            None,
        ))
        .reply(&routes)
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let preprocess = String::from_utf8(response.body().to_vec()).unwrap();
    let temp_base_name = json_field(&preprocess, "resource_temp_base_name").to_string();
    let sub_dir = json_field(&preprocess, "group_sub_dir").to_string();
    let chunks: Vec<&[u8]> = content.chunks(7).collect();
    let total = chunks.len();

    let response = warp::test::request()
        .method("POST")
        .path("/aetherupload/uploading")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(chunk_body(total, 1, &temp_base_name, &sub_dir, &md5, chunks[0]))
        .reply(&routes)
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    // 跳号（跳过第 2 块直接发第 3 块）→ expected_index 指回缺口
    let response = warp::test::request()
        .method("POST")
        .path("/aetherupload/uploading")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(chunk_body(total, 3, &temp_base_name, &sub_dir, &md5, chunks[2]))
        .reply(&routes)
        .await;
    assert_out_of_order(&response, 2);

    // 重发已落块（第 1 块）→ 同样 409，expected_index 不变
    let response = warp::test::request()
        .method("POST")
        .path("/aetherupload/uploading")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(chunk_body(total, 1, &temp_base_name, &sub_dir, &md5, chunks[0]))
        .reply(&routes)
        .await;
    assert_out_of_order(&response, 2);

    // 补齐缺口后照常完成：拒绝没有破坏已落进度
    for index in 2..=total {
        let response = warp::test::request()
            .method("POST")
            .path("/aetherupload/uploading")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(chunk_body(
                total,
                index,
                &temp_base_name,
                &sub_dir,
                &md5,
                chunks[index - 1],
            ))
            .reply(&routes)
            .await;
        assert_eq!(response.status(), StatusCode::OK, "第 {index} 块");

        if index == total {
            let body = String::from_utf8(response.body().to_vec()).unwrap();
            assert!(!json_field(&body, "saved_path").is_empty(), "{body}");
        }
    }

    // 4) 末块 md5 与 resource_hash 不符 → 422 hash_mismatch
    let response = warp::test::request()
        .method("POST")
        .path("/aetherupload/preprocess")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(multipart_body(
            &[
                ("resource_name", "bad.txt"),
                ("resource_size", &content.len().to_string()),
                ("group", "file"),
                ("resource_hash", &md5),
            ],
            None,
        ))
        .reply(&routes)
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    let preprocess = String::from_utf8(response.body().to_vec()).unwrap();
    let temp_base_name = json_field(&preprocess, "resource_temp_base_name").to_string();
    let sub_dir = json_field(&preprocess, "group_sub_dir").to_string();

    let response = warp::test::request()
        .method("POST")
        .path("/aetherupload/uploading")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(chunk_body(
            1,
            1,
            &temp_base_name,
            &sub_dir,
            &"0".repeat(32),
            content,
        ))
        .reply(&routes)
        .await;
    assert_problem(
        &response,
        StatusCode::UNPROCESSABLE_ENTITY,
        "hash_mismatch",
    );

    // 框架层 body 上限（= chunk_size + 1MB，warp 侧是 `content_length_limit`）先把关：
    // 超限请求到不了内核，适配层 catcher 也必须统一成 413 + payload_too_large（§11.4）
    let small_root = temp_root("problems-small");
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
    prepare(&small);
    let small_routes = aetherupload::integrations::warp::routes(small);

    let junk = "a".repeat(1_100_000);
    let response = warp::test::request()
        .method("POST")
        .path("/aetherupload/preprocess")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(multipart_body(
            &[("resource_name", "x.txt"), ("junk", junk.as_str())],
            None,
        ))
        .reply(&small_routes)
        .await;
    assert_problem(&response, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");

    let _ = std::fs::remove_dir_all(&small_root);
    let _ = std::fs::remove_dir_all(&root);
}
