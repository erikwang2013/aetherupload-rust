// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! warp 集成：`Filter` 组合式，四条路由拼成一条过滤器链。
//!
//! warp 没有路由表：方法、路径段、multipart 提取器都拼在一条 `Filter` 链上，
//! [`routes`] 返回的就是拼好的过滤器，交给 `warp::serve(routes)` 即可
//! （用 `warp::test::request()` 测也一样）。
//!
//! 分块走 `warp::multipart::form()` 读进内存 —— 默认分块 1MB，比 PHP 落临时文件少一次
//! 磁盘往返；请求体上限按 `chunk_size` 放开（warp 的默认值是 2MB，可能与分块同量级，
//! 见 [`crate::integrations::body_limit`]）。
//!
//! 本地文件下发用 [`warp::reply::stream`] + 64KB 分块读，不把整份文件读进内存。
//! **没有用 `warp::fs::file`**：它的路径在过滤器构造期就固定，而这里要下发的文件由
//! 请求里的 `saved_path` 运行时决定，warp 的 `Filter` 也不是 `Future`，无法在 handler
//! 里按运行时路径再构造一个文件过滤器。读盘与内核同为同步 IO（本机磁盘、一次一块），
//! 要完全避开 worker 占用请开 `x_accel_redirect` 交给前置服务器。
//! **不实现 HTTP Range** —— 与 PHP/webman 版一致，需要断点下载/视频拖动请开
//! `x_accel_redirect`。
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
use std::io::Read;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use warp::Filter;
use warp::filters::BoxedFilter;
use warp::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use warp::reject::Rejection;
use warp::{Buf, Reply, Stream};

use crate::controller::{ChunkBody, ResourceController, ResourceResponse, UploadController};
use crate::integrations::{FormData, body_limit};
use crate::runtime::Runtime;

/// 响应的统一类型：warp 的 `Response` 本身就是 `Reply`，四条路由的取出类型一致，
/// `or(...) + unify()` 才拼得起来。
type Response = warp::reply::Response;

/// 注册四条路由（路径取自配置的 `route_*`）。
///
/// 返回 `impl Filter<Extract = (Response,), Error = Rejection> + Clone`，可直接
/// `warp::serve(...)`，也可与宿主自己的过滤器 `.or(...)` 组合（注意 `.unify()`）。
pub fn routes(
    runtime: Arc<Runtime>,
) -> impl Filter<Extract = (Response,), Error = Rejection> + Clone {
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
}

/// 预处理：解析 multipart → 内核 `preprocess` → JSON 响应体。
fn preprocess(
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
                // 解析失败按 400 回（与 axum 适配层同一处置），不走 reject
                let response = match parse_multipart(form).await {
                    Ok(form) => {
                        let result = UploadController::new(runtime)
                            .preprocess(&form.to_preprocess_request());

                        buffered(StatusCode::OK, Vec::new(), result.to_json())
                    }
                    Err(_) => buffered(
                        StatusCode::BAD_REQUEST,
                        Vec::new(),
                        "invalid multipart body".to_string(),
                    ),
                };

                Ok::<_, Rejection>(response)
            },
        )
}

/// 分块写入：解析 multipart → 内核 `saveChunk` → JSON 响应体。
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
                        let result = UploadController::new(runtime)
                            .save_chunk(&form.to_save_chunk_request());

                        buffered(StatusCode::OK, Vec::new(), result.to_json())
                    }
                    Err(_) => buffered(
                        StatusCode::BAD_REQUEST,
                        Vec::new(),
                        "invalid multipart body".to_string(),
                    ),
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
        .map(|saved_path: String, runtime: Arc<Runtime>| {
            serve(ResourceController::new(runtime).display(&saved_path))
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
        .map(
            |saved_path: String, new_name: String, runtime: Arc<Runtime>| {
                serve(ResourceController::new(runtime).download(&saved_path, &new_name))
            },
        )
}

/// 内核响应 → warp 响应。文件走分块流；文件打不开时按 404 回（与 axum 适配层一致）。
pub fn serve(response: ResourceResponse) -> Response {
    let headers = response.headers();

    match response {
        ResourceResponse::NotFound(body) => {
            buffered(StatusCode::NOT_FOUND, headers, body.to_string())
        }

        // Location / X-Accel-Redirect 都在内核的 headers() 里，体为空
        ResourceResponse::Redirect { .. } => buffered(StatusCode::FOUND, headers, String::new()),

        ResourceResponse::AccelRedirect { .. } => buffered(StatusCode::OK, headers, String::new()),

        ResourceResponse::ServeFile {
            path,
            download_name,
        } => {
            let Ok(file) = FileStream::open(&path) else {
                return buffered(StatusCode::NOT_FOUND, headers, "not found".to_string());
            };

            let mut response = warp::reply::stream(file).into_response();
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
/// 逐个字段迭代用 `std::future::poll_fn` 直接驱动 `Stream::poll_next` —— warp 只
/// 转出了 `Stream` 这一个 trait，没有 `StreamExt`，这样不必额外引 futures-util。
async fn parse_multipart(form: warp::multipart::FormData) -> Result<FormData, warp::Error> {
    let mut form = std::pin::pin!(form);
    let mut parsed = FormData::new();

    loop {
        let next = std::future::poll_fn(|cx| form.as_mut().poll_next(cx)).await;

        let Some(part) = next else { break };
        let mut part = part?;
        let name = part.name().to_string();

        // 分块本体（文件字段）：字节读进内存；其余字段读成文本
        if name == "resource_chunk" {
            let mut bytes = Vec::new();

            while let Some(chunk) = part.data().await {
                let mut chunk = chunk?;
                let len = chunk.remaining();
                bytes.extend_from_slice(&chunk.copy_to_bytes(len));
            }

            parsed.push_file(name, ChunkBody::Bytes(bytes));
        } else {
            let mut text = String::new();

            while let Some(chunk) = part.data().await {
                let mut chunk = chunk?;
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

/// 内存体响应（JSON / 空体 / 错误文案）：状态 + 内核响应头。
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

/// 由扩展名推断 Content-Type（走内核的 MIME 表，与 PHP 的文件响应同一张表）。
fn content_type_for(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
    let mime = crate::mime::mime_for_extension(&extension, &[]);

    // 表里查不到时退回 octet-stream 没有意义 —— 交给浏览器自己嗅探
    (mime != "application/octet-stream").then_some(mime)
}

/// 本地文件的分块流：64KB 一读，整份文件不进内存。
///
/// warp 只在处理请求体时自己包了 `tokio::fs`，响应侧没有公开的“从路径造流”入口
/// （`Body` 是私有类型），所以这里自己实现 `Stream` 交给 `warp::reply::stream`。
struct FileStream {
    file: std::fs::File,
    buf: Vec<u8>,
    done: bool,
}

impl FileStream {
    fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            file: std::fs::File::open(path)?,
            buf: vec![0; 64 * 1024],
            done: false,
        })
    }
}

impl Stream for FileStream {
    type Item = Result<Vec<u8>, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // FileStream 全是 Unpin 字段，取回 &mut self 才能同时借 file 与 buf
        let this = self.get_mut();

        if this.done {
            return Poll::Ready(None);
        }

        match this.file.read(&mut this.buf) {
            Ok(0) => {
                this.done = true;
                Poll::Ready(None)
            }
            Ok(read) => Poll::Ready(Some(Ok(this.buf[..read].to_vec()))),
            Err(err) => {
                this.done = true;
                Poll::Ready(Some(Err(err)))
            }
        }
    }
}
