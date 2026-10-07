// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 随机化 + 边界回环测试：任意长度 × 任意切分 × 各种内容形态，走完整链路
//! （预处理 → 逐块上传 → 末块落盘），逐字节核对成品。
//!
//! 随机化用自带的小 PRNG（splitmix64，零依赖），**固定种子**：失败信息里带
//! 种子与参数（长度、切分、形态），照抄即可复现同一轮。
//!
//! 与 `tests/protocol.rs` 的分工：那边逐条核对协议语义（幂等 / 跳变 / 秒传 /
//! 参数边界，固定切分）；这边只压「切法 × 内容」的组合面 —— 任何切法都不该
//! 改变落盘的字节，也不该留下临时文件。
//!
//! 末块会按**真实内容**复核 MIME（魔数探测 → 反查扩展名 → 再走白名单），随机
//! 内容可能被判成 bin / mp3 / … 等任意扩展名，因此随机档位用「blob」分组
//! （白名单留空 = 只受黑名单约束），把内容形态与白名单配置解耦。
//!
//! 其中一条用例是**回归钉子**：512 字节 MIME 探测窗口曾把「切在多字节字符内部」
//! 的中文文本判成二进制（v1.0.2 已在 `mime.rs` 修好，细节见该函数注释）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aetherupload::config::GroupConfig;
use aetherupload::console;
use aetherupload::json::{PreprocessResult, SaveChunkResult};
use aetherupload::md5::md5_hex;
use aetherupload::{
    ChunkBody, Config, MemoryInstantStore, PreprocessRequest, Runtime, SaveChunkRequest,
    UploadController,
};

// ------------------------------------------------------------------ 自带的小 PRNG

/// splitmix64：零依赖、固定种子可复现（不引 rand；测试也不需要密码学质量）。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `[0, bound)`；测试里取模偏差无所谓。
    fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }

    /// 闭区间 `[lo, hi]`。
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

// ------------------------------------------------------------------ 内容形态

#[derive(Debug, Clone, Copy)]
enum Shape {
    /// UTF-8 文本（含多字节字符）→ 魔数探测判成 text/plain。
    Text,
    /// 全 0 字节 → 含 NUL，判成 application/octet-stream。
    Zeros,
    /// 全 0xFF → `0xFF 0xE0` 起头，判成 audio/mpeg。
    Ones,
    /// 随机字节并保证含 NUL → application/octet-stream。
    Binary,
    /// 纯随机字节（可能恰好是合法 UTF-8 → text/plain）。
    Random,
}

const SHAPES: [Shape; 5] = [
    Shape::Text,
    Shape::Zeros,
    Shape::Ones,
    Shape::Binary,
    Shape::Random,
];

impl Shape {
    fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Zeros => "zeros",
            Self::Ones => "ones",
            Self::Binary => "binary-nul",
            Self::Random => "random",
        }
    }

    fn build(self, rng: &mut Rng, len: usize) -> Vec<u8> {
        let mut bytes = match self {
            Self::Zeros => vec![0u8; len],
            Self::Ones => vec![0xffu8; len],
            Self::Text => {
                let mut text = String::with_capacity(len + 32);
                while text.len() < len {
                    text.push_str("以太兽 Aether Beast: 分块上传回环测试 ");
                }
                while text.len() > len {
                    text.pop();
                }
                if text.is_empty() {
                    text.push('a');
                }
                text.into_bytes()
            }
            Self::Binary => {
                let mut bytes: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
                if !bytes.is_empty() {
                    let slot = rng.below(bytes.len() as u64) as usize;
                    bytes[slot] = 0;
                }
                bytes
            }
            Self::Random => (0..len).map(|_| rng.next_u64() as u8).collect(),
        };

        // 随机内容不该掷中 `MZ` / ELF 开头：内核按内容判成 exe，而 exe 在默认黑名单里
        // —— 那是**正确的拒绝**，不是回环失败。换掉首字节，把这条边界留给显式用例。
        if bytes.starts_with(b"MZ") || bytes.starts_with(b"\x7fELF") {
            bytes[0] = bytes[0].wrapping_add(1);
        }

        bytes
    }
}

