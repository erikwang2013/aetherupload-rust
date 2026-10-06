// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 协议级端到端：不经过任何 Web 框架，直接走两个内核入口，逐条核对 PHP 版的行为。
//!
//! 覆盖：完整上传链路、断线续传（幂等跳过 / 序号跳变 / 截断分块）、秒传、
//! 参数与路径安全边界、大小与扩展名限制、完整性校验失败、事件、展示与下载、
//! 删除资源联动清理秒传索引。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aetherupload::config::{Config, GroupConfig};
use aetherupload::console;
use aetherupload::events::EventSink;
use aetherupload::instant::MemoryInstantStore;
use aetherupload::json::{PreprocessResult, SaveChunkResult};
use aetherupload::md5::md5_hex;
use aetherupload::{
    ChunkBody, PreprocessRequest, ResourceController, ResourceResponse, Runtime, SaveChunkRequest,
    UploadController,
};

// ------------------------------------------------------------------ 测试脚手架

struct Harness {
    base: PathBuf,
    runtime: Arc<Runtime>,
}

impl Harness {
    /// 建临时项目根 + 跑一遍 `groups`（与真实部署的先决条件一致：
    /// 分组目录必须先存在，内核不递归建目录）。
    fn new(tag: &str) -> Self {
        Self::with_config(tag, Config::default())
    }

    /// 秒传开启的脚手架（默认配置里 instant_completion = false）。
    fn with_instant(tag: &str) -> Self {
        let config = Config {
            instant_completion: true,
            ..Config::default()
        };

        Self::with_config(tag, config)
    }

