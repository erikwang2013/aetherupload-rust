// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 并发 / 竞态回归：用 `thread::scope` + `Barrier` 把能重叠的窗口尽量压在一起，
//! 断言「无论怎样交错都成立」的不变量 —— 不预设任何具体交错顺序。
//!
//! 三个靶子：
//!
//! 1. 抢建分组子目录：N 个线程同时对**不同内容**做完整上传，全都落在同一个
//!    刚被删掉的子目录里（压 `create_group_sub_dir` 的「抢建失败后再查一次」）；
//! 2. 同内容去重：N 个线程同时上传**完全相同的内容**（压 `LocalStorage::publish`
//!    的「目标已存在 → 丢弃临时文件」），最终只落一份成品；
//! 3. 断点竞争：同一 tempName 下并发**重发已落块**（2.0 起是 409，安全拒绝且不动现场）、
//!    并发**首次同序号**（一个写入、另一个 409 且 `expected_index > 本块`＝已落）与
//!    并发**乱序补齐**（跳变被拒 → 客户端重传），内容必须恰好被追加一次。
//!
//! 子目录规则固定为 `Const`（= `"subdir"`，与 `protocol.rs` 的断言一致）：
//! 所有线程必然落在同一个子目录里，也不依赖运行时刻。
//!
//! 其中两条用例是**回归钉子**，都按 [`RACE_ROUNDS`] 轮跑（旧实现下回退必现）：
//! 并发分块请求曾让 `Header::read` 撞上写入窗口再被 `cleanup()` 销毁整份上传
//! （v1.0.2 已修）；以及同序号**首次**并发发送会重复追加、末块校验失败整份被销毁
//! （v1.0.3 已修）。细节见各自函数注释。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use aetherupload::console;
use aetherupload::controller::{PreprocessOk, SaveChunkOk};
use aetherupload::json::Problem;
use aetherupload::md5::md5_hex;
use aetherupload::{
    ChunkBody, Config, MemoryInstantStore, PreprocessRequest, Runtime, SaveChunkRequest,
    SubdirRule, UploadController,
};

/// 问题响应的一句话描述（测试信息里带 status + code + title）。
fn describe(problem: &Problem) -> String {
    format!("{} {} {}", problem.status(), problem.code, problem.title)
}

/// 取扩成员 `expected_index`（`chunk_out_of_order` 必有）。
fn expected_index(problem: &Problem) -> usize {
    problem
        .extra
        .iter()
        .find(|(key, _)| *key == "expected_index")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "chunk_out_of_order 必须带 expected_index：{}",
                problem.to_json()
            )
        })
}

/// 线程外的准备步骤：成功就取出，失败就带问题响应 panic。
/// （不用 `expect()`：冻结稿没承诺 `Problem` / `*Ok` 实现 `Debug`。）
fn take_ok<T>(result: Result<T, Problem>, ctx: &str) -> T {
    match result {
        Ok(ok) => ok,
        Err(problem) => panic!("{ctx}：{}", describe(&problem)),
    }
}

/// 分块小、块数多，交错窗口才密。
const CHUNK: usize = 256;
const THREADS: usize = 8;
const ROUNDS: usize = 20;
/// 两条竞态回归钉子（乱序补齐、同序号首传并发）跑这么多轮：400 轮 × 旧实现
/// 6.75% / 74.5% 的单轮触发率 —— 真回退了必现；其余用例 20 轮够用。
const RACE_ROUNDS: usize = 400;

// ------------------------------------------------------------------ 测试脚手架

struct Harness {
    base: PathBuf,
    runtime: Arc<Runtime>,
}

impl Harness {
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "aetherupload-concurrency-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);

        let config = Config {
            chunk_size: CHUNK as u64,
            resource_subdir_rule: SubdirRule::Const,
            ..Config::default()
        };

        let runtime = Arc::new(
            Runtime::new(config, &base)
                .expect("运行时装配")
                .with_instant(Arc::new(MemoryInstantStore::new())),
        );

        // 分组目录与 _header 必须先存在（内核建子目录是非递归 mkdir）
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

    fn upload_root(&self) -> PathBuf {
        self.runtime.upload_root().to_path_buf()
    }

    /// `file` 分组的子目录：`Const` 规则固定为 `"subdir"`，所有线程共用。
    fn subdir(&self) -> PathBuf {
        self.upload_root().join("file").join("subdir")
    }

