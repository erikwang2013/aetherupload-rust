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
//! 比落临时文件少一次磁盘往返；把 `chunk_size` 调得很大时请自行换流式解析。
//!
//! 响应形态（2.0 协议）：两个上传入口成功是 200 + `application/json`（snake_case
//! 上传结果），失败是 RFC 9457 问题响应 —— 内核的 `Problem` 状态码与文案原样透出。
//! 本适配器自己判定的失败也回同一形态：坏 multipart / 坏 urlencoded / 缺
//! `Content-Type` → 400 `invalid_request_body`；请求体超过 [`body_limit`]（本层自己
//! 限量，见下）→ 413 `payload_too_large`；定位到文件但打开失败 → 404 `resource_not_found`。
//!
//! `preprocess` 的两种表单编码都收：urlencoded（1.x 既有部署的编码）与 multipart
//! （自带脚本与手工构造的请求）。
//!
//! **请求体上限**：actix 的 `PayloadConfig` 只约束 `Bytes` / `String` / `Json` / `Form`
//! 这几个提取器，原始的 `web::Payload` 不受它管 —— 上限因此自己拿（[`body_limit`] =
//! 分块大小 + 1MB 余量）：读请求体时逐块累计，超了就断开并按 413 回问题响应，不会被
//! 静默截断（actix 的 `Payload` 不是 `Send`，喂不进 multer 的流式解析器，所以先收下
//! 再解析）。
//!
//! 本地文件下发用流式响应：64KB 一读、边读边发，整份文件不进内存；打开与读盘都交给
//! `web::block` 的执行线程，不在 worker 上做同步文件 IO。**不实现 HTTP Range** —— 与
//! 1.x 一致，需要断点下载 / 视频拖动请开 `x_accel_redirect` 交给前置服务器。

use std::fs::File;
use std::io::{self, Read};
use std::sync::Arc;

use actix_web::http::{StatusCode, header};
use actix_web::{HttpRequest, HttpResponse, HttpResponseBuilder, Scope, web};
use futures_util::StreamExt;
use futures_util::stream::try_unfold;
use multer::{Multipart, parse_boundary};

use crate::controller::{ChunkBody, ResourceController, ResourceResponse, UploadController};
use crate::integrations::{
    FormData, ParseFailure, body_limit, display_route, download_route, is_urlencoded, ok_response,
    problem_response, resource_not_found_response,
};
use crate::json::{save_chunk_ok_to_json, upload_ok_to_json};
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
    let limit = body_limit(&runtime);

    // 按 `Content-Type` 分流：1.x 客户端发 urlencoded，自带脚本与手工请求发 multipart
    let form = match content_type(&req) {
        Some(content_type) if is_urlencoded(content_type) => {
            match read_body(payload, limit).await {
                Ok(body) => FormData::from_urlencoded(&String::from_utf8_lossy(&body)),
                Err(failure) => return json_response(failure.response()),
            }
        }
        _ => match parse_multipart(&req, payload, limit).await {
            Ok(form) => form,
            Err(failure) => return json_response(failure.response()),
        },
    };

    let triple = match UploadController::new(runtime.into_inner())
        .preprocess(&form.to_preprocess_request())
    {
        Ok(ok) => ok_response(upload_ok_to_json(&ok)),
        Err(problem) => problem_response(&problem),
    };

    json_response(triple)
}

