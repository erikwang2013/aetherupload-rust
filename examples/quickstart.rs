// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 快速开始：把四条路由挂进 Axum，顺带把示例页与前端脚本一起端出来。
//!
//! ```bash
//! cargo run --example quickstart --features axum
//! # 打开 http://127.0.0.1:3000/ ，选个文件就能看到分块上传的进度
//! ```
//!
//! 示例页与前端脚本都以常量形式内嵌在库里（`aetherupload::assets`），
//! 所以这个例子不需要任何静态目录。

use std::sync::Arc;

use aetherupload::assets;
use aetherupload::{Config, Runtime};
use axum::Router;
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::current_dir()?;
    let runtime = Arc::new(Runtime::new(Config::default(), &base)?);

    // 建上传根目录、_header 与各分组目录 —— 等价于 PHP 版的 `php webman aetherupload:groups`
    let mut say = |line: &str| println!("{line}");
    if aetherupload::console::list_groups(&runtime, &mut say) != 0 {
        eprintln!("建目录失败，先修好再启动");
        std::process::exit(1);
    }

    let app = Router::new()
        // 四条上传/读取路由
        .merge(aetherupload::integrations::axum::routes(runtime))
        // 示例页与前端脚本（真实项目里放你自己的前端即可）
        .route("/", get(example_page))
        .route("/aetherupload-assets/aetherupload-all.js", get(script))
        .route(
            "/aetherupload-assets/aetherupload-core.js",
            get(core_script),
        )
        .route("/aetherupload-assets/pet.svg", get(pet_svg));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    println!("\n示例页: http://127.0.0.1:3000/");

    axum::serve(listener, app).await?;

    Ok(())
}

async fn example_page() -> Html<&'static str> {
    Html(assets::EXAMPLE_PAGE)
}

async fn script() -> Response {
    asset(assets::SCRIPT, "text/javascript; charset=utf-8")
}

async fn core_script() -> Response {
    asset(assets::CORE_SCRIPT, "text/javascript; charset=utf-8")
}

async fn pet_svg() -> Response {
    asset(assets::PET_SVG, "image/svg+xml")
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}
