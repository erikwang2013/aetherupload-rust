// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 前端资产 —— 对应 PHP 版安装时分发到 `public/vendor/aetherupload/` 的那几个文件。
//!
//! 以 `include_str!` 打进库里：宿主不必再管静态目录，直接从常量里吐出来即可
//! （见 `examples/quickstart.rs` 的做法）。也正因如此 `assets/` **不能**进 Cargo 的
//! `exclude` —— 排掉会当场编译失败。
//!
//! 与 PHP 版的两处差异（都按 Rust 端最优来，不再逐字节相同）：
//!
//! - 脚本的 `preprocess` 改发 **multipart**（`FormData` + `processData/contentType: false`），
//!   与 `uploadChunk`、与所有适配层同一种形态；PHP 原版脚本发 urlencoded，Rust 端
//!   自 1.0.4 起两种编码都收，所以那份不改也能跑。
//! - 示例页补上了 `#aetherupload-wrapper` 容器（组件在它内部找 `#aetherupload-resource`
//!   等节点；PHP 的示例页一直有，本页早先漏了，导致示例页从来没跑到过预处理）。

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

    /// 前端契约不许悄悄漂移（这条守卫的由来：`preprocess` 曾发 urlencoded 而适配层只收
    /// multipart，示例页又缺组件要求的 wrapper —— 两个都不报错，只在浏览器里静默失败）。
    #[test]
    fn frontend_contract_stays_in_sync_with_the_adapters() {
        for (name, script) in [("all", SCRIPT), ("core", CORE_SCRIPT)] {
            let preprocess = script
                .split_once("preprocess: function")
                .unwrap_or_else(|| panic!("{name} 里没有 preprocess"))
                .1;
            let block = preprocess
                .split_once("success: function")
                .unwrap_or_else(|| panic!("{name} 的 preprocess 没有 success 回调"))
                .0;

            assert!(
                block.contains("new FormData()"),
                "{name} 的 preprocess 应当用 FormData（urlencoded 会被只收 multipart 的适配层拒掉）"
            );
            assert!(
                block.contains("processData: false") && block.contains("contentType: false"),
                "{name} 的 preprocess 少了这两项，FormData 会被当成字符串序列化"
            );

            for field in [
                "resource_name",
                "resource_size",
                "resource_hash",
                "locale",
                "group",
            ] {
                assert!(
                    block.contains(&format!("form.append('{field}'")),
                    "{name} 的 preprocess 缺字段 {field}"
                );
            }
        }

        // 组件按 id 在 wrapper 内部找节点：容器必须在三个节点之前出现（位置的近似判据：
        // true 的包含关系交给 tests/integrations 之外的浏览器端到端，这里只防「漏了容器」）
        let wrapper = EXAMPLE_PAGE
            .find(r#"id="aetherupload-wrapper""#)
            .expect("示例页缺组件要求的 #aetherupload-wrapper 容器");

        for id in [
            "aetherupload-resource",
            "aetherupload-output",
            "aetherupload-savedpath",
        ] {
            let node = EXAMPLE_PAGE
                .find(&format!(r#"id="{id}""#))
                .unwrap_or_else(|| panic!("示例页缺 #{id}"));
            assert!(wrapper < node, "#{id} 应当在 #aetherupload-wrapper 内部");
        }
    }
}
