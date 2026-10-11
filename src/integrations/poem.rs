// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! Poem 集成：一个 [`Route`]，四条路由一次挂好。
//!
//! 分块走 `Multipart` 提取器读进内存 —— 默认分块 1MB，比落临时文件少一次磁盘往返；
//! 把 `chunk_size` 调得很大时请自行换流式解析。
//!
//! 响应形态（2.0 协议）：两个上传入口成功是 200 + `application/json`（snake_case
//! 上传结果），失败是 RFC 9457 问题响应 —— 内核的 `Problem` 状态码与文案原样透出。
//! 本适配器自己判定的失败也回同一形态：坏 multipart / 坏 urlencoded / 缺
//! `Content-Type` → 400 `invalid_request_body`；定位到文件但打开失败 → 404
//! `resource_not_found`。（poem 对请求体没有默认上限，这里不存在框架层 413。）
//!
//! `preprocess` 的两种表单编码都收：urlencoded（1.x 既有部署的编码）与 multipart
//! （自带脚本与手工构造的请求）；两条分支都受下一节的 `SizeLimit` 建议约束。
//!
//! 本地文件下发用流式响应（`tokio::fs` + `ReaderStream`），不把整份文件读进内存；
//! **不实现 HTTP Range** —— 与 1.x 一致，需要断点下载/视频拖动请开
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
use poem::web::{Data, Multipart, Path, RequestBody};
use poem::{Body, EndpointExt, FromRequest, Request, Response, Route, get, handler, post};
use tokio_util::io::ReaderStream;

use crate::controller::{ChunkBody, ResourceController, ResourceResponse, UploadController};
use crate::integrations::{
    FormData, ParseFailure, display_route, download_route, invalid_request_body_response,
    is_urlencoded, ok_response, problem_response, resource_not_found_response,
};
use crate::json::{save_chunk_ok_to_json, upload_ok_to_json};
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
async fn preprocess(Data(runtime): Data<&Arc<Runtime>>, req: &Request, body: Body) -> Response {
    // 按 `Content-Type` 分流：1.x 客户端发 urlencoded，自带脚本与手工请求发 multipart
    let urlencoded = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_urlencoded);

    let form = if urlencoded {
        match body.into_vec().await {
            Ok(bytes) => FormData::from_urlencoded(&String::from_utf8_lossy(&bytes)),
            // 请求体读失败（连接中断等）：按坏体回问题响应
            Err(_) => return json_response(ParseFailure::new(FormData::new(), false).response()),
        }
    } else {
        let mut body = RequestBody::new(body);

        match Multipart::from_request(req, &mut body).await {
            Ok(multipart) => match parse_multipart(multipart).await {
                Ok(form) => form,
                Err(failure) => return json_response(failure.response()),
            },
            // 缺 Content-Type / boundary 非法：请求体无法解析
            Err(_) => return json_response(invalid_request_body_response(None)),
        }
    };

    let triple =
        match UploadController::new(runtime.clone()).preprocess(&form.to_preprocess_request()) {
            Ok(ok) => ok_response(upload_ok_to_json(&ok)),
            Err(problem) => problem_response(&problem),
        };

    json_response(triple)
}

#[handler]
async fn save_chunk(Data(runtime): Data<&Arc<Runtime>>, req: &Request, body: Body) -> Response {
    // 分块路由只可能是 multipart；提取失败同样回问题响应（不落回 poem 的纯文本拒绝）
    let mut body = RequestBody::new(body);

    let form = match Multipart::from_request(req, &mut body).await {
        Ok(multipart) => match parse_multipart(multipart).await {
            Ok(form) => form,
            Err(failure) => return json_response(failure.response()),
        },
        Err(_) => return json_response(invalid_request_body_response(None)),
    };

    let triple =
        match UploadController::new(runtime.clone()).save_chunk(&form.to_save_chunk_request()) {
            Ok(ok) => ok_response(save_chunk_ok_to_json(&ok)),
            Err(problem) => problem_response(&problem),
        };

    json_response(triple)
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
///
/// 失败带着解析到一半的表单回来 —— 能取到 `locale` 就按它本地化问题标题。
async fn parse_multipart(mut multipart: Multipart) -> Result<FormData, ParseFailure> {
    let mut form = FormData::new();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => return Ok(form),
            Err(_) => return Err(ParseFailure::new(form, false)),
        };

        let name = field.name().unwrap_or_default().to_string();

        if name == "resource_chunk" {
            match field.bytes().await {
                Ok(bytes) => form.push_file(name, ChunkBody::Bytes(bytes.to_vec())),
                Err(_) => return Err(ParseFailure::new(form, false)),
            }
        } else {
            match field.text().await {
                Ok(value) => form.push_text(name, value),
                Err(_) => return Err(ParseFailure::new(form, false)),
            }
        }
    }
}

/// 内核响应 → poem 响应。
pub async fn serve(response: ResourceResponse) -> Response {
    let headers = response.headers();

    match response {
        // 失败一律问题响应：状态码与文案原样来自内核
        ResourceResponse::Problem(problem) => json_response(problem_response(&problem)),

        // 302 与内部重定向都只带响应头（Location / X-Accel-Redirect 已在 headers() 里）
        ResourceResponse::Redirect { .. } => http_response(StatusCode::FOUND, headers),

        ResourceResponse::AccelRedirect { .. } => http_response(StatusCode::OK, headers),

        ResourceResponse::ServeFile {
            path,
            download_name,
        } => {
            // 寻址已通过但打开失败：仍按 404 resource_not_found 回
            let Ok(file) = tokio::fs::File::open(&path).await else {
                return json_response(resource_not_found_response());
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

/// 三元组（状态码 + Content-Type + 体）→ poem 响应。
fn json_response((status, content_type, body): (u16, &'static str, String)) -> Response {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(header::CONTENT_TYPE, content_type)
        .body(body)
}

fn http_response(status: StatusCode, headers: Vec<(String, String)>) -> Response {
    let mut builder = Response::builder().status(status);

    for (name, value) in headers {
        builder = builder.header(name, value);
    }

    builder.body(Body::empty())
}

/// axum 风格的 `{name}` → poem 的 `:name`（两条模板路由共用）。
fn poem_path(template: &str) -> String {
    template.replace('{', ":").replace('}', "")
}

/// 由扩展名推断 Content-Type（走内核的 MIME 表，与上传校验同一张）。
fn content_type_for(path: &std::path::Path) -> Option<String> {
    let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
    let mime = crate::mime::mime_for_extension(&extension, &[]);

    // 表里查不到时退回 octet-stream 没有意义 —— 交给浏览器自己嗅探
    (mime != "application/octet-stream").then_some(mime)
}
