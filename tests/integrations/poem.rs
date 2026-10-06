// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! Poem 适配层的端到端测试：预处理 → 分两块上传（续传协议）→ 展示 / 下载。
//!
//! 全程走 [`poem::test::TestClient`]，不起真实端口；`Runtime` 指向临时目录。

use std::path::PathBuf;
use std::sync::Arc;

use aetherupload::integrations::poem::routes;
use aetherupload::md5::md5_hex;
use aetherupload::{Config, Runtime};
use poem::test::{TestClient, TestForm, TestFormField};

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

    // 分两块传，覆盖「中间块返回空 savedPath + 末块落盘」的分支
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

    let json = response.0.into_body().into_string().await.unwrap();
    assert_eq!(json_field(&json, "error"), "0", "预处理应当成功：{json}");

    let chunk_size: u64 = json_field(&json, "chunkSize").parse().expect("chunkSize");
    let temp_base_name = json_field(&json, "resourceTempBaseName");
    let group_sub_dir = json_field(&json, "groupSubDir");
    let resource_ext = json_field(&json, "resourceExt");

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
    assert_eq!(json_field(&json, "error"), "0", "首块应当成功：{json}");
    assert_eq!(json_field(&json, "savedPath"), "", "非末块不返回 savedPath");

    let response = client
        .post(&uploading_path)
        .multipart(chunk_body("2", second))
        .send()
        .await;
    response.assert_status_is_ok();

    let json = response.0.into_body().into_string().await.unwrap();
    assert_eq!(json_field(&json, "error"), "0", "末块应当成功：{json}");

    let saved_path = json_field(&json, "savedPath");
    assert!(!saved_path.is_empty(), "末块必须返回 savedPath");
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

    // —— 5) 参数缺失 / 资源不存在 ——
    let response = client
        .post(&preprocess_path)
        .multipart(TestForm::new().text("group", "file"))
        .send()
        .await;
    response.assert_status_is_ok();

    let json = response.0.into_body().into_string().await.unwrap();
    assert_ne!(
        json_field(&json, "error"),
        "0",
        "缺必填字段应当报错：{json}"
    );

    let response = client
        .get(format!(
            "{}/file_202610_missing.txt",
            runtime.config().route_display
        ))
        .send()
        .await;
    response.assert_status(poem::http::StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(&root);
}
