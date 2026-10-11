// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! Poem 适配层的端到端测试：预处理 → 分两块上传（续传协议）→ 展示 / 下载。
//!
//! 全程走 [`poem::test::TestClient`]，不起真实端口；`Runtime` 指向临时目录。

use std::path::PathBuf;
use std::sync::Arc;

use aetherupload::integrations::poem::routes;
use aetherupload::md5::md5_hex;
use aetherupload::{Config, Runtime};
use poem::test::{TestClient, TestForm, TestFormField, TestResponse};

/// 建临时根目录并把**分组目录**建好 —— 内核建子目录用的是非递归 `mkdir`，
/// 分组目录缺失时第一个分块就会以「建子目录失败」收场。真实部署里这步由
/// `aetherupload groups` 完成，这里直接调同一个函数。
fn setup(tag: &str) -> (PathBuf, Arc<Runtime>) {
    let root = std::env::temp_dir().join(format!("aetherupload-poem-{tag}-{}", std::process::id()));
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

#[tokio::test]
async fn upload_display_download_roundtrip() {
    let (root, runtime) = setup("roundtrip");

    let preprocess_path = runtime.config().route_preprocess.clone();
    let uploading_path = runtime.config().route_uploading.clone();

    let client = TestClient::new(routes(Arc::clone(&runtime)));

    // 分两块传，覆盖「中间块返回空 saved_path + 末块落盘」的分支
    let content = b"hello aetherupload, chunked world!".to_vec();
    let (first, second) = content.split_at(11);
    let hash = md5_hex(&content);

    // —— 1) 预处理 ——
    let response = client
        .post(&preprocess_path)
        .multipart(
            TestForm::new()
                .text("resource_name", "a.txt")
                .text("resource_size", content.len().to_string())
                .text("group", "file")
                .text("resource_hash", hash.as_str()),
        )
        .send()
        .await;
    response.assert_status_is_ok();

    assert!(
        response
            .0
            .content_type()
            .unwrap_or("")
            .starts_with("application/json"),
        "成功体必须是 application/json"
    );

    let json = response.0.into_body().into_string().await.unwrap();
    let chunk_size: u64 = json_field(&json, "chunk_size").parse().expect("chunk_size");
    let temp_base_name = json_field(&json, "resource_temp_base_name");
    let group_sub_dir = json_field(&json, "group_sub_dir");
    let resource_ext = json_field(&json, "resource_ext");

    assert_eq!(chunk_size, 1_000_000);
    assert!(!temp_base_name.is_empty());
    assert_eq!(group_sub_dir.len(), 6, "默认按月：YYYYMM");
    assert_eq!(resource_ext, "txt");

    // —— 2) 分块上传 ——
    let chunk_body = |index: &str, bytes: &[u8]| {
        TestForm::new()
            .text("chunk_total", "2")
            .text("chunk_index", index)
            .text("resource_temp_basename", temp_base_name.as_str())
            .text("resource_ext", resource_ext.as_str())
            .text("group_subdir", group_sub_dir.as_str())
            .text("group", "file")
            .text("resource_hash", hash.as_str())
            .field(
                TestFormField::bytes(bytes.to_vec())
                    .name("resource_chunk")
                    .filename("chunk"),
            )
    };

    let response = client
        .post(&uploading_path)
        .multipart(chunk_body("1", first))
        .send()
        .await;
    response.assert_status_is_ok();

    let json = response.0.into_body().into_string().await.unwrap();
    assert_eq!(
        json_field(&json, "saved_path"),
        "",
        "非末块不返回 saved_path"
    );

    let response = client
        .post(&uploading_path)
        .multipart(chunk_body("2", second))
        .send()
        .await;
    response.assert_status_is_ok();

    let json = response.0.into_body().into_string().await.unwrap();

    let saved_path = json_field(&json, "saved_path");
    assert!(!saved_path.is_empty(), "末块必须返回 saved_path");
    assert_eq!(saved_path, format!("file_{group_sub_dir}_{hash}.txt"));

    // 成品落在 <root>/storage/app/aetherupload/file/<subdir>/<md5>.txt，内容一字不差
    let landed = root
        .join("storage/app/aetherupload/file")
        .join(&group_sub_dir)
        .join(format!("{hash}.txt"));
    assert_eq!(std::fs::read(&landed).unwrap(), content, "落盘内容应当一致");

    // —— 3) 展示 ——
    let response = client
        .get(format!("{}/{}", runtime.config().route_display, saved_path))
        .send()
        .await;
    response.assert_status_is_ok();
    assert_eq!(
        response.0.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(
        response.0.headers().get("content-type").unwrap(),
        "text/plain"
    );

    let served = response.0.into_body().into_bytes().await.unwrap();
    assert_eq!(served.as_ref(), content.as_slice());

    // —— 4) 下载（改文件名，扩展名仍是原资源的）——
    let response = client
        .get(format!(
            "{}/{}/report",
            runtime.config().route_download,
            saved_path
        ))
        .send()
        .await;
    response.assert_status_is_ok();

    let disposition = response
        .0
        .headers()
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(disposition.starts_with("attachment;"), "{disposition}");
    assert!(disposition.contains("report.txt"), "{disposition}");

    let served = response.0.into_body().into_bytes().await.unwrap();
    assert_eq!(served.as_ref(), content.as_slice());

    let _ = std::fs::remove_dir_all(&root);
}

/// 失败路径矩阵：每个失败都必须是「正确状态码 + problem+json + 稳定 code」。
#[tokio::test]
async fn problem_responses_carry_the_2_0_shape() {
    let (root, runtime) = setup("problems");

    let preprocess_path = runtime.config().route_preprocess.clone();
    let uploading_path = runtime.config().route_uploading.clone();
    let display_path = runtime.config().route_display.clone();

    let client = TestClient::new(routes(Arc::clone(&runtime)));

    // 参数缺失 → 400 invalid_parameter（1.x 是 200 + error 字段）
    let response = client
        .post(&preprocess_path)
        .multipart(TestForm::new().text("resource_name", "x.txt"))
        .send()
        .await;
    assert_problem(response, poem::http::StatusCode::BAD_REQUEST, "invalid_parameter").await;

    // 未配置的分组 → 400 invalid_group
    let response = client
        .post(&preprocess_path)
        .multipart(
            TestForm::new()
                .text("resource_name", "x.txt")
                .text("resource_size", "3")
                .text("group", "nope"),
        )
        .send()
        .await;
    assert_problem(response, poem::http::StatusCode::BAD_REQUEST, "invalid_group").await;

    // 没预处理直接打分块 → 409 upload_not_prepared
    let response = client
        .post(&uploading_path)
        .multipart(chunk_form(1, 1, "0123456789abcdef", "202610", "", b"orphan"))
        .send()
        .await;
    assert_problem(response, poem::http::StatusCode::CONFLICT, "upload_not_prepared").await;

    // 预处理后：跳号与重发已落块都是 409 chunk_out_of_order + expected_index
    let content = b"0123456789abcdefghij"; // 20 字节 → 按 7 切成 3 块
    let hash = md5_hex(content);
    let response = client
        .post(&preprocess_path)
        .multipart(
            TestForm::new()
                .text("resource_name", "seq.txt")
                .text("resource_size", content.len().to_string())
                .text("group", "file")
                .text("resource_hash", hash.as_str()),
        )
        .send()
        .await;
    response.assert_status_is_ok();

    let json = response.0.into_body().into_string().await.unwrap();
    let temp_base_name = json_field(&json, "resource_temp_base_name");
    let group_sub_dir = json_field(&json, "group_sub_dir");
    let chunks: Vec<&[u8]> = content.chunks(7).collect();
    let total = chunks.len();

    let response = client
        .post(&uploading_path)
        .multipart(chunk_form(
            1,
            total,
            &temp_base_name,
            &group_sub_dir,
            &hash,
            chunks[0],
        ))
        .send()
        .await;
    response.assert_status_is_ok();

    // 跳号（跳过第 2 块直接发第 3 块）→ expected_index 指回缺口
    let response = client
        .post(&uploading_path)
        .multipart(chunk_form(
            3,
            total,
            &temp_base_name,
            &group_sub_dir,
            &hash,
            chunks[2],
        ))
        .send()
        .await;
    assert_out_of_order(response, 2).await;

    // 重发已落块（第 1 块）→ 同样 409，expected_index 不变
    let response = client
        .post(&uploading_path)
        .multipart(chunk_form(
            1,
            total,
            &temp_base_name,
            &group_sub_dir,
            &hash,
            chunks[0],
        ))
        .send()
        .await;
    assert_out_of_order(response, 2).await;

    // 补齐缺口后照常完成：拒绝没有破坏已落进度
    for index in 2..=total {
        let response = client
            .post(&uploading_path)
            .multipart(chunk_form(
                index,
                total,
                &temp_base_name,
                &group_sub_dir,
                &hash,
                chunks[index - 1],
            ))
            .send()
            .await;
        response.assert_status_is_ok();

        if index == total {
            let json = response.0.into_body().into_string().await.unwrap();
            assert!(!json_field(&json, "saved_path").is_empty(), "{json}");
        }
    }

    // 不存在的资源 → 404 resource_not_found（problem+json，不再是纯文本）
    let response = client
        .get(format!("{display_path}/file_202610_missing.txt"))
        .send()
        .await;
    assert_problem(response, poem::http::StatusCode::NOT_FOUND, "resource_not_found").await;

    // 两种编码都不认的 content-type：适配层自己的解析失败 → 400 invalid_request_body
    let response = client
        .post(&preprocess_path)
        .content_type("application/json")
        .body("{}")
        .send()
        .await;
    assert_problem(response, poem::http::StatusCode::BAD_REQUEST, "invalid_request_body").await;

    // TODO(2.0 §11.4)：框架层 body 上限（= chunk_size + 1MB）被拒时的 413 payload_too_large
    // 本层未覆盖 —— 需构造 >1MB 的 multipart 体并确认 poem 的限额拒绝被 catcher 统一；
    // axum 与 warp 两层已各钉一条。

    let _ = std::fs::remove_dir_all(&root);
}

/// 组一条分块上传的 multipart 表单（字段与完整链路一致）。
fn chunk_form(
    index: usize,
    total: usize,
    temp_base_name: &str,
    group_sub_dir: &str,
    hash: &str,
    bytes: &[u8],
) -> TestForm {
    TestForm::new()
        .text("chunk_total", total.to_string())
        .text("chunk_index", index.to_string())
        .text("resource_temp_basename", temp_base_name.to_string())
        .text("resource_ext", "txt".to_string())
        .text("group_subdir", group_sub_dir.to_string())
        .text("group", "file".to_string())
        .text("resource_hash", hash.to_string())
        .field(
            TestFormField::bytes(bytes.to_vec())
                .name("resource_chunk")
                .filename("chunk"),
        )
}

/// 断言一条问题响应：状态码原样 + problem+json + 稳定 code + 非空 title。
/// 返回响应体，方便再断言 `expected_index` 之类的附加成员。
async fn assert_problem(
    response: TestResponse,
    status: poem::http::StatusCode,
    code: &str,
) -> String {
    assert_eq!(response.0.status(), status);

    let content_type = response.0.content_type().unwrap_or("").to_string();
    assert!(
        content_type.starts_with("application/problem+json"),
        "问题响应必须带 problem+json：{content_type}"
    );

    let json = response.0.into_body().into_string().await.unwrap();
    assert_eq!(json_field(&json, "type"), "about:blank", "{json}");
    assert_eq!(
        json_field(&json, "status"),
        status.as_u16().to_string(),
        "{json}"
    );
    assert_eq!(json_field(&json, "code"), code, "{json}");
    assert!(
        !json_field(&json, "title").is_empty(),
        "title 必须非空：{json}"
    );

    json
}

/// 409 chunk_out_of_order：在问题响应基础上再断言 expected_index 指向缺口。
async fn assert_out_of_order(response: TestResponse, expected_index: usize) {
    let json = assert_problem(response, poem::http::StatusCode::CONFLICT, "chunk_out_of_order").await;
    assert_eq!(
        json_field(&json, "expected_index"),
        expected_index.to_string(),
        "{json}"
    );
}

/// 前端脚本发的是 urlencoded —— zepto 对普通对象 `data` 的默认编码，不是 multipart。
/// 请求体按 `aetherupload-all.js` 里 `preprocess` 的原样拼（含中文名的 percent 编码）。
#[tokio::test]
async fn urlencoded_preprocess_matches_the_bundled_frontend() {
    let (root, runtime) = setup("urlencoded");
    let preprocess_path = runtime.config().route_preprocess.clone();

    let client = TestClient::new(routes(runtime));

    let response = client
        .post(&preprocess_path)
        .content_type("application/x-www-form-urlencoded; charset=UTF-8")
        .body(
            "resource_name=%E4%B8%AD%E6%96%87.txt&resource_size=12&group=file\
             &resource_hash=d41d8cd98f00b204e9800998ecf8427e&locale=zh",
        )
        .send()
        .await;
    response.assert_status_is_ok();

    let json = response.0.into_body().into_string().await.unwrap();
    assert_eq!(json_field(&json, "chunk_size"), "1000000");
    assert_eq!(json_field(&json, "resource_ext"), "txt");

    let _ = std::fs::remove_dir_all(&root);
}
