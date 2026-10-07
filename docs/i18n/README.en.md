# erikwang2013/aetherupload-rust

[![Test](https://github.com/erikwang2013/aetherupload-rust/actions/workflows/test.yml/badge.svg)](https://github.com/erikwang2013/aetherupload-rust/actions/workflows/test.yml)
[![Release](https://img.shields.io/github/v/release/erikwang2013/aetherupload-rust)](https://github.com/erikwang2013/aetherupload-rust/releases)
[![crates.io](https://img.shields.io/crates/v/aetherupload-rust)](https://crates.io/crates/aetherupload-rust)
[![docs.rs](https://docs.rs/aetherupload-rust/badge.svg)](https://docs.rs/aetherupload-rust)
![MSRV](https://img.shields.io/badge/MSRV-1.89-blue)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

<p align="center">
  <img src="https://raw.githubusercontent.com/erikwang2013/aetherupload-rust/main/docs/pet.svg" alt="Aether Beast — the AetherUpload project pet" width="160" />
</p>

<p align="center"><strong>以太兽 · Aether Beast</strong> — the project pet: the upward arrow on its head is the whole file, the progress bar on its belly is the chunks being appended, and the square flying in from the right is the next chunk</p>

**Languages:** [中文](../../README.md) · **English**

The file is sliced in the browser, appended chunk by chunk to a temporary file on the server, and named by the md5 of its contents when written to disk — so **upload, resume after disconnect, instant upload, deduplication, and integrity checking all share one mechanism**. The same code runs on native Rust (`Guard`) and on Axum, Actix Web, Rocket, Poem, Salvo, Warp, bee-rust, and e-cat, with **zero third-party dependencies** under the default feature set.

## Project overview

This project is ported from the PHP large-file upload package [AetherUpload-Webman](https://github.com/erikwang2013/aetherupload-webman) (itself descended from [AetherUpload-Laravel](https://github.com/peinhu/AetherUpload-Laravel)), with configuration reading, concurrency handling, and the storage layer rewritten for the Rust ecosystem. **The protocol matches the PHP version field for field**: the frontend script works as-is, and the field names (`resource_name` / `resource_chunk` / `chunk_index` …), error messages, `savedPath` addressing, and on-disk layout are unchanged.

**What it solves**

Uploading a large file straight from the browser runs into three problems: anything over the server's request-body limit cannot be sent at all; one network drop means starting over; and the same file gets transferred again and again. AetherUpload's approach is to slice the file in the browser, append it chunk by chunk to a temporary file on the server, and name it by the md5 of its contents when written to disk. So **upload, resume, instant upload, deduplication, and integrity checking** share one mechanism — the whole file is never read into memory, and no database table is needed to keep track of where files are.

**Its shape**

One Rust crate, with the **kernel decoupled from the host framework**: the same code works on native Rust (the `Guard` request guard) and on Axum, Actix Web, Rocket, Poem, Salvo, Warp, bee-rust, and e-cat. The eight adapters only do wiring — register four routes, translate the multipart form into kernel arguments, translate the kernel response into a framework response. Not one line of business logic is duplicated. No database dependency; Redis is needed only when instant upload is enabled, and it is injected through a trait, making it an optional dependency; S3-compatible storage is likewise an optional feature.

**Notes**

- Group names **must not contain underscores** — the name is part of the three-segment `savedPath` encoding, and an underscore shifts the decoding, permanently 404ing every resource in that group. Rejected at both the configuration and the upload stage.
- Group directories **must be created first** (`aetherupload groups` or the library API): the kernel creates subdirectories with a non-recursive `mkdir`, so a missing parent directory surfaces as a generic upload error and it is hard to tell from the message that the problem is a directory.
- The instant-upload index and the actual files are two separate stores. Delete resources through `Runtime::delete_resource()` (which clears the instant-upload record too), otherwise dead links are left behind; running `build` once a day to rebuild the index is recommended.
- `x_accel_redirect` is a fronting-server (nginx) concept and only takes effect for the local driver; under the s3 driver the read path is a 302 presigned URL.

## Features

**Protocol and capabilities**

- [x] Percentage progress bar
- [x] File type restrictions (allowlist + denylist + MIME re-checked against the actual content)
- [x] File size limits (declared size, increment, and final-chunk checks — three validations)
- [x] Multilingual support (Chinese / English)
- [x] Per-group resource configuration (directory, size limit, and extensions are each independent)
- [x] Upload-complete events (before / after)
- [x] Synchronous upload *①*
- [x] Resume after disconnect *②*
- [x] Instant upload *③*
- [x] Custom middleware *④*
- [x] Custom routes
- [x] Lax mode
- [x] Optional object storage (S3-compatible)
- [x] Operations commands (`groups` / `build` / `clean`)

*①: Compared with asynchronous upload, synchronous upload is slightly slower when upload bandwidth is plentiful — but synchronous upload can assemble the file while chunks are still arriving, whereas asynchronous upload cannot assemble until every chunk has arrived (chunk completion order is not guaranteed), which makes it stall for a long time near the end. Synchronous upload also sends only one chunk at a time, takes up less server memory per unit of time, and therefore supports more concurrent uploaders than the asynchronous approach.*

*②: Resume after disconnect is not the same as resumable transfer: when the network drops or Wi-Fi is unstable, the upload component retries on a timer as long as the page stays open, and once connectivity returns the file continues from the first chunk that never succeeded. Refreshing the page — or closing and reopening it — loses that state, and whatever was uploaded before becomes an invalid file.*

*③: Instant upload requires Redis on the server and browser support (FileReader, File.slice()); if either is missing, the feature cannot work. Off by default; enable it in the configuration.*

*④: The Rust version has no middleware registry — the host uses its own middleware mechanism (Axum's `layer`, Actix's `wrap`, …), and route paths come from the configured `route_*`.*

**Engineering properties**

- **Zero third-party dependencies under the default feature set**: MD5 (anchored to the RFC 1321 test vectors), JSON output, the 769-entry MIME table with magic-number detection, and directory addressing are all built in; the Redis client and the S3 transport are both injected through traits.
- **Synchronous kernel**: semantics match PHP, and `cargo test` needs no runtime. The final chunk of a large file requires computing the md5 of the whole file, so hosts that care about worker occupancy can wrap the call in `tokio::task::spawn_blocking`.
- **No global state**: the PHP version avoids cross-group bleed in a long-lived process by rebuilding a ConfigMapper snapshot per request; the Rust version simply passes the group snapshot down as an argument — the same isolation, without locks.
- **The whole file never sits in memory**: chunks are appended one at a time; the final chunk streams the full-file md5 through a 64KB buffer; local serving is a streaming response.

**Supported frameworks**

The kernel (chunking, resume, instant upload, validation, addressing) is decoupled from the host framework, and the same package works under the hosts below, **each backed by an end-to-end test that exercises the full upload path**:

| Host | crate | Integration | End-to-end test |
|---|---|---|---|
| **Native Rust (no framework)** | — | `Guard` request guard: four entry points + one route classification table | `tests/guard.rs` |
| Axum | axum 0.8 | `integrations::axum::routes(runtime)` returns a `Router` | `tests/integrations/axum.rs` |
| Actix Web | actix-web 4 | `integrations::actix::routes(runtime)` returns a `web::Scope` to mount into an `App` | `tests/integrations/actix.rs` |
| Rocket | rocket 0.5 | `integrations::rocket::mount(rocket, runtime)` returns a `Rocket` with the four routes mounted | `tests/integrations/rocket.rs` |
| Poem | poem 3 | `integrations::poem::routes(runtime)` returns a `Route` | `tests/integrations/poem.rs` |
| Salvo | salvo 1 | `integrations::salvo::routes(runtime)` returns a `Router` | `tests/integrations/salvo.rs` |
| Warp | warp 0.4 | `integrations::warp::routes(runtime)` returns a `Filter` | `tests/integrations/warp.rs` |
| bee-rust | bee_router 1 | `integrations::bee_rust::routes(runtime)` (the HTTP layer is axum) | `tests/integrations/bee_rust.rs` |
| e-cat | ecat 4 | `integrations::ecat::routes(runtime)` (the HTTP layer is axum) | `tests/integrations/ecat.rs` |

## Project layout

```text
aetherupload-rust/
├── src/
│   ├── lib.rs                    crate docs and re-exports
│   ├── runtime.rs                runtime binding: configuration + injection points for storage / instant upload / events / MIME detector
│   ├── config.rs                 configuration structs + group snapshot (the Rust form of PHP's ConfigMapper)
│   ├── controller/
│   │   ├── upload.rs             preprocess / saveChunk: protocol and security semantics ported item by item
│   │   └── resource.rs           display / download: three read paths + inline denylist
│   ├── guard.rs                  native Rust entry point: Guard request guard + route classification table
│   ├── form.rs                   form carrier (multipart parse output → kernel arguments)
│   ├── partial.rs                the chunk file itself: path, append, rename, size and type validation
│   ├── header.rs                 checkpoint state file: stores just one chunkIndex
│   ├── resource.rs               finished-resource object (the payload of the upload-complete event)
│   ├── saved_path.rs             savedPath's three-segment stateless addressing
│   ├── instant.rs                instant-upload index: InstantStore trait + key format + in-memory implementation
│   ├── storage/                  storage drivers: Storage trait + local (writing and reading finished resources) + s3 + sigv4
│   ├── mime.rs                   MIME table (769 entries, carried over from the PHP version) + magic-number content detector
│   ├── md5.rs                    hand-written MD5 (anchored to the RFC 1321 test vectors)
│   ├── json.rs                   JSON output for upload responses (field names identical to PHP)
│   ├── i18n.rs                   message catalog (aligned entry by entry with the PHP translation files)
│   ├── error.rs                  error types (only "known errors" are exposed externally)
│   ├── events.rs                 upload-complete events (EventSink trait)
│   ├── util.rs                   path safety, temp names, download-name sanitization, links
│   ├── console.rs                operations commands: groups / build / clean
│   ├── assets.rs                 frontend script and example page (embedded with include_str!)
│   ├── pet.rs                    the project pet "以太兽 · Aether Beast" in ASCII and SVG
│   ├── integrations/             eight framework adapters (feature-gated, wiring only)
│   └── bin/aetherupload/         operations CLI (contains a zero-dependency RESP client)
├── assets/                       frontend script: aetherupload-all.js (bundles zepto + spark-md5) + example page
├── examples/                     quick start (Axum + example page)
├── tests/                        kernel unit tests + protocol cases + randomized round-trip + concurrency + native Guard + eight-framework end-to-end + S3
└── docs/
    ├── pet.svg                   the project pet artwork
    ├── architecture.svg          architecture diagram
    ├── design.svg                design diagram
    ├── request-cycle.svg         request cycle diagram
    ├── upload-lifecycle.svg      upload lifecycle diagram
    └── i18n/                     English README and English diagrams
```

## Architecture

<img src="https://raw.githubusercontent.com/erikwang2013/aetherupload-rust/main/docs/i18n/img/architecture.en.svg" alt="AetherUpload-Rust architecture">

**Four layers with one-way dependencies**: each layer depends on the one below it, never the reverse:

| Layer | Responsibility | Location |
|----|------|------|
| Framework adapter layer | Wiring only: register routes, parse multipart, map responses | `src/integrations/` + `src/guard.rs` |
| Use-case layer | Two entry points: upload (preprocess + chunk) and read (display + download) | `src/controller/` |
| Domain layer | Chunk files, checkpoints, finished resources, addressing, MIME, config snapshots | `src/{partial,header,resource,saved_path,mime,config}.rs` |
| Port layer | The four traits — storage / instant upload / events / MIME detector — and their default implementations | `src/{storage,instant,events,mime}.rs` |

The kernel **knows nothing about any framework**: every file in the adapter layer does only three things — take the arguments, call the kernel, build the response.

## Design

<img src="https://raw.githubusercontent.com/erikwang2013/aetherupload-rust/main/docs/i18n/img/design.en.svg" alt="AetherUpload-Rust design">

- **Only three kinds of files ever exist on disk**: `*.part` (chunks), `_header/<temp name>` (checkpoints, storing just one chunkIndex), and `<md5>.<ext>` (finished resources). No database table, no metadata files.
- **Stateless addressing**: the client receives `savedPath` (`group_subdir_filename`) and sends it back verbatim; the server splits it into three segments and locates the file — nothing has to remember who uploaded what.
- **The instant-upload index is optional, with an independent TTL per record**: sites without Redis can still upload (instant upload just stops working); sites with Redis expire each record through its own `SETEX`, so they neither share an expiry nor grow without bound under sustained uploads.
- **Consistency is left to the operations commands**: `build` (rebuilds the instant-upload index from what is on disk) and `clean` (reaps temporary files by mtime) each run on a cron entry — far cheaper than adding locks to the request path.
- **The write path is synchronous and memory-bounded**: appends are sequential writes with `append` semantics, the final chunk is validated and then renamed into place; the whole file never enters memory.

## Request cycle

<img src="https://raw.githubusercontent.com/erikwang2013/aetherupload-rust/main/docs/i18n/img/request-cycle.en.svg" alt="AetherUpload-Rust request cycle">

Four routes, two response shapes:

| Route | Method | Entry point | Response |
|---|---|---|---|
| `route_preprocess` (default `/aetherupload/preprocess`) | POST | `UploadController::preprocess` | JSON: `chunkSize` / `resourceTempBaseName` / `groupSubDir` / `resourceExt` / `savedPath` |
| `route_uploading` (default `/aetherupload/uploading`) | POST | `UploadController::save_chunk` | JSON: `savedPath` (non-empty on the final chunk or an instant-upload hit) |
| `route_display` (default `/aetherupload/display/{saved_path}`) | GET | `ResourceController::display` | the file itself / 302 presigned / X-Accel-Redirect |
| `route_download` (default `/aetherupload/download/{saved_path}/{new_name}`) | GET | `ResourceController::download` | same, forced as an attachment |

Errors on the upload endpoints are always **HTTP 200 with an `error` field** (matching the PHP version; the frontend tests `error` for truthiness); read-path failures are 404 plain text (`display fail` / `download fail`).

## Upload lifecycle

<img src="https://raw.githubusercontent.com/erikwang2013/aetherupload-rust/main/docs/i18n/img/upload-lifecycle.en.svg" alt="AetherUpload-Rust upload lifecycle">

The main path is just four steps: **preprocess → chunk (loop) → final-chunk validation → write to disk**.

1. **Preprocess** (`preprocess`): validate the arguments and the group → generate the temporary name and subdirectory → instant-upload check (a hit returns `savedPath` directly, with not a single chunk sent) → create an empty `.part` and write `chunkIndex=0` into `_header`.
2. **Chunk** (`saveChunk`, in a loop): every chunk follows a fixed order — validate → append → write the chunkIndex back. A retransmission of the same index is skipped idempotently; a gap in the index or a truncated chunk only returns an error and **does not clean up** the `.part` or the header, so on a flaky network the client can keep retrying until the missing chunk is filled in.
3. **Final chunk**: validate the total size → re-check MIME against the actual content → fire the "before upload complete" event → recompute the full-file md5 by streaming → compare against the hash declared by the client (when `lax_mode` is off); a mismatch discards the whole file.
4. **Write to disk**: rename to `<md5>.<ext>` (if the same hash already exists, the temporary file is discarded as a duplicate) → write the instant-upload index → delete the checkpoint file → fire the "upload complete" event.

Only two situations actually discard assembled progress: a failed final-chunk validation (the whole file is dropped), and a closed page, where `clean` reaps it by mtime.

## Installation

```bash
cargo add aetherupload-rust                  # default features: a zero-dependency kernel
cargo add aetherupload-rust --features axum  # enable the feature for whichever host you need
```

Feature list: `axum` / `actix` / `rocket` / `poem` / `salvo` / `warp` / `bee-rust` / `ecat` (framework wiring) and `s3` (S3-compatible storage).

**Two steps in common** (whichever host you use):

1. **Create the storage directories**: `aetherupload groups` (or the library API `console::list_groups`) creates the root directory, `_header`, and every group directory. **Skip this and uploads will fail, guaranteed**.
2. **Mount the routes**: mount the four routes into the host application (see below).

## Configuration

Configuration is a Rust struct, with field names identical to the PHP version:

```rust
use std::collections::BTreeMap;
use aetherupload::config::{Config, GroupConfig, SubdirRule};

let mut config = Config::default();          // defaults match PHP's config/aetherupload.php

config.root_dir = "storage/app/aetherupload".into();  // upload root directory (relative to the project root)
config.chunk_size = 1_000_000;               // chunk size in bytes; 1MB–4MB recommended
config.resource_subdir_rule = SubdirRule::Month;      // year / month / date / const
config.instant_completion = false;           // instant-upload switch (requires Redis and browser support)
config.lax_mode = false;                     // lax mode: skip hash computation and integrity checking
config.x_accel_redirect = false;             // let nginx serve the file directly

// groups: group_dir is the on-disk directory name; resource_maxsize 0 means unlimited (a declared size of 0 is still rejected)
config.groups.insert("video".into(), GroupConfig {
    group_dir: "video".into(),
    resource_maxsize: 0,
    resource_extensions: vec!["mp4".into(), "mov".into()],
    event_before_upload_complete: false,
    event_upload_complete: false,
});
```

The four route paths (change them in the frontend with the matching `setXxxRoute()`): `route_preprocess`, `route_uploading`, `route_display`, `route_download`, defaulting to `/aetherupload/{preprocess,uploading,display,download}`.

## Native usage (Guard)

No framework required: a hand-written server (hyper / tiny-http / your own TCP service, or a framework not yet covered) can wire up with `Guard` — the eight framework adapters do these same four things internally, and the only difference is which framework's request object the arguments are pulled from.

```rust
use std::sync::Arc;

use aetherupload::{Config, FormData, Guard, GuardRoute, Runtime};

let runtime = Arc::new(Runtime::new(Config::default(), ".")?);

// construct once at wiring time, then clone per request (two atomic increments)
let guard = Guard::new(runtime);

// once a hand-written server has "method + path + form":
match guard.routes().classify(method, path) {
    Some(GuardRoute::Preprocess) => {
        let json = guard.preprocess(&form);        // JsonBody: 200 + JSON; the body goes straight back to the frontend
        respond(json.status, json.content_type, json.body)
    }
    Some(GuardRoute::Uploading) => {
        let json = guard.save_chunk(&form);        // the chunk body travels in the form field resource_chunk
        respond(json.status, json.content_type, json.body)
    }
    Some(GuardRoute::Display { saved_path }) => {
        serve(guard.display(&saved_path))          // ResourceResponse: status code + headers() + file path
    }
    Some(GuardRoute::Download { saved_path, new_name }) => {
        serve(guard.download(&saved_path, &new_name))
    }
    None => respond(404, "text/plain", "not found"),
}
```

Form fields are assembled with `FormData::from_pairs(...)` (text fields) plus `push_file("resource_chunk", ChunkBody::Bytes(..))` — duplicate names take the last one, matching how PHP parses forms.

The three shapes of `ResourceResponse` (`Redirect` / `AccelRedirect` / `ServeFile`) each carry their own `status()` and `headers()`; `ServeFile`'s attachment name lives in `download_name`, and the attachment header is produced by `aetherupload::controller::attachment_disposition(name)` (the eight adapters do the same) — for a complete example see `resource_response_to_http()` in `tests/guard.rs`.

## Axum

```rust
use std::sync::Arc;
use aetherupload::{Config, Runtime};

let runtime = Arc::new(Runtime::new(Config::default(), ".")?);

// all four routes (preprocess / chunk / display / download) mounted at once
let app = aetherupload::integrations::axum::routes(runtime.clone());

// to put authorization in front of the upload routes, just use your own layer
// let app = app.layer(middleware::from_fn(auth));
let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
axum::serve(listener, app).await?;
```

The other seven hosts work the same way; integration points are in the [Supported frameworks](#supported-frameworks) table above: Actix Web gets a `web::Scope` from `routes()`, Rocket uses `mount(rocket, runtime)`, Poem returns a `Route`, Salvo a `Router` (inject the Runtime through `hoop`), Warp a `Filter`, and bee-rust and e-cat reuse the axum `Router`. The repository's `examples/quickstart.rs` is a runnable Axum example (with the example page and the frontend script).

## Operations CLI

```bash
cargo run --bin aetherupload -- groups                  # create the root directory, _header, and every group directory
cargo run --bin aetherupload -- build --redis redis://127.0.0.1:6379/0   # rebuild the instant-upload index from disk
cargo run --bin aetherupload -- clean 2                 # clean up temporary files older than 2 days (_header and *.part)
cargo run --bin aetherupload -- pet                     # print the project pet
```

Hosts with custom configuration should call the library API directly (passing in their own `Config`):

```rust
use aetherupload::console;

let mut say = |line: &str| println!("{line}");
let code = console::list_groups(&runtime, &mut say);           // 0 = success
let code = console::build_redis_hashes(&runtime, &mut say);    // requires an InstantStore to be wired up
let code = console::clean_up_directory(&runtime, &mut say, 2); // older than 2 days
```

Recommended cron (same as the PHP version):

```cron
0 0 * * * /path/aetherupload clean 1
0 0 * * * /path/aetherupload build --redis redis://127.0.0.1:6379/0
```

## Instant upload (optional)

The instant-upload index is injected through a trait — this crate **does not depend on any Redis client**:

```rust
use std::sync::Arc;
use aetherupload::instant::InstantStore;

struct MyRedis(/* your client */);

impl InstantStore for MyRedis {
    fn get(&self, key: &str) -> aetherupload::Result<Option<String>> { /* GET */ Ok(None) }
    fn set_ex(&self, key: &str, value: &str, ttl_seconds: u64) -> aetherupload::Result<()> { /* SETEX */ Ok(()) }
    fn del(&self, key: &str) -> aetherupload::Result<()> { /* DEL, idempotent */ Ok(()) }
}

let runtime = runtime.with_instant(Arc::new(MyRedis(/* ... */)));
```

- The key format matches the PHP version: `aetherupload:resource:<group>_<hash>`, with an independent TTL per record (7 days by default; change it with `resource_redis_expire`).
- It also reads the legacy single hash (`aetherupload_resource`) as a fallback — sites migrating from the PHP version do not have to run `build` first.
- The legacy fallback fires only when the store advertises the capability (`InstantStore::supports_legacy_fallback`, `false` by default): without it a miss costs a single `GET` instead of a guaranteed-empty extra round trip; implementations that declare it (the RESP client in the bundled CLI) behave exactly as before.
- Enabling `instant_completion` without a Redis store **is an error** (rather than letting instant upload quietly do nothing), the same stance as the PHP version.
- Tests and examples can use the built-in `MemoryInstantStore`.

## Optional object storage (S3-compatible)

Finished resources are written to local disk by default; switch by setting `storage.driver` to `s3`:

```rust
use aetherupload::config::{PayloadSigning, S3Config, StorageConfig, StorageDriver};

config.storage = StorageConfig {
    driver: StorageDriver::S3,
    s3: S3Config {
        endpoint: "".into(),            // empty = AWS default (derived from the region); self-hosted setups put their own endpoint here
        region: "us-east-1".into(),
        bucket: "my-bucket".into(),
        access_key: "".into(),
        secret_key: "".into(),
        path_style: true,               // must be true for self-hosted stores such as MinIO / Ceph
        prefix: "uploads".into(),       // object key prefix (may be empty)
        multipart_threshold: 104_857_600,
        payload_signing: PayloadSigning::Hash,  // for services that only accept unsigned (e.g. Huawei Cloud OBS), use Unsigned
    },
};
```

- **Read path**: `display` / `download` return a 302 to a presigned URL, while keeping the usual `Content-Disposition` and inline-denylist semantics (svg is forced to an attachment).
- **Local semantics are preserved**: chunk staging and `groups` / `build` / `clean` still operate on local files; `x_accel_redirect` only takes effect for the local driver.
- **The HTTP transport is injected through a trait**: the same stance as instant upload — the crate bundles no HTTP client, so the host wraps `reqwest` / `ureq` / `curl` (with `HttpBody::{Empty, Bytes, File}`, where `File` streams large uploads):

```rust
use std::sync::Arc;
use aetherupload::{HttpRequest, HttpResponse, HttpTransport, S3Config, S3Storage};

struct MyTransport(/* your HTTP client */);

impl HttpTransport for MyTransport {
    fn send(&self, request: HttpRequest) -> aetherupload::Result<HttpResponse> {
        // network failures return Err (which triggers a retry); non-2xx responses come back as-is with their status code, not as Err
        todo!("plug in your client")
    }
}

let storage = S3Storage::new(S3Config { /* … */ , ..Default::default() }, ".")?
    .with_transport(Arc::new(MyTransport(/* … */)));

let runtime = runtime.with_storage(Arc::new(storage));
```

## Security

Before upload, extensions are filtered by an allowlist plus a denylist; after upload, the MIME is re-checked against the **actual content** (the magic-number detector) — the allowlist directly constrains the saved extension, the denylist blocks common executable extensions by default, and when the allowlist is empty a hard-deny list of executable extensions still applies. Path components submitted by the client (`group_subdir` / `resource_temp_basename` / `resource_ext`) all pass a safe-character-set check, and `savedPath` is validated segment by segment after decoding — directory traversal cannot get in. Extensions that can be rendered inline (svg / html / js / …) are forced to attachments on the way out, with `X-Content-Type-Options: nosniff`.

Despite all this hardening, malicious file uploads are impossible to fully defend against; set the upload directory permissions correctly, and make sure the relevant programs have no execute permission on resource files.

## Differences from the PHP version

| Item | PHP version (aetherupload-webman) | Rust version |
|---|---|---|
| Distribution | `composer require` automatically distributes config / routes / commands / frontend script | crate dependency + explicit route mounting (`integrations::*::routes`) + bundled frontend script in `assets/` |
| Configuration carrier | `config/aetherupload.php` array | `Config` struct (same key names, errors caught at compile time) |
| Middleware | middleware class names in the config (`middleware_*`) | the host's own middleware mechanism; route paths still come from `route_*` |
| Per-request state | `RequestContext` + a `ConfigMapper` singleton rebuilt every request | group snapshot passed explicitly as an argument; no global state |
| Checkpoint file concurrency | `file_get_contents` reads without a lock and can race `ftruncate` | `read` takes a **shared lock** (mutually exclusive with the writer’s exclusive lock): concurrent chunks never observe an empty checkpoint, and never destroy the whole upload because of it |
| Error messages | known exceptions all collapse into the translation of `upload_error` | a specific translation per error kind (clients only test `error` for truthiness, so the protocol stays compatible) |
| MIME detection | `mime_content_type()` from `ext-fileinfo` | built-in magic-number detector (replaceable through the `MimeDetector` trait); files such as `.docx` are judged as `application/zip`, which may differ from fileinfo. The 512-byte probe window treats a **truncated multi-byte sequence at the window edge as text** — a Chinese `.txt` is never misjudged as binary just because the window cut a character in half |
| Subdirectory rule | `date()` in the server's local timezone | **UTC** (no timezone library available with zero dependencies); differs only at timezone boundaries around the start and end of a month |
| Pseudo-random temp names | `random_bytes()` (CSPRNG) | `/dev/urandom` (the same CSPRNG); falls back to splitmix64 over time ^ pid when unreadable |
| HTTP Range | webman does not implement Range (it delegates to nginx through `x_accel_redirect`) | also not implemented, also delegated to the fronting server (Salvo's `NamedFile` happens to support Range) |
| Frontend i18n | READMEs and architecture diagrams in 13 languages | Chinese / English READMEs and diagrams (`docs/i18n/`) |

## Project pet

The Aether Beast lives in every upload — the arrow points up, the progress bar is not full yet, and the incoming chunk is still one short. Interrupted halfway? No problem: the `.part` and the `chunkIndex` are still there, so pick up where it left off.

```text
            ▲
            │              以太兽 · Aether Beast
        .-~~~-.
      .'  ● ●  '.          头顶箭头 = 整份文件（向上）
     /    ‿‿‿    \         腹部进度条 = 已追加的分块
    (  ▓▓▓▓░░░░░  )        浅色段 = 还没到的那几块
     '.__.___.__.'    ▣    右侧方块 = 正飞来的下一个分块
```

The artwork lives in `docs/pet.svg` (carried over from the PHP project unchanged), and both the ASCII version and the SVG are compiled into the library as constants (`aetherupload::pet`) — the example page's favicon and title icon, the mascot in the four diagrams, and the CLI's `aetherupload pet` all use the same artwork.

## Open Source is Not Easy, Your Support is Welcome

<p>
  <img src="https://raw.githubusercontent.com/erikwang2013/aetherupload-rust/main/docs/weixinpay.png" alt="WeChat donation" width="200" />
  <img src="https://raw.githubusercontent.com/erikwang2013/aetherupload-rust/main/docs/alipay.png" alt="Alipay donation" width="200" />
</p>

## Copyright

© 2026 erik · <https://erik.xyz>

## License

[MIT](../../LICENSE)
