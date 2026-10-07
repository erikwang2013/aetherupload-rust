# erikwang2013/aetherupload-rust

[![Test](https://github.com/erikwang2013/aetherupload-rust/actions/workflows/test.yml/badge.svg)](https://github.com/erikwang2013/aetherupload-rust/actions/workflows/test.yml)
[![Release](https://img.shields.io/github/v/release/erikwang2013/aetherupload-rust)](https://github.com/erikwang2013/aetherupload-rust/releases)
[![crates.io](https://img.shields.io/crates/v/aetherupload-rust)](https://crates.io/crates/aetherupload-rust)
[![docs.rs](https://docs.rs/aetherupload-rust/badge.svg)](https://docs.rs/aetherupload-rust)
![MSRV](https://img.shields.io/badge/MSRV-1.89-blue)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

<p align="center">
  <img src="https://raw.githubusercontent.com/erikwang2013/aetherupload-rust/main/docs/pet.svg" alt="以太兽 — AetherUpload 项目宠物" width="160" />
</p>

<p align="center"><strong>以太兽 · Aether Beast</strong> — 项目宠物：头顶向上的箭头是整份文件，腹部的进度条是正在追加的分块，右边飞来的方块是下一个分块</p>

**语言：** **中文** · [English](docs/i18n/README.en.md)

浏览器里把文件切片，逐块追加到服务端的一个临时文件，落盘时用文件内容的 md5 命名 —— 于是**上传、断线续传、秒传、去重、完整性校验共用同一套机制**，同一份代码跑在原生 Rust（`Guard`）与 Axum、Actix Web、Rocket、Poem、Salvo、Warp、bee-rust、e-cat 上，默认 feature 下**零第三方依赖**。

## 项目说明

本项目移植自 PHP 大文件上传扩展包 [AetherUpload-Webman](https://github.com/erikwang2013/aetherupload-webman)（一脉相承自 [AetherUpload-Laravel](https://github.com/peinhu/AetherUpload-Laravel)），并为 Rust 生态重写了配置读取、并发处理与存储层。**协议与 PHP 版逐字段一致**：前端脚本原样可用，字段名（`resource_name` / `resource_chunk` / `chunk_index` …）、错误文案、`savedPath` 寻址、磁盘布局都没变。

**它解决什么**

浏览器直接上传大文件，绕不开三个难题：超过服务端请求体上限就传不上去；网络一断就得从头再来；同样的文件被反复传输。AetherUpload 的做法是 —— 在浏览器里把文件切片，逐块追加到服务端的一个临时文件，落盘时用文件内容的 md5 命名。于是**上传、断线续传、秒传、去重、完整性校验**共用同一套机制，全程不把整个文件读进内存，也不需要为「文件在哪」维护一张数据库表。

**它的形状**

一个 Rust crate，**内核与宿主框架解耦**：同一份代码在原生 Rust（`Guard` 请求守卫）与 Axum、Actix Web、Rocket、Poem、Salvo、Warp、bee-rust、e-cat 下可用。八条适配只做接线 —— 注册四条路由、把 multipart 表单翻译成入参、把内核响应翻译成框架响应；业务逻辑一行都不重复。不依赖数据库；Redis 只在开启秒传时才需要，且以 trait 注入，属可选依赖；S3 兼容存储同样是可选的 feature。

**注意事项**

- 分组名**不能包含下划线** —— 它参与 `savedPath` 的三段式编码，含下划线会让解码错位、该分组下的资源永久 404。配置与上传两个阶段都会被拒。
- 分组目录**必须先建好**（`aetherupload groups` 或库 API）：内核建子目录用的是非递归 `mkdir`，父目录缺失时会报笼统的上传错误，排查起来很难看出是目录问题。
- 秒传索引与实际文件是两份数据。删除资源请走 `Runtime::delete_resource()`（联动清理秒传记录），否则会留下死链；建议每天跑一次 `build` 重建。
- `x_accel_redirect` 是前置服务器（nginx）概念，只对 local 驱动生效；s3 驱动下读路径是 302 预签名 URL。

## 项目功能

**协议与能力**

- [x] 百分比进度条
- [x] 文件类型限制（白名单 + 黑名单 + 按真实内容复核 MIME）
- [x] 文件大小限制（声明大小、增量、末块三次校验）
- [x] 多语言支持（中 / 英）
- [x] 资源分组配置（目录、上限、扩展名各自独立）
- [x] 上传完成事件（前置 / 后置）
- [x] 同步上传 *①*
- [x] 断线续传 *②*
- [x] 文件秒传 *③*
- [x] 自定义中间件 *④*
- [x] 自定义路由
- [x] 宽松模式
- [x] 可选对象存储（S3 兼容）
- [x] 运维命令（`groups` / `build` / `clean`）

*①：同步上传相比异步上传，在上传带宽足够大的情况下速度稍慢，但同步可在上传同时进行文件的拼合，而异步因文件块上传完成的先后顺序不确定，需要在所有文件块都完成时才能拼合，将会导致异步上传在接近完成时需等待较长时间。同步上传每次只有一个文件块在上传，在单位时间内占用服务器的内存较少，相比异步方式可支持更多人同时上传。*

*②：断线续传和断点续传不同，断线续传是指遇到断网或无线网络不稳定时，在不关闭页面的情况下，上传组件会定时自动重试，一旦网络恢复，文件会从未上传成功的那个文件块开始继续上传。断线续传在刷新页面或关闭后重开是无法续传的，之前上传的部分已成为无效文件。*

*③：文件秒传需服务端 Redis 和客户端浏览器支持（FileReader、File.slice()），两者缺一则秒传功能无法生效。默认关闭，需在配置中开启。*

*④：Rust 版不做中间件注册表 —— 宿主直接用自己的中间件机制（Axum 的 `layer`、Actix 的 `wrap`…），路由路径由配置的 `route_*` 给出。*

**工程特性**

- **默认 feature 零第三方依赖**：MD5（RFC 1321 向量锚定）、JSON 输出、769 条 MIME 表与魔数探测、目录寻址全部内置；Redis 客户端与 S3 传输都以 trait 注入。
- **同步内核**：与 PHP 语义一致，`cargo test` 不需要运行时。大文件末块要算整份 md5，在意 worker 占用的宿主把调用放进 `tokio::task::spawn_blocking` 即可。
- **没有全局状态**：PHP 版靠「每请求重建 ConfigMapper 快照」避免常驻进程下串组；Rust 版直接把分组快照作为参数传下去，同样的隔离，不需要锁。
- **内存不装整个文件**：分块逐块追加；末块按 64KB 缓冲流式算 md5；本地下发是流式响应。

**支持的框架**

内核（分块、续传、秒传、校验、寻址）与宿主框架解耦，同一份包在下列宿主下可用，**每个都有真跑完整上传链路的端到端测试**兜底：

| 宿主 | crate | 接入方式 | 端到端测试 |
|---|---|---|---|
| **原生 Rust（无框架）** | — | `Guard` 请求守卫：四个入口 + 一张路由判定表 | `tests/guard.rs` |
| Axum | axum 0.8 | `integrations::axum::routes(runtime)` 返回 `Router` | `tests/integrations/axum.rs` |
| Actix Web | actix-web 4 | `integrations::actix::routes(runtime)` 返回 `web::Scope`，挂进 `App` | `tests/integrations/actix.rs` |
| Rocket | rocket 0.5 | `integrations::rocket::mount(rocket, runtime)` 返回挂了四条路由的 `Rocket` | `tests/integrations/rocket.rs` |
| Poem | poem 3 | `integrations::poem::routes(runtime)` 返回 `Route` | `tests/integrations/poem.rs` |
| Salvo | salvo 1 | `integrations::salvo::routes(runtime)` 返回 `Router` | `tests/integrations/salvo.rs` |
| Warp | warp 0.4 | `integrations::warp::routes(runtime)` 返回 `Filter` | `tests/integrations/warp.rs` |
| bee-rust | bee_router 1 | `integrations::bee_rust::routes(runtime)`（HTTP 层即 axum） | `tests/integrations/bee_rust.rs` |
| e-cat | ecat 4 | `integrations::ecat::routes(runtime)`（HTTP 层即 axum） | `tests/integrations/ecat.rs` |

## 项目目录

```text
aetherupload-rust/
├── src/
│   ├── lib.rs                    crate 文档与再导出
│   ├── runtime.rs                运行时绑定：配置 + 存储 / 秒传 / 事件 / MIME 探测器的注入点
│   ├── config.rs                 配置结构 + 分组快照（PHP ConfigMapper 的 Rust 形态）
│   ├── controller/
│   │   ├── upload.rs             preprocess / saveChunk：协议与安全语义逐条照搬
│   │   └── resource.rs           display / download：三条读路径 + 内联黑名单
│   ├── guard.rs                  原生 Rust 入口：Guard 请求守卫 + 路由判定表
│   ├── form.rs                   表单载体（multipart 解析的产物 → 内核入参）
│   ├── partial.rs                分块文件本体：路径、追加、改名、大小与类型校验
│   ├── header.rs                 断点状态文件：只存一个 chunkIndex
│   ├── resource.rs               成品文件对象（上传完成事件的参数）
│   ├── saved_path.rs             savedPath 三段式无状态寻址
│   ├── instant.rs                秒传索引：InstantStore trait + 键格式 + 内存实现
│   ├── storage/                  存储驱动：Storage trait + local（成品落地与读取）+ s3 + sigv4
│   ├── mime.rs                   MIME 表（769 条，搬自 PHP 版）+ 魔数内容探测器
│   ├── md5.rs                    手写 MD5（RFC 1321 测试向量锚定）
│   ├── json.rs                   上传响应的 JSON 输出（字段名与 PHP 一致）
│   ├── i18n.rs                   消息目录（逐条对齐 PHP 翻译文件）
│   ├── error.rs                  错误类型（对外只暴露「已知错误」）
│   ├── events.rs                 上传完成事件（EventSink trait）
│   ├── util.rs                   路径安全、临时名、下载名消毒、链接
│   ├── console.rs                运维命令：groups / build / clean
│   ├── assets.rs                 前端脚本与示例页（include_str! 内嵌）
│   ├── pet.rs                    项目宠物「以太兽」的 ASCII 与 SVG
│   ├── integrations/             八个框架适配（feature 门控，只做接线）
│   └── bin/aetherupload/         运维 CLI（内含一个零依赖 RESP 客户端）
├── assets/                       前端脚本：aetherupload-all.js（含 zepto + spark-md5）+ 示例页
├── examples/                     快速开始（Axum + 示例页）
├── tests/                        内核单测 + 协议用例 + 随机回环 + 并发竞态 + 原生 Guard + 八框架端到端 + S3
└── docs/
    ├── pet.svg                   项目宠物形象
    ├── architecture.svg          架构设计图
    ├── design.svg                功能设计图
    ├── request-cycle.svg         请求周期图
    ├── upload-lifecycle.svg      上传生命周期图
    └── i18n/                     英文 README 与英文图
```

## 架构设计

<img src="https://raw.githubusercontent.com/erikwang2013/aetherupload-rust/main/docs/architecture.svg" alt="AetherUpload-Rust 架构图">

**四层单向依赖**，上层依赖下层，反向不成立：

| 层 | 职责 | 位置 |
|----|------|------|
| 框架适配层 | 只做接线：注册路由、解析 multipart、映射响应 | `src/integrations/` + `src/guard.rs` |
| 用例层 | 两个入口：上传（预处理 + 分块）与读取（展示 + 下载） | `src/controller/` |
| 领域层 | 分块文件、断点、成品、寻址、MIME、配置快照 | `src/{partial,header,resource,saved_path,mime,config}.rs` |
| 端口层 | 存储 / 秒传 / 事件 / MIME 探测器四个 trait 与默认实现 | `src/{storage,instant,events,mime}.rs` |

内核**不认识任何框架**：适配层的每个文件都只有「取参数 → 调内核 → 拼响应」三件事。

## 功能设计

<img src="https://raw.githubusercontent.com/erikwang2013/aetherupload-rust/main/docs/design.svg" alt="AetherUpload-Rust 功能设计">

- **磁盘上永远只有三类文件**：`*.part`（分块）、`_header/<临时名>`（断点，只存一个 chunkIndex）、`<md5>.<ext>`（成品）。没有数据库表，没有元数据文件。
- **无状态寻址**：客户端拿到 `savedPath`（`分组_子目录_文件名`）原样回传，服务端拆三段即可定位，不需要记「谁传了什么」。
- **秒传索引可选 + 每条记录独立 TTL**：不接 Redis 的站点照样能上传（只是秒传失效）；接了 Redis 的站点，每条记录 `SETEX` 各自过期，既不共享过期时间，也不会因为持续上传而无限膨胀。
- **一致性交给运维命令**：`build`（按磁盘现状重建秒传索引）与 `clean`（按 mtime 回收临时文件）各跑一条 cron，比在请求路径上加锁便宜得多。
- **写路径同步、内存可控**：追加是 `append` 语义的顺序写，末块校验后改名落盘；整个过程不把整份文件读进内存。

## 请求周期

<img src="https://raw.githubusercontent.com/erikwang2013/aetherupload-rust/main/docs/request-cycle.svg" alt="AetherUpload-Rust 请求周期">

四条路由，两种响应形态：

| 路由 | 方法 | 入口 | 响应 |
|---|---|---|---|
| `route_preprocess`（默认 `/aetherupload/preprocess`） | POST | `UploadController::preprocess` | JSON：`chunkSize` / `resourceTempBaseName` / `groupSubDir` / `resourceExt` / `savedPath` |
| `route_uploading`（默认 `/aetherupload/uploading`） | POST | `UploadController::save_chunk` | JSON：`savedPath`（末块或秒传命中时非空） |
| `route_display`（默认 `/aetherupload/display/{saved_path}`） | GET | `ResourceController::display` | 文件本体 / 302 预签名 / X-Accel-Redirect |
| `route_download`（默认 `/aetherupload/download/{saved_path}/{new_name}`） | GET | `ResourceController::download` | 同上，且强制附件 |

上传接口的错误一律 **HTTP 200 + `error` 字段**（与 PHP 版一致，前端按 `error` 真值判断）；读路径的失败是 404 文本（`display fail` / `download fail`）。

## 上传生命周期

<img src="https://raw.githubusercontent.com/erikwang2013/aetherupload-rust/main/docs/upload-lifecycle.svg" alt="AetherUpload-Rust 上传生命周期">

主路径只有四步：**预处理 → 分块（循环）→ 最后一块校验 → 落盘**。

1. **预处理**（`preprocess`）：校验参数与分组 → 生成临时名与子目录 → 秒传判定（命中直接返回 `savedPath`，一个分块都不用传）→ 建空的 `.part`，把 `chunkIndex=0` 写进 `_header`。
2. **分块**（`saveChunk`，循环）：每块的顺序固定为「校验 → 追加 → 回写 chunkIndex」。重发同一序号会被幂等跳过；序号跳变或分块被截断只返回错误，**不会清理** `.part` 与 header，所以弱网下客户端可以持续重试直到补上缺的那一块。
3. **最后一块**：校验整份大小 → 按真实内容复核 MIME → 触发「上传完成前」事件 → 流式重算整份 md5 → 与客户端声明的 hash 比对（`lax_mode` 关闭时），不一致整份丢弃。
4. **落盘**：改名成 `<md5>.<ext>`（同 hash 已存在则去重丢弃本次临时文件）→ 写秒传索引 → 删断点文件 → 触发「上传完成」事件。

真正会清理已拼好进度的只有两种情况：末块校验不通过（整份丢弃），以及页面关闭后由 `clean` 按 mtime 回收。

## 安装

```bash
cargo add aetherupload-rust                  # 默认 feature：零第三方依赖的内核
cargo add aetherupload-rust --features axum  # 需要哪个宿主就开哪个 feature
```

feature 一览：`axum` / `actix` / `rocket` / `poem` / `salvo` / `warp` / `bee-rust` / `ecat`（框架接线）、`s3`（S3 兼容存储）。

**通用两步**（无论哪个宿主）：

1. **建存储目录**：`aetherupload groups`（或库 API `console::list_groups`）创建根目录、`_header` 与各分组目录。**不建就一定失败**。
2. **挂路由**：把四条路由挂进宿主应用（见下）。

## 配置结构

配置是 Rust 结构体，字段名与 PHP 版逐字相同：

```rust
use std::collections::BTreeMap;
use aetherupload::config::{Config, GroupConfig, SubdirRule};

let mut config = Config::default();          // 默认值与 PHP config/aetherupload.php 一致

config.root_dir = "storage/app/aetherupload".into();  // 上传根目录（相对项目根）
config.chunk_size = 1_000_000;               // 分块大小（字节），建议 1MB～4MB
config.resource_subdir_rule = SubdirRule::Month;      // year / month / date / const
config.instant_completion = false;           // 秒传开关（需要 Redis 与浏览器支持）
config.lax_mode = false;                     // 宽松模式：跳过 hash 计算与完整性校验
config.x_accel_redirect = false;             // 交给 nginx 直发文件

// 分组：group_dir 是磁盘目录名，resource_maxsize 为 0 表示不限（声明大小为 0 仍会被拒）
config.groups.insert("video".into(), GroupConfig {
    group_dir: "video".into(),
    resource_maxsize: 0,
    resource_extensions: vec!["mp4".into(), "mov".into()],
    event_before_upload_complete: false,
    event_upload_complete: false,
});
```

四条路由路径（前端要同步用 `setXxxRoute()` 改）：`route_preprocess`、`route_uploading`、`route_display`、`route_download`，默认分别是 `/aetherupload/{preprocess,uploading,display,download}`。

## 原生用法（Guard）

不依赖任何框架：手写服务器（hyper / tiny-http / 自研 TCP 服务，或尚未适配的框架）用 `Guard` 接线即可 —— 八个框架适配器内部做的也是这四件事，区别只在于参数从哪个框架的请求对象里取出来。

```rust
use std::sync::Arc;

use aetherupload::{Config, FormData, Guard, GuardRoute, Runtime};

let runtime = Arc::new(Runtime::new(Config::default(), ".")?);

// 接线期构造一次，之后每请求克隆（两次原子计数）
let guard = Guard::new(runtime);

// 手写服务器拿到「方法 + 路径 + 表单」后：
match guard.routes().classify(method, path) {
    Some(GuardRoute::Preprocess) => {
        let json = guard.preprocess(&form);        // JsonBody：200 + JSON，body 直接回给前端
        respond(json.status, json.content_type, json.body)
    }
    Some(GuardRoute::Uploading) => {
        let json = guard.save_chunk(&form);        // 分块本体放在表单字段 resource_chunk
        respond(json.status, json.content_type, json.body)
    }
    Some(GuardRoute::Display { saved_path }) => {
        serve(guard.display(&saved_path))          // ResourceResponse：状态码 + headers() + 文件路径
    }
    Some(GuardRoute::Download { saved_path, new_name }) => {
        serve(guard.download(&saved_path, &new_name))
    }
    None => respond(404, "text/plain", "not found"),
}
```

表单字段用 `FormData::from_pairs(...)`（文本字段）+ `push_file("resource_chunk", ChunkBody::Bytes(..))` 组装 —— 重名取最后一个，与 PHP 解析表单的语义一致。

`ResourceResponse` 的三种形态（`Redirect` / `AccelRedirect` / `ServeFile`）各自带好 `status()` 与 `headers()`；`ServeFile` 的附件名在 `download_name` 里，附件头用 `aetherupload::controller::attachment_disposition(name)` 生成（八个适配器也是这么做的）—— 完整示例见 `tests/guard.rs` 里的 `resource_response_to_http()`。

## Axum

```rust
use std::sync::Arc;
use aetherupload::{Config, Runtime};

let runtime = Arc::new(Runtime::new(Config::default(), ".")?);

// 四条路由（预处理 / 分块 / 展示 / 下载）一次挂好
let app = aetherupload::integrations::axum::routes(runtime.clone());

// 需要给上传路由加权限控制时，直接用自己的 layer
// let app = app.layer(middleware::from_fn(auth));
let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
axum::serve(listener, app).await?;
```

其余七个宿主同理，接入点见上面的[支持的框架](#支持的框架)表：Actix Web 用 `routes()` 拿 `web::Scope`、Rocket 用 `mount(rocket, runtime)`、Poem 拿 `Route`、Salvo 拿 `Router`（`hoop` 注入 Runtime）、Warp 拿 `Filter`、bee-rust 与 e-cat 复用 axum 的 `Router`。仓库里 `examples/quickstart.rs` 是可直接跑起来的 Axum 示例（含示例页与前端脚本）。

## 运维命令

```bash
cargo run --bin aetherupload -- groups                  # 建根目录、_header 与各分组目录
cargo run --bin aetherupload -- build --redis redis://127.0.0.1:6379/0   # 按磁盘重建秒传索引
cargo run --bin aetherupload -- clean 2                 # 清理 2 天前的临时文件（_header 与 *.part）
cargo run --bin aetherupload -- pet                     # 打印项目宠物
```

自定义配置的宿主请直接调用库 API（把自家 `Config` 传进去）：

```rust
use aetherupload::console;

let mut say = |line: &str| println!("{line}");
let code = console::list_groups(&runtime, &mut say);           // 0 = 成功
let code = console::build_redis_hashes(&runtime, &mut say);    // 需要接了 InstantStore
let code = console::clean_up_directory(&runtime, &mut say, 2); // 2 天前
```

建议的 cron（与 PHP 版一致）：

```cron
0 0 * * * /路径/aetherupload clean 1
0 0 * * * /路径/aetherupload build --redis redis://127.0.0.1:6379/0
```

## 秒传（可选）

秒传索引以 trait 注入 —— 本 crate **不依赖任何 Redis 客户端**：

```rust
use std::sync::Arc;
use aetherupload::instant::InstantStore;

struct MyRedis(/* 你的客户端 */);

impl InstantStore for MyRedis {
    fn get(&self, key: &str) -> aetherupload::Result<Option<String>> { /* GET */ Ok(None) }
    fn set_ex(&self, key: &str, value: &str, ttl_seconds: u64) -> aetherupload::Result<()> { /* SETEX */ Ok(()) }
    fn del(&self, key: &str) -> aetherupload::Result<()> { /* DEL，幂等 */ Ok(()) }
}

let runtime = runtime.with_instant(Arc::new(MyRedis(/* ... */)));
```

- 键格式与 PHP 版一致：`aetherupload:resource:<分组>_<hash>`，每条独立 TTL（默认 7 天，`resource_redis_expire` 可改）。
- 也兼容旧版单 hash（`aetherupload_resource`）的回退读取 —— 从 PHP 版迁移过来的站点不用先跑一遍 `build`。
- 旧版回退按**能力位**触发（`InstantStore::supports_legacy_fallback`，默认 `false`）：没声明就不发那次注定为空的往返，未命中只花一次 `GET`；声明了的实现（仓库 CLI 里的 RESP 客户端）行为与不带能力位时一字不差。
- 没接 Redis 却开了 `instant_completion` 时**会报错**（而不是让秒传静默失效），与 PHP 版同一取向。
- 测试与示例可以用内置的 `MemoryInstantStore`。

## 可选对象存储（S3 兼容）

成品默认落本地磁盘；把 `storage.driver` 改成 `s3` 即可切换：

```rust
use aetherupload::config::{PayloadSigning, S3Config, StorageConfig, StorageDriver};

config.storage = StorageConfig {
    driver: StorageDriver::S3,
    s3: S3Config {
        endpoint: "".into(),            // 留空 = AWS 默认（按 region 推导）；自托管填自家端点
        region: "us-east-1".into(),
        bucket: "my-bucket".into(),
        access_key: "".into(),
        secret_key: "".into(),
        path_style: true,               // MinIO / Ceph 等自托管必须 true
        prefix: "uploads".into(),       // 对象键前缀（可空）
        multipart_threshold: 104_857_600,
        payload_signing: PayloadSigning::Hash,  // 华为云 OBS 等只接受 unsigned 的填 Unsigned
    },
};
```

- **读路径**：`display` / `download` 返回 302 到预签名 URL，`Content-Disposition` 与内联黑名单（svg 强制转附件）语义照常保留。
- **保持本地语义**：分块暂存、`groups` / `build` / `clean` 仍作用于本地文件；`x_accel_redirect` 只对 local 驱动生效。
- **HTTP 传输以 trait 注入**：与秒传同一取向，本 crate 不捆绑 HTTP 客户端，宿主用 `reqwest` / `ureq` / `curl` 包一层即可（`HttpBody::{Empty, Bytes, File}`，`File` 用于大文件的流式上传）：

```rust
use std::sync::Arc;
use aetherupload::{HttpRequest, HttpResponse, HttpTransport, S3Config, S3Storage};

struct MyTransport(/* 你的 HTTP 客户端 */);

impl HttpTransport for MyTransport {
    fn send(&self, request: HttpRequest) -> aetherupload::Result<HttpResponse> {
        // 网络异常返回 Err（会触发重试）；非 2xx 按状态码原样返回，不算 Err
        todo!("把你的客户端接进来")
    }
}

let storage = S3Storage::new(S3Config { /* … */ , ..Default::default() }, ".")?
    .with_transport(Arc::new(MyTransport(/* … */)));

let runtime = runtime.with_storage(Arc::new(storage));
```

## 安全性

上传前用白名单 + 黑名单过滤扩展名，上传后按**真实内容**（魔数探测器）复核 MIME —— 白名单直接限制保存扩展名，黑名单默认屏蔽常见的可执行文件扩展名；白名单为空时另有可执行扩展名的硬拒列表兜底。客户端提交的路径分量（`group_subdir` / `resource_temp_basename` / `resource_ext`）一律过安全字符集校验，`savedPath` 解码后逐段校验 —— 目录穿越进不来。可内联渲染的扩展名（svg / html / js / …）在下发时强制转附件并带 `X-Content-Type-Options: nosniff`。

虽然做了诸多安全工作，但恶意文件上传是防不胜防的，建议正确设置上传目录权限，确保相关程序对资源文件没有执行权限。

## 与 PHP 版的差异

| 项 | PHP 版（aetherupload-webman） | Rust 版 |
|---|---|---|
| 分发方式 | `composer require` 自动分发配置 / 路由 / 命令 / 前端脚本 | crate 依赖 + 显式挂路由（`integrations::*::routes`）+ 自带 `assets/` 前端脚本 |
| 配置载体 | `config/aetherupload.php` 数组 | `Config` 结构体（键名相同，编译期查错） |
| 中间件 | 配置里写中间件类名（`middleware_*`） | 用宿主自己的中间件机制；路由路径仍取 `route_*` |
| 每请求状态 | `RequestContext` + 每请求重建的 `ConfigMapper` 单例 | 分组快照显式传参，无全局状态 |
| 断点文件并发 | `file_get_contents` 无锁读，可能与 `ftruncate` 撞窗口 | `read` 取**共享锁**（与写入的排他锁互斥）：并发分块不会读到空断点、更不会因此清掉整份上传 |
| 错误文案 | 已知异常统一折叠成 `upload_error` 的译文 | 按错误种类给出具体译文（客户端只判 `error` 真值，协议兼容） |
| MIME 探测 | `ext-fileinfo` 的 `mime_content_type()` | 内置魔数探测器（`MimeDetector` trait 可替换）；`.docx` 这类会按 `application/zip` 判定，与 fileinfo 的判断可能不同。探测窗口 512 字节，**落在窗口末尾的残缺多字节序列按文本处理** —— 中文 `.txt` 不会因为窗口切在汉字中间被误判成二进制 |
| 子目录规则 | 服务器本地时区的 `date()` | **UTC**（零依赖下拿不到时区库）；只在月末月初的时区边界上有差异 |
| 伪随机临时名 | `random_bytes()`（CSPRNG） | `/dev/urandom`（同一 CSPRNG）；读不到时退化为时间 ^ pid 的 splitmix64 |
| HTTP Range | webman 不实现 Range（靠 `x_accel_redirect` 交给 nginx） | 同样不实现，同样交给前置服务器（Salvo 的 `NamedFile` 会顺带支持 Range） |
| 前端 i18n | 13 种语言的 README 与架构图 | 中 / 英双份 README 与图（`docs/i18n/`） |

## 项目宠物

以太兽住在每一次上传里 —— 箭头朝上，进度条还没满，飞来的分块还差一块。断在半路也没关系：`.part` 与 `chunkIndex` 还在，接上就是。

```text
            ▲
            │              以太兽 · Aether Beast
        .-~~~-.
      .'  ● ●  '.          头顶箭头 = 整份文件（向上）
     /    ‿‿‿    \         腹部进度条 = 已追加的分块
    (  ▓▓▓▓░░░░░  )        浅色段 = 还没到的那几块
     '.__.___.__.'    ▣    右侧方块 = 正飞来的下一个分块
```

形象文件在 `docs/pet.svg`（从 PHP 版项目原样搬来），ASCII 版与 SVG 都以常量形式打进库里（`aetherupload::pet`）—— 示例页的 favicon 与标题图标、四张架构图里的吉祥物、CLI 的 `aetherupload pet` 用的都是同一份形象。

## 开源不易，欢迎支持 / Open Source is Not Easy, Your Support is Welcome

<p>
  <img src="https://raw.githubusercontent.com/erikwang2013/aetherupload-rust/main/docs/weixinpay.png" alt="微信赞赏" width="200" />
  <img src="https://raw.githubusercontent.com/erikwang2013/aetherupload-rust/main/docs/alipay.png" alt="支付宝赞赏" width="200" />
</p>

## 版权

© 2026 erik · <https://erik.xyz>

## License

[MIT](./LICENSE)
