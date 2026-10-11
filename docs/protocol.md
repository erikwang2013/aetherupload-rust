# AetherUpload 协议 2.0 规范

本文是 AetherUpload 2.0 的 HTTP 协议规范：四条端点、请求字段、上传结果与问题响应，以及全部边界语义。路由挂载与配置项见 [README](../README.md)，从 1.x 迁移的变更清单见 README 的「从 1.x 迁移」小节。

术语：**上传结果**指 HTTP 200 的成功体；**问题响应**指 RFC 9457 `application/problem+json`。

## 1. 端点与请求字段

| # | 方法 + 路径 | 请求 |
|---|---|---|
| 1 | `POST {route_preprocess}` | 表单（urlencoded 或 multipart 都收）：`resource_name`、`resource_size`、`resource_hash?`、`group`、`locale?` |
| 2 | `POST {route_uploading}` | multipart：`resource_chunk`（文件）、`chunk_total`、`chunk_index`、`resource_temp_basename`、`resource_ext`、`group_subdir`、`group`、`resource_hash?`、`locale?` |
| 3 | `GET {route_display}/{saved_path}` | 无体 |
| 4 | `GET {route_download}/{saved_path}/{new_name}` | 无体 |

**预处理（端点 1）字段**

- `resource_name`：原始文件名。只从它派生扩展名，永远不会作为路径分量落盘。
- `resource_size`：整份文件的字节数，只认十进制正整数；非数字、为 0 或超过分组上限 → 400 `invalid_parameter`。
- `resource_hash?`：整份文件的 md5（32 位十六进制）。提供即参与秒传判定与末块完整性比对；空串＝不提供。
- `group`：分组名。未配置或含下划线 → 400 `invalid_group`。
- `locale?`：问题响应 `title` 用什么语言；缺省 `en`。

**分块（端点 2）字段**

- `resource_chunk`：分块本体（multipart 文件字段）。
- `chunk_total`：分块总数；有上限（10000），且必须 ≥ 1。
- `chunk_index`：本块序号，从 1 开始（第 k 块 = k），且 ≤ `chunk_total`；唯一被接受的推进是 `last + 1`（见 §6.1）。
- `resource_temp_basename`：预处理返回的 `resource_temp_base_name`，原样回传。
- `resource_ext`：预处理返回的 `resource_ext`，原样回传。
- `group_subdir`：预处理返回的 `group_sub_dir`，原样回传。
- `group` / `resource_hash?` / `locale?`：同端点 1。

`resource_temp_basename`、`resource_ext`、`group_subdir` 都是客户端可控的路径分量，一律过安全字符集校验；`group_subdir` 含下划线会破坏 `saved_path` 三段式解码 → 400 `invalid_parameter`。

**display / download（端点 3、4）**：`{saved_path}` 是资源标识（`分组_子目录_文件名`）；download 额外的 `{new_name}` 只影响下发时的文件名，扩展名仍取原资源。两者的失败也走问题响应（见 §6.6、§6.7），成功路径（文件本体 / 302 / `X-Accel-Redirect`）不变。

## 2. 上传结果（HTTP 200）

成功一律 HTTP 200 + `application/json`，字段 snake_case，没有 `error` 字段。

```text
preprocess → {"chunk_size":1000000,"resource_temp_base_name":"34c0fbcc4b7b1258",
              "group_sub_dir":"202610","resource_ext":"txt","saved_path":""}
save_chunk → {"saved_path":""}                          // 中间块：还没完成
             {"saved_path":"file_202610_<md5>.txt"}     // 末块落盘 / 秒传命中
```

预处理返回五个字段：

- `chunk_size`：服务端分块大小（字节），客户端按它切片；
- `resource_temp_base_name`：本次上传的临时资源名，分块时原样回传；
- `group_sub_dir`：落盘子目录；
- `resource_ext`：派生出的扩展名；
- `saved_path`：非空＝秒传命中（整份已经完成，一个分块都不用传）；空串＝继续传分块。

分块返回一个字段：

- `saved_path`：空串＝中间块已接受，继续传下一块；非空＝整份已完成（末块落盘或秒传命中），该值就是资源标识。

## 3. 问题响应

失败一律 RFC 9457 `application/problem+json`，HTTP 状态码就是真实结果，不兜底成 200。

```json
{"type":"about:blank","title":"错误：分块序号不连续","status":409,"code":"chunk_out_of_order","expected_index":5}
```

- `type`：固定 `about:blank`；
- `title`：人类可读消息，按请求里的 `locale` 本地化（缺省 `en`，如 `Error: chunk index out of order`）；
- `status`：与 HTTP 状态码一致；
- `code`：稳定、snake_case、可被程序判断 —— 客户端分支请按 `code`，不要按 `title`；新增失败场景就新增 code；
- 扩成员：目前只有 `expected_index`（仅 `chunk_out_of_order`，见 §6.1）。

## 4. 状态码与 code

