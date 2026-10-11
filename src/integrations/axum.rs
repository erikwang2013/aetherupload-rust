// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! Axum 集成：一个 [`Router`]，四条路由一次挂好。
//!
//! 分块走 `Multipart` 提取器读进内存 —— 默认分块 1MB，比落临时文件少一次磁盘往返；
//! 把 `chunk_size` 调得很大时请自行换流式解析。
//!
//! 响应形态（2.0 协议）：两个上传入口成功是 200 + `application/json`（snake_case
//! 上传结果），失败是 RFC 9457 问题响应 —— 内核的 `Problem` 状态码与文案原样透出。
//! 本适配器自己判定的失败也回同一形态，不落回 axum 的纯文本拒绝：坏 multipart /
//! 坏 urlencoded / 缺 `Content-Type` → 400 `invalid_request_body`；撞上
//! `DefaultBodyLimit`（分块超过 [`body_limit`]）→ 413 `payload_too_large`；
//! 定位到文件但打开失败 → 404 `resource_not_found`。
//!
//! `preprocess` 的两种表单编码都收：urlencoded（1.x 既有部署的编码）与 multipart
//! （自带脚本与手工构造的请求）。
//!
//! 本地文件下发用流式响应（`tokio::fs` + `ReaderStream`），不把整份文件读进内存；
//! **不实现 HTTP Range** —— 与 1.x 一致，需要断点下载/视频拖动请开
//! `x_accel_redirect` 交给前置服务器。
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use aetherupload::{Config, Runtime};
//!
//! let runtime = Arc::new(Runtime::new(Config::default(), ".").unwrap());
//! let app = aetherupload::integrations::axum::routes(runtime);
//! // axum::serve(listener, app).await
//! ```

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::multipart::{MultipartError, MultipartRejection};
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tokio_util::io::ReaderStream;

use crate::controller::{ChunkBody, ResourceController, ResourceResponse, UploadController};
use crate::integrations::{
    FormData, ParseFailure, body_limit, display_route, download_route,
    invalid_request_body_response, is_urlencoded, ok_response, problem_response,
    resource_not_found_response,
};
use crate::json::{save_chunk_ok_to_json, upload_ok_to_json};
use crate::runtime::Runtime;

/// 注册四条路由（路径取自配置的 `route_*`）。
pub fn routes(runtime: Arc<Runtime>) -> Router {
    let preprocess_path = runtime.config().route_preprocess.clone();
    let uploading_path = runtime.config().route_uploading.clone();
    let display_path = display_route(&runtime);
    let download_path = download_route(&runtime);
    let limit = body_limit(&runtime);

    Router::new()
        .route(&preprocess_path, post(preprocess))
        .route(&uploading_path, post(save_chunk))
        .route(&display_path, get(display))
        .route(&download_path, get(download))
        // 框架默认的 body 上限比一个分块还小，不放开会先被 axum 自己拒掉
        .layer(DefaultBodyLimit::max(limit))
        .with_state(runtime)
}

async fn preprocess(State(runtime): State<Arc<Runtime>>, request: Request) -> Response {
    let form = match parse_form(request).await {
        Ok(form) => form,
        Err(failure) => return json_response(failure),
    };

    match UploadController::new(runtime).preprocess(&form.to_preprocess_request()) {
        // 成功体：200 + application/json（协议 §2）
        Ok(ok) => json_response(ok_response(upload_ok_to_json(&ok))),
        // 失败体：状态码与文案逐字来自内核（协议 §3）
        Err(problem) => json_response(problem_response(&problem)),
    }
}

async fn save_chunk(
    State(runtime): State<Arc<Runtime>>,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    // boundary 缺失/非法这类提取器拒绝也走问题响应，不用 axum 的纯文本 400
    let Ok(multipart) = multipart else {
        return json_response(invalid_request_body_response(None));
    };

    let form = match parse_multipart(multipart).await {
        Ok(form) => form,
        Err(failure) => return json_response(failure.response()),
    };

    match UploadController::new(runtime).save_chunk(&form.to_save_chunk_request()) {
        Ok(ok) => json_response(ok_response(save_chunk_ok_to_json(&ok))),
        Err(problem) => json_response(problem_response(&problem)),
    }
}

async fn display(State(runtime): State<Arc<Runtime>>, Path(saved_path): Path<String>) -> Response {
    let response = ResourceController::new(runtime).display(&saved_path);

    serve(response).await
}

async fn download(
    State(runtime): State<Arc<Runtime>>,
    Path((saved_path, new_name)): Path<(String, String)>,
) -> Response {
    let response = ResourceController::new(runtime).download(&saved_path, &new_name);

    serve(response).await
}

