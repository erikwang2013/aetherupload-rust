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

    let preprocess = String::from_utf8(response.body().to_vec()).unwrap();
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
    assert_eq!(json_field(&body, "error"), "0", "预处理失败：{body}");
    assert_eq!(json_field(&body, "resourceExt"), "txt");

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
    assert_eq!(json_field(&body, "error"), "0", "预处理失败：{body}");

    // 两种都不认的 content-type 仍按 multipart 分支原来的判定回 400
    // （warp 的 content-type 检查是 `InvalidHeader`）—— urlencoded 分支的 404
    // 优先级最低，不会把它的状态码顶上来
    let response = warp::test::request()
        .method("POST")
        .path("/aetherupload/preprocess")
        .header("content-type", "application/json")
        .body("{}")
        .reply(&routes)
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let _ = std::fs::remove_dir_all(&root);
}
