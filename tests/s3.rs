// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! S3 驱动的端到端测试 —— 对应 PHP 版 `tests/S3StorageTest.php` + `S3ClientTest.php`。
//!
//! 两个替身传输：
//! - [`ScriptedTransport`]：脚本化响应队列 + 原样记录请求（逐字节断言 URL / 方法序列 /
//!   body 切片），与 PHP 的 `FakeS3Transport` 同语义；
//! - [`MemoryS3`]：内存对象存储，真跑一遍 create → part → complete / abort 编排，
//!   把「落地内容是否等于原文件」与「去重是否真的零上传」压成事实断言。
//!
//! 需要真实 S3 服务的用例**不存在** —— 全部走替身，`cargo test --features s3` 离线可真跑通。

#![cfg(feature = "s3")]

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aetherupload::Result;
use aetherupload::config::{PayloadSigning, S3Config};
use aetherupload::error::Error;
use aetherupload::storage::Storage;
use aetherupload::storage::s3::{HttpBody, HttpRequest, HttpResponse, HttpTransport, S3Storage};
use aetherupload::storage::sigv4::{self, Signer, UNSIGNED_PAYLOAD};

const ACCESS: &str = "AKIDEXAMPLE";
const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
/// 与驱动里的固定分片大小一致（非末片恒为 64MB）。
const PART_SIZE: u64 = 67_108_864;

/// 与上传链路真实产物同构的最小 GIF（内容探测认作 image/gif）
const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\xff\xff\xff\x00\x00\x00!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;";

fn config() -> S3Config {
    S3Config {
        endpoint: "http://ep:9000".to_string(),
        region: "us-east-1".to_string(),
        bucket: "b".to_string(),
        access_key: ACCESS.to_string(),
        secret_key: SECRET.to_string(),
        path_style: true,
        prefix: "up".to_string(),
        multipart_threshold: 104_857_600,
        payload_signing: PayloadSigning::Hash,
    }
}

fn storage(config: S3Config, transport: Arc<dyn HttpTransport>) -> S3Storage {
    S3Storage::new(config, Path::new("/nonexistent"))
        .expect("测试配置合法")
        .with_transport(transport)
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aetherupload-s3-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    dir
}

fn write_file(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();

    path
}

fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// 独立复算：请求里的 `Authorization` 必须正好签在「同一方法、同一 URL、同一 payload 哈希、
/// 同一时刻」上 —— 这样「实现签了别的字节」不可能蒙混过关。
fn assert_signature_is_consistent(request: &HttpRequest) {
    let (_, remainder) = request.url.split_once("://").expect("URL 必须带 scheme");
    let (host, path_and_query) = remainder.split_once('/').expect("URL 必须带路径");
    let (path, query) = match path_and_query.split_once('?') {
        Some((path, query)) => (path, query),
        None => (path_and_query, ""),
    };

    let payload_hash = header(request, "x-amz-content-sha256").expect("必须签 payload 哈希位");
    let amz_date = header(request, "x-amz-date").expect("必须带 x-amz-date");
    let signed = [
        ("host".to_string(), host.to_string()),
        ("x-amz-content-sha256".to_string(), payload_hash.to_string()),
        ("x-amz-date".to_string(), amz_date.to_string()),
    ];

    let expected = Signer::new(ACCESS, SECRET, "us-east-1", "s3").sign_request(
        &request.method,
        &format!("/{path}"),
        query,
        &signed,
        payload_hash,
        amz_date,
    );

    assert_eq!(
        header(request, "Authorization"),
        Some(expected.as_str()),
        "Authorization 与请求本身不符：{}",
        request.url
    );
}

/// 脚本化传输：响应队列 + 原样记录请求（对应 PHP 的 `FakeS3Transport`）。
#[derive(Default)]
struct ScriptedTransport {
    requests: Mutex<Vec<HttpRequest>>,
    queue: Mutex<VecDeque<Scripted>>,
}

enum Scripted {
    Respond(HttpResponse),
    Fail(String),
}