/// 解析 `preprocess` 的表单：按 `Content-Type` 分流，urlencoded 与 multipart 都收。
///
/// 失败直接给出问题响应三元组（与内核失败同一形态）。
async fn parse_form(request: Request) -> Result<FormData, (u16, &'static str, String)> {
    let urlencoded = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_urlencoded);

    if !urlencoded {
        let Ok(multipart) = Multipart::from_request(request, &()).await else {
            // 缺 Content-Type 或 boundary 非法：请求体无法解析
            return Err(invalid_request_body_response(None));
        };

        return parse_multipart(multipart)
            .await
            .map_err(|failure| failure.response());
    }

    // 用 `Bytes` 提取器而不是 `body::to_bytes`：上限同样来自路由器的 `DefaultBodyLimit`
    // （[`body_limit`]），但它把「超限」（`LengthLimitError`，413）与「读失败」
    // （`UnknownBodyError`，400）分成两个变体 —— 状态码就能如实映射，不必搅在一起
    let bytes = match Bytes::from_request(request, &()).await {
        Ok(bytes) => bytes,
        Err(err) => {
            return Err(ParseFailure::new(
                FormData::new(),
                err.status() == StatusCode::PAYLOAD_TOO_LARGE,
            )
            .response());
        }
    };

    Ok(FormData::from_urlencoded(&String::from_utf8_lossy(&bytes)))
}

/// 把 multipart 表单读成 [`FormData`]（分块字段读进内存）。
///
/// 失败带着解析到一半的表单回来 —— 能取到 `locale` 就按它本地化问题标题。
async fn parse_multipart(mut multipart: Multipart) -> Result<FormData, ParseFailure> {
    let mut form = FormData::new();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => return Ok(form),
            Err(err) => return Err(parse_failure(form, &err)),
        };

        let name = field.name().unwrap_or_default().to_string();

        if name == "resource_chunk" {
            match field.bytes().await {
                Ok(bytes) => form.push_file(name, ChunkBody::Bytes(bytes.to_vec())),
                Err(err) => return Err(parse_failure(form, &err)),
            }
        } else {
            match field.text().await {
                Ok(value) => form.push_text(name, value),
                Err(err) => return Err(parse_failure(form, &err)),
            }
        }
    }
}

/// multipart 读失败 → [`ParseFailure`]（413 归「超限」，其余归「坏体」）。
fn parse_failure(form: FormData, err: &MultipartError) -> ParseFailure {
    ParseFailure::new(form, err.status() == StatusCode::PAYLOAD_TOO_LARGE)
}

/// 内核响应 → axum 响应。
pub async fn serve(response: ResourceResponse) -> Response {
    let headers = response.headers();

    match response {
        // 失败一律问题响应：状态码与文案原样来自内核（协议 §3）
        ResourceResponse::Problem(problem) => json_response(problem_response(&problem)),

        ResourceResponse::Redirect { location } => {
            http_response(StatusCode::FOUND, headers, Body::empty(), location)
        }

        ResourceResponse::AccelRedirect { .. } => {
            http_response(StatusCode::OK, headers, Body::empty(), String::new())
        }

        ResourceResponse::ServeFile {
            path,
            download_name,
        } => {
            // 寻址已通过但打开失败（文件刚被删/权限）：仍按 404 resource_not_found 回
            let Ok(file) = tokio::fs::File::open(&path).await else {
                return json_response(resource_not_found_response());
            };

            let mut builder = Response::builder().status(StatusCode::OK);

            for (name, value) in headers {
                builder = builder.header(name, value);
            }

            if let Some(content_type) = content_type_for(&path) {
                builder = builder.header(header::CONTENT_TYPE, content_type);
            }

            if let Some(name) = download_name {
                builder = builder.header(
                    header::CONTENT_DISPOSITION,
                    crate::controller::attachment_disposition(&name),
                );
            }

            builder
                .body(Body::from_stream(ReaderStream::new(file)))
                .unwrap_or_else(|_| {
                    (StatusCode::INTERNAL_SERVER_ERROR, "response error").into_response()
                })
        }
    }
}

/// 三元组（状态码 + Content-Type + 体）→ axum 响应。
fn json_response((status, content_type, body): (u16, &'static str, String)) -> Response {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        [(header::CONTENT_TYPE, content_type)],
        body,
    )
        .into_response()
}

fn http_response(
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: Body,
    location: String,
) -> Response {
    let mut builder = Response::builder().status(status);

    if !location.is_empty() {
        builder = builder.header(header::LOCATION, location);
    }

    for (name, value) in headers {
        builder = builder.header(name, value);
    }

    builder
        .body(body)
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "response error").into_response())
}

/// 由扩展名推断 Content-Type（走内核的 MIME 表，与上传校验同一张）。
fn content_type_for(path: &std::path::Path) -> Option<String> {
    let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
    let mime = crate::mime::mime_for_extension(&extension, &[]);

    // 表里查不到时退回 octet-stream 没有意义 —— 交给浏览器自己嗅探
    (mime != "application/octet-stream").then_some(mime)
}
