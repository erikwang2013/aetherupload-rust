// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 原生 Rust（Guard 请求守卫）端到端：**不经过任何 Web 框架**。
//!
//! 这里模拟一个手写服务器：拿到「方法 + 路径 + 表单」后，交给 [`Guard`] 判定路由、
//! 调对应入口、按返回的状态码与响应头回包。八个框架适配器做的事与此完全一致，
//! 区别只在于参数是从哪个框架的请求对象里取出来的。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aetherupload::md5::md5_hex;
use aetherupload::{ChunkBody, Config, FormData, Guard, GuardRoute, ResourceResponse, Runtime};

/// 模拟的「一条请求」：方法 + 路径 + 表单字段（文本与分块）。
struct Request {
    method: &'static str,
    path: String,
    form: FormData,
}

/// 模拟的「一条响应」：状态码 + 响应头 + 响应体。
struct Response {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

/// 手写服务器的一行分发：Guard 判定路由 → 调入口 → 拼响应。
/// 这就是「原生 Rust 接入」的全部接线代码。
fn serve(guard: &Guard, request: Request) -> Response {
    match guard.routes().classify(request.method, &request.path) {
        Some(GuardRoute::Preprocess) => {
            let json = guard.preprocess(&request.form);
            json_response(json)
        }
        Some(GuardRoute::Uploading) => {
            let json = guard.save_chunk(&request.form);
            json_response(json)
        }
        Some(GuardRoute::Display { saved_path })
        | Some(GuardRoute::Download { saved_path, .. })
            if request.method == "GET" =>
        {
            let resource_response = if let Some(GuardRoute::Download { new_name, .. }) =
                guard.routes().classify(request.method, &request.path)
            {
                guard.download(&saved_path, &new_name)
            } else {
                guard.display(&saved_path)
            };

            resource_response_to_http(resource_response)
        }
        _ => Response {
            status: 404,
            headers: HashMap::new(),
            body: b"not found".to_vec(),
        },
    }
}

fn json_response(json: aetherupload::JsonBody) -> Response {
    Response {
        status: json.status,
        headers: HashMap::from([("Content-Type".to_string(), json.content_type.to_string())]),
        body: json.into_bytes(),
    }
}

fn resource_response_to_http(response: ResourceResponse) -> Response {
    let status = response.status();
    let mut headers: HashMap<String, String> = response.headers().into_iter().collect();

    let body = match response {
        ResourceResponse::NotFound(text) => text.as_bytes().to_vec(),
        ResourceResponse::ServeFile {
            path,
            download_name,
        } => {
            // 附件头由宿主按 `download_name` 生成（八个框架适配器同样在这一步生成它）
            if let Some(name) = download_name {
                headers.insert(
                    "Content-Disposition".to_string(),
                    aetherupload::controller::attachment_disposition(&name),
                );
            }

            // Content-Type 由扩展名反查（与内核校验用的是同一张表）
            if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
                headers.insert(
                    "Content-Type".to_string(),
                    aetherupload::mime::mime_for_extension(ext, &[]),
                );
            }

            std::fs::read(path).unwrap_or_default()
        }
        ResourceResponse::Redirect { .. } | ResourceResponse::AccelRedirect { .. } => Vec::new(),
    };

    Response {
        status,
        headers,
        body,
    }
}

fn project(tag: &str) -> (PathBuf, Guard) {
    let base =
        std::env::temp_dir().join(format!("aetherupload-native-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);

    let runtime = Arc::new(Runtime::new(Config::default(), &base).expect("运行时装配"));

    let mut lines = Vec::new();
    assert_eq!(
        aetherupload::console::list_groups(&runtime, &mut |line| lines.push(line.to_string())),
        0,
        "{lines:?}"
    );

    (base, Guard::new(runtime))
}

/// 从 JSON 响应体里取字段（不引解析器，字段集固定）。
fn json_value<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":");
    let rest = &body[body.find(&needle)? + needle.len()..];

    if let Some(quoted) = rest.strip_prefix('"') {
        return Some(&quoted[..quoted.find('"')?]);
    }

    rest.split([',', '}']).next()
}

fn text_form(fields: &[(&str, &str)]) -> FormData {
    FormData::from_pairs(fields.iter().map(|(k, v)| (k.to_string(), v.to_string())))
}

