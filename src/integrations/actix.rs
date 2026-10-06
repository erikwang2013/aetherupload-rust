// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! Actix Web 集成：一个 [`web::Scope`]，四条路由一次挂好。
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use actix_web::App;
//! use aetherupload::{Config, Runtime};
//!
//! let runtime = Arc::new(Runtime::new(Config::default(), ".").unwrap());
//! let app = App::new().service(aetherupload::integrations::actix::routes(runtime));
//! // 交给 HttpServer::new(|| app) 起服务
//! ```
//!
//! multipart 用 `multer` 解析（`Content-Type` 里取 boundary），字段名与前端一致：
//! `resource_chunk` 是文件字段，其余按文本字段读。分块读进内存 —— 默认分块 1MB，
//! 比 PHP 落临时文件少一次磁盘往返；把 `chunk_size` 调得很大时请自行换流式解析。
//!
//! **请求体上限**：actix 的 `PayloadConfig` 只约束 `Bytes` / `String` / `Json` / `Form`
//! 这几个提取器，原始的 `web::Payload` 不受它管 —— 上限因此自己拿（[`body_limit`] =
//! 分块大小 + 1MB 余量）：读请求体时逐块累计，超了就断开，不会被静默截断（actix 的
//! `Payload` 不是 `Send`，喂不进 multer 的流式解析器，所以先收下再解析）。
//!
//! 本地文件下发用流式响应：64KB 一读、边读边发，整份文件不进内存；读盘交给 `web::block`
//! 的执行线程，不占 worker。**不实现 HTTP Range** —— 与 PHP/webman 版一致，需要断点下载 /
//! 视频拖动请开 `x_accel_redirect` 交给前置服务器。

use std::fs::File;
use std::io::{self, Read};
use std::sync::Arc;

use actix_web::http::header;
use actix_web::{HttpRequest, HttpResponse, HttpResponseBuilder, Scope, web};
use futures_util::StreamExt;
use futures_util::stream::try_unfold;
use multer::{Multipart, parse_boundary};

use crate::controller::{ChunkBody, ResourceController, ResourceResponse, UploadController};
use crate::integrations::{FormData, body_limit, display_route, download_route};
use crate::runtime::Runtime;

/// 流式下发时每次从磁盘读出的字节数。
const STREAM_CHUNK: usize = 64 * 1024;

/// 注册四条路由（路径取自配置的 `route_*`）。
///
/// 用法：`App::new().service(routes(runtime))`；`Scope` 自带 app data，重复挂载会覆盖同一份
/// [`Runtime`]，不需要使用方再 `app_data`。
pub fn routes(runtime: Arc<Runtime>) -> Scope {
    let preprocess_path = runtime.config().route_preprocess.clone();
    let uploading_path = runtime.config().route_uploading.clone();
    let display_path = display_route(&runtime);
    let download_path = download_route(&runtime);

    // 空前缀的 scope 只做「四路由 + app data」的分组，不改变任何路径
    web::scope("")
        .app_data(web::Data::from(runtime))
        .route(&preprocess_path, web::post().to(preprocess))
        .route(&uploading_path, web::post().to(save_chunk))
        .route(&display_path, web::get().to(display))
        .route(&download_path, web::get().to(download))
}

async fn preprocess(
    runtime: web::Data<Runtime>,
    req: HttpRequest,
    payload: web::Payload,
) -> HttpResponse {
    let Ok(form) = parse_multipart(&req, payload, body_limit(&runtime)).await else {
        return HttpResponse::BadRequest().body("invalid multipart body");
    };

    let result =
        UploadController::new(runtime.into_inner()).preprocess(&form.to_preprocess_request());

    HttpResponse::Ok().body(result.to_json())
}

async fn save_chunk(
    runtime: web::Data<Runtime>,
    req: HttpRequest,
    payload: web::Payload,
) -> HttpResponse {
    let Ok(form) = parse_multipart(&req, payload, body_limit(&runtime)).await else {
        return HttpResponse::BadRequest().body("invalid multipart body");
    };

    let result =
        UploadController::new(runtime.into_inner()).save_chunk(&form.to_save_chunk_request());

    HttpResponse::Ok().body(result.to_json())
}

async fn display(runtime: web::Data<Runtime>, saved_path: web::Path<String>) -> HttpResponse {
    let response = ResourceController::new(runtime.into_inner()).display(&saved_path);

    serve(response).await
}

