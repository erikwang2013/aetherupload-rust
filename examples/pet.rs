// Copyright (c) 2026 erik <erik@erik.xyz> — https://erik.xyz

//! 打印项目宠物「以太兽 · Aether Beast」。
//!
//! ```bash
//! cargo run --example pet
//! ```

fn main() {
    println!("{}", aetherupload::pet::ASCII);
    println!();
    println!("{} —— {}", aetherupload::pet::NAME, aetherupload::pet::TAGLINE);
    println!();
    println!("形象文件 docs/pet.svg 以常量形式内嵌在 [`pet::SVG`]（{} 字节）：", aetherupload::pet::SVG.len());
    println!("示例页 favicon、四张架构图里的吉祥物、CLI 的 `aetherupload pet` 用的都是同一份。");
}