    /// 断点目录必须干净（每个上传完成时都会删掉自己的 header）。
    fn assert_no_headers(&self, ctx: &str) {
        let headers = dir_entries(&self.upload_root().join("_header"));
        assert!(headers.is_empty(), "{ctx}：断点文件应已清理：{headers:?}");
    }

    /// 子目录里未改名的残留 `.part`。
    fn part_path(&self, pre: &PreprocessOk) -> PathBuf {
        self.subdir().join(format!(
            "{}.{}.part",
            pre.resource_temp_base_name, pre.resource_ext
        ))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn dir_entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("目录应存在")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();

    names.sort();
    names
}

// ------------------------------------------------------------------ 协议动作（无断言，供线程调用）

fn preprocess(
    uploader: &UploadController,
    group: &str,
    name: &str,
    content: &[u8],
) -> Result<PreprocessOk, Problem> {
    uploader.preprocess(&PreprocessRequest {
        resource_name: Some(name.to_string()),
        resource_size: Some(content.len().to_string()),
        group: Some(group.to_string()),
        resource_hash: Some(md5_hex(content)),
        locale: Some("zh".into()),
    })
}

fn save_chunk(
    uploader: &UploadController,
    pre: &PreprocessOk,
    group: &str,
    hash: &str,
    index: usize,
    total: usize,
    body: &[u8],
) -> Result<SaveChunkOk, Problem> {
    uploader.save_chunk(&SaveChunkRequest {
        chunk_total: Some(total.to_string()),
        chunk_index: Some(index.to_string()),
        resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
        resource_ext: Some(pre.resource_ext.clone()),
        group_subdir: Some(pre.group_sub_dir.clone()),
        group: Some(group.to_string()),
        resource_hash: Some(hash.to_string()),
        locale: Some("zh".into()),
        chunk: Some(ChunkBody::Bytes(body.to_vec())),
    })
}

/// 完整链路，返回 `(saved_path, 失败描述)` —— 并发线程直接调用，不 panic。
fn upload(
    uploader: &UploadController,
    group: &str,
    name: &str,
    content: &[u8],
) -> (String, Option<String>) {
    let pre = match preprocess(uploader, group, name, content) {
        Ok(pre) => pre,
        Err(problem) => return (String::new(), Some(describe(&problem))),
    };

    let chunks: Vec<&[u8]> = content.chunks(CHUNK).collect();
    let total = chunks.len();
    let mut saved_path = String::new();

    for (offset, chunk) in chunks.iter().enumerate() {
        match save_chunk(
            uploader,
            &pre,
            group,
            &md5_hex(content),
            offset + 1,
            total,
            chunk,
        ) {
            Ok(ok) => saved_path = ok.saved_path,
            Err(problem) => {
                return (
                    String::new(),
                    Some(format!(
                        "第 {}/{total} 块失败：{}",
                        offset + 1,
                        describe(&problem)
                    )),
                );
            }
        }
    }

    (saved_path, None)
}

/// 重传直到本块被接受（「序号跳变」在缺块补上后自然消失）；未收敛则返回最后一条问题。
///
/// 2.0 的分支按 §7 的口径：
/// - 200 → 本块本次被写入；
/// - 409 `chunk_out_of_order` 且 `expected_index > 本块` → 本块其实已落（对端线程写掉了），
///   等价于成功；
/// - 409 且 `expected_index <= 本块` → 缺的是前面的块（或就是本块），等对端补上后重传；
/// - 其余失败（4xx 硬失败）→ 不再重试，直接报错。
fn send_until_accepted(
    uploader: &UploadController,
    pre: &PreprocessOk,
    group: &str,
    hash: &str,
    index: usize,
    total: usize,
    body: &[u8],
) -> Result<(), String> {
    let mut last = String::new();

    for _ in 0..500 {
        match save_chunk(uploader, pre, group, hash, index, total, body) {
            Ok(_) => return Ok(()),
            Err(problem) if problem.code == "chunk_out_of_order" => {
                if expected_index(&problem) > index {
                    return Ok(());
                }

                last = describe(&problem);
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(problem) => return Err(describe(&problem)),
        }
    }

    Err(last)
}

// ------------------------------------------------------------------ 1. 抢建子目录

#[test]
fn concurrent_uploads_into_a_fresh_subdir_all_land() {
    let harness = Harness::new("subdir-race");
    let uploader = harness.uploader();
    let barrier = Barrier::new(THREADS);

    for round in 0..ROUNDS {
        // 每轮都把子目录删掉，让「N 个线程抢建同一个目录」的窗口重新出现
        let _ = std::fs::remove_dir_all(harness.subdir());

        let contents: Vec<Vec<u8>> = (0..THREADS)
            .map(|worker| {
                format!("round-{round}-worker-{worker}-payload-")
                    .repeat(48)
                    .into_bytes()
            })
            .collect();

        let results: Vec<(String, Option<String>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = contents
                .iter()
                .enumerate()
                .map(|(worker, content)| {
                    let uploader = &uploader;
                    let barrier = &barrier;
                    let name = format!("race-{round}-{worker}.txt");

                    scope.spawn(move || {
                        barrier.wait();
                        upload(uploader, "file", &name, content)
                    })
                })
                .collect();

            handles
                .into_iter()
                .map(|handle| handle.join().expect("工作线程不应 panic"))
                .collect()
        });

        for (worker, (saved_path, error)) in results.iter().enumerate() {
            assert_eq!(
                *error, None,
                "round={round} worker={worker} 上传失败：{error:?}"
            );

            let name = format!("{}.txt", md5_hex(&contents[worker]));
            assert_eq!(
                *saved_path,
                format!("file_subdir_{name}"),
                "round={round} worker={worker}：saved_path 应与成品一致"
            );

            let landed = harness.subdir().join(&name);
            assert_eq!(
                std::fs::read(&landed).unwrap_or_default(),
                contents[worker],
                "round={round} worker={worker}：成品内容必须逐字节一致"
            );
        }

        assert_eq!(
            dir_entries(&harness.subdir()).len(),
            THREADS,
            "round={round}：应恰好留下 {THREADS} 份成品"
        );
        harness.assert_no_headers(&format!("round={round}"));
    }
}

// ------------------------------------------------------------------ 2. 同内容并发去重

#[test]
fn concurrent_identical_uploads_dedupe_to_one_file() {
    let harness = Harness::new("dedupe-race");
    let uploader = harness.uploader();
    let barrier = Barrier::new(THREADS);

    for round in 0..ROUNDS {
        let _ = std::fs::remove_dir_all(harness.subdir());

        let content = format!("identical-payload-round-{round}-")
            .repeat(60)
            .into_bytes();
        let name = format!("{}.txt", md5_hex(&content));

        let results: Vec<(String, Option<String>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    let uploader = &uploader;
                    let barrier = &barrier;
                    let content = &content;

                    scope.spawn(move || {
                        barrier.wait();
                        upload(uploader, "file", "same.txt", content)
                    })
                })
                .collect();

            handles
                .into_iter()
                .map(|handle| handle.join().expect("工作线程不应 panic"))
                .collect()
        });

        for (worker, (saved_path, error)) in results.iter().enumerate() {
            assert_eq!(
                *error, None,
                "round={round} worker={worker} 上传失败：{error:?}"
            );
            assert_eq!(
                *saved_path,
                format!("file_subdir_{name}"),
                "round={round} worker={worker}：同内容必须给出同一 saved_path"
            );
        }

        // 要么自己 rename 落盘，要么在 publish 时发现目标已存在而丢弃临时文件 ——
        // 无论谁赢，磁盘上只有一份成品，且内容正确
        assert_eq!(
            dir_entries(&harness.subdir()),
            vec![name.clone()],
            "round={round}：同内容只应落一份成品"
        );
        assert_eq!(
            std::fs::read(harness.subdir().join(&name)).unwrap(),
            content
        );
        harness.assert_no_headers(&format!("round={round}"));
    }
}

