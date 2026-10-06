// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! Axum 集成：一个 [`Router`]，四条路由一次挂好。
//!
//! 分块走 `Multipart` 提取器读进内存 —— 默认分块 1MB，比 PHP 落临时文件少一次
//! 磁盘往返；把 `chunk_size` 调得很大时请自行换流式解析。
//!
//! 本地文件下发用流式响应（`tokio::fs` + `ReaderStream`），不把整份文件读进内存；
//! **不实现 HTTP Range** —— 与 PHP/webman 版一致，需要断点下载/视频拖动请开
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
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Multipart, Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use tokio_util::io::ReaderStream;

use crate::controller::{ChunkBody, ResourceController, ResourceResponse, UploadController};
use crate::integrations::{FormData, body_limit, display_route, download_route};
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

async fn preprocess(State(runtime): State<Arc<Runtime>>, multipart: Multipart) -> Response {
    let Ok(form) = parse_multipart(multipart).await else {
        return (StatusCode::BAD_REQUEST, "invalid multipart body").into_response();
    };

    let result = UploadController::new(runtime).preprocess(&form.to_preprocess_request());

    (StatusCode::OK, result.to_json()).into_response()
}

async fn save_chunk(State(runtime): State<Arc<Runtime>>, multipart: Multipart) -> Response {
    let Ok(form) = parse_multipart(multipart).await else {
        return (StatusCode::BAD_REQUEST, "invalid multipart body").into_response();
    };

    let result = UploadController::new(runtime).save_chunk(&form.to_save_chunk_request());

    (StatusCode::OK, result.to_json()).into_response()
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

/// 把 multipart 表单读成 [`FormData`]（分块字段读进内存）。
async fn parse_multipart(mut multipart: Multipart) -> Result<FormData, String> {
    let mut form = FormData::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| err.to_string())?
    {
        let name = field.name().unwrap_or_default().to_string();

        if name == "resource_chunk" {
            let bytes = field.bytes().await.map_err(|err| err.to_string())?;
            form.push_file(name, ChunkBody::Bytes(bytes.to_vec()));
        } else {
            let value = field.text().await.map_err(|err| err.to_string())?;
            form.push_text(name, value);
        }
    }

    Ok(form)
}

/// 内核响应 → axum 响应。
pub async fn serve(response: ResourceResponse) -> Response {
    let headers = response.headers();

    match response {
        ResourceResponse::NotFound(body) => (StatusCode::NOT_FOUND, body).into_response(),

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
            let Ok(file) = tokio::fs::File::open(&path).await else {
                return (StatusCode::NOT_FOUND, "not found").into_response();
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

/// 由扩展名推断 Content-Type（走内核的 MIME 表，与 PHP 的文件响应同一张表）。
fn content_type_for(path: &std::path::Path) -> Option<String> {
    let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
    let mime = crate::mime::mime_for_extension(&extension, &[]);

    // 表里查不到时退回 octet-stream 没有意义 —— 交给浏览器自己嗅探
    (mime != "application/octet-stream").then_some(mime)
}