    fn with_config(tag: &str, config: Config) -> Self {
        let base = std::env::temp_dir().join(format!(
            "aetherupload-protocol-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);

        let runtime = Arc::new(
            Runtime::new(config, &base)
                .expect("运行时装配")
                .with_instant(Arc::new(MemoryInstantStore::new())),
        );

        let mut lines = Vec::new();
        assert_eq!(
            console::list_groups(&runtime, &mut |line| lines.push(line.to_string())),
            0,
            "建目录失败：{lines:?}"
        );

        Self { base, runtime }
    }

    fn uploader(&self) -> UploadController {
        UploadController::new(self.runtime.clone())
    }

    fn resources(&self) -> ResourceController {
        ResourceController::new(self.runtime.clone())
    }

    fn upload_root(&self) -> PathBuf {
        self.runtime.upload_root().to_path_buf()
    }

    /// 预处理一个指定内容的上传。
    fn preprocess(&self, name: &str, content: &[u8], group: &str) -> PreprocessResult {
        self.uploader().preprocess(&PreprocessRequest {
            resource_name: Some(name.to_string()),
            resource_size: Some(content.len().to_string()),
            group: Some(group.to_string()),
            resource_hash: Some(md5_hex(content)),
            locale: Some("zh".into()),
        })
    }

    /// 按分块大小切分并逐块上传，返回末块的响应。
    fn upload_chunks(
        &self,
        pre: &PreprocessResult,
        content: &[u8],
        group: &str,
        chunk_size: usize,
    ) -> SaveChunkResult {
        let chunks: Vec<&[u8]> = content.chunks(chunk_size).collect();
        let total = chunks.len();

        let mut last = SaveChunkResult::default();
        for (index, chunk) in chunks.into_iter().enumerate() {
            last = self.uploader().save_chunk(&SaveChunkRequest {
                chunk_total: Some(total.to_string()),
                chunk_index: Some((index + 1).to_string()),
                resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
                resource_ext: Some(pre.resource_ext.clone()),
                group_subdir: Some(pre.group_sub_dir.clone()),
                group: Some(group.to_string()),
                resource_hash: Some(md5_hex(content)),
                locale: Some("zh".into()),
                chunk: Some(ChunkBody::Bytes(chunk.to_vec())),
            });
        }

        last
    }

    /// 走一遍完整链路，返回 (savedPath, 磁盘上的成品路径)。
    fn upload(&self, name: &str, content: &[u8]) -> (String, PathBuf) {
        let pre = self.preprocess(name, content, "file");
        assert_eq!(pre.error, None, "预处理失败：{:?}", pre.error);

        let last = self.upload_chunks(&pre, content, "file", 7);
        assert_eq!(last.error, None, "上传失败：{:?}", last.error);
        assert!(!last.saved_path.is_empty(), "末块必须给出 savedPath");

        let expected = self
            .upload_root()
            .join("file")
            .join(&pre.group_sub_dir)
            .join(format!("{}.{}", md5_hex(content), pre.resource_ext));

        (last.saved_path, expected)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

// ------------------------------------------------------------------ 完整链路

#[test]
fn full_upload_flow_lands_md5_named_file() {
    let harness = Harness::new("flow");
    let content = b"the quick brown fox jumps over the lazy dog".repeat(20);

    let (saved_path, expected) = harness.upload("报告.pdf", &content);

    // savedPath 三段式：分组_子目录_文件名，文件名 = 内容 md5 + 原扩展名
    let parts: Vec<&str> = saved_path.splitn(3, '_').collect();
    assert_eq!(parts.len(), 3, "savedPath 应为三段：{saved_path}");
    assert_eq!(parts[0], "file");
    assert_eq!(parts[2], format!("{}.pdf", md5_hex(&content)));

    assert!(expected.is_file(), "成品未落盘：{}", expected.display());
    assert_eq!(
        std::fs::read(&expected).unwrap(),
        content,
        "内容必须逐字节一致"
    );

    // 临时文件与断点都应清理干净
    let header_dir = harness.upload_root().join("_header");
    assert_eq!(
        std::fs::read_dir(&header_dir).unwrap().count(),
        0,
        "断点文件应已删除"
    );

    let leftovers: Vec<_> = std::fs::read_dir(expected.parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().to_string_lossy().ends_with(".part"))
        .collect();
    assert!(leftovers.is_empty(), "分块文件应已改名：{leftovers:?}");
}

#[test]
fn display_and_download_serve_the_file() {
    let harness = Harness::new("serve");
    let content = "图片内容（假装是 jpg）".as_bytes().to_vec();
    let (saved_path, _) = harness.upload("photo.jpg", &content);

    match harness.resources().display(&saved_path) {
        ResourceResponse::ServeFile {
            path,
            download_name,
        } => {
            assert_eq!(std::fs::read(path).unwrap(), content);
            assert!(download_name.is_none(), "jpg 可内联展示");
        }
        other => panic!("预期本地文件响应，实际 {other:?}"),
    }

    match harness.resources().download(&saved_path, "改名后的图片") {
        ResourceResponse::ServeFile { download_name, .. } => {
            assert_eq!(download_name.as_deref(), Some("改名后的图片.jpg"));
        }
        other => panic!("预期本地文件响应，实际 {other:?}"),
    }

    // 不存在的资源 → 404 文本
    assert_eq!(
        harness.resources().display("file_202610_deadbeef.jpg"),
        ResourceResponse::NotFound("display fail")
    );
}

#[test]
fn inline_blocked_extensions_are_forced_to_attachment() {
    let harness = Harness::new("inline");
    let content = b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>".to_vec();
    let (saved_path, _) = harness.upload("icon.svg", &content);

    match harness.resources().display(&saved_path) {
        ResourceResponse::ServeFile { download_name, .. } => {
            // 落盘文件名是内容 md5（与 PHP 一致），附件名就是它
            assert_eq!(
                download_name.as_deref(),
                Some(format!("{}.svg", md5_hex(&content)).as_str()),
                "svg 可内联渲染，必须强制转附件（否则变成同源 XSS）"
            );
        }
        other => panic!("预期本地文件响应，实际 {other:?}"),
    }
}

// ------------------------------------------------------------------ 秒传

#[test]
fn instant_completion_skips_the_second_upload() {
    let harness = Harness::with_instant("instant");
    let content = b"repeat me".repeat(50);

    let (first_path, file) = harness.upload("data.txt", &content);

    // 第二次上传同样的内容：预处理阶段就命中，一个分块都不用传
    let pre = harness.preprocess("data.txt", &content, "file");
    assert_eq!(pre.error, None);
    assert_eq!(pre.saved_path, first_path, "秒传应返回既有 savedPath");
    assert!(file.is_file());

    // 复制一份再传：秒传命中不影响磁盘
    let before = std::fs::read(&file).unwrap();
    let _ = harness.upload("data.txt", &content);
    assert_eq!(std::fs::read(&file).unwrap(), before);
}

#[test]
fn instant_completion_stays_off_when_disabled() {
    let harness = Harness::new("instant-off");
    let content = b"same bytes".to_vec();

    let (first_path, _) = harness.upload("a.txt", &content);

    // 默认 instant_completion = false：不会命中秒传，但同 hash 会去重落到同一个文件名
    let (second_path, _) = harness.upload("a.txt", &content);
    assert_eq!(
        first_path, second_path,
        "同内容同扩展名 → 同文件名（去重语义）"
    );
}

// ------------------------------------------------------------------ 断线续传

#[test]
fn resend_is_idempotent_and_gaps_are_rejected() {
    let harness = Harness::new("resume");
    let content = b"0123456789abcdefghijklmnopqrstuvwxyz".to_vec();
    let chunk = |index: usize| {
        content[(index * 6).min(content.len())..((index + 1) * 6).min(content.len())].to_vec()
    };

    let pre = harness.preprocess("resume.txt", &content, "file");
    assert_eq!(pre.error, None);

    let send = |index: usize, body: Vec<u8>| {
        harness.uploader().save_chunk(&SaveChunkRequest {
            chunk_total: Some("6".into()),
            chunk_index: Some(index.to_string()),
            resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
            resource_ext: Some("txt".into()),
            group_subdir: Some(pre.group_sub_dir.clone()),
            group: Some("file".into()),
            resource_hash: Some(md5_hex(&content)),
            locale: Some("en".into()),
            chunk: Some(ChunkBody::Bytes(body)),
        })
    };

    // 第 1 块
    assert_eq!(send(1, chunk(0)).error, None);

    // 重发第 1 块：幂等跳过，不报错也不重复写入
    assert_eq!(send(1, chunk(0)).error, None);

    // 跳到第 3 块：中间缺块 → 报错，且**不清理**已拼好的进度
    let gap = send(3, chunk(2));
    assert!(gap.error.is_some(), "序号跳变必须报错");

    // 补齐第 2 块，回到正轨
    assert_eq!(send(2, chunk(1)).error, None);

    // 下一块（第 3 块）不带分块本体：报错但不清理
    let mut missing = SaveChunkRequest {
        chunk_total: Some("6".into()),
        chunk_index: Some("3".into()),
        resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
        resource_ext: Some("txt".into()),
        group_subdir: Some(pre.group_sub_dir.clone()),
        group: Some("file".into()),
        resource_hash: Some(md5_hex(&content)),
        locale: Some("en".into()),
        chunk: None,
    };
    assert!(harness.uploader().save_chunk(&missing).error.is_some());

    // 补上本体即可继续
    missing.chunk = Some(ChunkBody::Bytes(chunk(2)));
    assert_eq!(harness.uploader().save_chunk(&missing).error, None);

    assert_eq!(send(4, chunk(3)).error, None);
    assert_eq!(send(5, chunk(4)).error, None);
    let last = send(6, chunk(5));

    assert_eq!(last.error, None, "补齐后应能完成：{:?}", last.error);
    assert!(!last.saved_path.is_empty());
}

#[test]
fn wrong_hash_discards_the_whole_upload() {
    let harness = Harness::new("badhash");
    let content = b"real content".to_vec();

    let pre = harness.preprocess("x.txt", &content, "file");
    assert_eq!(pre.error, None);

    // 客户端声明的 hash 与真实内容不符 → 末块整份丢弃
    let result = harness.uploader().save_chunk(&SaveChunkRequest {
        chunk_total: Some("1".into()),
        chunk_index: Some("1".into()),
        resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
        resource_ext: Some("txt".into()),
        group_subdir: Some(pre.group_sub_dir.clone()),
        group: Some("file".into()),
        resource_hash: Some(md5_hex(b"something else")),
        locale: Some("en".into()),
        chunk: Some(ChunkBody::Bytes(content)),
    });

    assert!(result.error.is_some(), "错误 hash 必须被拒");
    assert_eq!(result.saved_path, "");

    // .part 与断点都必须清理
    let dir = harness.upload_root().join("file").join(&pre.group_sub_dir);
    if dir.is_dir() {
        let parts: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".part"))
            .collect();
        assert!(parts.is_empty(), "失败后 .part 必须清理：{parts:?}");
    }
}

#[test]
fn lax_mode_skips_the_hash_check() {
    let harness = Harness::new("lax");
    let content = b"lax content".to_vec();

    let pre = harness.preprocess("lax.txt", &content, "file");
    assert_eq!(pre.error, None);

    let result = harness.uploader().save_chunk(&SaveChunkRequest {
        chunk_total: Some("1".into()),
        chunk_index: Some("1".into()),
        resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
        resource_ext: Some("txt".into()),
        group_subdir: Some(pre.group_sub_dir.clone()),
        group: Some("file".into()),
        resource_hash: Some(md5_hex(b"wrong")),
        locale: Some("en".into()),
        chunk: Some(ChunkBody::Bytes(content.clone())),
    });

    // 默认（lax_mode = false）拒绝
    assert!(result.error.is_some());

    // 宽松模式：跳过比对，照常落盘（文件名仍是真实内容的 md5）
    let lax = send_lax(&harness, &content);
    assert_eq!(lax.error, None, "{:?}", lax.error);
    assert!(
        lax.saved_path
            .ends_with(&format!("{}.txt", md5_hex(&content)))
    );
}

fn send_lax(harness: &Harness, content: &[u8]) -> SaveChunkResult {
    // 直接改配置快照不可行（快照在调用内解析），这里用宽松模式的项目配置另开一套
    let config = Config {
        lax_mode: true,
        ..Config::default()
    };

    let base = harness.base.join("lax-project");
    let runtime = Arc::new(
        Runtime::new(config, &base)
            .unwrap()
            .with_instant(Arc::new(MemoryInstantStore::new())),
    );

    let mut lines = Vec::new();
    console::list_groups(&runtime, &mut |line| lines.push(line.to_string()));

    let uploader = UploadController::new(runtime);
    let pre = uploader.preprocess(&PreprocessRequest {
        resource_name: Some("lax.txt".into()),
        resource_size: Some(content.len().to_string()),
        group: Some("file".into()),
        resource_hash: Some("deadbeef".into()),
        locale: Some("en".into()),
    });

    uploader.save_chunk(&SaveChunkRequest {
        chunk_total: Some("1".into()),
        chunk_index: Some("1".into()),
        resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
        resource_ext: Some("txt".into()),
        group_subdir: Some(pre.group_sub_dir.clone()),
        group: Some("file".into()),
        resource_hash: Some("deadbeef".into()),
        locale: Some("en".into()),
        chunk: Some(ChunkBody::Bytes(content.to_vec())),
    })
}

// ------------------------------------------------------------------ 参数与安全边界

#[test]
fn missing_or_invalid_parameters_are_rejected() {
    let harness = Harness::new("params");
    let uploader = harness.uploader();

    // 必填缺失
    let pre = uploader.preprocess(&PreprocessRequest {
        resource_name: None,
        resource_size: Some("10".into()),
        group: Some("file".into()),
        locale: Some("zh".into()),
        ..Default::default()
    });
    assert!(pre.error.is_some());

    // 分组名含下划线：会破坏 savedPath 三段式解码
    let pre = uploader.preprocess(&PreprocessRequest {
        resource_name: Some("a.txt".into()),
        resource_size: Some("10".into()),
        group: Some("my_file".into()),
        locale: Some("zh".into()),
        ..Default::default()
    });
    assert_eq!(pre.error.as_deref(), Some("错误：非法操作"));

    // 未配置的分组
    let pre = uploader.preprocess(&PreprocessRequest {
        resource_name: Some("a.txt".into()),
        resource_size: Some("10".into()),
        group: Some("nope".into()),
        locale: Some("zh".into()),
        ..Default::default()
    });
    assert!(pre.error.is_some());

    // 声明大小超出分组上限
    let pre = uploader.preprocess(&PreprocessRequest {
        resource_name: Some("a.txt".into()),
        resource_size: Some("999999999999".into()),
        group: Some("file".into()),
        locale: Some("zh".into()),
        ..Default::default()
    });
    assert_eq!(pre.error.as_deref(), Some("错误：无效的文件大小"));

    // 声明大小为 0
    let pre = uploader.preprocess(&PreprocessRequest {
        resource_name: Some("a.txt".into()),
        resource_size: Some("0".into()),
        group: Some("file".into()),
        locale: Some("zh".into()),
        ..Default::default()
    });
    assert!(pre.error.is_some());

    // 黑名单扩展名（.php 在默认黑名单里）
    let pre = uploader.preprocess(&PreprocessRequest {
        resource_name: Some("shell.php".into()),
        resource_size: Some("10".into()),
        group: Some("file".into()),
        locale: Some("zh".into()),
        ..Default::default()
    });
    assert_eq!(pre.error.as_deref(), Some("错误：无效的文件类型"));
}

#[test]
fn chunk_parameters_are_validated() {
    let harness = Harness::new("chunk-params");
    let content = b"abc".to_vec();
    let pre = harness.preprocess("a.txt", &content, "file");
    assert_eq!(pre.error, None);

    let send = |index: &str, total: &str, subdir: &str| {
        harness.uploader().save_chunk(&SaveChunkRequest {
            chunk_total: Some(total.into()),
            chunk_index: Some(index.into()),
            resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
            resource_ext: Some("txt".into()),
            group_subdir: Some(subdir.into()),
            group: Some("file".into()),
            resource_hash: Some(md5_hex(&content)),
            locale: Some("en".into()),
            chunk: Some(ChunkBody::Bytes(content.clone())),
        })
    };

    // 序号不是纯数字 / 小于 1
    assert!(send("abc", "1", &pre.group_sub_dir).error.is_some());
    assert!(send("0", "1", &pre.group_sub_dir).error.is_some());
    // 总数上限 10000
    assert!(send("1", "10001", &pre.group_sub_dir).error.is_some());
    // group_subdir 含下划线 → 参数错误
    assert!(send("1", "1", "2026_10").error.is_some());
    // group_subdir 目录穿越 → 参数错误
    assert!(send("1", "1", "..").error.is_some());
    assert!(send("1", "1", "../etc").error.is_some());
}

#[test]
fn oversized_chunks_are_rejected_incrementally() {
    let mut config = Config::default();
    config.groups.insert(
        "tiny".into(),
        GroupConfig {
            group_dir: "file".into(), // 复用同一目录，省一个 mkdir
            resource_maxsize: 100,
            resource_extensions: vec!["txt".into()],
            event_before_upload_complete: false,
            event_upload_complete: false,
        },
    );

    let harness = Harness::with_config("oversize", config);
    let content = vec![b'x'; 60]; // 声明 60 字节（合法），但第二块会把它顶过 100

    let pre = harness.preprocess("big.txt", &content, "tiny");
    assert_eq!(pre.error, None, "{:?}", pre.error);

    let send = |index: usize, body: Vec<u8>| {
        harness.uploader().save_chunk(&SaveChunkRequest {
            chunk_total: Some("2".into()),
            chunk_index: Some(index.to_string()),
            resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
            resource_ext: Some("txt".into()),
            group_subdir: Some(pre.group_sub_dir.clone()),
            group: Some("tiny".into()),
            resource_hash: Some(md5_hex(&content)),
            locale: Some("en".into()),
            chunk: Some(ChunkBody::Bytes(body)),
        })
    };

    assert_eq!(send(1, vec![b'x'; 60]).error, None);
    // 增量校验：已落盘 60 + 本块 60 > 100
    let second = send(2, vec![b'x'; 60]);
    assert!(second.error.is_some(), "增量大小超限必须被拒");
}

// ------------------------------------------------------------------ 事件

#[derive(Default)]
struct Recorder {
    events: std::sync::Mutex<Vec<String>>,
}

impl EventSink for Recorder {
    fn before_upload_complete(&self, _partial: &aetherupload::PartialResource) {
        self.events.lock().unwrap().push("before".into());
    }

    fn upload_complete(&self, resource: &aetherupload::Resource) {
        self.events
            .lock()
            .unwrap()
            .push(format!("after:{}", resource.name));
    }
}

struct PanickingEvents;

impl EventSink for PanickingEvents {
    fn before_upload_complete(&self, _partial: &aetherupload::PartialResource) {
        panic!("监听器炸了");
    }
}

fn events_config(sink: Arc<dyn EventSink>) -> (Config, Arc<dyn EventSink>) {
    let mut config = Config::default();
    if let Some(group) = config.groups.get_mut("file") {
        group.event_before_upload_complete = true;
        group.event_upload_complete = true;
    }

    (config, sink)
}

#[test]
fn events_fire_in_order() {
    let recorder = Arc::new(Recorder::default());
    let (config, sink) = events_config(recorder.clone());

    let harness = Harness::with_config("events", config);
    let runtime = Arc::new(
        Runtime::new(harness.runtime.config().clone(), &harness.base)
            .unwrap()
            .with_instant(Arc::new(MemoryInstantStore::new()))
            .with_events(sink),
    );
    let uploader = UploadController::new(runtime);

    let content = b"event content".to_vec();
    let pre = uploader.preprocess(&PreprocessRequest {
        resource_name: Some("e.txt".into()),
        resource_size: Some(content.len().to_string()),
        group: Some("file".into()),
        resource_hash: Some(md5_hex(&content)),
        locale: Some("en".into()),
    });
    assert_eq!(pre.error, None);

    let last = uploader.save_chunk(&SaveChunkRequest {
        chunk_total: Some("1".into()),
        chunk_index: Some("1".into()),
        resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
        resource_ext: Some("txt".into()),
        group_subdir: Some(pre.group_sub_dir.clone()),
        group: Some("file".into()),
        resource_hash: Some(md5_hex(&content)),
        locale: Some("en".into()),
        chunk: Some(ChunkBody::Bytes(content)),
    });
    assert_eq!(last.error, None);

    let events = recorder.events.lock().unwrap().clone();
    assert_eq!(events.len(), 2, "两个事件各触发一次：{events:?}");
    assert_eq!(events[0], "before");
    assert!(events[1].starts_with("after:"));
}

#[test]
fn listener_panic_does_not_break_the_upload() {
    // 与 PHP 版一致的可观察行为：监听器抛异常不冒泡，上传照常成功
    let (config, sink) = events_config(Arc::new(PanickingEvents));

    let harness = Harness::with_config("panic", config);
    let runtime = Arc::new(
        Runtime::new(harness.runtime.config().clone(), &harness.base)
            .unwrap()
            .with_instant(Arc::new(MemoryInstantStore::new()))
            .with_events(sink),
    );
    let uploader = UploadController::new(runtime);

    let content = b"still fine".to_vec();
    let pre = uploader.preprocess(&PreprocessRequest {
        resource_name: Some("p.txt".into()),
        resource_size: Some(content.len().to_string()),
        group: Some("file".into()),
        resource_hash: Some(md5_hex(&content)),
        locale: Some("en".into()),
    });
    assert_eq!(pre.error, None);

    // 静音 panic 输出，避免测试日志被刷屏（panic 仍会被 catch_unwind 接住）
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let last = uploader.save_chunk(&SaveChunkRequest {
        chunk_total: Some("1".into()),
        chunk_index: Some("1".into()),
        resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
        resource_ext: Some("txt".into()),
        group_subdir: Some(pre.group_sub_dir.clone()),
        group: Some("file".into()),
        resource_hash: Some(md5_hex(&content)),
        locale: Some("en".into()),
        chunk: Some(ChunkBody::Bytes(content.clone())),
    });

    std::panic::set_hook(previous);

    assert_eq!(last.error, None, "监听器 panic 不应影响上传");
    assert!(!last.saved_path.is_empty());
}

// ------------------------------------------------------------------ 删除资源

#[test]
fn delete_resource_also_clears_the_instant_index() {
    let harness = Harness::with_instant("delete");
    let content = b"to be deleted".to_vec();

    let (saved_path, file) = harness.upload("d.txt", &content);
    assert!(file.is_file());

    // 秒传命中在删除前是通的
    let pre = harness.preprocess("d.txt", &content, "file");
    assert_eq!(pre.saved_path, saved_path);

    assert!(harness.runtime.delete_resource(&saved_path), "删除应成功");
    assert!(!file.exists(), "成品应被删除");

    // 秒传记录联动清理 → 同样的内容再传一次走完整链路（不再是秒传）
    let pre = harness.preprocess("d.txt", &content, "file");
    assert_eq!(pre.error, None);
    assert!(pre.saved_path.is_empty(), "秒传记录应已清理");

    // 幂等：重复删除返回 false（文件已不在）
    assert!(!harness.runtime.delete_resource(&saved_path));
    // 非法路径不 panic
    assert!(!harness.runtime.delete_resource("not_a_saved_path"));
}

// ------------------------------------------------------------------ 分组隔离与自定义配置

#[test]
fn groups_are_isolated_by_directory_and_limits() {
    let mut config = Config::default();
    config.groups.insert(
        "video".into(),
        GroupConfig {
            group_dir: "video".into(),
            resource_maxsize: 16,
            resource_extensions: vec!["mp4".into()],
            event_before_upload_complete: false,
            event_upload_complete: false,
        },
    );

    let harness = Harness::with_config("groups", config);

    // video 分组：白名单只有 mp4，且上限 16 字节
    let pre = harness.uploader().preprocess(&PreprocessRequest {
        resource_name: Some("a.txt".into()),
        resource_size: Some("3".into()),
        group: Some("video".into()),
        locale: Some("zh".into()),
        ..Default::default()
    });
    assert_eq!(pre.error.as_deref(), Some("错误：无效的文件类型"));

    let pre = harness.uploader().preprocess(&PreprocessRequest {
        resource_name: Some("a.mp4".into()),
        resource_size: Some("20".into()),
        group: Some("video".into()),
        locale: Some("zh".into()),
        ..Default::default()
    });
    assert_eq!(pre.error.as_deref(), Some("错误：无效的文件大小"));

    // 合法请求：内容要真是 mp4（末块按魔数复核 MIME，纯文本会被判成 txt 而被拒）
    let content = b"\x00\x00\x00\x18ftypmp42".to_vec();
    assert_eq!(content.len(), 12);

    let pre = harness.preprocess("a.mp4", &content, "video");
    assert_eq!(pre.error, None);
    let last = harness.upload_chunks(&pre, &content, "video", 5);
    assert_eq!(last.error, None, "{:?}", last.error);
    assert!(
        last.saved_path.starts_with("video_"),
        "savedPath 的分组段应为 video"
    );

    let landed = harness
        .upload_root()
        .join("video")
        .join(&pre.group_sub_dir)
        .join(format!("{}.mp4", md5_hex(&content)));
    assert!(
        landed.is_file(),
        "成品应在 video 分组目录下：{}",
        landed.display()
    );
}

#[test]
fn subdir_rule_year_and_const() {
    let config = Config {
        resource_subdir_rule: aetherupload::config::SubdirRule::Const,
        ..Config::default()
    };

    let harness = Harness::with_config("subdir", config);
    let content = b"const subdir".to_vec();

    let pre = harness.preprocess("c.txt", &content, "file");
    assert_eq!(pre.group_sub_dir, "subdir");

    let last = harness.upload_chunks(&pre, &content, "file", 4);
    assert_eq!(last.error, None);
    assert!(last.saved_path.contains("_subdir_"));
}

#[test]
fn temp_and_header_paths_live_under_the_upload_root() {
    let harness = Harness::new("paths");
    let content = b"where am i".to_vec();

    let pre = harness.preprocess("w.txt", &content, "file");
    let header = harness
        .upload_root()
        .join("_header")
        .join(&pre.resource_temp_base_name);

    assert!(Path::new(&header).is_file(), "断点文件应在 _header 下");
    assert_eq!(std::fs::read_to_string(&header).unwrap().trim(), "0");
}
