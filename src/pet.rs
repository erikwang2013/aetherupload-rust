// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 项目宠物：**以太兽 · Aether Beast**。
//!
//! 本模块不含逻辑，只有形象本身 —— README、CLI banner、下游管理界面共用同一份。
//! 形象文件是 `docs/pet.svg`（从 PHP 版 aetherupload-webman 原样搬来），
//! 终端里用 [`ASCII`]。
//!
//! 人设取自本库的上传模型：头顶向上的箭头是**整份文件**，腹部的进度条是
//! **正在追加的分块**（浅色那段还没到），右边飞来的小方块是**下一个分块** ——
//! 它断在半路也没关系，`.part` 与 `chunkIndex` 还在，接上就能继续。
//! 纯装饰：这里没有任何一行碰得到文件、哈希或路径。

/// 项目宠物名。
pub const NAME: &str = "以太兽 · Aether Beast";

/// 一句话人设。
pub const TAGLINE: &str = "箭头朝上，进度条未满 —— 断了接上就是";

/// ASCII 版形象，给终端、日志、CLI banner 用。
pub const ASCII: &str = r#"            ▲
            │              以太兽 · Aether Beast
        .-~~~-.
      .'  ● ●  '.          头顶箭头 = 整份文件（向上）
     /    ‿‿‿    \         腹部进度条 = 已追加的分块
    (  ▓▓▓▓░░░░░  )        浅色段 = 还没到的那几块
     '.__.___.__.'    ▣    右侧方块 = 正飞来的下一个分块"#;

/// SVG 版形象（`docs/pet.svg`），给 README 与下游界面用。
///
/// 以 `include_str!` 打进库里：零运行时开销，不用就不链接。
/// 也正因如此 `docs/pet.svg` **不能**进 Cargo 的 `exclude` —— 排掉会当场编译失败。
pub const SVG: &str = include_str!("../docs/pet.svg");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_is_bundled_whole() {
        assert!(
            SVG.starts_with("<svg"),
            "SVG 头部: {:?}",
            &SVG[..40.min(SVG.len())]
        );
        assert!(SVG.trim_end().ends_with("</svg>"));
        assert!(SVG.contains(r#"viewBox="0 0 240 240""#), "viewBox 变了");
    }

    /// 形象与库的对账：箭头、进度条、飞来的分块 —— 这三样正是上传链路讲的故事。
    /// 数画法不数颜色：改配色不该让这里变红。
    #[test]
    fn the_beast_still_tells_the_upload_story() {
        assert!(SVG.contains("以太兽"), "形象上的名字没了");
        // 上传箭头（天线）
        assert!(SVG.contains("M120 70 V48"), "头顶箭头没了");
        // 腹部进度条：底槽 + 进度段
        assert_eq!(SVG.matches("height=\"11\"").count(), 2, "进度条结构变了");
        // 正在飞来的分块（三面立方体）
        assert_eq!(
            SVG.matches("<path d=\"M186 64").count(),
            1,
            "飞来的分块没了"
        );
        // 断线续传的意象：分块后面拖着点线轨迹
        assert!(SVG.contains("stroke-dasharray"), "传输轨迹没了");
    }
}