impl ScriptedTransport {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn respond(&self, status: u16) -> &Self {
        self.respond_full(status, Vec::new(), &[])
    }

    fn respond_full(&self, status: u16, headers: Vec<(&str, &str)>, body: &[u8]) -> &Self {
        let response = HttpResponse {
            status,
            headers: headers
                .into_iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body: body.to_vec(),
        };
        self.queue
            .lock()
            .unwrap()
            .push_back(Scripted::Respond(response));

        self
    }

    fn fail(&self, message: &str) -> &Self {
        self.queue
            .lock()
            .unwrap()
            .push_back(Scripted::Fail(message.to_string()));

        self
    }

    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn methods(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.method.clone())
            .collect()
    }

    fn urls(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.url.clone())
            .collect()
    }

    /// 还剩多少条未被消费的脚本响应（应恒为 0，否则说明实现少发了请求）。
    fn remaining(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
}

impl HttpTransport for ScriptedTransport {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse> {
        self.requests.lock().unwrap().push(request);

        let next = self.queue.lock().unwrap().pop_front();

        match next {
            None => Err(Error::Backend(
                "ScriptedTransport：这次调用没有对应的脚本响应（实现发出了未预期的请求）"
                    .to_string(),
            )),
            Some(Scripted::Fail(message)) => Err(Error::Backend(message)),
            Some(Scripted::Respond(response)) => Ok(response),
        }
    }
}

/// 内存对象存储：实现本驱动用得到的五个操作，真跑一遍编排。
#[derive(Default)]
struct MemoryS3 {
    objects: Mutex<HashMap<String, Vec<u8>>>,
    uploads: Mutex<HashMap<String, Vec<Option<Vec<u8>>>>>,
    puts: AtomicUsize,
    calls: Mutex<Vec<String>>,
}

impl MemoryS3 {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn object(&self, key: &str) -> Option<Vec<u8>> {
        self.objects.lock().unwrap().get(key).cloned()
    }

    fn puts(&self) -> usize {
        self.puts.load(Ordering::SeqCst)
    }

    fn pending_uploads(&self) -> usize {
        self.uploads.lock().unwrap().len()
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl HttpTransport for MemoryS3 {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse> {
        let Some((path, query)) = split_url(&request.url) else {
            panic!("内存对象存储：无法解析 URL {}", request.url);
        };
        let key = path.trim_start_matches("/b/").to_string();
        self.calls.lock().unwrap().push(request.method.clone());

        match request.method.as_str() {
            "HEAD" => {
                let exists = self.objects.lock().unwrap().contains_key(&key);

                Ok(HttpResponse {
                    status: if exists { 200 } else { 404 },
                    ..HttpResponse::default()
                })
            }

            "PUT" => {
                let bytes = read_body(&request.body);

                let Some(number) = param(query, "partNumber") else {
                    self.objects.lock().unwrap().insert(key, bytes);
                    self.puts.fetch_add(1, Ordering::SeqCst);

                    return Ok(HttpResponse {
                        status: 200,
                        ..HttpResponse::default()
                    });
                };

                let upload_id = param(query, "uploadId").expect("分片请求必须带 uploadId");
                let index: usize = number.parse().expect("partNumber 必须是数字");
                let mut uploads = self.uploads.lock().unwrap();
                let parts = uploads
                    .get_mut(upload_id)
                    .expect("partNumber 指向未知的 uploadId");

                if parts.len() < index {
                    parts.resize(index, None);
                }
                parts[index - 1] = Some(bytes);

                Ok(HttpResponse {
                    status: 200,
                    headers: vec![("etag".to_string(), format!("\"e{index}\""))],
                    body: Vec::new(),
                })
            }

            "POST" => {
                if query == "uploads=" {
                    let upload_id = format!("u-{}", self.uploads.lock().unwrap().len() + 1);
                    self.uploads
                        .lock()
                        .unwrap()
                        .insert(upload_id.clone(), Vec::new());

                    return Ok(xml_response(&format!(
                        "<InitiateMultipartUploadResult><UploadId>{upload_id}</UploadId></InitiateMultipartUploadResult>"
                    )));
                }

                let upload_id = param(query, "uploadId").expect("complete 必须带 uploadId");
                let parts = self
                    .uploads
                    .lock()
                    .unwrap()
                    .remove(upload_id)
                    .expect("complete 指向未知的 uploadId");

                let mut assembled = Vec::new();
                for (index, part) in parts.into_iter().enumerate() {
                    let part = part.unwrap_or_else(|| panic!("分片 {} 缺失", index + 1));
                    assembled.extend_from_slice(&part);
                }

                self.objects.lock().unwrap().insert(key, assembled);
                self.puts.fetch_add(1, Ordering::SeqCst);

                Ok(xml_response("<CompleteMultipartUploadResult/>"))
            }

            "DELETE" => {
                if let Some(upload_id) = param(query, "uploadId") {
                    self.uploads.lock().unwrap().remove(upload_id);
                } else {
                    let removed = self.objects.lock().unwrap().remove(&key).is_some();
                    if !removed {
                        return Ok(HttpResponse {
                            status: 404,
                            ..HttpResponse::default()
                        });
                    }
                }

                Ok(HttpResponse {
                    status: 204,
                    ..HttpResponse::default()
                })
            }

            other => panic!("内存对象存储不支持 {other}"),
        }
    }
}

fn xml_response(body: &str) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "application/xml".to_string())],
        body: body.as_bytes().to_vec(),
    }
}

