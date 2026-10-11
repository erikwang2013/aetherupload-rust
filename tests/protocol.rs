// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 协议级端到端：不经过任何 Web 框架，直接走两个内核入口，逐条核对 2.0 冻结稿。
//!
//! 覆盖：完整上传链路、断线续传（重发已落块 / 序号跳变 / 缺分块本体）、秒传、
//! 参数与路径安全边界、大小与扩展名限制、完整性校验失败、**问题响应**
//! （RFC 9457 problem+json）的状态码 × code 映射、事件、展示与下载、
//! 删除资源联动清理秒传索引。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aetherupload::config::{Config, GroupConfig, SubdirRule};
use aetherupload::console;
use aetherupload::controller::{PreprocessOk, SaveChunkOk};
use aetherupload::events::EventSink;
use aetherupload::instant::MemoryInstantStore;
use aetherupload::json::Problem;
use aetherupload::md5::md5_hex;
use aetherupload::{
    ChunkBody, PreprocessRequest, ResourceController, ResourceResponse, Runtime, SaveChunkRequest,
    UploadController,
};

// ------------------------------------------------------------------ 断言辅助
//
// 一律不用 `unwrap()/unwrap_err()/expect()`：那要求 `Problem` / `*Ok` 实现 `Debug`，
// 而冻结稿没承诺（测试要按契约写，不按实现的 derive 写）。

/// 取成功值；失败则带着问题响应报错。
fn expect_ok<T>(result: Result<T, Problem>) -> T {
    match result {
        Ok(ok) => ok,
        Err(problem) => panic!(
            "这次调用应当成功，实际 {} {}",
            problem.status(),
            problem.to_json()
        ),
    }
}

/// 取问题响应；成功则说明契约被破坏。
fn expect_problem<T>(result: Result<T, Problem>) -> Problem {
    match result {
        Ok(_) => panic!("这次调用应当失败，实际返回了成功"),
        Err(problem) => problem,
    }
}

