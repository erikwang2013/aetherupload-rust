// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! Rocket 0.5 适配层的端到端测试：预处理 → 分块上传 → 展示 / 下载。
//!
//! 用 `rocket::local::blocking::Client` 在进程内跑，不开端口。请求体照前端
//! `aetherupload-all.js` 发包的真实形态手写（boundary、字段名、文件字段都照抄），
//! 顺便验证 `mount()` 把 `data-form` 限额放开到位 —— 不放开的话第一块就 413。

use std::sync::Arc;

use aetherupload::integrations::rocket::mount;
use aetherupload::{Config, Runtime, md5};
use rocket::http::{Header, Status};
use rocket::local::blocking::{Client, LocalResponse};

/// multipart 分隔符（真实前端每次随机，测试里固定）。
const BOUNDARY: &str = "----aetheruploadrockettest";

/// 分块大小：调小一些，三块就能跑完整个流程。
const CHUNK_SIZE: u64 = 300_000;

#[test]
fn upload_then_display_and_download() {
    let root = std::env::temp_dir().join(format!("aetherupload-rocket-it-{}", std::process::id()));
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

    let client = Client::tracked(mount(rocket::build(), runtime)).expect("rocket 应当能点火");

    // 内容必须带魔数：末块会按**真实内容**反查扩展名，纯文本会被判成「未知类型」
    let content = pdf_payload(700_000);
    let hash = md5::md5_hex(&content);
    let size = content.len().to_string();

    // —— 1) 预处理：拿到分块大小与临时名 ——
    let response = client
        .post("/aetherupload/preprocess")
        .header(content_type())
        .body(multipart_body(
            &[
                ("resource_name", "report.pdf"),
                ("resource_size", &size),
                ("group", "file"),
                ("resource_hash", &hash),
            ],
            None,
        ))
        .dispatch();

    assert_eq!(response.status(), Status::Ok);
    assert!(
        response
            .headers()
            .get_one("Content-Type")
            .unwrap_or("")
            .starts_with("application/json"),
        "成功体必须是 application/json"
    );

    let body = text(response.into_bytes().expect("预处理应当有响应体"));
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

        let response = client
            .post("/aetherupload/uploading")
            .header(content_type())
            .body(multipart_body(
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
            ))
            .dispatch();

        assert_eq!(response.status(), Status::Ok);
        let body = text(response.into_bytes().expect("上传应当有响应体"));

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
    let response = client
        .get(format!("/aetherupload/display/{saved_path}"))
        .dispatch();

    assert_eq!(response.status(), Status::Ok);
    assert_eq!(
        response.headers().get_one("Content-Type"),
        Some("application/pdf")
    );
    assert_eq!(
        response.into_bytes().expect("展示应当有响应体"),
        content,
        "展示下发的字节要与上传的一致"
    );

    // —— 4) 下载：改文件名（扩展名仍取原资源）——
    let response = client
        .get(format!(
            "/aetherupload/download/{saved_path}/renamed-report"
        ))
        .dispatch();

    assert_eq!(response.status(), Status::Ok);

    let disposition = response
        .headers()
        .get_one("Content-Disposition")
        .expect("下载必须带 Content-Disposition")
        .to_string();

    assert!(disposition.starts_with("attachment"), "实际: {disposition}");
    assert!(
        disposition.contains("renamed-report.pdf"),
        "下载名应当是客户端给的名字 + 原扩展名：{disposition}"
    );
    assert_eq!(
        response.into_bytes().expect("下载应当有响应体"),
        content,
        "下载的字节要与上传的一致"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// 前端脚本的 `preprocess` 发的是 urlencoded（zepto 对普通对象 `data` 的默认编码），
/// 不是 multipart —— rocket 的 `Form<T>` 两种编码都收，这条把它钉住（其余适配层
/// 按 `Content-Type` 分流到同一个内核解析）。
#[test]
fn urlencoded_preprocess_matches_the_bundled_frontend() {
    let root = std::env::temp_dir().join(format!(
        "aetherupload-rocket-urlencoded-{}",
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

    let client = Client::tracked(mount(rocket::build(), runtime)).expect("rocket 应当能点火");

    let response = client
        .post("/aetherupload/preprocess")
        .header(Header::new(
            "Content-Type",
            "application/x-www-form-urlencoded; charset=UTF-8",
        ))
        .body(
            "resource_name=%E4%B8%AD%E6%96%87.txt&resource_size=12&group=file\
             &resource_hash=d41d8cd98f00b204e9800998ecf8427e&locale=zh",
        )
        .dispatch();

    assert_eq!(response.status(), Status::Ok);
    let body = text(response.into_bytes().expect("响应体"));
    assert_eq!(json_field(&body, "chunk_size"), "1000000");
    assert_eq!(json_field(&body, "resource_ext"), "txt");

    let _ = std::fs::remove_dir_all(&root);
}

/// 失败路径矩阵：每个失败都必须是「正确状态码 + problem+json + 稳定 code」。
///
/// rocket 特有的一条：表单缺字段时 rocket 自己的 `Form` 判定会给出 422，
/// 适配层必须改写成内核的 400 + `invalid_parameter`，不许把框架码透出去。
#[test]
fn problem_responses_carry_the_2_0_shape() {
    let root = std::env::temp_dir().join(format!(
        "aetherupload-rocket-problems-{}",
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

    let client = Client::tracked(mount(rocket::build(), runtime)).expect("rocket 应当能点火");

    // 参数缺失 → 400 invalid_parameter（1.x 是 200 + error 字段）
    let response = client
        .post("/aetherupload/preprocess")
        .header(content_type())
        .body(multipart_body(&[("resource_name", "x.txt")], None))
        .dispatch();
    assert_problem(response, Status::BadRequest, "invalid_parameter");

    // 未配置的分组 → 400 invalid_group
    let response = client
        .post("/aetherupload/preprocess")
        .header(content_type())
        .body(multipart_body(
            &[
                ("resource_name", "x.txt"),
                ("resource_size", "3"),
                ("group", "nope"),
            ],
            None,
        ))
        .dispatch();
    assert_problem(response, Status::BadRequest, "invalid_group");

    // 没预处理直接打分块 → 409 upload_not_prepared
    let response = send_chunk(&client, 1, 1, "0123456789abcdef", "202610", "", b"orphan");
    assert_problem(response, Status::Conflict, "upload_not_prepared");

    // 预处理后：跳号与重发已落块都是 409 chunk_out_of_order + expected_index
    let content = b"0123456789abcdefghij"; // 20 字节 → 按 7 切成 3 块
    let hash = md5::md5_hex(content);
    let response = client
        .post("/aetherupload/preprocess")
        .header(content_type())
        .body(multipart_body(
            &[
                ("resource_name", "seq.txt"),
                ("resource_size", &content.len().to_string()),
                ("group", "file"),
                ("resource_hash", &hash),
            ],
            None,
        ))
        .dispatch();
    assert_eq!(response.status(), Status::Ok);

    let body = text(response.into_bytes().expect("响应体"));
    let temp_base_name = json_field(&body, "resource_temp_base_name");
    let group_sub_dir = json_field(&body, "group_sub_dir");
    let chunks: Vec<&[u8]> = content.chunks(7).collect();
    let total = chunks.len();

    let response = send_chunk(
        &client,
        1,
        total,
        &temp_base_name,
        &group_sub_dir,
        &hash,
        chunks[0],
    );
    assert_eq!(response.status(), Status::Ok);

    // 跳号（跳过第 2 块直接发第 3 块）→ expected_index 指回缺口
    let response = send_chunk(
        &client,
        3,
        total,
        &temp_base_name,
        &group_sub_dir,
        &hash,
        chunks[2],
    );
    assert_out_of_order(response, 2);

    // 重发已落块（第 1 块）→ 同样 409，expected_index 不变
    let response = send_chunk(
        &client,
        1,
        total,
        &temp_base_name,
        &group_sub_dir,
        &hash,
        chunks[0],
    );
    assert_out_of_order(response, 2);

    // 补齐缺口后照常完成：拒绝没有破坏已落进度
    for index in 2..=total {
        let response = send_chunk(
            &client,
            index,
            total,
            &temp_base_name,
            &group_sub_dir,
            &hash,
            chunks[index - 1],
        );
        assert_eq!(response.status(), Status::Ok, "第 {index} 块");

        if index == total {
            let body = text(response.into_bytes().expect("响应体"));
            assert!(!json_field(&body, "saved_path").is_empty(), "{body}");
        }
    }

    // 不存在的资源 → 404 resource_not_found（problem+json，不再是纯文本）
    let response = client
        .get("/aetherupload/display/file_202610_missing.txt")
        .dispatch();
    assert_problem(response, Status::NotFound, "resource_not_found");

    // 两种编码都不认的 content-type：适配层自己的解析失败 → 400 invalid_request_body
    let response = client
        .post("/aetherupload/preprocess")
        .header(Header::new("Content-Type", "application/json"))
        .body("{}")
        .dispatch();
    assert_problem(response, Status::BadRequest, "invalid_request_body");

    // 被截断的 multipart（收尾边界在中间断掉）：rocket 自己解析就失败，
    // 适配层按 rocket 的 `ErrorKind::Multipart` 分流 → 400 invalid_request_body
    let mut truncated = multipart_body(&[("resource_name", "x.txt")], None);
    truncated.truncate(truncated.len() - 12);

    let response = client
        .post("/aetherupload/preprocess")
        .header(content_type())
        .body(truncated)
        .dispatch();
    assert_problem(response, Status::BadRequest, "invalid_request_body");

    // TODO(2.0 §11.4)：rocket 的 data-form 限额（= chunk_size + 1MB）被框架拒时的
    // 413 payload_too_large 形态本用例未覆盖 —— 需要构造 >1MB 的 multipart 体。

    let _ = std::fs::remove_dir_all(&root);
}

/// 发一条分块上传请求（字段与完整链路一致）。
fn send_chunk<'c>(
    client: &'c Client,
    index: usize,
    total: usize,
    temp_base_name: &str,
    group_sub_dir: &str,
    hash: &str,
    chunk: &[u8],
) -> LocalResponse<'c> {
    client
        .post("/aetherupload/uploading")
        .header(content_type())
        .body(multipart_body(
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
        ))
        .dispatch()
}

/// 断言一条问题响应：状态码原样 + problem+json + 稳定 code + 非空 title。
/// 返回响应体，方便再断言 `expected_index` 之类的附加成员。
fn assert_problem(response: LocalResponse<'_>, status: Status, code: &str) -> String {
    assert_eq!(response.status(), status);

    let content_type = response.headers().get_one("Content-Type").unwrap_or("");
    assert!(
        content_type.starts_with("application/problem+json"),
        "问题响应必须带 problem+json：{content_type}"
    );

    let body = text(response.into_bytes().expect("问题响应应当有响应体"));
    assert_eq!(json_field(&body, "type"), "about:blank", "{body}");
    assert_eq!(
        json_field(&body, "status"),
        status.code.to_string(),
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
fn assert_out_of_order(response: LocalResponse<'_>, expected_index: usize) {
    let body = assert_problem(response, Status::Conflict, "chunk_out_of_order");
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

fn content_type() -> Header<'static> {
    Header::new(
        "Content-Type",
        format!("multipart/form-data; boundary={BOUNDARY}"),
    )
}

/// 组一个 multipart 请求体（`file` 给 `resource_chunk` 用）。
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

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).expect("响应体是 UTF-8")
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
