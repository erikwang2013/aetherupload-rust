// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 前端资产 —— 对应 PHP 版安装时分发到 `public/vendor/aetherupload/` 的那几个文件。
//!
//! 内容与 PHP 版逐字节相同（`aetherupload-all.js` 含 zepto 与 spark-md5，开箱即用），
//! 以 `include_str!` 打进库里：宿主不必再管静态目录，直接从常量里吐出来即可
//! （见 `examples/quickstart.rs` 的做法）。也正因如此 `assets/` **不能**进 Cargo 的
//! `exclude` —— 排掉会当场编译失败。

/// 上传组件（含 zepto + spark-md5，单文件引入即可）。
pub const SCRIPT: &str = include_str!("../assets/aetherupload-all.js");

/// 上传组件内核（不带 zepto / spark-md5，页面已单独引入这两个时用）。
pub const CORE_SCRIPT: &str = include_str!("../assets/aetherupload-core.js");

/// 示例页（可直接作为一个路由的响应体）。
pub const EXAMPLE_PAGE: &str = include_str!("../assets/example.html");

/// 项目宠物形象，也是示例页的 favicon。
pub const PET_SVG: &str = crate::pet::SVG;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_are_embedded_whole() {
        // 三个文件都不是空的，且形状正确
        assert!(SCRIPT.len() > 40_000, "上传组件应有几十 KB");
        assert!(SCRIPT.contains("aetherupload"));
        assert!(CORE_SCRIPT.contains("spark"));
        assert!(EXAMPLE_PAGE.starts_with("<!DOCTYPE html>"));
        assert!(EXAMPLE_PAGE.contains("/aetherupload/preprocess"));
        assert!(PET_SVG.starts_with("<svg"));
    }

    /// 示例页引用的路由必须与默认配置一致 —— 改了配置记得同步示例页。
    #[test]
    fn example_page_matches_default_routes() {
        let config = crate::config::Config::default();

        assert!(EXAMPLE_PAGE.contains(&config.route_preprocess));
        assert!(EXAMPLE_PAGE.contains(&config.route_uploading));
    }
}