fn split_url(url: &str) -> Option<(&str, &str)> {
    let (_, remainder) = url.split_once("://")?;
    let (path, query) = match remainder.split_once('?') {
        Some((path, query)) => (path, query),
        None => (remainder, ""),
    };
    let start = path.find('/')?;

    Some((&path[start..], query))
}

fn param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

/// 按 `HttpBody` 的契约取字节：文件体只取 `[offset, offset + len)` 切片（驱动若把整文件
/// 哈希当分片发出去，这里读到的内容也会随之错位）。
fn read_body(body: &HttpBody) -> Vec<u8> {
    match body {
        HttpBody::Empty => Vec::new(),
        HttpBody::Bytes(bytes) => bytes.clone(),
        HttpBody::File { path, offset, len } => {
            let mut file = std::fs::File::open(path).expect("文件必须可打开");
            file.seek(SeekFrom::Start(*offset))
                .expect("切片偏移必须合法");
            let mut buffer = vec![0u8; *len as usize];
            file.read_exact(&mut buffer).expect("切片长度必须合法");

            buffer
        }
    }
}

fn assert_hex_signature(url: &str) {
    let signature = url
        .rsplit("X-Amz-Signature=")
        .next()
        .expect("预签名 URL 必须带 X-Amz-Signature");
    assert_eq!(signature.len(), 64, "{url}");
    assert!(
        signature.chars().all(|c| c.is_ascii_hexdigit()),
        "签名必须是十六进制：{url}"
    );
}

// —— 去重与上传 ——