// ------------------------------------------------------------------ 测试脚手架

/// 「blob」分组的白名单留空（只受黑名单约束）：随机内容会被魔数探测判成
/// bin / mp3 / txt / … 任意扩展名，白名单非空就会把回环测试变成白名单测试。
fn roundtrip_config() -> Config {
    let mut config = Config {
        chunk_size: 1024,
        ..Config::default()
    };

    config.groups.insert(
        "blob".into(),
        GroupConfig {
            group_dir: "file".into(), // 复用默认分组目录，省一个 mkdir
            resource_maxsize: 104_857_600,
            resource_extensions: Vec::new(),
            event_before_upload_complete: false,
            event_upload_complete: false,
        },
    );

    config
}

struct Harness {
    base: PathBuf,
    runtime: Arc<Runtime>,
}

impl Harness {
    fn new(tag: &str) -> Self {
        Self::with_config(tag, roundtrip_config())
    }

    /// 秒传开启的脚手架（默认配置里 instant_completion = false）。
    fn instant(tag: &str) -> Self {
        Self::with_config(
            tag,
            Config {
                instant_completion: true,
                ..roundtrip_config()
            },
        )
    }

    fn with_config(tag: &str, config: Config) -> Self {
        let base = std::env::temp_dir().join(format!(
            "aetherupload-roundtrip-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);

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

    fn group_dir(&self, group: &str) -> String {
        self.runtime
            .config()
            .groups
            .get(group)
            .expect("分组应存在")
            .group_dir
            .clone()
    }

    fn preprocess(&self, ctx: &str, group: &str, name: &str, content: &[u8]) -> PreprocessResult {
        let pre = self.uploader().preprocess(&PreprocessRequest {
            resource_name: Some(name.to_string()),
            resource_size: Some(content.len().to_string()),
            group: Some(group.to_string()),
            resource_hash: Some(md5_hex(content)),
            locale: Some("zh".into()),
        });

        assert_eq!(pre.error, None, "{ctx}：预处理失败：{:?}", pre.error);
        pre
    }

    fn save_chunk(
        &self,
        pre: &PreprocessResult,
        group: &str,
        content: &[u8],
        index: usize,
        total: usize,
        body: &[u8],
    ) -> SaveChunkResult {
        self.uploader().save_chunk(&SaveChunkRequest {
            chunk_total: Some(total.to_string()),
            chunk_index: Some(index.to_string()),
            resource_temp_basename: Some(pre.resource_temp_base_name.clone()),
            resource_ext: Some(pre.resource_ext.clone()),
            group_subdir: Some(pre.group_sub_dir.clone()),
            group: Some(group.to_string()),
            resource_hash: Some(md5_hex(content)),
            locale: Some("zh".into()),
            chunk: Some(ChunkBody::Bytes(body.to_vec())),
        })
    }

    /// 完整链路：预处理 → 逐块上传 → 核对落盘。返回（预处理结果，末块响应）。
    fn roundtrip(
        &self,
        ctx: &str,
        group: &str,
        name: &str,
        content: &[u8],
        chunk_size: usize,
    ) -> (PreprocessResult, SaveChunkResult) {
        let pre = self.preprocess(ctx, group, name, content);
        assert!(
            pre.saved_path.is_empty(),
            "{ctx}：非秒传场景预处理不应给出 savedPath"
        );

        let chunks: Vec<&[u8]> = content.chunks(chunk_size).collect();
        let total = chunks.len();
        let mut last = SaveChunkResult::default();

        for (offset, chunk) in chunks.iter().enumerate() {
            let index = offset + 1;
            last = self.save_chunk(&pre, group, content, index, total, chunk);
            assert_eq!(
                last.error, None,
                "{ctx}：第 {index}/{total} 块失败：{:?}",
                last.error
            );

            if index == total {
                assert!(!last.saved_path.is_empty(), "{ctx}：末块必须给出 savedPath");
            } else {
                assert!(
                    last.saved_path.is_empty(),
                    "{ctx}：第 {index}/{total} 中间块的 savedPath 必须为空"
                );
            }
        }

        self.assert_landed(ctx, group, &pre, content, &last);

        (pre, last)
    }

    /// 成品：`<root>/<group_dir>/<subdir>/<md5>.<ext>`，逐字节一致，临时件清光。
    fn assert_landed(
        &self,
        ctx: &str,
        group: &str,
        pre: &PreprocessResult,
        content: &[u8],
        last: &SaveChunkResult,
    ) {
        let name = format!("{}.{}", md5_hex(content), pre.resource_ext);
        assert_eq!(
            last.saved_path,
            format!("{group}_{}_{name}", pre.group_sub_dir),
            "{ctx}：savedPath 应为三段式「分组_子目录_md5.ext」"
        );

        let landed = self
            .upload_root()
            .join(self.group_dir(group))
            .join(&pre.group_sub_dir)
            .join(&name);

        assert!(landed.is_file(), "{ctx}：成品未落盘：{}", landed.display());
        assert_eq!(
            std::fs::read(&landed).unwrap(),
            content,
            "{ctx}：落盘内容必须与原文逐字节一致"
        );

        self.assert_clean(ctx, landed.parent().expect("有分组子目录"));
    }

    /// `_header` 断点与 `*.part` 都不该留下。
    fn assert_clean(&self, ctx: &str, dir: &Path) {
        let headers = dir_entries(&self.upload_root().join("_header"));
        assert!(headers.is_empty(), "{ctx}：断点文件应已清理：{headers:?}");

        let parts: Vec<String> = dir_entries(dir)
            .into_iter()
            .filter(|name| name.ends_with(".part"))
            .collect();
        assert!(parts.is_empty(), "{ctx}：分块文件应已清理：{parts:?}");
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

// ------------------------------------------------------------------ 随机长度 × 随机切分

#[test]
fn random_lengths_and_splits_roundtrip() {
    const SEED: u64 = 0x5EED_0A17_2026_1007;
    const ROUNDS: usize = 12;

    let harness = Harness::new("random");
    let mut rng = Rng::new(SEED);

    for round in 0..ROUNDS {
        let len = rng.range(1, 200_000) as usize;
        let shape = *rng.pick(&SHAPES);
        let content = shape.build(&mut rng, len);

        // 切分下限按「块数不超过协议的 10000 上限」兜底
        let floor = content.len().div_ceil(10_000).max(1) as u64;
        let chunk_size = rng.range(floor, content.len() as u64 * 2) as usize;

        let ctx = format!(
            "seed={SEED:#x} round={round} len={} chunk_size={chunk_size} shape={}",
            content.len(),
            shape.name()
        );

        harness.roundtrip(&ctx, "blob", "random.bin", &content, chunk_size);
    }
}

// ------------------------------------------------------------------ 边界档位（显式枚举）

#[test]
fn boundary_lengths_and_split_points_roundtrip() {
    const CHUNK: usize = 1024;

    let harness = Harness::new("boundary");

    let cases: [(usize, usize); 8] = [
        (1, CHUNK),             // 一字节文件
        (CHUNK - 1, CHUNK),     // 差一字节满一块
        (CHUNK, CHUNK),         // 恰好一块
        (CHUNK + 1, CHUNK),     // 一块零一字节
        (CHUNK * 2, CHUNK),     // 恰好两块
        (CHUNK * 2 + 1, CHUNK), // 两块零一字节
        (300, CHUNK * 4),       // 切分远大于文件
        (40, 1),                // 每块一字节
    ];

    for (len, chunk_size) in cases {
        let content: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
        let ctx = format!("boundary len={len} chunk_size={chunk_size}");

        // 配置里的分块大小原样透出（用例自己的切分只是客户端行为）
        let (pre, _) = harness.roundtrip(&ctx, "blob", "boundary.bin", &content, chunk_size);
        assert_eq!(pre.chunk_size, CHUNK as u64, "{ctx}：chunk_size 应来自配置");
    }
}

// ------------------------------------------------------------------ 内容形态

#[test]
fn content_shapes_roundtrip() {
    let harness = Harness::new("shapes");
    let mut rng = Rng::new(0x5EED_5A9E_2026_1111);

    // 全 0 字节 → octet-stream → bin
    harness.roundtrip("shape=zeros", "blob", "zeros.bin", &vec![0u8; 5000], 333);

    // 全 0xFF → audio/mpeg → mp3（扩展名由客户端声明，白名单为空才放行）
    harness.roundtrip("shape=ones", "blob", "ones.bin", &vec![0xffu8; 5000], 1024);

    // UTF-8 文本 → text/plain → txt 在**默认白名单**内，走默认分组。
    // 长度压在 512 字节以内：超过 512 的多字节文本会踩内核的探测窗口缺陷，
    // 那条单独钉在 `utf8_text_longer_than_the_probe_window_is_still_text_plain`。
    let text = "以太兽 Aether Beast 上传测试 ".repeat(10).into_bytes();
    harness.roundtrip("shape=utf8-text", "file", "报告.txt", &text, 137);

    // 纯 ASCII 大文本（1440 字节 > 512）：同样是 text/plain → txt —— 用以对照下面那条
    // 回归钉子（v1.0.2 前是内核缺陷）：出问题的不是「文本超过 512 字节」，而是
    // 512 边界切在多字节字符内部
    let ascii = "aether-upload-ascii-text-".repeat(60).into_bytes();
    harness.roundtrip("shape=ascii-text", "file", "readme.txt", &ascii, 1024);

    // 含 NUL 的二进制 → octet-stream → bin
    let binary = Shape::Binary.build(&mut rng, 8192);
    harness.roundtrip("shape=binary-nul", "blob", "payload.bin", &binary, 500);

    // 真 PNG 魔数 + 随机尾 → image/png → png 在默认白名单内
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend(Shape::Random.build(&mut rng, 3000));
    harness.roundtrip("shape=png", "file", "截图.png", &png, 1024);

    // 重复内容 → 去重路径：两次上传给出同一个 savedPath，磁盘只留一份。
    // 单独一套脚手架：这一轮要在「只装去重产物的目录」上数文件。
    let dedup = Harness::new("dedup");
    let duplicate = Shape::Binary.build(&mut rng, 4096);
    let (first_pre, first_last) = dedup.roundtrip("dedup#1", "blob", "dup.bin", &duplicate, 1000);
    let (second_pre, second_last) = dedup.roundtrip("dedup#2", "blob", "dup.bin", &duplicate, 999);

    assert_eq!(first_pre.group_sub_dir, second_pre.group_sub_dir);
    assert_eq!(
        first_last.saved_path, second_last.saved_path,
        "同内容同扩展名 → 同 savedPath（去重语义）"
    );

    let dir = dedup
        .upload_root()
        .join(dedup.group_dir("blob"))
        .join(&first_pre.group_sub_dir);
    assert_eq!(
        dir_entries(&dir),
        vec![format!("{}.bin", md5_hex(&duplicate))],
        "同内容两次上传只应留一份成品"
    );
}

// ------------------------------------------------------------------ 回归钉子（已修缺陷）

/// **回归钉子**：512 字节探测窗口切在多字节字符内部时，整份文本仍须判成
/// `text/plain`（→ `txt`，在默认白名单内）。
///
/// 历史（v1.0.2 前）：`MagicBytesDetector::detect` 只取文件头 512 字节、且要求这
/// 512 字节**整体**是合法 UTF-8；3 字节汉字被 512 边界切断时 `from_utf8` 失败 →
/// 回落 `application/octet-stream` → `bin`，而 `check_mime_type`（`src/partial.rs`）
/// 是拿**探测出的**扩展名 `bin` 再走白名单，默认 `file` 分组白名单不含 `bin`，
/// 于是末块报「无效的文件类型」。影响面不止本用例：任何「有效 UTF-8 文本 + 长度超过
/// 512 字节 + 第 512 字节落在多字节字符内部」的上传都会被拒，与声明的扩展名无关
/// （大一点的中文 .txt 传不上去）。
///
/// v1.0.2 已修：`mime.rs` 新增 `probe_text()`，仅末尾残缺序列按 `valid_up_to()`
/// 截断后再判文本，中间就损坏的字节仍判二进制。这里内容每周期 36 字节、
/// `512 % 36 == 8`，必然切在汉字内部 —— 一旦回退到旧行为，本用例立刻红。
#[test]
fn utf8_text_longer_than_the_probe_window_is_still_text_plain() {
    let harness = Harness::new("mime-probe");

    let text = "以太兽 Aether Beast 上传测试 ".repeat(200).into_bytes();
    assert_eq!(text.len(), 7200, "周期 36 字节 × 200");

    harness.roundtrip(
        "内核缺陷复现：7200 字节中文文本，第 512 字节切在汉字内部",
        "file",
        "报告.txt",
        &text,
        777,
    );
}

// ------------------------------------------------------------------ 秒传回环

#[test]
fn instant_completion_hit_skips_every_chunk() {
    const SEED: u64 = 0x5EED_1A57_2026_1007;

    let harness = Harness::instant("instant");
    let mut rng = Rng::new(SEED);

    for round in 0..4 {
        let content = Shape::Binary.build(&mut rng, 3000 + round * 137);
        let ctx = format!("seed={SEED:#x} round={round} len={}", content.len());

        // 第一次：完整链路落盘
        let (pre, last) = harness.roundtrip(&ctx, "blob", "instant.bin", &content, 700);

        // 第二次：预处理阶段就命中，一个分块都不用传
        let hit = harness.preprocess(&ctx, "blob", "instant.bin", &content);
        assert_eq!(
            hit.saved_path, last.saved_path,
            "{ctx}：秒传应命中同一 savedPath"
        );
        assert_eq!(hit.group_sub_dir, pre.group_sub_dir);

        let landed = harness
            .upload_root()
            .join(harness.group_dir("blob"))
            .join(&pre.group_sub_dir)
            .join(format!("{}.bin", md5_hex(&content)));
        assert!(landed.is_file(), "{ctx}：秒传目标应真实存在");
        assert_eq!(std::fs::read(&landed).unwrap(), content);

        harness.assert_clean(&ctx, landed.parent().unwrap());
    }
}

// ------------------------------------------------------------------ 总控矩阵

#[test]
fn roundtrip_matrix() {
    const SEED: u64 = 0x5EED_0A7E_2026_1007;
    const ROUNDS: usize = 25;
    const BUCKETS: [usize; 10] = [1, 2, 3, 1023, 1024, 1025, 4096, 65_536, 131_072, 200_000];

    let harness = Harness::new("matrix");
    let mut rng = Rng::new(SEED);

    for round in 0..ROUNDS {
        // 先按显式档位走一遍，再混入随机长度
        let len = match BUCKETS.get(round) {
            Some(&bucket) => bucket,
            None => rng.range(1, 200_000) as usize,
        };

        let shape = *rng.pick(&SHAPES);
        let content = shape.build(&mut rng, len);

        // 块数不能越过协议的 10000 上限（切分下限按此兜底）
        let floor = content.len().div_ceil(10_000).max(1) as u64;
        let chunk_size = rng.range(floor, content.len() as u64 * 2) as usize;

        let ctx = format!(
            "seed={SEED:#x} round={round} len={} chunk_size={chunk_size} shape={}",
            content.len(),
            shape.name()
        );

        harness.roundtrip(&ctx, "blob", "matrix.bin", &content, chunk_size);
    }
}