#[test]
fn native_rust_full_flow() {
    let (base, guard) = project("flow");
    let content = b"native rust end to end".repeat(23);
    let hash = md5_hex(&content);

    // 1) 预处理
    let response = serve(
        &guard,
        Request {
            method: "POST",
            path: "/aetherupload/preprocess".to_string(),
            form: text_form(&[
                ("resource_name", "说明.txt"),
                ("resource_size", &content.len().to_string()),
                ("group", "file"),
                ("resource_hash", &hash),
                ("locale", "zh"),
            ]),
        },
    );

    assert_eq!(response.status, 200);
    let body = String::from_utf8(response.body).unwrap();
    assert_eq!(json_value(&body, "error"), Some("0"), "{body}");

    let temp_base = json_value(&body, "resourceTempBaseName")
        .unwrap()
        .to_string();
    let sub_dir = json_value(&body, "groupSubDir").unwrap().to_string();
    let ext = json_value(&body, "resourceExt").unwrap().to_string();

    // 2) 分块（按 13 字节切）
    let chunks: Vec<&[u8]> = content.chunks(13).collect();
    let total = chunks.len();
    let mut last_body = String::new();

    for (index, chunk) in chunks.iter().enumerate() {
        let mut form = text_form(&[
            ("chunk_total", &total.to_string()),
            ("chunk_index", &(index + 1).to_string()),
            ("resource_temp_basename", &temp_base),
            ("resource_ext", &ext),
            ("group_subdir", &sub_dir),
            ("group", "file"),
            ("resource_hash", &hash),
            ("locale", "zh"),
        ]);
        form.push_file("resource_chunk", ChunkBody::Bytes(chunk.to_vec()));

        let response = serve(
            &guard,
            Request {
                method: "POST",
                path: "/aetherupload/uploading".to_string(),
                form,
            },
        );

        assert_eq!(response.status, 200);
        last_body = String::from_utf8(response.body).unwrap();
        assert_eq!(
            json_value(&last_body, "error"),
            Some("0"),
            "第 {} 块失败：{last_body}",
            index + 1
        );
    }

    let saved_path = json_value(&last_body, "savedPath").unwrap().to_string();
    assert!(!saved_path.is_empty(), "{last_body}");

    // 3) 成品落盘（内容逐字节一致）
    let landed = base
        .join("storage/app/aetherupload/file")
        .join(&sub_dir)
        .join(format!("{hash}.txt"));
    assert_eq!(std::fs::read(&landed).unwrap(), content);

    // 4) 展示：带 nosniff，内容是文件本体
    let response = serve(
        &guard,
        Request {
            method: "GET",
            path: format!("/aetherupload/display/{saved_path}"),
            form: FormData::new(),
        },
    );

    assert_eq!(response.status, 200);
    assert_eq!(
        response
            .headers
            .get("X-Content-Type-Options")
            .map(String::as_str),
        Some("nosniff")
    );
    assert_eq!(response.body, content);

    // 5) 下载：附件头（含 RFC 5987 编码）
    let response = serve(
        &guard,
        Request {
            method: "GET",
            path: format!("/aetherupload/download/{saved_path}/新名字"),
            form: FormData::new(),
        },
    );

    assert_eq!(response.status, 200);
    let disposition = response
        .headers
        .get("Content-Disposition")
        .expect("下载必须带 Content-Disposition");
    assert!(
        disposition.contains("%E6%96%B0%E5%90%8D%E5%AD%97.txt"),
        "{disposition}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn native_rust_rejects_bad_requests() {
    let (base, guard) = project("errors");

    // 参数缺失：200 + JSON 错误（与 PHP 一致）
    let response = serve(
        &guard,
        Request {
            method: "POST",
            path: "/aetherupload/preprocess".to_string(),
            form: text_form(&[("resource_name", "x.txt")]),
        },
    );

    assert_eq!(response.status, 200);
    let body = String::from_utf8(response.body).unwrap();
    assert!(
        json_value(&body, "error").unwrap().starts_with("Error"),
        "{body}"
    );

    // 目录穿越：savedPath 解码即拒
    let response = serve(
        &guard,
        Request {
            method: "GET",
            path: "/aetherupload/display/file_..%2F..%2Fetc%2Fpasswd".to_string(),
            form: FormData::new(),
        },
    );
    assert_eq!(response.status, 404);

    // 不属于四条路由的请求：交回宿主
    let response = serve(
        &guard,
        Request {
            method: "GET",
            path: "/healthz".to_string(),
            form: FormData::new(),
        },
    );
    assert_eq!(response.status, 404);

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn guard_holds_only_an_arc_and_is_clonable() {
    let (base, guard) = project("clone");

    // 每请求克隆一份是廉价操作（两次原子计数），八个框架适配器与原生宿主都这么做
    let per_request = guard.clone();
    assert_eq!(per_request.routes().uploading, "/aetherupload/uploading");
    assert!(Path::new(&base).exists());

    let _ = std::fs::remove_dir_all(&base);
}
