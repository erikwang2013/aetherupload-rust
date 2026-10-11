// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! Actix Web 适配层的端到端测试：预处理 → 分块上传 → 展示 / 下载。
//!
//! 请求体按前端脚本发包的真实形态手写（boundary、字段名、文件字段都照
//! `aetherupload-all.js` 来），不借框架的表单构造器 —— 适配层要能接的正是前端真正
//! 发出来的东西。

use std::sync::Arc;

use actix_web::http::{StatusCode, header};
use actix_web::{App, test};
use aetherupload::integrations::actix::routes;
use aetherupload::{Config, Runtime, md5};

/// multipart 分隔符（真实前端每次随机，测试里固定）。
const BOUNDARY: &str = "----aetheruploadactixtest";

/// 分块大小：调小一些，三块就能跑完整个流程。
const CHUNK_SIZE: u64 = 300_000;

#[actix_web::test]
async fn upload_then_display_and_download() {
    let root = std::env::temp_dir().join(format!("aetherupload-actix-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    let runtime = Arc::new(
        Runtime::new(
            Config {
                chunk_size: CHUNK_SIZE,
                ..Config::default()
            },
            &root,
        )
        .unwrap(),
    );

    // 先建目录 —— 内核用非递归 mkdir，分组目录与 `_header` 都得先有；
    // 真实部署里这一步是 `aetherupload groups`
    let mut lines = Vec::new();
    assert_eq!(
        aetherupload::console::list_groups(&runtime, &mut |line| lines.push(line.to_string())),
        0,
        "{lines:?}"
    );

    let app = test::init_service(App::new().service(routes(runtime))).await;

    // 内容必须带魔数：末块会按**真实内容**反查扩展名，纯文本会被判成「未知类型」
    let content = pdf_payload(700_000);
    let hash = md5::md5_hex(&content);
    let size = content.len().to_string();

    // —— 1) 预处理：拿到分块大小与临时名 ——
    let resp = test::call_service(
        &app,
        multipart_request(
            "/aetherupload/preprocess",
            &[
                ("resource_name", "report.pdf"),
                ("resource_size", &size),
                ("group", "file"),
                ("resource_hash", &hash),
            ],
            None,
        )
        .to_request(),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/json"),
        "成功体必须是 application/json"
    );

    let body = text(test::read_body(resp).await);
    assert_eq!(json_field(&body, "chunk_size"), CHUNK_SIZE.to_string());
    assert_eq!(json_field(&body, "resource_ext"), "pdf");
    assert_eq!(json_field(&body, "saved_path"), "", "{body}");

    let temp_base_name = json_field(&body, "resource_temp_base_name");
    let group_sub_dir = json_field(&body, "group_sub_dir");
    assert!(
        !temp_base_name.is_empty() && !group_sub_dir.is_empty(),
        "临时名与子目录都要有：{body}"
    );

    // —— 2) 逐块上传：末块才给 saved_path ——
    let chunks: Vec<&[u8]> = content.chunks(CHUNK_SIZE as usize).collect();
    let chunk_total = chunks.len().to_string();
    assert_eq!(chunks.len(), 3, "700000 字节按 300000 切应当是三块");

    let mut saved_path = String::new();

    for (offset, chunk) in chunks.iter().enumerate() {
        let index = (offset + 1).to_string();

        let resp = test::call_service(
            &app,
            multipart_request(
                "/aetherupload/uploading",
                &[
                    ("chunk_total", &chunk_total),
                    ("chunk_index", &index),
                    ("resource_temp_basename", &temp_base_name),
                    ("resource_ext", "pdf"),
                    ("group_subdir", &group_sub_dir),
                    ("group", "file"),
                    ("resource_hash", &hash),
                ],
                Some(("resource_chunk", chunk)),
            )
            .to_request(),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let body = text(test::read_body(resp).await);

        let path = json_field(&body, "saved_path");

        if offset + 1 == chunks.len() {
            assert!(!path.is_empty(), "末块必须给出 saved_path：{body}");
            saved_path = path;
        } else {
            assert!(path.is_empty(), "非末块不该落盘：{body}");
        }
    }

    // 落盘位置：<root>/storage/app/aetherupload/file/<subdir>/<md5>.pdf
    let landed = root
        .join("storage/app/aetherupload/file")
        .join(&group_sub_dir)
        .join(format!("{hash}.pdf"));

    assert_eq!(
        std::fs::read(&landed).expect("成品文件应当落在分组子目录里"),
        content,
        "磁盘内容要与上传的逐字节一致"
    );

    // —— 3) 展示：内联下发，内容与上传一致 ——
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/aetherupload/display/{saved_path}"))
            .to_request(),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/pdf"
    );
    assert_eq!(test::read_body(resp).await.as_ref(), content.as_slice());

    // —— 4) 下载：改文件名（扩展名仍取原资源）——
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!(
                "/aetherupload/download/{saved_path}/renamed-report"
            ))
            .to_request(),
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);

    let disposition = resp
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .expect("下载必须带 Content-Disposition")
        .to_str()
        .unwrap()
        .to_string();

    assert!(disposition.starts_with("attachment"), "实际: {disposition}");
    assert!(
        disposition.contains("renamed-report.pdf"),
        "下载名应当是客户端给的名字 + 原扩展名：{disposition}"
    );
    assert_eq!(test::read_body(resp).await.as_ref(), content.as_slice());

    let _ = std::fs::remove_dir_all(&root);
}