/// 断言一条问题响应的公共形态：状态码原样、稳定 code、非空 title、problem+json 体。
fn assert_problem(problem: &Problem, status: u16, code: &str) {
    assert_eq!(
        problem.status(),
        status,
        "问题响应的状态码：{}",
        problem.to_json()
    );
    assert_eq!(problem.code, code, "问题响应的 code：{}", problem.to_json());
    assert!(
        !problem.title.is_empty(),
        "title 必须是非空的人类可读消息：{}",
        problem.to_json()
    );
    assert_eq!(problem.content_type(), "application/problem+json");

    let json = problem.to_json();
    assert!(json.contains(r#""type":"about:blank""#), "{json}");
    assert!(json.contains(&format!(r#""status":{status}"#)), "{json}");
    assert!(json.contains(&format!(r#""code":"{code}""#)), "{json}");
}

/// 取扩成员 `expected_index`（只有 `chunk_out_of_order` 才有）。
fn expected_index(problem: &Problem) -> u64 {
    problem
        .extra
        .iter()
        .find(|(key, _)| *key == "expected_index")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or_else(|| panic!("必须带 expected_index：{}", problem.to_json()))
}

/// 序号不连续专用的断言：409 `chunk_out_of_order` + `expected_index == last + 1`。
fn assert_out_of_order(problem: &Problem, index: u64) {
    assert_problem(problem, 409, "chunk_out_of_order");
    assert_eq!(expected_index(problem), index, "{}", problem.to_json());

    // 冻结稿只写 `"expected_index":<n>`，没写死 JSON 里是数字还是字符串，两种都认
    let json = problem.to_json();
    assert!(
        json.contains(&format!(r#""expected_index":{index}"#))
            || json.contains(&format!(r#""expected_index":"{index}""#)),
        "{json}"
    );
}

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

    /// 预处理一个指定内容的上传（原始结果，失败用例取问题响应）。
    fn preprocess(&self, name: &str, content: &[u8], group: &str) -> Result<PreprocessOk, Problem> {
        self.uploader().preprocess(&PreprocessRequest {
            resource_name: Some(name.to_string()),
            resource_size: Some(content.len().to_string()),
            group: Some(group.to_string()),
            resource_hash: Some(md5_hex(content)),
            locale: Some("zh".into()),
        })
    }

    /// 预处理（应当成功）。
    fn preprocess_ok(&self, name: &str, content: &[u8], group: &str) -> PreprocessOk {
        expect_ok(self.preprocess(name, content, group))
    }

    /// 组装一条分块请求（字段齐全，分块本体由调用方决定）。
    fn chunk_request(
        &self,
        pre: &PreprocessOk,
        group: &str,
        index: u64,
        total: u64,
        hash: &str,
        body: Option<Vec<u8>>,
    ) -> SaveChunkRequest {
        SaveChunkRequest {
            chunk_total: Some(total.to_string()),
            chunk_index: Some(index.to_string()),
            resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
            resource_ext: Some(pre.resource_ext.clone()),
            group_subdir: Some(pre.group_sub_dir.clone()),
            group: Some(group.to_string()),
            resource_hash: Some(hash.to_string()),
            locale: Some("zh".into()),
            chunk: body.map(ChunkBody::Bytes),
        }
    }

    /// 按分块大小切分并逐块上传，返回末块的成功体。
    fn upload_chunks(
        &self,
        pre: &PreprocessOk,
        content: &[u8],
        group: &str,
        chunk_size: usize,
    ) -> SaveChunkOk {
        let chunks: Vec<&[u8]> = content.chunks(chunk_size).collect();
        let total = chunks.len() as u64;

        let mut last = SaveChunkOk {
            saved_path: String::new(),
        };
        for (offset, chunk) in chunks.into_iter().enumerate() {
            let request = self.chunk_request(
                pre,
                group,
                offset as u64 + 1,
                total,
                &md5_hex(content),
                Some(chunk.to_vec()),
            );

            last = expect_ok(self.uploader().save_chunk(&request));
        }

        last
    }

    /// 走一遍完整链路，返回 (saved_path, 磁盘上的成品路径)。
    fn upload(&self, name: &str, content: &[u8]) -> (String, PathBuf) {
        let pre = self.preprocess_ok(name, content, "file");
        assert!(
            pre.saved_path.is_empty(),
            "非秒传场景预处理不该给出 saved_path"
        );

        let last = self.upload_chunks(&pre, content, "file", 7);
        assert!(!last.saved_path.is_empty(), "末块必须给出 saved_path");

        let expected = self
            .upload_root()
            .join("file")
            .join(&pre.group_sub_dir)
            .join(format!("{}.{}", md5_hex(content), pre.resource_ext));

        (last.saved_path, expected)
    }

    /// 临时 `*.part` 与 `_header` 断点是否还在（断线续传的可恢复状态）。
    fn progress(&self, pre: &PreprocessOk) -> (bool, bool) {
        let part = self
            .upload_root()
            .join("file")
            .join(&pre.group_sub_dir)
            .join(format!(
                "{}.{}.part",
                pre.resource_temp_base_name, pre.resource_ext
            ));
        let header = self
            .upload_root()
            .join("_header")
            .join(&pre.resource_temp_base_name);

        (part.is_file(), header.is_file())
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

    // saved_path 三段式：分组_子目录_文件名，文件名 = 内容 md5 + 原扩展名
    let parts: Vec<&str> = saved_path.splitn(3, '_').collect();
    assert_eq!(parts.len(), 3, "saved_path 应为三段：{saved_path}");
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
        other => panic!("预期本地文件响应，实际状态 {}", other.status()),
    }

    match harness.resources().download(&saved_path, "改名后的图片") {
        ResourceResponse::ServeFile { download_name, .. } => {
            assert_eq!(download_name.as_deref(), Some("改名后的图片.jpg"));
        }
        other => panic!("预期本地文件响应，实际状态 {}", other.status()),
    }

    // 不存在的资源 → 404 resource_not_found（文件端点同样是 problem+json）
    match harness.resources().display("file_202610_deadbeef.jpg") {
        ResourceResponse::Problem(problem) => assert_problem(&problem, 404, "resource_not_found"),
        other => panic!("预期问题响应，实际状态 {}", other.status()),
    }

    // saved_path 解码失败（段数不足 / 段内含不安全字符）→ 400 invalid_parameter
    for bad in [
        "noseparator",
        "file_202610",
        "file__abc.txt",
        "file_202610_bad name.txt",
        "file_202610_..%2f..%2fetc",
    ] {
        match harness.resources().display(bad) {
            ResourceResponse::Problem(problem) => {
                assert_problem(&problem, 400, "invalid_parameter")
            }
            other => panic!(
                "saved_path 解码失败应报参数错误（{bad}），实际状态 {}",
                other.status()
            ),
        }
    }

    // 分组段对不上任何配置 → 404 resource_not_found（「a_b_c_d」解出来是分组 a）：
    // 读路径只是「标识符指不到资源」，与文件不存在同一结局，也顺带不泄露分组配置；
    // 上传端点上的未知分组才是 400 invalid_group（请求要求往不存在的分组里写）
    for bad in ["nope_202610_deadbeef.jpg", "a_b_c_d"] {
        match harness.resources().display(bad) {
            ResourceResponse::Problem(problem) => {
                assert_problem(&problem, 404, "resource_not_found")
            }
            other => panic!("未知分组应报「找不到」({bad})，实际状态 {}", other.status()),
        }
    }

    // download 的 new_name 非法（空串 / 含分隔符 / 含控制字符）→ 400 invalid_parameter
    for bad in ["", "a/b", "a\\b", "换\n行"] {
        match harness.resources().download(&saved_path, bad) {
            ResourceResponse::Problem(problem) => {
                assert_problem(&problem, 400, "invalid_parameter")
            }
            other => panic!(
                "new_name 非法应报参数错误（{bad:?}），实际状态 {}",
                other.status()
            ),
        }
    }
}

#[test]
fn inline_blocked_extensions_are_forced_to_attachment() {
    let harness = Harness::new("inline");
    let content = b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>".to_vec();
    let (saved_path, _) = harness.upload("icon.svg", &content);

    match harness.resources().display(&saved_path) {
        ResourceResponse::ServeFile { download_name, .. } => {
            // 落盘文件名是内容 md5（与 1.x 一致），附件名就是它
            assert_eq!(
                download_name.as_deref(),
                Some(format!("{}.svg", md5_hex(&content)).as_str()),
                "svg 可内联渲染，必须强制转附件（否则变成同源 XSS）"
            );
        }
        other => panic!("预期本地文件响应，实际状态 {}", other.status()),
    }
}

// ------------------------------------------------------------------ 秒传

#[test]
fn instant_completion_skips_the_second_upload() {
    let harness = Harness::with_instant("instant");
    let content = b"repeat me".repeat(50);

    let (first_path, file) = harness.upload("data.txt", &content);

    // 第二次上传同样的内容：预处理阶段就命中，一个分块都不用传
    let pre = harness.preprocess_ok("data.txt", &content, "file");
    assert_eq!(pre.saved_path, first_path, "秒传应返回既有 saved_path");
    assert!(file.is_file());

    // 复制一份再传：秒传命中不影响磁盘（这轮预处理器就会给 saved_path，
    // 走不了 `upload()` 里「非秒传场景」的断言，所以手工走一遍）
    let before = std::fs::read(&file).unwrap();
    let again = harness.preprocess_ok("data.txt", &content, "file");
    assert_eq!(
        again.saved_path, first_path,
        "秒传命中应直接给既有 saved_path"
    );
    assert_eq!(std::fs::read(&file).unwrap(), before, "秒传命中不该改磁盘");
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

// ------------------------------------------------------------------ 断线续传与序号

#[test]
fn resend_keeps_progress_and_out_of_order_blocks_are_rejected() {
    let harness = Harness::new("resume");
    let content = b"0123456789abcdefghijklmnopqrstuvwxyz".to_vec();
    let chunk = |index: usize| {
        content[(index * 6).min(content.len())..((index + 1) * 6).min(content.len())].to_vec()
    };
    let hash = md5_hex(&content);

    let pre = harness.preprocess_ok("resume.txt", &content, "file");
    let send = |index: u64, body: Option<Vec<u8>>| {
        harness
            .uploader()
            .save_chunk(&harness.chunk_request(&pre, "file", index, 6, &hash, body))
    };

    // 第 1 块
    let first = send(1, Some(chunk(0)));
    assert!(first.is_ok(), "第 1 块应被接收");

    // 重发已落块（index <= last）：2.0 起是 409 chunk_out_of_order（1.x 是静默 200）
    let resend = expect_problem(send(1, Some(chunk(0))));
    assert_out_of_order(&resend, 2);
    assert_eq!(
        harness.progress(&pre),
        (true, true),
        "序号类的失败必须保留已拼好的进度"
    );

    // 跳到第 3 块（中间缺块）：409 + expected_index == 2
    let gap = expect_problem(send(3, Some(chunk(2))));
    assert_out_of_order(&gap, 2);
    assert_eq!(harness.progress(&pre), (true, true), "跳变必须保留进度");

    // 补齐第 2 块，回到正轨
    assert!(send(2, Some(chunk(1))).is_ok());

    // 下一块不带分块本体：400 参数错误，但保留进度（客户端补上本体即可继续）
    let missing = expect_problem(send(3, None));
    assert_problem(&missing, 400, "invalid_parameter");
    assert_eq!(
        harness.progress(&pre),
        (true, true),
        "缺分块本体必须保留进度"
    );

    // 补上本体即可继续
    assert!(send(3, Some(chunk(2))).is_ok());
    assert!(send(4, Some(chunk(3))).is_ok());
    assert!(send(5, Some(chunk(4))).is_ok());

    let last = expect_ok(send(6, Some(chunk(5))));
    assert!(!last.saved_path.is_empty());
}

#[test]
fn out_of_order_covers_gap_last_block_and_landed_resend() {
    let harness = Harness::new("ooo");
    let content = b"out of order, three flavors".repeat(8).to_vec();
    let hash = md5_hex(&content);
    let chunks: Vec<&[u8]> = content.chunks(32).collect();
    let total = chunks.len() as u64;
    assert!(total >= 4, "这轮至少要四块，实际 {total}");

    let pre = harness.preprocess_ok("ooo.txt", &content, "file");
    let send = |index: u64, body: &[u8]| {
        harness.uploader().save_chunk(&harness.chunk_request(
            &pre,
            "file",
            index,
            total,
            &hash,
            Some(body.to_vec()),
        ))
    };

    assert!(send(1, chunks[0]).is_ok(), "第 1 块应被接收（last = 1）");

    // 三态都必须是 409 chunk_out_of_order + expected_index == last + 1 == 2：
    // 1) 跳号：直接发第 4 块；
    assert_out_of_order(&expect_problem(send(4, chunks[3])), 2);
    // 2) 末块特权不成立：即使 index == chunk_total，缺块一样要拒；
    assert_out_of_order(&expect_problem(send(total, chunks[total as usize - 1])), 2);
    // 3) 重发已落块（index <= last）。
    assert_out_of_order(&expect_problem(send(1, chunks[0])), 2);

    // 按序补齐仍然全部接受，末块落盘（失败路径不该留下任何副作用）
    for index in 2..=total {
        let ok = expect_ok(send(index, chunks[index as usize - 1]));

        if index == total {
            assert!(!ok.saved_path.is_empty(), "末块必须给出 saved_path");
        } else {
            assert!(ok.saved_path.is_empty(), "中间块不该给出 saved_path");
        }
    }
}

// ------------------------------------------------------------------ 未预处理 / 断点缺失

#[test]
fn unprepared_uploads_are_409_and_never_destroy_the_data() {
    let harness = Harness::new("unprepared");
    let content = b"prepared? nope".repeat(6).to_vec();
    let hash = md5_hex(&content);

    // 1) 没预处理直接打分块：临时资源根本不存在 → 409 upload_not_prepared
    let ghost = PreprocessOk {
        chunk_size: harness.runtime.config().chunk_size,
        resource_temp_base_name: "0123456789abcdef".into(),
        group_sub_dir: "202610".into(),
        resource_ext: "txt".into(),
        saved_path: String::new(),
    };

    let problem = expect_problem(harness.uploader().save_chunk(&harness.chunk_request(
        &ghost,
        "file",
        1,
        3,
        &hash,
        Some(content.clone()),
    )));
    assert_problem(&problem, 409, "upload_not_prepared");

    // 2) 预处理过但 `.part` 被删（服务端状态丢失）→ 409，且**不 cleanup**：
    //    断点是可恢复状态，不许连带销毁
    let pre = harness.preprocess_ok("gone.txt", &content, "file");
    let part = harness
        .upload_root()
        .join("file")
        .join(&pre.group_sub_dir)
        .join(format!(
            "{}.{}.part",
            pre.resource_temp_base_name, pre.resource_ext
        ));
    std::fs::remove_file(&part).unwrap();

    let problem = expect_problem(harness.uploader().save_chunk(&harness.chunk_request(
        &pre,
        "file",
        1,
        3,
        &hash,
        Some(content.clone()),
    )));
    assert_problem(&problem, 409, "upload_not_prepared");
    assert_eq!(
        harness.progress(&pre),
        (false, true),
        "`.part` 不在时也不许清理断点（可恢复状态不许销毁数据）"
    );

    // 3) `.part` 还在但断点缺失：409 且**绝不 cleanup** —— 1.x 会把 `.part`
    //    连数据一起清掉（可恢复状态被销毁），2.0 的硬要求是原样保留
    let pre = harness.preprocess_ok("headerless.txt", &content, "file");
    let (part_ok, header_ok) = harness.progress(&pre);
    assert_eq!(
        (part_ok, header_ok),
        (true, true),
        "预处理应建好 .part 与断点"
    );

    let header = harness
        .upload_root()
        .join("_header")
        .join(&pre.resource_temp_base_name);

    std::fs::remove_file(&header).unwrap();
    let problem = expect_problem(harness.uploader().save_chunk(&harness.chunk_request(
        &pre,
        "file",
        1,
        3,
        &hash,
        Some(content.clone()),
    )));
    assert_problem(&problem, 409, "upload_not_prepared");
    assert!(
        harness.progress(&pre).0,
        "断点缺失时 `.part` 必须原样保留（可恢复状态不许销毁数据）"
    );

    // 断点内容不可解析（截断 / 乱码）同样按「没准备好」处置，且不删数据
    std::fs::write(&header, b"not a number").unwrap();
    let problem = expect_problem(harness.uploader().save_chunk(&harness.chunk_request(
        &pre,
        "file",
        1,
        3,
        &hash,
        Some(content.clone()),
    )));
    assert_problem(&problem, 409, "upload_not_prepared");
    assert!(
        harness.progress(&pre).0,
        "断点不可解析时 `.part` 必须原样保留"
    );
}

// ------------------------------------------------------------------ 完整性与大小

#[test]
fn wrong_hash_discards_the_whole_upload() {
    let harness = Harness::new("badhash");
    let content = b"real content".to_vec();

    let pre = harness.preprocess_ok("x.txt", &content, "file");

    // 客户端声明的 hash 与真实内容不符 → 末块整份丢弃（422 hash_mismatch）
    let problem = expect_problem(harness.uploader().save_chunk(&harness.chunk_request(
        &pre,
        "file",
        1,
        1,
        &md5_hex(b"something else"),
        Some(content.clone()),
    )));
    assert_problem(&problem, 422, "hash_mismatch");

    // 完整性校验失败 = 整份作废：`.part` 与断点都必须清理
    assert_eq!(
        harness.progress(&pre),
        (false, false),
        "校验失败必须整份清理"
    );
}

#[test]
fn lax_mode_skips_the_hash_check() {
    let harness = Harness::new("lax");
    let content = b"lax content".to_vec();

    let pre = harness.preprocess_ok("lax.txt", &content, "file");

    // 默认（lax_mode = false）拒绝
    let problem = expect_problem(harness.uploader().save_chunk(&harness.chunk_request(
        &pre,
        "file",
        1,
        1,
        &md5_hex(b"wrong"),
        Some(content.clone()),
    )));
    assert_problem(&problem, 422, "hash_mismatch");

    // 宽松模式：跳过比对，照常落盘（文件名仍是真实内容的 md5）
    let lax = send_lax(&harness, &content);
    assert!(
        lax.saved_path
            .ends_with(&format!("{}.txt", md5_hex(&content))),
        "宽松模式应照常落盘：{}",
        lax.saved_path
    );
}

/// 用宽松模式另开一套 runtime（分组快照在调用内解析，改不了现有实例的配置）。
fn send_lax(harness: &Harness, content: &[u8]) -> SaveChunkOk {
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

    // 宽松模式下 hash 形态也不校验（§11.1）：随便给个短 hash 也要能通过
    let pre = expect_ok(uploader.preprocess(&PreprocessRequest {
        resource_name: Some("lax.txt".into()),
        resource_size: Some(content.len().to_string()),
        group: Some("file".into()),
        resource_hash: Some("deadbeef".into()),
        locale: Some("en".into()),
    }));

    expect_ok(uploader.save_chunk(&SaveChunkRequest {
        chunk_total: Some("1".into()),
        chunk_index: Some("1".into()),
        resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
        resource_ext: Some("txt".into()),
        group_subdir: Some(pre.group_sub_dir.clone()),
        group: Some("file".into()),
        resource_hash: Some("deadbeef".into()),
        locale: Some("en".into()),
        chunk: Some(ChunkBody::Bytes(content.to_vec())),
    }))
}

// ------------------------------------------------------------------ 参数与安全边界

#[test]
fn preprocess_parameter_problems_are_coded() {
    let harness = Harness::new("params");
    let uploader = harness.uploader();

    let preprocess = |name: Option<&str>,
                      size: Option<&str>,
                      group: Option<&str>,
                      hash: Option<&str>,
                      locale: &str| {
        uploader.preprocess(&PreprocessRequest {
            resource_name: name.map(str::to_string),
            resource_size: size.map(str::to_string),
            group: group.map(str::to_string),
            resource_hash: hash.map(str::to_string),
            locale: Some(locale.to_string()),
        })
    };

    // 必填缺失 → 400 invalid_parameter
    let problem = expect_problem(preprocess(None, Some("10"), Some("file"), None, "zh"));
    assert_problem(&problem, 400, "invalid_parameter");

    // 分组名含下划线：会破坏 saved_path 三段式解码 → 400 invalid_group
    let problem = expect_problem(preprocess(
        Some("a.txt"),
        Some("10"),
        Some("my_file"),
        None,
        "zh",
    ));
    assert_problem(&problem, 400, "invalid_group");

    // 未配置的分组 → 400 invalid_group
    let problem = expect_problem(preprocess(
        Some("a.txt"),
        Some("10"),
        Some("nope"),
        None,
        "zh",
    ));
    assert_problem(&problem, 400, "invalid_group");

    // 声明大小超分组上限 / 为 0 / 非严格数字 → 400 invalid_parameter
    //（「5.5」「1e3」1.x 会静默截断，2.0 从严）
    for size in ["999999999999", "0", "5.5", "1e3", "abc", ""] {
        let problem = expect_problem(preprocess(
            Some("a.txt"),
            Some(size),
            Some("file"),
            None,
            "zh",
        ));
        assert_problem(&problem, 400, "invalid_parameter");
    }

    // 派生出的扩展名落在黑名单（.php 在默认黑名单里）→ 415 unsupported_media_type
    let problem = expect_problem(preprocess(
        Some("shell.php"),
        Some("10"),
        Some("file"),
        None,
        "zh",
    ));
    assert_problem(&problem, 415, "unsupported_media_type");

    // 派生出的扩展名自身不安全（非安全字符集 / 超长）→ 400 invalid_parameter。
    // 安全检查只针对扩展名这一段（§11.5.1），参数类判定先于白名单，
    // 所以这些「又不在白名单又不安全」的扩展名应报 400 而不是 415。
    let long_ext = "x".repeat(65);
    for name in [
        "报告.超长".to_string(),
        format!("报告.{long_ext}"),
        "报告.a b".to_string(),
    ] {
        let problem = expect_problem(preprocess(
            Some(&name),
            Some("10"),
            Some("file"),
            None,
            "zh",
        ));
        assert_problem(&problem, 400, "invalid_parameter");
    }

    // hash 非空但不是 32 位 hex → 400 invalid_parameter（空串＝客户端不提供，走跳过比对）
    let problem = expect_problem(preprocess(
        Some("a.txt"),
        Some("10"),
        Some("file"),
        Some("abc"),
        "zh",
    ));
    assert_problem(&problem, 400, "invalid_parameter");

    // 正向防回归：正常中文与空格的文件名必须通过（安全检查只针对派生扩展名）
    assert!(
        preprocess(Some("我的 报告.pdf"), Some("10"), Some("file"), None, "zh").is_ok(),
        "正常中文文件名不该被安全判定误杀"
    );

    // title 走请求里的 locale：同一个失败在 zh / en 下必须给出不同文案
    let zh = expect_problem(preprocess(
        Some("a.txt"),
        Some("10"),
        Some("nope"),
        None,
        "zh",
    ));
    let en = expect_problem(preprocess(
        Some("a.txt"),
        Some("10"),
        Some("nope"),
        None,
        "en",
    ));
    assert_ne!(zh.title, en.title, "title 应按 locale 本地化");
    assert!(zh.title.contains("错误"), "中文文案：{}", zh.title);
    assert!(en.title.contains("Error"), "英文文案：{}", en.title);
}

#[test]
fn chunk_parameters_are_validated() {
    let harness = Harness::new("chunk-params");
    let content = b"abc".to_vec();
    let hash = md5_hex(&content);
    let pre = harness.preprocess_ok("a.txt", &content, "file");

    let send = |index: &str, total: &str, subdir: &str| {
        harness.uploader().save_chunk(&SaveChunkRequest {
            chunk_total: Some(total.into()),
            chunk_index: Some(index.into()),
            resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
            resource_ext: Some("txt".into()),
            group_subdir: Some(subdir.into()),
            group: Some("file".into()),
            resource_hash: Some(hash.clone()),
            locale: Some("en".into()),
            chunk: Some(ChunkBody::Bytes(content.clone())),
        })
    };

    // 序号不是纯数字 / 小于 1
    for index in ["abc", "0", "-1", ""] {
        let problem = expect_problem(send(index, "1", &pre.group_sub_dir));
        assert_problem(&problem, 400, "invalid_parameter");
    }

    // 总数上限 10000
    let problem = expect_problem(send("1", "10001", &pre.group_sub_dir));
    assert_problem(&problem, 400, "invalid_parameter");

    // 序号超过总数 → 400 invalid_parameter（1.x 无此校验）
    let problem = expect_problem(send("3", "2", &pre.group_sub_dir));
    assert_problem(&problem, 400, "invalid_parameter");

    // group_subdir 含下划线 / 目录穿越 → 参数错误
    for subdir in ["2026_10", "..", "../etc", "a/b"] {
        let problem = expect_problem(send("1", "1", subdir));
        assert_problem(&problem, 400, "invalid_parameter");
    }

    // 临时名 / 扩展名同样要过安全字符集
    for (name, ext) in [("bad name", "txt"), ("0123456789abcdef", "t xt")] {
        let problem = expect_problem(harness.uploader().save_chunk(&SaveChunkRequest {
            chunk_total: Some("1".into()),
            chunk_index: Some("1".into()),
            resource_temp_basename: Some(name.into()),
            resource_ext: Some(ext.into()),
            group_subdir: Some(pre.group_sub_dir.clone()),
            group: Some("file".into()),
            resource_hash: Some(hash.clone()),
            locale: Some("en".into()),
            chunk: Some(ChunkBody::Bytes(content.clone())),
        }));
        assert_problem(&problem, 400, "invalid_parameter");
    }
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
    let hash = md5_hex(&content);

    let pre = harness.preprocess_ok("big.txt", &content, "tiny");

    let send = |index: u64, body: Vec<u8>| {
        harness.uploader().save_chunk(&harness.chunk_request(
            &pre,
            "tiny",
            index,
            2,
            &hash,
            Some(body),
        ))
    };

    assert!(send(1, vec![b'x'; 60]).is_ok(), "第 1 块在声明大小内");

    // 增量校验：已落盘 60 + 本块 60 > 100 → 413 payload_too_large
    let problem = expect_problem(send(2, vec![b'x'; 60]));
    assert_problem(&problem, 413, "payload_too_large");
}

#[test]
fn a_single_chunk_larger_than_chunk_size_is_413() {
    let config = Config {
        chunk_size: 64,
        ..Config::default()
    };

    let harness = Harness::with_config("chunk-limit", config);
    // 声明大小 128：单块 65 字节既没越过声明总量、也没越过分组上限，
    // 却越过了 chunk_size —— 1.x 没有这条校验，2.0 必须拒
    let content = vec![b'y'; 128];
    let hash = md5_hex(&content);

    let pre = harness.preprocess_ok("big.txt", &content, "file");
    assert_eq!(pre.chunk_size, 64, "chunk_size 应来自配置");

    let problem = expect_problem(harness.uploader().save_chunk(&harness.chunk_request(
        &pre,
        "file",
        1,
        2,
        &hash,
        Some(vec![b'y'; 65]),
    )));
    assert_problem(&problem, 413, "payload_too_large");

    // 边界内（恰好等于 chunk_size）照常接受 —— 413 按「整份作废」语义已把这轮的
    // `.part` 清掉，所以要另起一轮预处理（不是同一份上传的续传）
    let pre = harness.preprocess_ok("edge.txt", &content, "file");
    assert!(
        harness
            .uploader()
            .save_chunk(&harness.chunk_request(&pre, "file", 1, 2, &hash, Some(vec![b'y'; 64]),))
            .is_ok(),
        "恰好等于 chunk_size 的块必须被接受"
    );
}

#[test]
fn last_block_verifies_the_real_mime() {
    let harness = Harness::new("mime-check");
    // 内容含 NUL → 魔数探测判成 application/octet-stream → bin，不在 file 分组白名单内；
    // 声明的是 .txt，末块按真实内容复核时必须拒
    let content = vec![0u8; 32];
    let hash = md5_hex(&content);

    let pre = harness.preprocess_ok("fake.txt", &content, "file");
    let problem = expect_problem(harness.uploader().save_chunk(&harness.chunk_request(
        &pre,
        "file",
        1,
        1,
        &hash,
        Some(content),
    )));
    assert_problem(&problem, 415, "unsupported_media_type");
}

// ------------------------------------------------------------------ 内部故障

#[test]
fn internal_io_failure_is_500() {
    // 子目录位置被一个普通文件占住：`create_dir` 必失败（真 IO 故障，与权限无关，
    // root 跑测试也拦得住）
    let config = Config {
        resource_subdir_rule: SubdirRule::Const,
        ..Config::default()
    };

    let harness = Harness::with_config("io-fail", config);
    let subdir = harness.upload_root().join("file").join("subdir");
    let _ = std::fs::remove_dir_all(&subdir);
    std::fs::write(&subdir, b"occupied").unwrap();

    // 服务端部署问题，客户端无从修 → 500 internal_error
    let problem = expect_problem(harness.preprocess("a.txt", b"payload", "file"));
    assert_problem(&problem, 500, "internal_error");
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
    let pre = expect_ok(uploader.preprocess(&PreprocessRequest {
        resource_name: Some("e.txt".into()),
        resource_size: Some(content.len().to_string()),
        group: Some("file".into()),
        resource_hash: Some(md5_hex(&content)),
        locale: Some("en".into()),
    }));

    expect_ok(uploader.save_chunk(&SaveChunkRequest {
        chunk_total: Some("1".into()),
        chunk_index: Some("1".into()),
        resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
        resource_ext: Some("txt".into()),
        group_subdir: Some(pre.group_sub_dir.clone()),
        group: Some("file".into()),
        resource_hash: Some(md5_hex(&content)),
        locale: Some("en".into()),
        chunk: Some(ChunkBody::Bytes(content)),
    }));

    let events = recorder.events.lock().unwrap().clone();
    assert_eq!(events.len(), 2, "两个事件各触发一次：{events:?}");
    assert_eq!(events[0], "before");
    assert!(events[1].starts_with("after:"));
}

#[test]
fn listener_panic_does_not_break_the_upload() {
    // 可观察行为（与 1.x 一致）：监听器抛异常不冒泡，上传照常成功
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
    let pre = expect_ok(uploader.preprocess(&PreprocessRequest {
        resource_name: Some("p.txt".into()),
        resource_size: Some(content.len().to_string()),
        group: Some("file".into()),
        resource_hash: Some(md5_hex(&content)),
        locale: Some("en".into()),
    }));

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

    let ok = expect_ok(last);
    assert!(!ok.saved_path.is_empty());
}

// ------------------------------------------------------------------ 删除资源

#[test]
fn delete_resource_also_clears_the_instant_index() {
    let harness = Harness::with_instant("delete");
    let content = b"to be deleted".to_vec();

    let (saved_path, file) = harness.upload("d.txt", &content);
    assert!(file.is_file());

    // 秒传命中在删除前是通的
    let pre = harness.preprocess_ok("d.txt", &content, "file");
    assert_eq!(pre.saved_path, saved_path);

    assert!(harness.runtime.delete_resource(&saved_path), "删除应成功");
    assert!(!file.exists(), "成品应被删除");

    // 秒传记录联动清理 → 同样的内容再传一次走完整链路（不再是秒传）
    let pre = harness.preprocess_ok("d.txt", &content, "file");
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
    let problem = expect_problem(harness.uploader().preprocess(&PreprocessRequest {
        resource_name: Some("a.txt".into()),
        resource_size: Some("3".into()),
        group: Some("video".into()),
        resource_hash: None,
        locale: Some("zh".into()),
    }));
    assert_problem(&problem, 415, "unsupported_media_type");

    let problem = expect_problem(harness.uploader().preprocess(&PreprocessRequest {
        resource_name: Some("a.mp4".into()),
        resource_size: Some("20".into()),
        group: Some("video".into()),
        resource_hash: None,
        locale: Some("zh".into()),
    }));
    assert_problem(&problem, 400, "invalid_parameter");

    // 合法请求：内容要真是 mp4（末块按魔数复核 MIME，纯文本会被判成别的类型而被拒）
    let content = b"\x00\x00\x00\x18ftypmp42".to_vec();

    let pre = harness.preprocess_ok("a.mp4", &content, "video");
    let last = harness.upload_chunks(&pre, &content, "video", 5);
    assert!(
        last.saved_path.starts_with("video_"),
        "saved_path 的分组段应为 video：{}",
        last.saved_path
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
        resource_subdir_rule: SubdirRule::Const,
        ..Config::default()
    };

    let harness = Harness::with_config("subdir", config);
    let content = b"const subdir".to_vec();

    let pre = harness.preprocess_ok("c.txt", &content, "file");
    assert_eq!(pre.group_sub_dir, "subdir");

    let last = harness.upload_chunks(&pre, &content, "file", 4);
    assert!(last.saved_path.contains("_subdir_"));
}

#[test]
fn temp_and_header_paths_live_under_the_upload_root() {
    let harness = Harness::new("paths");
    let content = b"where am i".to_vec();

    let pre = harness.preprocess_ok("w.txt", &content, "file");
    let header = harness
        .upload_root()
        .join("_header")
        .join(&pre.resource_temp_base_name);

    assert!(Path::new(&header).is_file(), "断点文件应在 _header 下");
    assert_eq!(std::fs::read_to_string(&header).unwrap().trim(), "0");
}
