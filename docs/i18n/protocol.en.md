# AetherUpload Protocol 2.0 Specification

This document is the HTTP protocol specification of AetherUpload 2.0: the four endpoints, the request fields, the upload result and problem responses, and all boundary semantics. Route mounting and configuration are in the [README](README.en.md); for the list of changes when migrating from 1.x, see the "Migrating from 1.x" section of the README.

Terminology: an **upload result** is the HTTP 200 success body; a **problem response** is RFC 9457 `application/problem+json`.

## 1. Endpoints and request fields

| # | Method + path | Request |
|---|---|---|
| 1 | `POST {route_preprocess}` | form (urlencoded or multipart — both accepted): `resource_name`, `resource_size`, `resource_hash?`, `group`, `locale?` |
| 2 | `POST {route_uploading}` | multipart: `resource_chunk` (file), `chunk_total`, `chunk_index`, `resource_temp_basename`, `resource_ext`, `group_subdir`, `group`, `resource_hash?`, `locale?` |
| 3 | `GET {route_display}/{saved_path}` | no body |
| 4 | `GET {route_download}/{saved_path}/{new_name}` | no body |

**Preprocess (endpoint 1) fields**

- `resource_name`: the original file name. Only the extension is derived from it; it is never written to disk as a path component.
- `resource_size`: the size of the whole file in bytes, decimal digits only; missing, non-numeric, 0, or above the group cap → 400 `invalid_parameter`.
- `resource_hash?`: md5 of the whole file (32 hex digits). Once provided it takes part in the instant-upload check and the final-chunk integrity comparison; an empty string means "not provided".
- `group`: the group name. Unconfigured, or containing an underscore → 400 `invalid_group`.
- `locale?`: the language of the problem response `title`; defaults to `en`.

**Chunk (endpoint 2) fields**

- `resource_chunk`: the chunk body (multipart file field).
- `chunk_total`: total number of chunks; it has an upper bound (10000) and must be ≥ 1.
- `chunk_index`: this chunk's index, starting at 1 (chunk k = k) and ≤ `chunk_total`; the only accepted advance is `last + 1` (see §6.1).
- `resource_temp_basename`: the `resource_temp_base_name` returned by preprocess, sent back verbatim.
- `resource_ext`: the `resource_ext` returned by preprocess, sent back verbatim.
- `group_subdir`: the `group_sub_dir` returned by preprocess, sent back verbatim.
- `group` / `resource_hash?` / `locale?`: same as endpoint 1.

`resource_temp_basename`, `resource_ext`, and `group_subdir` are all client-controlled path components and pass the safe-character-set check; an underscore in `group_subdir` breaks the three-segment `saved_path` decoding → 400 `invalid_parameter`.

**display / download (endpoints 3, 4)**: `{saved_path}` is the resource identifier (`group_subdir_filename`); the extra `{new_name}` of download only affects the file name on the way out — the extension still comes from the original resource. Their failures are problem responses too (see §6.6, §6.7), and the success paths (file body / 302 / `X-Accel-Redirect`) are unchanged.

## 2. Upload result (HTTP 200)

Success is always HTTP 200 + `application/json`, snake_case fields, no `error` field.

```text
preprocess → {"chunk_size":1000000,"resource_temp_base_name":"34c0fbcc4b7b1258",
              "group_sub_dir":"202610","resource_ext":"txt","saved_path":""}
save_chunk → {"saved_path":""}                          // middle chunk: not finished yet
             {"saved_path":"file_202610_<md5>.txt"}     // final chunk landed / instant-upload hit
```

Preprocess returns five fields:

- `chunk_size`: the server's chunk size in bytes — the client slices by it;
- `resource_temp_base_name`: this upload's temporary resource name, sent back verbatim with every chunk;
- `group_sub_dir`: the subdirectory it lands in;
- `resource_ext`: the derived extension;
- `saved_path`: non-empty = instant-upload hit (the whole file is already complete, not a single chunk needs sending); empty = keep sending chunks.

Each chunk returns one field:

- `saved_path`: empty = the middle chunk was accepted, send the next one; non-empty = the whole file is complete (final chunk landed or instant-upload hit), and this value is the resource identifier.

## 3. Problem response

Every failure is RFC 9457 `application/problem+json`; the HTTP status code is the real outcome and is never collapsed into 200.

```json
{"type":"about:blank","title":"Error: chunk index out of order","status":409,"code":"chunk_out_of_order","expected_index":5}
```

- `type`: always `about:blank`;
- `title`: a human-readable message localized from the request's `locale` (default `en`, e.g. `错误：分块序号不连续`);
- `status`: identical to the HTTP status code;
- `code`: stable, snake_case, machine-checkable — branch on `code`, never on `title`; a new failure scenario gets a new code;
- extension members: currently only `expected_index` (for `chunk_out_of_order` only, see §6.1).

## 4. Status codes and codes

