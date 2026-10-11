// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! warp 集成：`Filter` 组合式，四条路由拼成一条过滤器链。
//!
//! warp 没有路由表：方法、路径段、multipart 提取器都拼在一条 `Filter` 链上，
//! [`routes`] 返回的就是拼好的过滤器，交给 `warp::serve(routes)` 即可
//! （用 `warp::test::request()` 测也一样）。
//!
//! 分块走 `warp::multipart::form()` 读进内存 —— 默认分块 1MB，比落临时文件少一次
//! 磁盘往返；请求体上限按 `chunk_size` 放开（warp 的默认值是 2MB，可能与分块同量级，
//! 见 [`crate::integrations::body_limit`]）。
//!
//! 响应形态（2.0 协议）：两个上传入口成功是 200 + `application/json`（snake_case
//! 上传结果），失败是 RFC 9457 问题响应 —— 内核的 `Problem` 状态码与文案原样透出。
//! 本适配器自己判定的失败也回同一形态：坏 multipart / 坏 urlencoded / 缺
//! `Content-Type` → 400 `invalid_request_body`；定位到文件但打开失败 → 404
//! `resource_not_found`。**框架层的拒绝没有例外**：`recover`（`handle_rejection`）
//! 把这条链上所有 `Rejection` 都翻成问题响应 —— `content_length_limit` /
//! `multipart::form().max_length()` 的 413 保持 413 `payload_too_large`，其余
//! （`multipart` 过滤器的 content-type 判定等）一律 400 `invalid_request_body`。
//!
//! 本地文件下发用 [`warp::reply::stream`] + `tokio::fs::File` / `ReaderStream`：64KB
//! 一分块、边读边发，整份文件不进内存，读盘走 `tokio::fs` 的阻塞线程池（不在 worker
//! 上做同步 IO）。**没有用 `warp::fs::file`**：它的路径在过滤器构造期就固定，而这里
//! 要下发的文件由请求里的 `saved_path` 运行时决定，warp 的 `Filter` 也不是 `Future`，
//! 无法在 handler 里按运行时路径再构造一个文件过滤器。
//! **不实现 HTTP Range** —— 与 1.x 一致，需要断点下载/视频拖动请开
//! `x_accel_redirect` 交给前置服务器。
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use aetherupload::{Config, Runtime};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let runtime = Arc::new(Runtime::new(Config::default(), ".")?);
//! let routes = aetherupload::integrations::warp::routes(runtime);
//! // warp::serve(routes).run(([127, 0, 0, 1], 8080)).await
//! # let _ = routes;
//! # Ok(())
//! # }
//! ```

use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;

use tokio_util::io::ReaderStream;
use warp::Filter;
use warp::filters::BoxedFilter;
use warp::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use warp::reject::Rejection;
use warp::{Buf, Reply, Stream};

use crate::controller::{ChunkBody, ResourceController, ResourceResponse, UploadController};
use crate::integrations::{
    FormData, ParseFailure, body_limit, invalid_request_body_response, is_urlencoded, ok_response,
    payload_too_large_response, problem_response, resource_not_found_response,
};
use crate::json::{save_chunk_ok_to_json, upload_ok_to_json};
use crate::runtime::Runtime;

/// 响应的统一类型：warp 的 `Response` 本身就是 `Reply`，四条路由的取出类型一致，
/// `or(...) + unify()` 才拼得起来。
type Response = warp::reply::Response;