// ------------------------------------------------------------------ 3a. 并发重发已落块

#[test]
fn concurrent_resend_of_a_received_chunk_never_appends_twice() {
    let harness = Harness::new("resend-race");
    let uploader = harness.uploader();
    let barrier = Barrier::new(THREADS);

    for round in 0..ROUNDS {
        // 每轮清空子目录：成品断言只针对本轮，且让「建目录」竞争重新出现
        let _ = std::fs::remove_dir_all(harness.subdir());

        let content = format!("resend-round-{round}-").repeat(70).into_bytes();
        let hash = md5_hex(&content);
        let pre = take_ok(
            preprocess(&uploader, "file", "resend.txt", &content),
            "预处理",
        );
        assert!(pre.saved_path.is_empty(), "round={round}：预处理不该秒传");

        let chunks: Vec<&[u8]> = content.chunks(CHUNK).collect();
        let total = chunks.len();
        assert!(total >= 3, "round={round}：这轮至少要三块，实际 {total}");

        // 单线程先发第 1 块：断点推到 1
        let first = save_chunk(&uploader, &pre, "file", &hash, 1, total, chunks[0]);
        assert!(first.is_ok(), "round={round}：第 1 块应被接收");

        // 并发重发已落块：2.0 起是 409 chunk_out_of_order（1.x 是静默 200），
        // 且一个字节都不许动现场 —— 这正是「绝不重复追加」要守的东西
        let first_body = chunks[0];
        let results: Vec<Result<SaveChunkOk, Problem>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    let uploader = &uploader;
                    let barrier = &barrier;
                    let pre = &pre;
                    let hash = hash.as_str();

                    scope.spawn(move || {
                        barrier.wait();
                        save_chunk(uploader, pre, "file", hash, 1, total, first_body)
                    })
                })
                .collect();

            handles
                .into_iter()
                .map(|handle| handle.join().expect("工作线程不应 panic"))
                .collect()
        });

        for (worker, result) in results.iter().enumerate() {
            match result {
                Ok(_) => panic!("round={round} worker={worker}：重发已落块必须是 409"),
                Err(problem) => {
                    assert_eq!(
                        problem.status(),
                        409,
                        "round={round} worker={worker}：{}",
                        describe(problem)
                    );
                    assert_eq!(
                        problem.code,
                        "chunk_out_of_order",
                        "round={round} worker={worker}：{}",
                        describe(problem)
                    );
                    assert_eq!(
                        expected_index(problem),
                        2,
                        "round={round} worker={worker}：预期的下一块应是 2"
                    );
                }
            }
        }

        // 拒绝归拒绝，进度必须原样保留（可恢复状态不许销毁数据），
        // 且一个字节都不许多出来（重发被拒 ≠ 再追加一次）
        assert_eq!(
            std::fs::metadata(harness.part_path(&pre)).unwrap().len() as usize,
            chunks[0].len(),
            "round={round}：并发重发被拒后 `.part` 必须原样保留且只含第 1 块"
        );

        // 补齐剩余块（末块落盘）：任何一次重复追加都会让整份 md5 对不上而失败
        for index in 2..=total {
            let ok = take_ok(
                save_chunk(
                    &uploader,
                    &pre,
                    "file",
                    &hash,
                    index,
                    total,
                    chunks[index - 1],
                ),
                &format!("round={round} 第 {index}/{total} 块"),
            );

            if index == total {
                assert_eq!(ok.saved_path, format!("file_subdir_{hash}.txt"));
            } else {
                assert!(ok.saved_path.is_empty(), "中间块不该给出 saved_path");
            }
        }

        let name = format!("{hash}.txt");
        assert_eq!(
            std::fs::read(harness.subdir().join(&name)).unwrap(),
            content,
            "round={round}：落盘内容必须逐字节一致（重复追加会在这里现形）"
        );
        assert_eq!(
            dir_entries(&harness.subdir()),
            vec![name],
            "round={round}：只应留一份成品"
        );
        harness.assert_no_headers(&format!("round={round}"));
    }
}

