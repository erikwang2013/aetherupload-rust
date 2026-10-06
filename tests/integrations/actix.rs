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
    let body = text(test::read_body(resp).await);
    assert_eq!(json_field(&body, "error"), "0", "预处理应当成功：{body}");
    assert_eq!(json_field(&body, "chunkSize"), CHUNK_SIZE.to_string());
    assert_eq!(json_field(&body, "resourceExt"), "pdf");

    let temp_base_name = json_field(&body, "resourceTempBaseName");
    let group_sub_dir = json_field(&body, "groupSubDir");
    assert!(
        !temp_base_name.is_empty() && !group_sub_dir.is_empty(),
        "临时名与子目录都要有：{body}"
    );

    // —— 2) 逐块上传：末块才给 savedPath ——
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
        assert_eq!(json_field(&body, "error"), "0", "第 {index} 块失败：{body}");

        let path = json_field(&body, "savedPath");

        if offset + 1 == chunks.len() {
            assert!(!path.is_empty(), "末块必须给出 savedPath：{body}");
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

/// 一段带 `%PDF` 魔数的内容。
fn pdf_payload(len: usize) -> Vec<u8> {
    let mut content = b"%PDF-1.7\n".to_vec();
    content.resize(len, b'x');

    content
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