| Scenario | HTTP | code |
|---|---|---|
| Missing / invalid parameter (name, size, group, chunk_total, chunk_index, temp_basename, ext, subdir…) | 400 | `invalid_parameter` |
| Unparsable body / missing `Content-Type` (**decided by the adapter**) | 400 | `invalid_request_body` |
| Path exists, method does not (**adapter fallback**, where warp has no router) | 405 | `method_not_allowed` |
| Group not configured, or the name contains an underscore (**upload endpoints**) | 400 | `invalid_group` |
| Not preprocessed / temporary resource gone (neither `.part` nor checkpoint remains) | 409 | `upload_not_prepared` |
| Chunk index out of order (neither `last+1` nor a valid final chunk) | 409 | `chunk_out_of_order` + `expected_index` extension member |
| Cumulative write exceeds the group cap `resource_maxsize` (0 = unlimited), or one chunk exceeds `chunk_size` | 413 | `payload_too_large` |
| Final-chunk whole-file md5 does not match the declared hash (`lax_mode=false`) | 422 | `hash_mismatch` |
| Extension / MIME not in the allowed set | 415 | `unsupported_media_type` |
| Disk / IO / index and other internal failures | 500 | `internal_error` |
| display / download cannot find the file, or `saved_path` names an unconfigured group | 404 | `resource_not_found` |
| display / download got an invalid `saved_path` / `new_name` | 400 | `invalid_parameter` |
| Instant-upload index unavailable / miss | —— not a failure: continue as a miss, never an error | —— |

The decision point of every code is in the matching subsection of §6.

## 5. Validation order

Parameters (format level: field presence / type) → group → parameters (group-dependent: extension safety, size cap, hash format) → temporary resource → index → size → final chunk (MIME → md5).

A chunk request is validated step by step, and whichever step fails first returns its problem response; the trailing MIME and md5 checks only run on the final chunk.

## 6. Boundary semantics

### 6.1 Chunk index and `expected_index`

- Indices start at 1. `chunk_index` and `chunk_total` accept decimal digits only: `0`, `-1`, `abc`, `chunk_index > chunk_total`, or `chunk_total` above the bound (10000) → 400 `invalid_parameter`.
- Preprocess sets the stored received-index to 0, so the only accepted advance is `index == last + 1` (the first chunk must be 1); **everything else is 409** `chunk_out_of_order` with the extension member `expected_index = last + 1`: gaps (`index > last + 1`), an early final chunk (`index == chunk_total` while `last + 1 < chunk_total`), and re-sends of already-landed chunks (`index <= last`) all fall here.
- `expected_index` spares the client from guessing server state; handle it in three branches: `> this index` → the chunk is actually already landed, advance silently; `== this index` → this is exactly the chunk the server wants, resend it; `< this index` → the server is waiting for an earlier chunk, the checkpoint state is gone, resuming is impossible → fail out from the last resumable point.
- Serial premise: the protocol assumes the client waits for the previous chunk's confirmation before sending the next one; only then does `expected_index < this index` mean "the server state is lost".

### 6.2 Recoverable failures do not clean up

These failures keep the `.part` and the checkpoint, so the client can simply retry:

- `upload_not_prepared` (409): `.part` does not exist, or the `_header` checkpoint is missing / unparsable — this path **never cleans up** (recoverable state must not destroy data); only a real IO fault (the disk cannot be read) becomes 500 `internal_error`.
- `chunk_out_of_order` (409): likewise no cleanup — on a flaky network the same chunk can be re-sent until the missing one is filled in.
- A missing chunk body field → 400 `invalid_parameter`, also with no cleanup (just resend the chunk).

Every other failure (parameter format, group, extension, hash format, size, MIME, md5) keeps the existing semantics: the whole file is discarded, `.part` and the checkpoint cleaned together.

### 6.3 Size limits

- **One chunk longer than `chunk_size` → 413** `payload_too_large`: rejected before anything is written.
- **Cumulative write above the size cap → 413**: the check is incremental (bytes on disk + this chunk), measured against the group cap `resource_maxsize` (0 means unlimited); before the final chunk lands, the whole real size is checked against the cap once more.
- The declared size's relation to the cap is checked at preprocess time: a `resource_size` of 0, a non-strict number, or above the group cap → 400 `invalid_parameter`.

### 6.4 Hash and integrity checking

- A non-empty `resource_hash` must be 32 hex digits, otherwise 400 `invalid_parameter` (checked both at preprocess and per chunk; format is not checked under `lax_mode`).
- When a hash is provided and the deployment has `instant_completion` on: preprocess consults the instant-upload index, and a hit returns `saved_path` directly with not a single chunk sent.
- With `lax_mode = false` (default) the final chunk always performs the whole-file md5 comparison: the client must provide `resource_hash`; missing, or not matching the real content → 422 `hash_mismatch`, the whole file discarded (`.part` and checkpoint cleaned together).
- With `lax_mode = true`: the hash format check and the md5 comparison are skipped; oddities such as a final chunk whose real size is 0 are accepted too (the price of lax mode).
- A final chunk with real size 0 is not reported separately: it is left to the md5 comparison (rejected with 422 in non-lax mode).