| 场景 | HTTP | code |
|---|---|---|
| 参数缺失 / 非法（name、size、group、chunk_total、chunk_index、temp_basename、ext、subdir…） | 400 | `invalid_parameter` |
| 请求体无法解析 / 缺 `Content-Type`（**适配层判定**） | 400 | `invalid_request_body` |
| 路径在、方法不对（**适配层兜底**，warp 无路由表时） | 405 | `method_not_allowed` |
| 分组未配置，或名字含下划线（**上传端点**） | 400 | `invalid_group` |
| 未预处理 / 临时资源已不存在（`.part` 与断点都没了） | 409 | `upload_not_prepared` |
| 分块序号不连续（既不是 `last+1`，也不是合法末块） | 409 | `chunk_out_of_order`＋扩成员 `expected_index` |
| 累计写入超过声明的 `resource_size`，或单块超过 `chunk_size` | 413 | `payload_too_large` |
| 末块整份 md5 与声明不符（`lax_mode=false`） | 422 | `hash_mismatch` |
| 扩展名 / MIME 不在允许集 | 415 | `unsupported_media_type` |
| 磁盘 / IO / 索引等内部失败 | 500 | `internal_error` |
| display / download 找不到文件，或 `saved_path` 指向未配置的分组 | 404 | `resource_not_found` |
| display / download 的 `saved_path` / `new_name` 非法 | 400 | `invalid_parameter` |
| 秒传索引不可用 / 未命中 | —— 不算失败：按未命中继续，绝不报错 | —— |

每个 code 的判定点见 §6 的对应小节。

## 5. 检查顺序

参数（格式级：字段存在性 / 类型）→ 分组 → 参数（与分组相关：扩展名安全、大小上限、hash 格式）→ 临时资源 → 序号 → 大小 → 末块（MIME → md5）。

分块请求逐级校验，先命中哪一条就返回哪一条的问题响应；末尾的 MIME 与 md5 校验只在最后一块执行。

## 6. 边界语义

### 6.1 分块序号与 `expected_index`

- 序号从 1 起。`chunk_index` 与 `chunk_total` 都只认十进制正整数：`0`、`-1`、`abc`、`chunk_index > chunk_total`、`chunk_total` 超过上限（10000）→ 400 `invalid_parameter`。
- 预处理会把断点里的已收序号置 0，因此唯一被接受的推进是 `index == last + 1`（第一块必须是 1）；**其余一律 409** `chunk_out_of_order` 并附扩成员 `expected_index = last + 1`：跳号（`index > last + 1`）、末块提前（`index == chunk_total` 但 `last + 1 < chunk_total`）、重发已落块（`index <= last`）都在此列。
- `expected_index` 让客户端不必猜服务端状态，按三分支处理：`> 本块序号` → 本块其实已落盘，静默推进；`== 本块序号` → 服务端要的正是本块，重发；`< 本块序号` → 服务端在等更早的块，断点状态已丢，无法续传 → 从最近可续处失败退出。
- 串行前提：协议假定客户端等上一块确认后再发下一块；只有串行上传时，`expected_index < 本块` 才等价于「服务端状态丢了」。

### 6.2 可恢复失败不清理数据

以下失败保留 `.part` 与断点，客户端直接重试即可：

- `upload_not_prepared`（409）：`.part` 不存在，或 `_header` 断点缺失 / 不可解析 —— 此路**绝不清理**（可恢复状态不许销毁数据）；只有真正的 IO 故障（读不了盘）才是 500 `internal_error`。
- `chunk_out_of_order`（409）：同样不清理，弱网下可以反复重发同一块，直到补上缺的那一块。
- 分块本体字段缺失 → 400 `invalid_parameter`，也不清理（重发该块即可）。

其余失败（参数格式、分组、扩展名、hash 格式、大小、MIME、md5）沿用既有语义：整份作废，`.part` 与断点一起清。

### 6.3 大小上限

- **单块长度 > `chunk_size` → 413** `payload_too_large`：写入前就拦下，不落盘。
- **累计写入超过大小上限 → 413**：校验是增量的（已落盘字节 + 本块字节），基准是分组上限 `resource_maxsize`（0 表示不限）；末块落盘前还会对整份真实大小再查一次。
- 声明大小与上限的关系在预处理阶段就查：`resource_size` 为 0、非严格数字或超分组上限 → 400 `invalid_parameter`。

### 6.4 hash 与完整性校验

- 非空 `resource_hash` 必须是 32 位十六进制，否则 400 `invalid_parameter`（预处理与分块两处都查；`lax_mode` 下不校验格式）。
- 提供了 hash 且部署开了 `instant_completion`：预处理即查秒传索引，命中直接返回 `saved_path`，一个分块都不用传。
- `lax_mode = false`（默认）时末块必做整份 md5 比对：客户端必须提供 `resource_hash`；未提供或与真实内容不符 → 422 `hash_mismatch`，整份丢弃（`.part` 与断点一并清理）。
- `lax_mode = true`：跳过 hash 格式检查与 md5 比对；末块真实大小为 0 这类异常也会被接受（宽松模式的代价）。
- 末块真实大小为 0 不单独报错：交给 md5 比对（非宽松模式以 422 拒掉）。