async fn download(runtime: web::Data<Runtime>, path: web::Path<(String, String)>) -> HttpResponse {
    let (saved_path, new_name) = path.into_inner();
    let response = ResourceController::new(runtime.into_inner()).download(&saved_path, &new_name);

    serve(response).await
}

/// 把 multipart 表单读成 [`FormData`]（分块字段读进内存）。
///
/// `limit` 是整条请求体的字节上限：actix 默认对原始 `Payload` 不设防，这里必须显式给，
/// 否则一个超大请求体会被一路读进内存。
async fn parse_multipart(
    req: &HttpRequest,
    payload: web::Payload,
    limit: usize,
) -> Result<FormData, String> {
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| "missing multipart Content-Type".to_string())?;

    let boundary = parse_boundary(content_type).map_err(|err| err.to_string())?;
    let body = read_body(payload, limit).await?;

    let mut multipart = Multipart::with_reader(std::io::Cursor::new(body), boundary);
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

/// 收下整条请求体：逐块累计，超过 `limit` 立刻报错（不会一路读进内存）。
///
/// actix 的 `Payload` 不是 `Send`（底层是单线程的 `Rc<RefCell<..>>`），喂不进 `multer`
/// 的流式解析器；而分块本来就要进内存，所以这里先收下、再让 multer 从内存解析。
async fn read_body(mut payload: web::Payload, limit: usize) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();

    while let Some(chunk) = payload.next().await {
        let chunk = chunk.map_err(|err| err.to_string())?;

        if body.len() + chunk.len() > limit {
            return Err("request body too large".to_string());
        }

        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

/// 内核响应 → actix 响应。
pub async fn serve(response: ResourceResponse) -> HttpResponse {
    let headers = response.headers();

    match response {
        ResourceResponse::NotFound(body) => HttpResponse::NotFound().body(body),

        ResourceResponse::Redirect { location } => {
            let mut builder = HttpResponse::Found();
            apply_headers(&mut builder, headers);
            builder.insert_header((header::LOCATION, location));

            builder.finish()
        }

        ResourceResponse::AccelRedirect { .. } => {
            let mut builder = HttpResponse::Ok();
            apply_headers(&mut builder, headers);

            builder.finish()
        }

        ResourceResponse::ServeFile {
            path,
            download_name,
        } => {
            let Ok(file) = File::open(&path) else {
                return HttpResponse::NotFound().body("not found");
            };

            let mut builder = HttpResponse::Ok();
            apply_headers(&mut builder, headers);

            if let Some(content_type) = content_type_for(&path) {
                builder.insert_header((header::CONTENT_TYPE, content_type));
            }

            if let Some(name) = download_name {
                builder.insert_header((
                    header::CONTENT_DISPOSITION,
                    crate::controller::attachment_disposition(&name),
                ));
            }

            builder.streaming(file_stream(file))
        }
    }
}

/// 内核响应附带的安全头（nosniff / Location / X-Accel-Redirect / Content-Disposition）。
fn apply_headers(builder: &mut HttpResponseBuilder, headers: Vec<(String, String)>) {
    for (name, value) in headers {
        builder.insert_header((name, value));
    }
}

/// 流式读盘：64KB 一块，读完即发，整份文件不进内存。
fn file_stream(
    file: File,
) -> impl futures_util::Stream<Item = Result<web::Bytes, io::Error>> + 'static {
    try_unfold(file, |mut file| async move {
        let (file, chunk) = web::block(move || {
            let mut buf = vec![0u8; STREAM_CHUNK];
            let read = file.read(&mut buf)?;
            buf.truncate(read);

            Ok::<_, io::Error>((file, buf))
        })
        .await
        .map_err(|_| io::Error::other("chunk read task cancelled"))??;

        if chunk.is_empty() {
            return Ok(None);
        }

        Ok(Some((web::Bytes::from(chunk), file)))
    })
}

/// 由扩展名推断 Content-Type（走内核的 MIME 表，与 PHP 的文件响应同一张表）。
fn content_type_for(path: &std::path::Path) -> Option<String> {
    let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
    let mime = crate::mime::mime_for_extension(&extension, &[]);

    // 表里查不到时退回 octet-stream 没有意义 —— 交给浏览器自己嗅探
    (mime != "application/octet-stream").then_some(mime)
}