/// 前端脚本发的是 urlencoded —— zepto 对普通对象 `data` 的默认编码，不是 multipart。
/// 请求体按 `aetherupload-all.js` 里 `preprocess` 的原样拼（含中文名的 percent 编码）。
#[actix_web::test]
async fn urlencoded_preprocess_matches_the_bundled_frontend() {
    let root = std::env::temp_dir().join(format!(
        "aetherupload-actix-urlencoded-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);

    let runtime = Arc::new(Runtime::new(Config::default(), &root).unwrap());

    let mut lines = Vec::new();
    assert_eq!(
        aetherupload::console::list_groups(&runtime, &mut |line| lines.push(line.to_string())),
        0,
        "{lines:?}"
    );

    let app = test::init_service(App::new().service(routes(runtime))).await;

    let resp = test::call_service(
        &app,
        urlencoded_request(
            "/aetherupload/preprocess",
            "resource_name=%E4%B8%AD%E6%96%87.txt&resource_size=12&group=file\
             &resource_hash=d41d8cd98f00b204e9800998ecf8427e&locale=zh",
        )
        .to_request(),
    )
    .await;

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "urlencoded 的预处理必须被接受"
    );

    let body = text(test::read_body(resp).await);
    assert_eq!(json_field(&body, "chunk_size"), "1000000");
    assert_eq!(json_field(&body, "resource_ext"), "txt");

    let _ = std::fs::remove_dir_all(&root);
}

/// 失败路径矩阵：每个失败都必须是「正确状态码 + problem+json + 稳定 code」。
#[actix_web::test]
async fn problem_responses_carry_the_2_0_shape() {
    let root = std::env::temp_dir().join(format!(
        "aetherupload-actix-problems-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);

    let runtime = Arc::new(Runtime::new(Config::default(), &root).unwrap());

    let mut lines = Vec::new();
    assert_eq!(
        aetherupload::console::list_groups(&runtime, &mut |line| lines.push(line.to_string())),
        0,
        "{lines:?}"
    );

    let app = test::init_service(App::new().service(routes(runtime))).await;

    // 参数缺失 → 400 invalid_parameter（1.x 是 200 + error 字段）
    let resp = test::call_service(
        &app,
        multipart_request(
            "/aetherupload/preprocess",
            &[("resource_name", "x.txt")],
            None,
        )
        .to_request(),
    )
    .await;
    assert_problem(resp, StatusCode::BAD_REQUEST, "invalid_parameter").await;

    // 未配置的分组 → 400 invalid_group
    let resp = test::call_service(
        &app,
        multipart_request(
            "/aetherupload/preprocess",
            &[
                ("resource_name", "x.txt"),
                ("resource_size", "3"),
                ("group", "nope"),
            ],
            None,
        )
        .to_request(),
    )
    .await;
    assert_problem(resp, StatusCode::BAD_REQUEST, "invalid_group").await;

    // 没预处理直接打分块 → 409 upload_not_prepared
    let resp = test::call_service(
        &app,
        chunk_request(1, 1, "0123456789abcdef", "202610", "", b"orphan").to_request(),
    )
    .await;
    assert_problem(resp, StatusCode::CONFLICT, "upload_not_prepared").await;

    // 预处理后：跳号与重发已落块都是 409 chunk_out_of_order + expected_index
    let content = b"0123456789abcdefghij"; // 20 字节 → 按 7 切成 3 块
    let hash = md5::md5_hex(content);
    let resp = test::call_service(
        &app,
        preprocess_request("seq.txt", content.len(), &hash).to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = text(test::read_body(resp).await);
    let temp_base_name = json_field(&body, "resource_temp_base_name");
    let group_sub_dir = json_field(&body, "group_sub_dir");
    let chunks: Vec<&[u8]> = content.chunks(7).collect();
    let total = chunks.len();

    let resp = test::call_service(
        &app,
        chunk_request(1, total, &temp_base_name, &group_sub_dir, &hash, chunks[0]).to_request(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // 跳号（跳过第 2 块直接发第 3 块）→ expected_index 指回缺口
    let resp = test::call_service(
        &app,
        chunk_request(3, total, &temp_base_name, &group_sub_dir, &hash, chunks[2]).to_request(),
    )
    .await;
    assert_out_of_order(resp, 2).await;

    // 重发已落块（第 1 块）→ 同样 409，expected_index 不变
    let resp = test::call_service(
        &app,
        chunk_request(1, total, &temp_base_name, &group_sub_dir, &hash, chunks[0]).to_request(),
    )
    .await;
    assert_out_of_order(resp, 2).await;

    // 补齐缺口后照常完成：拒绝没有破坏已落进度
    for index in 2..=total {
        let resp = test::call_service(
            &app,
            chunk_request(
                index,
                total,
                &temp_base_name,
                &group_sub_dir,
                &hash,
                chunks[index - 1],
            )
            .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK, "第 {index} 块");

        if index == total {
            let body = text(test::read_body(resp).await);
            assert!(!json_field(&body, "saved_path").is_empty(), "{body}");
        }
    }

    // 不存在的资源 → 404 resource_not_found（problem+json，不再是纯文本）
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/aetherupload/display/file_202610_missing.txt")
            .to_request(),
    )
    .await;
    assert_problem(resp, StatusCode::NOT_FOUND, "resource_not_found").await;

    // 两种编码都不认的 content-type：适配层自己的解析失败 → 400 invalid_request_body
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/aetherupload/preprocess")
            .insert_header((header::CONTENT_TYPE, "application/json"))
            .set_payload("{}")
            .to_request(),
    )
    .await;
    assert_problem(resp, StatusCode::BAD_REQUEST, "invalid_request_body").await;

    // TODO(2.0 §11.4)：框架层 body 上限（= chunk_size + 1MB）被拒时的 413 payload_too_large
    // 本层未覆盖 —— 需构造 >1MB 的 multipart 体并确认 actix 的限额拒绝被 catcher 统一；
    // axum 与 warp 两层已各钉一条。

    let _ = std::fs::remove_dir_all(&root);
}

/// 拼一条预处理请求（multipart）。
fn preprocess_request(name: &str, size: usize, hash: &str) -> test::TestRequest {
    multipart_request(
        "/aetherupload/preprocess",
        &[
            ("resource_name", name),
            ("resource_size", &size.to_string()),
            ("group", "file"),
            ("resource_hash", hash),
        ],
        None,
    )
}

/// 拼一条分块上传请求（字段与完整链路一致）。
fn chunk_request(
    index: usize,
    total: usize,
    temp_base_name: &str,
    group_sub_dir: &str,
    hash: &str,
    chunk: &[u8],
) -> test::TestRequest {
    multipart_request(
        "/aetherupload/uploading",
        &[
            ("chunk_total", &total.to_string()),
            ("chunk_index", &index.to_string()),
            ("resource_temp_basename", temp_base_name),
            ("resource_ext", "txt"),
            ("group_subdir", group_sub_dir),
            ("group", "file"),
            ("resource_hash", hash),
        ],
        Some(("resource_chunk", chunk)),
    )
}

/// 断言一条问题响应：状态码原样 + problem+json + 稳定 code + 非空 title。
/// 返回响应体，方便再断言 `expected_index` 之类的附加成员。
async fn assert_problem<B>(
    resp: actix_web::dev::ServiceResponse<B>,
    status: StatusCode,
    code: &str,
) -> String
where
    B: actix_web::body::MessageBody,
{
    assert_eq!(resp.status(), status);

    let content_type = resp
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

    let body = text(test::read_body(resp).await);
    assert_eq!(json_field(&body, "type"), "about:blank", "{body}");
    assert_eq!(
        json_field(&body, "status"),
        status.as_u16().to_string(),
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
async fn assert_out_of_order<B>(resp: actix_web::dev::ServiceResponse<B>, expected_index: usize)
where
    B: actix_web::body::MessageBody,
{
    let body = assert_problem(resp, StatusCode::CONFLICT, "chunk_out_of_order").await;
    assert_eq!(
        json_field(&body, "expected_index"),
        expected_index.to_string(),
        "{body}"
    );
}

/// 一段带 `%PDF` 魔数的内容。
fn pdf_payload(len: usize) -> Vec<u8> {
    let mut content = b"%PDF-1.7\n".to_vec();
    content.resize(len, b'x');

    content
}

/// 组一个 urlencoded 请求（前端脚本发 `preprocess` 的形态）。
fn urlencoded_request(path: &str, body: &str) -> test::TestRequest {
    test::TestRequest::post()
        .uri(path)
        .insert_header((header::CONTENT_TYPE, "application/x-www-form-urlencoded"))
        .set_payload(body.to_string())
}

/// 组一个 multipart 请求（`file` 给 `resource_chunk` 用）。
fn multipart_request(
    path: &str,
    fields: &[(&str, &str)],
    file: Option<(&str, &[u8])>,
) -> test::TestRequest {
    test::TestRequest::post()
        .uri(path)
        .insert_header((
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        ))
        .set_payload(multipart_body(fields, file))
}

fn multipart_body(fields: &[(&str, &str)], file: Option<(&str, &[u8])>) -> Vec<u8> {
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
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"blob\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }

    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

    body
}

fn text(bytes: actix_web::web::Bytes) -> String {
    String::from_utf8(bytes.to_vec()).expect("响应体是 UTF-8")
}

/// 从内核的紧凑 JSON 里取一个字段（值可能是数字或字符串）。
fn json_field(json: &str, key: &str) -> String {
    let marker = format!("\"{key}\":");
    let start = json
        .find(&marker)
        .unwrap_or_else(|| panic!("没有 {key} 字段：{json}"));
    let rest = &json[start + marker.len()..];
    let value = rest.split([',', '}']).next().unwrap();

    value.trim_matches('"').to_string()
}