### 6.5 参数校验口径

- 安全检查只针对**派生出的扩展名**：`resource_name` 整体不做路径分量检查（它从不作为路径分量落盘），含中文、空格的文件名（如「我的 报告.pdf」）照常通过；扩展名本身过安全字符集与白 / 黑名单。
- `resource_size` 解析从严：只认十进制正整数，「5.5」「1e3」这类不再被静默截断，直接 400。
- `new_name` 非法（400 `invalid_parameter`）：空串（URL 里没有该段）、含 `/` 或 `\`、含控制字符；其余按现有 sanitize 语义。
- 客户端可控路径分量（`group_subdir`、`resource_temp_basename`、`resource_ext`）一律过安全字符集，目录穿越进不来。

### 6.6 分组判定：上传端点与读路径

- 上传端点（预处理 / 分块）：分组未配置或名字含下划线 → 400 `invalid_group`。
- 读路径（display / download）：`saved_path` 指向未配置 / 不可解析的分组 → 404 `resource_not_found`。

同一件事按端点给不同答案：上传时「分组名不对」要明确告诉上传者（400）；读路径上它等同于「这个标识指不到资源」（404），且不外泄本部署配了哪些分组（见 §9）。

### 6.7 文件端点的失败三态

display / download 的失败按原因分档，不再折成一句 404 文本：

- 400 `invalid_parameter`：`saved_path` 解码失败；download 的 `new_name` 非法。
- 404 `resource_not_found`：分组未配置 / 不可解析；文件不存在。
- 500 `internal_error`：存储 / IO 故障（读不了盘不等于「文件不存在」）。

### 6.8 框架层拒绝

适配层与框架自身的拒绝也走同一个问题响应形态：

- 请求体无法解析 / 缺 `Content-Type` → 400 `invalid_request_body`（适配层判定；内核的参数错误仍是 `invalid_parameter`）；
- 框架的请求体大小上限 → 413 `payload_too_large`（与内核 413 同 code，不降级成 400）；
- 路径匹配、方法不对 → 405 `method_not_allowed`。

## 7. 客户端契约（自带脚本）

`assets/aetherupload-all.js` 与示例页按以下契约实现，第三方客户端建议照此对齐：

- **multipart**：预处理与分块都用 `FormData` 发送（`processData` / `contentType: false`）；
- **成功**：读 snake_case 字段（`saved_path` / `chunk_size` / `resource_temp_base_name` / `group_sub_dir` / `resource_ext`）；
- **失败**：解析问题响应，把 `title` 显示给用户；
- **4xx 硬失败**：显示 `title` 后停止，不重试（`chunk_out_of_order` 的三分支除外）；
- **5xx 与网络错误自动重试**：每块间隔 5 秒；同一分块连续失败 5 次后放弃并显示 `title`；成功接受或 409 静默推进时清零计数，「重发本块」分支也计入连续失败；
- **409 三分支**：按 §6.1 的 `expected_index` 处理；
- **串行**：等上一块确认再发下一块；
- 示例页的 `#aetherupload-wrapper` 容器契约不变。

## 8. 幂等与重试

- 服务端只接受 `last + 1` 这一块：断线后重试「服务端尚未收到的那一块」是幂等的，不会重复写入；已落块的重发得到 409，客户端据 `expected_index` 判定「其实已落」并推进。
- 整份完成后 `.part` 与断点已删除：开了秒传且提供了 hash 时，重发会命中秒传索引直接返回 200 + `saved_path`；否则得到 409 `upload_not_prepared`。
- 磁盘布局与 `saved_path` 形态与 1.x 相同（2.0 不涉及数据迁移）：`<root>/storage/app/aetherupload/{file/<子目录>/<md5>.<ext>, _header/<临时名>, *.part}`；`saved_path` 是 `分组_子目录_文件名` 三段式。

## 9. 为什么这样设计

- **读路径未知分组报 404 而不是 400**：`saved_path` 是自包含的三段式标识，格式合法却指不到资源时，「找不到」才是准确结论；报 `invalid_group` 等于向任何请求者确认或否认某个分组是否存在。
- **`upload_not_prepared` 绝不清理数据**：断点缺失或损坏是**可恢复**状态（例如另一并发请求正在写）。此时销毁 `.part` 会把还能接上的数据一起清掉；只有真 IO 故障才升为 500。
- **体级拒绝的判据先于方法判定**：四条路由挂在同一条链上，GET 分支对 POST 请求（以及反之）总会留下「方法不对」的痕迹；若先看方法，「`Content-Type` 不认识」「请求体太大」都会被误判成 405，盖掉真实原因。
- **唯一接受的推进只有 `last + 1`**：让重传、跳号、重复在服务端有唯一且可判定的答案，并通过 `expected_index` 把答案直接回给客户端，客户端不需要自行推断服务端状态。