### 6.5 Parameter validation rules

- The safety check only covers the **derived extension**: `resource_name` as a whole is not checked as a path component (it never lands on disk as one), so ordinary names with Chinese characters or spaces (e.g. `my report.pdf`) keep passing; the extension itself passes the safe-character-set check plus the allow / deny lists.
- `resource_size` is parsed strictly: decimal digits only — values like `5.5` or `1e3` are no longer silently truncated but rejected with 400.
- Invalid `new_name` (400 `invalid_parameter`): empty string (the URL has no such segment), containing `/` or `\`, containing control characters; anything else follows the existing sanitize semantics.
- Client-controlled path components (`group_subdir`, `resource_temp_basename`, `resource_ext`) all pass the safe-character-set check — directory traversal cannot get in.

### 6.6 Group detection: upload endpoints vs read path

- Upload endpoints (preprocess / chunk): a group that is unconfigured, or whose name contains an underscore → 400 `invalid_group`.
- Read path (display / download): a `saved_path` pointing at an unconfigured / unresolvable group → 404 `resource_not_found`.

The same fact gets a different answer per endpoint: while uploading, "wrong group name" must be told to the uploader plainly (400); on the read path it amounts to "this identifier points at nothing" (404), and does not disclose which groups this deployment has configured (see §9).

### 6.7 The three failure states of file endpoints

display / download failures are graded by cause, no longer collapsed into a single 404 text:

- 400 `invalid_parameter`: `saved_path` failed to decode; download's `new_name` is invalid.
- 404 `resource_not_found`: the group is unconfigured / unresolvable; the file does not exist.
- 500 `internal_error`: storage / IO failure (not being able to read the disk is not "the file does not exist").

### 6.8 Framework-level rejections

The adapter's own rejections and the framework's rejections use the same problem-response shape:

- Unparsable body / missing `Content-Type` → 400 `invalid_request_body` (decided by the adapter; the kernel's parameter errors stay `invalid_parameter`);
- The framework's request-body size limit → 413 `payload_too_large` (same code as the kernel's 413, never downgraded to 400);
- Path matched, method wrong → 405 `method_not_allowed`.

## 7. Client contract (bundled script)

`assets/aetherupload-all.js` and the example page implement the following contract; third-party clients are advised to align with it:

- **multipart**: both preprocess and chunk use `FormData` (`processData` / `contentType: false`);
- **success**: read the snake_case fields (`saved_path` / `chunk_size` / `resource_temp_base_name` / `group_sub_dir` / `resource_ext`);
- **failure**: parse the problem response and show `title` to the user;
- **4xx is a hard failure**: show `title` and stop, no retries (except the three branches of `chunk_out_of_order`);
- **5xx and network errors are retried automatically**: 5 seconds between attempts; after 5 consecutive failures on the same chunk it gives up and shows `title`; the counter resets once a chunk is accepted or a 409 is silently advanced, and the "resend this chunk" branch counts toward the consecutive failures;
- **the three branches of 409**: per `expected_index`, as in §6.1;
- **serial**: wait for the previous chunk's confirmation before sending the next one;
- the example page's `#aetherupload-wrapper` container contract is unchanged.

## 8. Idempotency and retries

- The server only accepts `last + 1`: after a disconnect, retrying "the chunk the server has not received yet" is idempotent and never writes twice; re-sending an already-landed chunk returns 409, from which the client concludes "it is actually already there" via `expected_index` and advances.
- Once the whole file is complete, `.part` and the checkpoint are deleted: with instant upload on and a hash provided, a re-send hits the instant-upload index and gets 200 + `saved_path`; otherwise it gets 409 `upload_not_prepared`.
- The disk layout and the `saved_path` shape are the same as in 1.x (2.0 involves no data migration): `<root>/storage/app/aetherupload/{file/<subdir>/<md5>.<ext>, _header/<temp name>, *.part}`; `saved_path` is the three-segment `group_subdir_filename`.

## 9. Why it works this way

- **An unknown group on the read path is 404, not 400**: `saved_path` is a self-contained three-segment identifier; when it is well-formed but points at nothing, "not found" is the accurate conclusion — answering `invalid_group` would confirm or deny the existence of a group to any requester.
- **`upload_not_prepared` never cleans up**: a missing or corrupt checkpoint is a **recoverable** state (e.g. another concurrent request is writing). Destroying the `.part` here would also throw away data that can still be resumed; only a real IO fault escalates to 500.
- **Body-level evidence is examined before method detection**: the four routes share one chain, so the GET arm always leaves a "method not allowed" trace on a POST request (and vice versa); looking at the method first would misread "unrecognized `Content-Type`" or "body too large" as a 405 and bury the real cause.
- **`last + 1` is the only accepted advance**: it gives resends, gaps, and duplicates a single decidable answer on the server side, and hands that answer straight back to the client via `expected_index`, so the client never has to infer server state.