// ------------------------------------------------------------------ 3b. 并发乱序补齐

/// **回归钉子**：同一上传里两个分块请求并发（乱序补齐时很常见），最终必须收敛成
/// 「每块恰好追加一次」的成品，任何时刻都不该让在传的上传蒸发。
///
/// 历史（v1.0.2 前，数据销毁级）：`Header::read`（`src/header.rs`）**不加锁**，
/// 会撞上并发 `Header::write` 的 `set_len(0) → write_all` 窗口，读到空文件 →
/// `read_index()` 解析失败 → `Error::ReadHeaderFail`；而 `save_chunk` 的错误分支
/// （`src/controller/upload.rs`）对这个错误调用了 `partial.cleanup()` ——
/// **整份上传的 `.part` 与断点一起被删除**，此后同一 tempName 的任何重传都只拿到
/// 「错误：非法操作」（`.part` 已不存在），客户端只能从零重来。
/// 旧实现下每轮约 6.75% 概率触发（400 轮独立复现：373 轮正常、27 轮上传被销毁，
/// 销毁那轮报错恒为「错误：读头文件失败」）。
///
/// v1.0.2 已修：`Header::read` 改**共享锁**（`lock_shared()`，与 write 的排他锁互斥），
/// 读者不再看得到截断窗口。2.0 又把这条修复钉进协议：断点缺失/不可解析一律
/// 409 `upload_not_prepared` 且**绝不 cleanup**（§11.1）。这里仍按 [`RACE_ROUNDS`]
/// 轮跑，把窗口压回概率里 —— 一旦回退到无锁读，本用例会以肉眼可见的频率重新变红。
#[test]
fn concurrent_out_of_order_fill_converges() {
    let harness = Harness::new("out-of-order");
    let uploader = harness.uploader();
    let barrier = Barrier::new(2);

    for round in 0..RACE_ROUNDS {
        let _ = std::fs::remove_dir_all(harness.subdir());

        let content = format!("out-of-order-round-{round}-")
            .repeat(40)
            .into_bytes();
        let hash = md5_hex(&content);
        let pre = take_ok(preprocess(&uploader, "file", "ooo.txt", &content), "预处理");

        let chunks: Vec<&[u8]> = content.chunks(CHUNK).collect();
        let total = chunks.len();
        assert_eq!(total, 4, "round={round}：这轮要恰好四块，实际 {total}");

        let first = save_chunk(&uploader, &pre, "file", &hash, 1, total, chunks[0]);
        assert!(first.is_ok(), "round={round}：第 1 块应被接收");

        // 线程 A 发第 2 块、线程 B 发第 3 块，Barrier 起跑：B 可能先读到断点 1
        // 而撞上「序号跳变」被拒（2.0 起是 409 chunk_out_of_order）—— 真实客户端会
        // 重传，这里照做。任何交错下收敛结果都必须是「每块恰好被追加一次」。
        std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                barrier.wait();
                send_until_accepted(&uploader, &pre, "file", &hash, 2, total, chunks[1])
            });
            let b = scope.spawn(|| {
                barrier.wait();
                send_until_accepted(&uploader, &pre, "file", &hash, 3, total, chunks[2])
            });

            let a = a.join().expect("线程 A 不应 panic");
            let b = b.join().expect("线程 B 不应 panic");

            assert!(a.is_ok(), "round={round}：第 2 块未收敛：{a:?}");
            assert!(
                b.is_ok(),
                "round={round}：第 3 块未收敛：{b:?}（.part 是否存在：{}）",
                harness.part_path(&pre).exists()
            );
        });

        let last = take_ok(
            save_chunk(
                &uploader,
                &pre,
                "file",
                &hash,
                total,
                total,
                chunks[total - 1],
            ),
            &format!("round={round} 末块"),
        );
        assert_eq!(last.saved_path, format!("file_subdir_{hash}.txt"));

        let name = format!("{hash}.txt");
        assert_eq!(
            std::fs::read(harness.subdir().join(&name)).unwrap(),
            content,
            "round={round}：乱序补齐后内容必须逐字节一致"
        );
        assert_eq!(
            dir_entries(&harness.subdir()),
            vec![name],
            "round={round}：只应留一份成品"
        );
        harness.assert_no_headers(&format!("round={round}"));
    }
}