/// 注册四条路由（路径取自配置的 `route_*`）。
///
/// 返回 `impl Filter<Extract = (Response,), Error = Infallible> + Clone`，可直接
/// `warp::serve(...)`，也可与宿主自己的过滤器 `.or(...)` 组合（注意 `.unify()`）。
/// 末尾的 `recover` 兜住这条链上所有拒绝点，统一成问题响应；宿主自己 `.or()` 进来
/// 的过滤器若在**之后**被组合，拒绝就不再经过这里。
pub fn routes(
    runtime: Arc<Runtime>,
) -> impl Filter<Extract = (Response,), Error = Infallible> + Clone {
    let limit = body_limit(&runtime);
    let preprocess_path = runtime.config().route_preprocess.clone();
    let uploading_path = runtime.config().route_uploading.clone();
    // 只要路径前缀：`{saved_path}` / `{new_name}` 由 warp::path::param 承接，
    // 不能拿 axum 那种带占位符的模板（那会把 "{saved_path}" 当成字面量段去匹配）
    let display_path = runtime.config().route_display.clone();
    let download_path = runtime.config().route_download.clone();

    preprocess(runtime.clone(), &preprocess_path, limit)
        .or(upload(runtime.clone(), &uploading_path, limit))
        .unify()
        .or(display(runtime.clone(), &display_path))
        .unify()
        .or(download(runtime, &download_path))
        .unify()
        .recover(handle_rejection)
        // recover 的取出值被包成 `Either<正常路径, 恢复路径>`（warp 的私有类型），
        // 两条携带的都是同一种响应：就地走 `Reply` 归约展平，对外保持 `(Response,)`
        .map(|reply| reply.into_response())
}

/// 预处理：解析表单（urlencoded 或 multipart）→ 内核 `preprocess` → 上传结果体。
///
/// 1.x 客户端发的是 urlencoded，multipart 留给自带脚本与手工构造的请求 —— 两条分支
/// 各自成链、用 `.or()` 合流：`content-type` 不是 urlencoded 时第一条拒绝（404，
/// 优先级最低，不覆盖 multipart 分支的判定），落到第二条。
fn preprocess(
    runtime: Arc<Runtime>,
    path: &str,
    limit: usize,
) -> impl Filter<Extract = (Response,), Error = Rejection> + Clone + use<> {
    let urlencoded = warp::post()
        .and(path_prefix(path))
        .and(warp::path::end())
        .and(urlencoded_content_type())
        .and(warp::body::content_length_limit(limit as u64))
        .and(warp::body::bytes())
        .and(with_runtime(runtime.clone()))
        .and_then(
            // `warp::body::bytes()` 给的是 `bytes::Bytes`，而 warp 只转出了 `bytes::Buf`
            // （`warp::hyper` 又要 `server` feature）—— 同一个类型从 tokio-util 的转出里取
            |bytes: tokio_util::bytes::Bytes, runtime: Arc<Runtime>| async move {
                let form = FormData::from_urlencoded(&String::from_utf8_lossy(&bytes));

                let triple = match UploadController::new(runtime)
                    .preprocess(&form.to_preprocess_request())
                {
                    Ok(ok) => ok_response(upload_ok_to_json(&ok)),
                    Err(problem) => problem_response(&problem),
                };

                Ok::<_, Rejection>(json_response(triple))
            },
        );

    let multipart = warp::post()
        .and(path_prefix(path))
        .and(warp::path::end())
        .and(warp::multipart::form().max_length(limit as u64))
        .and(with_runtime(runtime))
        .and_then(
            |form: warp::multipart::FormData, runtime: Arc<Runtime>| async move {
                // 解析失败按坏体回问题响应（与 axum 适配层同一处置），不走 reject
                let response = match parse_multipart(form).await {
                    Ok(form) => {
                        let triple = match UploadController::new(runtime)
                            .preprocess(&form.to_preprocess_request())
                        {
                            Ok(ok) => ok_response(upload_ok_to_json(&ok)),
                            Err(problem) => problem_response(&problem),
                        };

                        json_response(triple)
                    }
                    Err(failure) => json_response(failure.response()),
                };

                Ok::<_, Rejection>(response)
            },
        );

    urlencoded.or(multipart).unify()
}

/// 只让 `application/x-www-form-urlencoded`（`; charset=…` 参数忽略）通过。
///
/// 用 404 拒绝：warp 合并两条分支的拒绝时以状态码取优先（404 最低），
/// 于是「两边都不认」的请求仍按 multipart 分支原来的状态码回。
fn urlencoded_content_type() -> impl Filter<Extract = (), Error = Rejection> + Clone {
    warp::header::value("content-type")
        .and_then(|value: HeaderValue| async move {
            let matched = value.to_str().ok().is_some_and(is_urlencoded);

            if matched { Ok(()) } else { Err(warp::reject()) }
        })
        .untuple_one()
}