async fn save_chunk(
    runtime: web::Data<Runtime>,
    req: HttpRequest,
    payload: web::Payload,
) -> HttpResponse {
    let form = match parse_multipart(&req, payload, body_limit(&runtime)).await {
        Ok(form) => form,
        Err(failure) => return json_response(failure.response()),
    };

    let triple = match UploadController::new(runtime.into_inner())
        .save_chunk(&form.to_save_chunk_request())
    {
        Ok(ok) => ok_response(save_chunk_ok_to_json(&ok)),
        Err(problem) => problem_response(&problem),
    };

    json_response(triple)
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

/// 请求的 `Content-Type`（缺头或非法字节一律当没有）。
fn content_type(req: &HttpRequest) -> Option<&str> {
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
}

/// 把 multipart 表单读成 [`FormData`]（分块字段读进内存）。
///
/// `limit` 是整条请求体的字节上限：actix 默认对原始 `Payload` 不设防，这里必须显式给，
/// 否则一个超大请求体会被一路读进内存。
async fn parse_multipart(
    req: &HttpRequest,
    payload: web::Payload,
    limit: usize,
) -> Result<FormData, ParseFailure> {
    let Some(content_type) = content_type(req) else {
        // 缺 Content-Type：请求体无法解析
        return Err(ParseFailure::new(FormData::new(), false));
    };

    let boundary = parse_boundary(content_type)
        .map_err(|_| ParseFailure::new(FormData::new(), false))?;
    let body = read_body(payload, limit).await?;

    // 用 `Multipart::new` 而不是 `with_reader`：后者要 multer 的 `tokio-io` feature
    // （全量构建时被 poem/warp/axum 的依赖统一打开，单 feature 构建下没有 → CI 的单
    // feature 检查会挂）。体已经整条收进内存，包成单元素流即可，行为等价。
    let stream = futures_util::stream::once(async move { Ok::<_, std::io::Error>(body) });
    let mut multipart = Multipart::new(stream, boundary);
    let mut form = FormData::new();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => return Ok(form),
            // 坏体：带着解析到一半的表单回来（能取到 locale 就本地化）
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

/// 收下整条请求体：逐块累计，超过 `limit` 立刻报错（不会一路读进内存）。
///
/// actix 的 `Payload` 不是 `Send`（底层是单线程的 `Rc<RefCell<..>>`），喂不进 `multer`
/// 的流式解析器；而分块本来就要进内存，所以这里先收下、再让 multer 从内存解析。
/// 失败按「超限（413）/ 其它读失败（400）」区分。
async fn read_body(mut payload: web::Payload, limit: usize) -> Result<Vec<u8>, ParseFailure> {
    let mut body = Vec::new();

    while let Some(chunk) = payload.next().await {
        let chunk = chunk.map_err(|_| ParseFailure::new(FormData::new(), false))?;

        if body.len() + chunk.len() > limit {
            return Err(ParseFailure::new(FormData::new(), true));
        }

        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

/// 内核响应 → actix 响应。
pub async fn serve(response: ResourceResponse) -> HttpResponse {
    let headers = response.headers();

    match response {
        // 失败一律问题响应：状态码与文案原样来自内核
        ResourceResponse::Problem(problem) => json_response(problem_response(&problem)),

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
            // 打开文件同样要走阻塞线程池：serve 本身在 worker 上，同步的 open 会占住它
            let open_path = path.clone();
            let Ok(Ok(file)) = web::block(move || File::open(open_path)).await else {
                // 寻址已通过但打开失败：仍按 404 resource_not_found 回
                return json_response(resource_not_found_response());
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

/// 三元组（状态码 + Content-Type + 体）→ actix 响应。
fn json_response((status, content_type, body): (u16, &'static str, String)) -> HttpResponse {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut builder = HttpResponseBuilder::new(status);
    builder.insert_header((header::CONTENT_TYPE, content_type));

    builder.body(body)
}

/// 内核响应附带的安全头（nosniff / Location / X-Accel-Redirect / Content-Disposition）。
fn apply_headers(builder: &mut HttpResponseBuilder, headers: Vec<(String, String)>) {
    for (name, value) in headers {
        builder.insert_header((name, value));
    }
}

/// 流式读盘：64KB 一块，读完即发，整份文件不进内存；每块读都跑在 `web::block` 的
/// 阻塞线程池上，不占 worker。
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

/// 由扩展名推断 Content-Type（走内核的 MIME 表，与上传校验同一张）。
fn content_type_for(path: &std::path::Path) -> Option<String> {
    let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
    let mime = crate::mime::mime_for_extension(&extension, &[]);

    // 表里查不到时退回 octet-stream 没有意义 —— 交给浏览器自己嗅探
    (mime != "application/octet-stream").then_some(mime)
}
