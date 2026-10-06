// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! Poem 集成：一个 [`Route`]，四条路由一次挂好。
//!
//! 分块走 `Multipart` 提取器读进内存 —— 默认分块 1MB，比 PHP 落临时文件少一次
//! 磁盘往返；把 `chunk_size` 调得很大时请自行换流式解析。
//!
//! 本地文件下发用流式响应（`tokio::fs` + `ReaderStream`），不把整份文件读进内存；
//! **不实现 HTTP Range** —— 与 PHP/webman 版一致，需要断点下载/视频拖动请开
//! `x_accel_redirect` 交给前置服务器。
//!
//! # 两处与 axum 适配层的差异
//!
//! **路径参数语法**：config 里的 `route_display` / `route_download` 是 axum 风格的
//! `{saved_path}`，poem 用的是 `:saved_path`，[`routes`] 里统一改写。
//!
//! **请求体上限**：poem 3 对请求体**没有默认上限**（axum 的 `DefaultBodyLimit` 是
//! 2MB，actix 是 256KB，poem 没有对应物），所以这里不存在「放开」这一步 ——
//! 一次分块原样收下。需要显式设限时，用 poem 自带的 `SizeLimit` 中间件挂在
//! **两条 POST 路由**上（它要求请求带 `Content-Length`，否则回 411）：
//!
//! ```text
//! let limit = aetherupload::integrations::body_limit(&runtime);
//! Route::new()
//!     .at(&preprocess_path, post(preprocess).with(SizeLimit::new(limit)))
//!     .at(&uploading_path,  post(save_chunk).with(SizeLimit::new(limit)))
//!     // …display / download 不带上限：GET 没有请求体，挂了会一律 411
//! ```
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use aetherupload::{Config, Runtime};
//!
//! let runtime = Arc::new(Runtime::new(Config::default(), ".").unwrap());
//! let app = aetherupload::integrations::poem::routes(runtime);
//! // poem::Server::new(TcpListener::bind("0.0.0.0:8080")).run(app).await
//! ```

use std::sync::Arc;

use poem::http::{StatusCode, header};
use poem::web::{Data, Multipart, Path};
use poem::{Body, EndpointExt, Response, Route, get, handler, post};
use tokio_util::io::ReaderStream;

use crate::controller::{ChunkBody, ResourceController, ResourceResponse, UploadController};
use crate::integrations::{FormData, display_route, download_route};
use crate::runtime::Runtime;

/// 注册四条路由（路径取自配置的 `route_*`）。
pub fn routes(runtime: Arc<Runtime>) -> Route {
    let preprocess_path = runtime.config().route_preprocess.clone();
    let uploading_path = runtime.config().route_uploading.clone();
    let display_path = poem_path(&display_route(&runtime));
    let download_path = poem_path(&download_route(&runtime));

    // 四条路由共用同一个 runtime（poem 的 `Data` 提取器）。`.data()` 挂在单条路由上，
    // 返回类型才保持是 `Route`（挂在 `Route::new()` 上会变成 `AddDataEndpoint<Route, _>`）。
    Route::new()
        .at(preprocess_path, post(preprocess).data(runtime.clone()))
        .at(uploading_path, post(save_chunk).data(runtime.clone()))
        .at(display_path, get(display).data(runtime.clone()))
        .at(download_path, get(download).data(runtime))
}

#[handler]
async fn preprocess(Data(runtime): Data<&Arc<Runtime>>, multipart: Multipart) -> Response {
    let Ok(form) = parse_multipart(multipart).await else {
        return bad_request("invalid multipart body");
    };

    let result = UploadController::new(runtime.clone()).preprocess(&form.to_preprocess_request());

    Response::builder().body(result.to_json())
}

#[handler]
async fn save_chunk(Data(runtime): Data<&Arc<Runtime>>, multipart: Multipart) -> Response {
    let Ok(form) = parse_multipart(multipart).await else {
        return bad_request("invalid multipart body");
    };

    let result = UploadController::new(runtime.clone()).save_chunk(&form.to_save_chunk_request());

    Response::builder().body(result.to_json())
}

#[handler]
async fn display(Data(runtime): Data<&Arc<Runtime>>, Path(saved_path): Path<String>) -> Response {
    let response = ResourceController::new(runtime.clone()).display(&saved_path);

    serve(response).await
}

#[handler]
async fn download(
    Data(runtime): Data<&Arc<Runtime>>,
    Path((saved_path, new_name)): Path<(String, String)>,
) -> Response {
    let response = ResourceController::new(runtime.clone()).download(&saved_path, &new_name);

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

/// 内核响应 → poem 响应。
pub async fn serve(response: ResourceResponse) -> Response {
    let headers = response.headers();

    match response {
        ResourceResponse::NotFound(body) => {
            Response::builder().status(StatusCode::NOT_FOUND).body(body)
        }

        // 302 与内部重定向都只带响应头（Location / X-Accel-Redirect 已在 headers() 里）
        ResourceResponse::Redirect { .. } => http_response(StatusCode::FOUND, headers),

        ResourceResponse::AccelRedirect { .. } => http_response(StatusCode::OK, headers),

        ResourceResponse::ServeFile {
            path,
            download_name,
        } => {
            let Ok(file) = tokio::fs::File::open(&path).await else {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body("not found");
            };

            let mut builder = Response::builder().status(StatusCode::OK);

            for (name, value) in headers {
                builder = builder.header(name, value);
            }

            if let Some(content_type) = content_type_for(&path) {
                builder = builder.content_type(content_type);
            }

            if let Some(name) = download_name {
                builder = builder.header(
                    header::CONTENT_DISPOSITION,
                    crate::controller::attachment_disposition(&name),
                );
            }

            builder.body(Body::from_bytes_stream(ReaderStream::new(file)))
        }
    }
}

fn http_response(status: StatusCode, headers: Vec<(String, String)>) -> Response {
    let mut builder = Response::builder().status(status);

    for (name, value) in headers {
        builder = builder.header(name, value);
    }

    builder.body(Body::empty())
}

fn bad_request(message: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(message)
}

/// axum 风格的 `{name}` → poem 的 `:name`（两条模板路由共用）。
fn poem_path(template: &str) -> String {
    template.replace('{', ":").replace('}', "")
}

/// 由扩展名推断 Content-Type（走内核的 MIME 表，与 PHP 的文件响应同一张表）。
fn content_type_for(path: &std::path::Path) -> Option<String> {
    let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
    let mime = crate::mime::mime_for_extension(&extension, &[]);

    // 表里查不到时退回 octet-stream 没有意义 —— 交给浏览器自己嗅探
    (mime != "application/octet-stream").then_some(mime)
}