/// 分块写入：解析 multipart → 内核 `saveChunk` → 上传结果体。
fn upload(
    runtime: Arc<Runtime>,
    path: &str,
    limit: usize,
) -> impl Filter<Extract = (Response,), Error = Rejection> + Clone + use<> {
    warp::post()
        .and(path_prefix(path))
        .and(warp::path::end())
        .and(warp::multipart::form().max_length(limit as u64))
        .and(with_runtime(runtime))
        .and_then(
            |form: warp::multipart::FormData, runtime: Arc<Runtime>| async move {
                let response = match parse_multipart(form).await {
                    Ok(form) => {
                        let triple = match UploadController::new(runtime)
                            .save_chunk(&form.to_save_chunk_request())
                        {
                            Ok(ok) => ok_response(save_chunk_ok_to_json(&ok)),
                            Err(problem) => problem_response(&problem),
                        };

                        json_response(triple)
                    }
                    Err(failure) => json_response(failure.response()),
                };

                Ok::<_, Rejection>(response)
            },
        )
}

/// 展示：`GET {route_display}/{saved_path}`。
fn display(
    runtime: Arc<Runtime>,
    path: &str,
) -> impl Filter<Extract = (Response,), Error = Rejection> + Clone + use<> {
    warp::get()
        .and(path_prefix(path))
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(with_runtime(runtime))
        .and_then(|saved_path: String, runtime: Arc<Runtime>| async move {
            let response = ResourceController::new(runtime).display(&saved_path);

            Ok::<_, Rejection>(serve(response).await)
        })
}

/// 下载：`GET {route_download}/{saved_path}/{new_name}`。
fn download(
    runtime: Arc<Runtime>,
    path: &str,
) -> impl Filter<Extract = (Response,), Error = Rejection> + Clone + use<> {
    warp::get()
        .and(path_prefix(path))
        .and(warp::path::param::<String>())
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(with_runtime(runtime))
        .and_then(
            |saved_path: String, new_name: String, runtime: Arc<Runtime>| async move {
                let response = ResourceController::new(runtime).download(&saved_path, &new_name);

                Ok::<_, Rejection>(serve(response).await)
            },
        )
}

/// 框架层拒绝 → 问题响应；这里没有拦不住的拒绝点。
///
/// `PayloadTooLarge`（`content_length_limit` / `multipart::form().max_length()`）保持
/// 413，其余（content-type 不匹配、boundary 缺失、自定义 `warp::reject()`…）按
/// 「请求体无法解析」回 400 `invalid_request_body`。
async fn handle_rejection(err: Rejection) -> Result<Response, Infallible> {
    let triple = if err.find::<warp::reject::PayloadTooLarge>().is_some() {
        payload_too_large_response(None)
    } else {
        invalid_request_body_response(None)
    };

    Ok(json_response(triple))
}

/// 内核响应 → warp 响应。文件走分块流；文件打不开时按 404 问题响应回（与 axum 适配层一致）。
pub async fn serve(response: ResourceResponse) -> Response {
    let headers = response.headers();

    match response {
        // 失败一律问题响应：状态码与文案原样来自内核
        ResourceResponse::Problem(problem) => json_response(problem_response(&problem)),

        // Location / X-Accel-Redirect 都在内核的 headers() 里，体为空
        ResourceResponse::Redirect { .. } => buffered(StatusCode::FOUND, headers, String::new()),

        ResourceResponse::AccelRedirect { .. } => buffered(StatusCode::OK, headers, String::new()),

        ResourceResponse::ServeFile {
            path,
            download_name,
        } => {
            // warp 只在处理请求体时自己包了 `tokio::fs`，响应侧没有公开的「从路径造流」
            // 入口（`Body` 是私有类型），所以这里用 `tokio::fs` 打开后交给 `ReaderStream`
            // 造流：64KB 一读，整份文件不进内存。
            let Ok(file) = tokio::fs::File::open(&path).await else {
                // 寻址已通过但打开失败：仍按 404 resource_not_found 回
                return json_response(resource_not_found_response());
            };

            let stream = ReaderStream::with_capacity(file, 64 * 1024);
            let mut response = warp::reply::stream(stream).into_response();
            apply_headers(response.headers_mut(), headers);

            if let Some(content_type) = content_type_for(&path) {
                set_header(response.headers_mut(), header::CONTENT_TYPE, &content_type);
            }

            if let Some(name) = download_name {
                set_header(
                    response.headers_mut(),
                    header::CONTENT_DISPOSITION,
                    &crate::controller::attachment_disposition(&name),
                );
            }

            response
        }
    }
}