#[test]
fn publish_skips_the_upload_when_the_object_already_exists() {
    let dir = temp_dir("dedupe");
    let file = write_file(&dir, "a.gif", GIF);
    let transport = ScriptedTransport::new();
    transport.respond(200); // HEAD → 已存在

    storage(config(), transport.clone())
        .publish(&file, "gd", "sub", "a.gif")
        .unwrap();

    assert_eq!(
        transport.methods(),
        ["HEAD"],
        "同 hash 去重：对象已存在时零 PUT"
    );
    assert_eq!(transport.urls()[0], "http://ep:9000/b/up/gd/sub/a.gif");
    assert!(!file.exists(), "去重命中后本地临时文件必须删除");
    assert_eq!(transport.remaining(), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn publish_uploads_and_then_removes_the_local_file() {
    let dir = temp_dir("put");
    let file = write_file(&dir, "a.gif", GIF);
    let transport = ScriptedTransport::new();
    transport.respond(404); // HEAD → 不存在
    transport.respond(200); // PUT → 成功

    storage(config(), transport.clone())
        .publish(&file, "gd", "sub", "a.gif")
        .unwrap();

    assert_eq!(transport.methods(), ["HEAD", "PUT"]);
    let requests = transport.requests();
    assert_eq!(requests[1].url, "http://ep:9000/b/up/gd/sub/a.gif");
    assert_eq!(
        header(&requests[1], "Content-Type"),
        Some("image/gif"),
        "Content-Type 必须按本地文件真实内容探测"
    );

    match &requests[1].body {
        HttpBody::File { path, offset, len } => {
            assert_eq!(path, &file);
            assert_eq!(
                (*offset, *len),
                (0, GIF.len() as u64),
                "必须只发该文件的字节"
            );
        }
        other => panic!("PUT 必须带文件体，实际 {other:?}"),
    }

    assert_signature_is_consistent(&requests[1]);
    assert!(!file.exists(), "上传成功后本地 .part 必须删除");
    assert_eq!(transport.remaining(), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn publish_wraps_upload_failures_and_keeps_the_local_file() {
    let dir = temp_dir("put-fail");
    let file = write_file(&dir, "a.gif", GIF);
    let transport = ScriptedTransport::new();
    transport.respond(404);
    for _ in 0..3 {
        transport.respond_full(
            500,
            Vec::new(),
            b"<Error><Code>InternalError</Code><Message>boom</Message></Error>",
        );
    }

    let error = storage(config(), transport.clone())
        .publish(&file, "gd", "sub", "a.gif")
        .unwrap_err();

    assert!(
        matches!(error, Error::RenameResourceFail),
        "对外统一为上传链路既有错误键，实际 {error:?}"
    );
    assert!(
        file.exists(),
        "上传失败时本地文件必须保留，交由 UploadController 的 cleanup 兜底"
    );
    assert_eq!(
        transport.methods(),
        ["HEAD", "PUT", "PUT", "PUT"],
        "5xx 重试 3 次后才放弃"
    );
    assert_eq!(transport.remaining(), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn publish_wraps_transport_failures_on_the_existence_check() {
    let dir = temp_dir("head-fail");
    let file = write_file(&dir, "a.gif", GIF);
    let transport = ScriptedTransport::new();
    for _ in 0..3 {
        transport.fail("dns lookup failed");
    }

    let error = storage(config(), transport.clone())
        .publish(&file, "gd", "sub", "a.gif")
        .unwrap_err();

    assert!(matches!(error, Error::RenameResourceFail));
    assert!(file.exists());
    assert_eq!(transport.methods(), ["HEAD", "HEAD", "HEAD"]);

    let _ = std::fs::remove_dir_all(&dir);
}

// —— multipart ——

#[test]
fn publish_routes_through_multipart_above_the_threshold() {
    let dir = temp_dir("multipart");
    let file = write_file(&dir, "a.gif", GIF);
    let transport = ScriptedTransport::new();
    transport.respond(404);
    transport.respond_full(200, Vec::new(), b"<UploadId>u-1</UploadId>");
    transport.respond_full(200, vec![("etag", "\"e1\"")], &[]);
    transport.respond(200);

    let mut config = config();
    config.multipart_threshold = 10; // GIF 有 42 字节 > 10
    storage(config, transport.clone())
        .publish(&file, "gd", "sub", "a.gif")
        .unwrap();

    assert_eq!(
        transport.methods(),
        ["HEAD", "POST", "PUT", "POST"],
        "超过阈值必须走 multipart 编排"
    );

    let requests = transport.requests();
    assert_eq!(requests[1].url, "http://ep:9000/b/up/gd/sub/a.gif?uploads=");
    assert_eq!(
        requests[2].url,
        "http://ep:9000/b/up/gd/sub/a.gif?partNumber=1&uploadId=u-1"
    );
    assert_eq!(
        requests[3].url,
        "http://ep:9000/b/up/gd/sub/a.gif?uploadId=u-1"
    );
    assert_eq!(
        header(&requests[3], "Content-Type"),
        Some("application/xml")
    );

    match &requests[3].body {
        HttpBody::Bytes(xml) => assert_eq!(
            String::from_utf8_lossy(xml),
            "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>\"e1\"</ETag></Part></CompleteMultipartUpload>",
            "complete 请求体必须逐字节等于约定 XML（ETag 的双引号是值的一部分）"
        ),
        other => panic!("complete 必须带 XML body，实际 {other:?}"),
    }

    assert!(!file.exists());
    assert_eq!(transport.remaining(), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 分片几何 + 分片哈希覆盖面：文件 = 64MiB + 16B，正好两片。
#[test]
fn multipart_part_hashes_cover_the_slice_not_the_whole_file() {
    let dir = temp_dir("parts");
    let mut data = Vec::with_capacity(PART_SIZE as usize + 16);
    let block: Vec<u8> = (0..(1 << 20)).map(|index| (index % 251) as u8).collect();
    for _ in 0..64 {
        data.extend_from_slice(&block);
    }
    data.extend_from_slice(b"TAIL-16-BYTES!!!");
    assert_eq!(data.len() as u64, PART_SIZE + 16, "前置条件：两片");

    let file = write_file(&dir, "large.bin", &data);
    let transport = ScriptedTransport::new();
    transport.respond(404);
    transport.respond_full(200, Vec::new(), b"<UploadId>u-1</UploadId>");
    transport.respond_full(200, vec![("etag", "\"e1\"")], &[]);
    transport.respond_full(200, vec![("etag", "\"e2\"")], &[]);
    transport.respond(200);

    let mut config = config();
    config.multipart_threshold = 1;
    storage(config, transport.clone())
        .publish(&file, "gd", "sub", "large.bin")
        .unwrap();

    let requests = transport.requests();
    assert_eq!(
        requests.len(),
        5,
        "HEAD + create + part1 + part2 + complete"
    );

    let (offset1, len1) = file_slice(&requests[2]);
    let (offset2, len2) = file_slice(&requests[3]);
    assert_eq!((offset1, len1), (0, PART_SIZE));
    assert_eq!((offset2, len2), (PART_SIZE, 16));

    // 分片签名必须覆盖「该分片自己的字节」——用整文件哈希签分片会被真 S3 拒绝
    let part1 = header(&requests[2], "x-amz-content-sha256").unwrap();
    let part2 = header(&requests[3], "x-amz-content-sha256").unwrap();
    assert_eq!(part1, sigv4::sha256_hex(&data[..PART_SIZE as usize]));
    assert_eq!(part2, sigv4::sha256_hex(&data[PART_SIZE as usize..]));
    assert_ne!(part1, sigv4::sha256_hex(&data), "分片签名不得用整文件哈希");
    assert_ne!(part2, sigv4::sha256_hex(&data));

    for request in &requests {
        assert_signature_is_consistent(request);
    }
    assert!(!file.exists());
    assert_eq!(transport.remaining(), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

fn file_slice(request: &HttpRequest) -> (u64, u64) {
    match &request.body {
        HttpBody::File { offset, len, .. } => (*offset, *len),
        other => panic!("分片必须是文件切片，实际 {other:?}"),
    }
}

#[test]
fn failed_part_aborts_the_upload_and_wraps_the_error() {
    let dir = temp_dir("abort");
    let file = write_file(&dir, "a.gif", GIF);
    let transport = ScriptedTransport::new();
    transport.respond(404);
    transport.respond_full(200, Vec::new(), b"<UploadId>u-1</UploadId>");
    for _ in 0..3 {
        transport.respond_full(
            500,
            Vec::new(),
            b"<Error><Code>InternalError</Code><Message>boom</Message></Error>",
        );
    }
    transport.respond(204); // AbortMultipartUpload

    let mut config = config();
    config.multipart_threshold = 1;
    let error = storage(config, transport.clone())
        .publish(&file, "gd", "sub", "a.gif")
        .unwrap_err();

    assert!(matches!(error, Error::RenameResourceFail));
    assert_eq!(
        transport.methods(),
        ["HEAD", "POST", "PUT", "PUT", "PUT", "DELETE"]
    );
    let urls = transport.urls();
    assert!(
        urls.last().unwrap().ends_with("?uploadId=u-1"),
        "分片失败必须发 AbortMultipartUpload，实际 {:?}",
        urls.last()
    );
    assert!(file.exists(), "失败时本地文件必须保留");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 经典陷阱：HTTP 200 但体内嵌 `<Error>` 仍是失败 → abort + 报错。
#[test]
fn complete_with_embedded_error_aborts_and_fails() {
    let dir = temp_dir("embedded-error");
    let file = write_file(&dir, "a.gif", GIF);
    let transport = ScriptedTransport::new();
    transport.respond(404);
    transport.respond_full(200, Vec::new(), b"<UploadId>u-1</UploadId>");
    transport.respond_full(200, vec![("etag", "\"e1\"")], &[]);
    transport.respond_full(
        200,
        Vec::new(),
        b"<Error><Code>InternalError</Code><Message>x</Message></Error>",
    );
    transport.respond(404); // 失败后的 HEAD：对象不在
    transport.respond(204); // AbortMultipartUpload

    let mut config = config();
    config.multipart_threshold = 1;
    let error = storage(config, transport.clone())
        .publish(&file, "gd", "sub", "a.gif")
        .unwrap_err();

    assert!(matches!(error, Error::RenameResourceFail));
    assert_eq!(
        transport.methods(),
        ["HEAD", "POST", "PUT", "POST", "HEAD", "DELETE"]
    );
    assert!(file.exists());

    let _ = std::fs::remove_dir_all(&dir);
}

/// complete 的响应丢了但对象其实已写入 → HEAD 确认后视为成功（不发 abort）。
#[test]
fn lost_complete_response_is_rescued_by_the_head_check() {
    let dir = temp_dir("lost-complete");
    let file = write_file(&dir, "a.gif", GIF);
    let transport = ScriptedTransport::new();
    transport.respond(404);
    transport.respond_full(200, Vec::new(), b"<UploadId>u-1</UploadId>");
    transport.respond_full(200, vec![("etag", "\"e1\"")], &[]);
    for _ in 0..3 {
        transport.fail("timeout"); // complete 三次都网络异常
    }
    transport.respond(200); // HEAD 确认对象存在

    let mut config = config();
    config.multipart_threshold = 1;
    storage(config, transport.clone())
        .publish(&file, "gd", "sub", "a.gif")
        .unwrap();

    assert_eq!(
        transport.methods(),
        ["HEAD", "POST", "PUT", "POST", "POST", "POST", "HEAD"],
        "complete 重试 3 次后 HEAD 兜底"
    );
    assert!(
        !transport.methods().contains(&"DELETE".to_string()),
        "成功路径不得发 abort"
    );
    assert!(!file.exists());
    assert_eq!(transport.remaining(), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

// —— exists / delete ——

#[test]
fn exists_maps_statuses_to_booleans_and_never_swallows_other_errors() {
    let transport = ScriptedTransport::new();
    let storage = storage(config(), transport.clone());

    transport.respond(200);
    assert!(storage.exists("gd", "sub", "a.gif").unwrap());
    assert_eq!(transport.urls()[0], "http://ep:9000/b/up/gd/sub/a.gif");

    transport.respond(404);
    assert!(!storage.exists("gd", "sub", "a.gif").unwrap());

    transport.respond_full(
        403,
        Vec::new(),
        b"<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>",
    );
    let error = storage.exists("gd", "sub", "a.gif").unwrap_err();
    let message = format!("{error}");
    assert!(message.contains("HTTP 403"), "{message}");
    assert!(message.contains("AccessDenied"), "{message}");
    assert!(message.contains("up/gd/sub/a.gif"), "{message}");
    assert_eq!(
        transport.methods(),
        ["HEAD", "HEAD", "HEAD"],
        "403 不重试，也不能当成「不存在」"
    );
}

#[test]
fn delete_is_idempotent_and_retries_server_errors() {
    let transport = ScriptedTransport::new();
    let storage = storage(config(), transport.clone());

    transport.respond(204);
    storage.delete("gd", "sub", "a.gif").unwrap();

    transport.respond(404);
    storage.delete("gd", "sub", "a.gif").unwrap();

    assert_eq!(
        transport.methods(),
        ["DELETE", "DELETE"],
        "404 是幂等成功，不得重试"
    );
    assert_eq!(
        transport.urls()[1],
        "http://ep:9000/b/up/gd/sub/a.gif",
        "删除走的是对象键"
    );

    for _ in 0..3 {
        transport.respond_full(
            500,
            Vec::new(),
            b"<Error><Code>InternalError</Code><Message>boom</Message></Error>",
        );
    }
    let error = storage.delete("gd", "sub", "a.gif").unwrap_err();
    let message = format!("{error}");
    assert!(message.contains("HTTP 500"), "{message}");
    assert_eq!(transport.methods().len(), 5, "500 重试至 3 次后抛");
}

// —— 预签名 ——

#[test]
fn url_presigns_a_download_and_never_leaks_the_secret() {
    let transport = ScriptedTransport::new();
    let storage = storage(config(), transport.clone());

    let plain = storage.url("gd", "sub", "a.gif", &[]).unwrap().unwrap();

    assert!(
        plain.starts_with("http://ep:9000/b/up/gd/sub/a.gif?"),
        "{plain}"
    );
    assert!(
        plain.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"),
        "{plain}"
    );
    assert!(plain.contains("X-Amz-Credential=AKIDEXAMPLE%2F"), "{plain}");
    assert!(
        plain.contains("X-Amz-Expires=300"),
        "默认有效期与 PHP 的 PRESIGN_TTL 一致"
    );
    assert!(plain.contains("X-Amz-SignedHeaders=host"), "{plain}");
    assert!(!plain.contains(SECRET), "预签名 URL 不得泄漏 secret_key");
    assert!(!plain.contains("response-content-disposition"), "{plain}");
    assert_hex_signature(&plain);

    let disposition = "attachment; filename=\"a b.gif\"; filename*=UTF-8''a%20b.gif";
    let with_params = storage
        .url(
            "gd",
            "sub",
            "a.gif",
            &[(
                "response-content-disposition".to_string(),
                disposition.to_string(),
            )],
        )
        .unwrap()
        .unwrap();

    assert!(
        with_params.contains(&format!(
            "response-content-disposition={}",
            sigv4::uri_encode(disposition.as_bytes(), true)
        )),
        "responseParams 必须只编码一次后进入签名查询串：{with_params}"
    );
    assert!(with_params.contains("%20"), "空格必须编码为 %20");
    assert_hex_signature(&with_params);

    assert!(
        transport.requests().is_empty(),
        "预签名是纯本地计算，不得发出任何 HTTP 请求"
    );
}

// —— payload_signing = unsigned ——

#[test]
fn unsigned_payload_mode_signs_over_the_token_on_every_request() {
    let dir = temp_dir("unsigned");
    let file = write_file(&dir, "a.gif", GIF);
    let transport = ScriptedTransport::new();
    transport.respond(404);
    transport.respond_full(200, Vec::new(), b"<UploadId>u-1</UploadId>");
    transport.respond_full(200, vec![("etag", "\"e1\"")], &[]);
    transport.respond(200);

    let mut config = config();
    config.multipart_threshold = 1; // 强制走 multipart：create / part / complete 都受模式影响
    config.payload_signing = PayloadSigning::Unsigned;
    storage(config, transport.clone())
        .publish(&file, "gd", "sub", "a.gif")
        .unwrap();

    let requests = transport.requests();
    assert_eq!(requests.len(), 4);
    for request in &requests {
        assert_eq!(
            header(request, "x-amz-content-sha256"),
            Some(UNSIGNED_PAYLOAD),
            "unsigned 模式下 {} 也必须发占位符",
            request.method
        );
        // 复算用的正是 x-amz-content-sha256 的值 —— 若实现仍按真实哈希签，这里必红
        assert_signature_is_consistent(request);
        assert!(!header(request, "Authorization").unwrap().contains(SECRET));
    }

    assert!(!file.exists());
    assert_eq!(transport.remaining(), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

// —— 未接传输 ——

#[test]
fn missing_transport_fails_loudly_instead_of_silently() {
    let storage = S3Storage::new(config(), Path::new("/nonexistent")).unwrap();

    let error = storage.exists("gd", "sub", "a.gif").unwrap_err();
    assert!(format!("{error}").contains("HttpTransport"), "{error}");

    // publish 边界仍然归一到上传链路的错误键（细节不对外）
    let dir = temp_dir("null-transport");
    let file = write_file(&dir, "a.gif", GIF);
    let error = storage.publish(&file, "gd", "sub", "a.gif").unwrap_err();
    assert!(matches!(error, Error::RenameResourceFail));
    assert!(file.exists());

    let _ = std::fs::remove_dir_all(&dir);
}

// —— 内存对象存储：真跑一遍编排 ——

#[test]
fn end_to_end_against_an_in_memory_object_store() {
    let store = MemoryS3::new();
    let mut config = config();
    config.endpoint = String::new(); // AWS 默认端点：路径形如 /b/<key>
    let storage = storage(config, store.clone());

    let dir = temp_dir("e2e");
    let file = write_file(&dir, "a.gif", GIF);

    storage.publish(&file, "gd", "sub", "a.gif").unwrap();
    assert!(!file.exists(), "上传成功后本地临时文件必须删除");
    assert_eq!(store.object("up/gd/sub/a.gif").as_deref(), Some(GIF));
    assert!(storage.exists("gd", "sub", "a.gif").unwrap());

    // 同 hash 去重：第二次 publish 一个字节都不再写
    let again = write_file(&dir, "again.gif", GIF);
    storage.publish(&again, "gd", "sub", "a.gif").unwrap();
    assert!(!again.exists());
    assert_eq!(store.puts(), 1, "去重命中时不得重复上传");
    assert_eq!(
        store.calls(),
        ["HEAD", "PUT", "HEAD", "HEAD"],
        "去重命中只多发一个 HEAD，不再 PUT"
    );

    // 预签名 URL 指向内存对象（AWS 默认端点 + path style）
    let url = storage.url("gd", "sub", "a.gif", &[]).unwrap().unwrap();
    assert!(
        url.starts_with("https://s3.us-east-1.amazonaws.com/b/up/gd/sub/a.gif?"),
        "{url}"
    );

    // 删除幂等：第一次真删，第二次服务端 404 也算成功
    storage.delete("gd", "sub", "a.gif").unwrap();
    assert!(!storage.exists("gd", "sub", "a.gif").unwrap());
    storage.delete("gd", "sub", "a.gif").unwrap();

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn multipart_end_to_end_against_an_in_memory_object_store() {
    let store = MemoryS3::new();
    let mut config = config();
    config.endpoint = String::new();
    config.multipart_threshold = 4; // 强制 multipart
    let storage = storage(config, store.clone());

    let dir = temp_dir("e2e-multipart");
    let mut payload = GIF.to_vec();
    payload.extend(std::iter::repeat_n(0x2a, 100));
    let file = write_file(&dir, "a.gif", &payload);

    storage.publish(&file, "gd", "sub", "a.gif").unwrap();

    assert_eq!(
        store.object("up/gd/sub/a.gif").as_deref(),
        Some(payload.as_slice()),
        "分片拼装后的对象必须与原文件逐字节相同"
    );
    assert_eq!(
        store.calls(),
        ["HEAD", "POST", "PUT", "POST"],
        "create → part → complete"
    );
    assert_eq!(store.pending_uploads(), 0, "complete 后不得留下未完成分片");
    assert!(!file.exists());

    let _ = std::fs::remove_dir_all(&dir);
}