// ------------------------------------------------------------------ 3c. 同序号首次并发

/// **回归钉子**：同一序号**首次**并发发送必须被安全吸收 —— 一个真正写入，另一个走
/// 幂等跳过；内容恰好被追加一次，成品逐字节一致。
///
/// 与上面那条「重发已收块」的区别：这里两个线程起跑前**谁都没收到过该块**（断点为 0），
/// 双方都会通过 `last + 1` 检查 —— 这正是客户端超时重传与首个请求重叠时发生的事。
///
/// 历史（v1.0.3 前，数据销毁级）：只给单次读写加锁不够，「读断点 → 序号校验 →
/// 追加 → 回写断点」整段不原子，两个线程双双读到同一个 `last`、双双追加 →
/// `.part` 多出一块 → 末块 `check_size` 尺寸不符 → `Err` → `cleanup()` 销毁整份上传
/// （fail-closed，不落错文件，但客户端白传且无法续传）。独立复现：两线程首传同一序号，
/// 400 轮里 **298 轮（74.5%）上传被销毁**。
///
/// v1.0.3 已修：`Header::lock_exclusive()` 的 RAII guard 把整段包进同一把排他锁
/// （`save_chunk` 里同一临时名的分块写入串行化）。2.0 下败者的答复从静默 200 变成
/// 409 `chunk_out_of_order` 且 `expected_index > 本块`（＝本块已落，按 §7 静默推进），
/// 这里按 [`RACE_ROUNDS`] 轮跑。
#[test]
fn concurrent_first_sends_of_the_same_chunk_never_double_append() {
    let harness = Harness::new("first-send");
    let uploader = harness.uploader();
    let barrier = Barrier::new(2);

    for round in 0..RACE_ROUNDS {
        let _ = std::fs::remove_dir_all(harness.subdir());

        let content = format!("first-send-round-{round}-").repeat(40).into_bytes();
        let hash = md5_hex(&content);
        let pre = take_ok(
            preprocess(&uploader, "file", "first.txt", &content),
            "预处理",
        );
        assert!(pre.saved_path.is_empty(), "round={round}：预处理不该秒传");

        let chunks: Vec<&[u8]> = content.chunks(CHUNK).collect();
        let total = chunks.len();
        assert!(total >= 3, "round={round}：至少要三块，实际 {total}");

        // 两个线程 Barrier 齐发第 1 块：此前谁都没收到过它（断点为 0）
        let first_body = chunks[0];
        let results: Vec<Result<SaveChunkOk, Problem>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let uploader = &uploader;
                    let barrier = &barrier;
                    let pre = &pre;
                    let hash = hash.as_str();

                    scope.spawn(move || {
                        barrier.wait();
                        save_chunk(uploader, pre, "file", hash, 1, total, first_body)
                    })
                })
                .collect();

            handles
                .into_iter()
                .map(|handle| handle.join().expect("工作线程不应 panic"))
                .collect()
        });

        // 恰好一个写入、另一个拿到「已经落了」的 409（排他锁把读断点→追加→回写
        // 整段串行化），绝不允许两个都写、也不允许两个都拒
        let mut wrote = 0;
        for (worker, result) in results.iter().enumerate() {
            match result {
                Ok(ok) => {
                    wrote += 1;
                    assert!(
                        ok.saved_path.is_empty(),
                        "round={round} worker={worker}：第 1 块不是末块，不该给出 saved_path"
                    );
                }
                Err(problem) => {
                    assert_eq!(
                        problem.status(),
                        409,
                        "round={round} worker={worker}：{}",
                        describe(problem)
                    );
                    assert_eq!(
                        problem.code,
                        "chunk_out_of_order",
                        "round={round} worker={worker}：{}",
                        describe(problem)
                    );
                    assert!(
                        expected_index(problem) > 1,
                        "round={round} worker={worker}：expected_index 应大于本块（＝本块已落）：{}",
                        describe(problem)
                    );
                }
            }
        }

        assert_eq!(wrote, 1, "round={round}：并发首传必须恰好写入一次");

        // 直接量 `.part`：重复追加会在这里现形（比等到末块校验更早、更明确）
        assert_eq!(
            std::fs::metadata(harness.part_path(&pre)).unwrap().len() as usize,
            chunks[0].len(),
            "round={round}：并发首传后 `.part` 必须恰好等于一块"
        );

        // 补齐到末块：重复追加会让 `.part` 多出一块，末块的大小校验会在这里炸
        for index in 2..=total {
            let ok = take_ok(
                save_chunk(
                    &uploader,
                    &pre,
                    "file",
                    &hash,
                    index,
                    total,
                    chunks[index - 1],
                ),
                &format!("round={round} 第 {index}/{total} 块"),
            );

            if index == total {
                assert_eq!(ok.saved_path, format!("file_subdir_{hash}.txt"));
            }
        }

        let name = format!("{hash}.txt");
        assert_eq!(
            std::fs::read(harness.subdir().join(&name)).unwrap(),
            content,
            "round={round}：成品必须逐字节一致（重复追加会让尺寸/md5 对不上）"
        );
        assert_eq!(
            dir_entries(&harness.subdir()),
            vec![name],
            "round={round}：只应留一份成品"
        );
        assert!(
            !harness.part_path(&pre).exists(),
            "round={round}：发布后不该留下 .part 残留"
        );
        harness.assert_no_headers(&format!("round={round}"));
    }
}