/// 把 multipart 表单读成 [`FormData`]（分块字段读进内存）。
///
/// 失败带着解析到一半的表单回来 —— 能取到 `locale` 就按它本地化问题标题。
/// （超限走 `multipart::form().max_length` 的拒绝，不经这里，见 [`handle_rejection`]。）
///
/// 逐个字段迭代用 `std::future::poll_fn` 直接驱动 `Stream::poll_next` —— warp 只
/// 转出了 `Stream` 这一个 trait，没有 `StreamExt`，这样不必额外引 futures-util。
async fn parse_multipart(form: warp::multipart::FormData) -> Result<FormData, ParseFailure> {
    let mut form = std::pin::pin!(form);
    let mut parsed = FormData::new();

    loop {
        let next = std::future::poll_fn(|cx| form.as_mut().poll_next(cx)).await;

        let Some(part) = next else { break };
        let Ok(mut part) = part else {
            return Err(ParseFailure::new(parsed, false));
        };
        let name = part.name().to_string();

        // 分块本体（文件字段）：字节读进内存；其余字段读成文本
        if name == "resource_chunk" {
            let mut bytes = Vec::new();

            while let Some(chunk) = part.data().await {
                let Ok(mut chunk) = chunk else {
                    return Err(ParseFailure::new(parsed, false));
                };
                let len = chunk.remaining();
                bytes.extend_from_slice(&chunk.copy_to_bytes(len));
            }

            parsed.push_file(name, ChunkBody::Bytes(bytes));
        } else {
            let mut text = String::new();

            while let Some(chunk) = part.data().await {
                let Ok(mut chunk) = chunk else {
                    return Err(ParseFailure::new(parsed, false));
                };
                let len = chunk.remaining();
                text.push_str(&String::from_utf8_lossy(&chunk.copy_to_bytes(len)));
            }

            parsed.push_text(name, text);
        }
    }

    Ok(parsed)
}

/// 把配置里的路径（`/aetherupload/preprocess`）拆成逐段匹配的过滤器。
fn path_prefix(path: &str) -> BoxedFilter<()> {
    path.split('/').filter(|segment| !segment.is_empty()).fold(
        warp::any().boxed(),
        |filter, segment| {
            // boxed() 要求 'static：路径段要拷成 String 带进过滤器
            filter.and(warp::path(segment.to_string())).boxed()
        },
    )
}

/// 每个请求克隆一份 `Arc<Runtime>` 注入过滤器链。
fn with_runtime(
    runtime: Arc<Runtime>,
) -> impl Filter<Extract = (Arc<Runtime>,), Error = Infallible> + Clone {
    warp::any().map(move || runtime.clone())
}

/// 三元组（状态码 + Content-Type + 体）→ warp 响应。
fn json_response((status, content_type, body): (u16, &'static str, String)) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = warp::reply::with_status(body, status).into_response();
    set_header(
        response.headers_mut(),
        header::CONTENT_TYPE,
        content_type,
    );

    response
}

/// 内存体响应（重定向 / 空体）：状态 + 内核响应头。
fn buffered(status: StatusCode, headers: Vec<(String, String)>, body: String) -> Response {
    let mut response = warp::reply::with_status(body, status).into_response();

    apply_headers(response.headers_mut(), headers);

    response
}

/// 内核给的头清单并进响应（`X-Content-Type-Options: nosniff` 等）。
fn apply_headers(target: &mut HeaderMap, headers: Vec<(String, String)>) {
    for (name, value) in headers {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            set_header(target, name, &value);
        }
    }
}

/// 值非法（含 CRLF 等控制字符）时整条丢弃 —— 头都来自服务端已校验的数据。
fn set_header(target: &mut HeaderMap, name: HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        target.insert(name, value);
    }
}

/// 由扩展名推断 Content-Type（走内核的 MIME 表，与上传校验同一张）。
fn content_type_for(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
    let mime = crate::mime::mime_for_extension(&extension, &[]);

    // 表里查不到时退回 octet-stream 没有意义 —— 交给浏览器自己嗅探
    (mime != "application/octet-stream").then_some(mime)
}
