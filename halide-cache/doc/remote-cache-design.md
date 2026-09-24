# Remote cache for halide-cache — design

Status: **implemented on branch `halide-server`** (2026-09-20). Decisions taken during review are in section 9.

## 1. Goal

Share Halide generator output (the `.o` and `.h` pair) between machines, à la
[ctcache](https://github.com/matus-chochlik/ctcache): a build on a CI agent or a
developer laptop that misses locally asks a central server, and a build that
misses everywhere uploads its result so the next machine hits.

Non-goals for the first version: multi-tenant auth, TLS termination inside the
server, cloud storage backends (S3 etc.), a fancy dashboard.

## 2. Components

```
                 ┌──────────────────────────┐        HTTP        ┌─────────────────────────┐
  cmake/ninja ──▶│ halide-cache (client)     │◀──────────────────▶│ halide-cache-server      │
                 │  L1: local lager (~/.cache)│                   │  lager on /var/lib/...   │
                 │  L2: --remote URL          │                   │  LRU bounded by --size   │
                 └──────────────────────────┘                    └─────────────────────────┘
```

**Recommendation: one client binary, a `--remote` flag, and a separate server
binary in a new workspace crate.** Reasons:

* The CMake integration already spells out the `halide-cache` command line;
  adding one flag is less invasive than swapping in a differently-named wrapper.
* Two-level caching (local first, remote second) needs the address computation,
  the local lager and the remote client in one process anyway. A separate
  `halide-cache-client` would end up re-implementing `halide-cache` and pass
  through to it.
* The server shares the `lager` crate for on-disk layout and eviction, so a
  server disk can even be seeded by `rsync`ing a local cache dir.

Workspace after the change:

```
halide-cache/          # client binary (existing), gains `remote` module
halide-cache-server/   # new binary crate
lager/                 # shared storage crate (small additions, see §5)
```

## 3. Client behaviour

### 3.1 Lookup / store flow

```
compute addresses
├─ local hit           → extract, done                       (unchanged)
├─ local miss
│  ├─ remote disabled  → build, store local                  (unchanged)
│  └─ remote enabled
│     ├─ remote hit    → write blob into local lager, extract, done
│     └─ remote miss   → build, store local, upload (unless --remote-read-only)
└─ any remote error    → log a warning, behave as "remote miss"; never fail the build
```

The remote is **fail-open**: connection refused, timeouts, 5xx, and auth
failures downgrade to local-only behaviour with a one-line warning on stderr.
A cache server outage must never break a build. Timeouts: 3 s connect, 60 s per
transfer (configurable later if needed).

The blob uploaded is exactly the zstd stream `lager` already writes, so the
server stores it byte-for-byte and the client never recompresses. Downloads
are written to the local lager's sharded path (temp file + rename) and then
extracted from there with the existing `retrieve()`.

### 3.2 Partial hits

Today `cache_hit()` returns an *error* when only one of object/header is found.
Locally that is rare because both are written together. With a remote whose
LRU evicts the two entries independently, and with uploads that can be
interrupted between the two PUTs, partial hits become normal. Two options:

* **A (recommended): one cache entry per generator invocation.** Store the
  object and header together as a single `.tar.zst` (lager already supports
  directory entries) under one address computed from both output paths. One
  GET, one PUT, atomic by construction. This changes the local on-disk format:
  the existing local caches simply miss once and repopulate. Bump to v0.3.0.
* B: keep two entries and treat a partial hit as a miss (drop the error,
  rebuild both). Simpler diff, twice the requests, still racy on upload.

### 3.3 Configuration

| Setting | Flag | Default |
|---|---|---|
| Server URL | `--remote http://host:port` | none (remote off) |
| Auth token for uploads | `--remote-token` | none (reads still work) |
| Read only | `--remote-read-only` | off |
| Path stripping | `--strip PREFIX` (repeatable) | `base_dir` is always stripped |
| Builder identity | `--builder-id <string>` | none |

Flags only, no environment variable fallbacks (decision Q3).

HTTP client crate: `ureq` (blocking, small, rustls, no tokio in the CLI). It
builds fine for the three release targets including Windows MSVC.

## 4. Address stability across machines (the CTCACHE_STRIP question)

The address is `blake3(path ‖ dep contents ‖ ZIVID_* env ‖ builder cmdline)`.
Each input was reviewed for machine-specific content:

| Input | Today | Cross-machine risk | Proposed fix |
|---|---|---|---|
| Output path | `strip_prefix(base_dir)`, absolute if that fails | Output in a build dir *outside* the repo (common with out-of-tree builds) hashes as absolute. Windows uses `\`. | Strip every `--strip` prefix (default `base_dir`, plus the build dir if it can be found); normalise separators to `/`; if still absolute, warn. |
| Dependency files | Contents hashed, paths ignored | None | — |
| `ZIVID_*` env | `KEY=VALUE` hashed verbatim | Any value that is a path (e.g. pointing into a checkout or home dir) is per-machine | Apply the same `--strip` prefixes to values. Need to know which `ZIVID_*` vars exist (Q5). |
| Builder cmdline | Each arg is `strip_prefix(base_dir)` only if the **whole** arg is a path | `-o/abs/path`, `--flag=/abs/path`, and anything under the conan cache (`~/.conan/...`) or the build dir survive as absolute | Substring replacement of each `--strip` prefix inside every arg (this is what `CTCACHE_STRIP` does), plus separator normalisation. |
| Builder binary | Only its **path** is hashed, not its contents | Two machines with the same relative path but different Halide/generator builds collide. Locally the conan cache path embeds a package revision, which is why it works today; after stripping the conan prefix it may not. | Hash the builder binary contents (blake3 of the file, ~ms for a few MB), or accept `--builder-id <string>` from CMake carrying the conan reference + revision. |
| Target architecture | Only if present in the cmdline | If the generator is invoked with `target=host`, the object depends on the host CPU, and a hit from another machine yields an object with the wrong ISA. Windows vs Linux objects (COFF vs ELF) also only differ if the target string differs. | Must be answered before shipping (Q1). Mitigation if `host` is used: mix `std::env::consts::{OS, ARCH}` plus the resolved Halide target into the hash. |
| Key format | none | Any change to the hashing above silently collides with old entries on a shared server | Prefix the hash input with a scheme version string (`halide-cache-key-v2`). |

So yes: a `--strip` mechanism is needed, and the existing `base_dir` stripping
is the right idea but not applied widely enough. The recommended order of
work is: (1) key version prefix, (2) generalised `--strip`, (3) builder
content hash, (4) target/ISA answer from Q1.

Note the outputs themselves do not need path stripping: Halide-generated `.h`
files carry no absolute paths, and any absolute paths in `.o` debug info are
harmless for correctness.

## 5. Protocol

Plain HTTP/1.1, REST, no framing. `{addr}` is the 128-hex-char address.

| Method | Path | Request | Response |
|---|---|---|---|
| `GET` | `/v1/blobs/{addr}` | — | `200` body = stored zstd blob, header `X-Lager-Kind: file\|dir`; `404` if absent |
| `HEAD` | `/v1/blobs/{addr}` | — | `200`/`404` (cheap existence check, not used by the client initially) |
| `PUT` | `/v1/blobs/{addr}` | body = zstd blob, `X-Lager-Kind`, `Authorization: Bearer …` if configured | `201` created, `200` already present (idempotent), `401`, `413` too large |
| `GET` | `/v1/stats` | — | JSON: entries, bytes, capacity, hits, misses, uploads, evictions, uptime |
| `GET` | `/healthz` | — | `200 ok` (for systemd/monitoring) |

A `GET` touches the blob's mtime (that is what `lager::retrieve` does today and
what the LRU keys on). `PUT` writes to `<path>.tmp-<random>` and renames, so
a concurrent reader never sees a truncated blob and a killed upload leaves at
most a temp file that a periodic sweep removes. The server never verifies the
blob against the address: the address hashes *inputs*, not the output, so this
is by design and identical to ctcache.

Blobs are already compressed; the server sets no `Content-Encoding` and does
no transformation. Max upload size defaults to 1 GiB (Q6 on real object sizes).

## 6. Server

New crate `halide-cache-server`, Rust, `axum` + `tokio`, storage via `lager`.

```
halide-cache-server
  --listen 0.0.0.0:8080
  --data-dir /var/lib/halide-cache
  --size 40GiB              # max on-disk size, default 40 GiB; accepts 40G, 40GiB, 40000000000
  --token <secret>          # optional; if set, PUT requires it. Also via HALIDE_CACHE_SERVER_TOKEN
  --require-token           # refuse to start without a token (used by the systemd unit)
  --evict-interval 300      # seconds between LRU passes
  --max-blob-size 512MiB
```

* **Eviction**: reuse `lager::LRU` (scan mtimes, pop oldest until under
  `--size`). Run it in a background task on `--evict-interval` and also
  opportunistically when accounted bytes cross `--size + 5 %` after a PUT.
  The scan is a `walkdir` over the shard tree; for a 40 GiB cache of a few
  hundred thousand entries it takes well under a second on local disk and
  needs no persistent index, which keeps the server crash-safe with no
  database. Evict to a low-water mark (e.g. 95 % of `--size`) so it does not
  run on every PUT once the cache is full.
* **Concurrency**: reads open the file and stream it; Linux keeps an unlinked
  file readable so eviction racing a download is safe. Writes are
  temp+rename. Eviction holds a tokio mutex so only one pass runs at a time.
* **Stats**: in-memory counters exposed at `/v1/stats`; reset on restart.
  Optional later: a tiny HTML page at `/` like ctcache's dashboard.
* **Logging**: `tracing` to stderr (journald picks it up), one line per
  request at `info`, configurable with `RUST_LOG`.
* **Packaging**: static `x86_64-unknown-linux-musl` (and `aarch64` musl)
  binary so it runs on any Linux without matching glibc. Ship
  `halide-cache-server/contrib/halide-cache-server.service`:

  ```ini
  [Unit]
  Description=Halide object cache server
  After=network-online.target

  [Service]
  ExecStart=/usr/local/bin/halide-cache-server --listen 0.0.0.0:8080 --data-dir /var/lib/halide-cache --size 40GiB
  DynamicUser=yes
  StateDirectory=halide-cache
  Restart=on-failure
  EnvironmentFile=-/etc/halide-cache-server.env   # HALIDE_CACHE_SERVER_TOKEN=...

  [Install]
  WantedBy=multi-user.target
  ```

* **TLS / exposure**: none in-process. It is meant for an office/VPN network;
  if it must cross the internet put nginx/caddy in front.
* **CI**: `release.yml` gains the server binary for the two Linux targets;
  `rust.yml` builds and clippies the whole workspace (it currently lists the
  crates one by one).

## 7. `lager` changes

Small, additive:

* `Lager::path_for(&Address) -> (PathBuf, Kind)` / `Lager::open_raw(&Address)`
  so client and server can move the compressed blob without decompressing.
* `Lager::store_raw(&Address, Kind, impl Read)` with temp+rename.
* `LRU::evict_until` gains a low-water target; `scan` skips `*.tmp-*` files.
* `Kind` enum (`File`/`Dir`) replacing the extension strings.

## 8. Milestones

1. **Key hardening** (client only, no server needed): key version prefix,
   generalised `--strip`, builder content hash, separator normalisation.
   Ship as v0.3.0 so local caches are already remote-compatible.
2. **Server** crate with `GET`/`PUT`/`stats`/`healthz`, `--size`, LRU task,
   systemd unit, musl release artefacts.
3. **Client `--remote`**: two-level lookup, fail-open, read-only flag, token.
4. **System test**: extend `tests/run` to start the server on a random port
   and verify hit/miss/upload/eviction end to end.

## 9. Decisions (2026-09-20)

| # | Question | Decision |
|---|---|---|
| Q1 | Halide target / `target=host` | Mix `std::env::consts::OS` and `ARCH` into the key. |
| Q2 | One entry vs two | Keep two entries: `.o` and `.h` are always produced together. A partial hit stays an error locally. A partial *remote* hit (possible after independent LRU eviction on the server) is treated as a remote miss, per the fail-open rule. |
| Q3 | Opt-in | Flags only, no env var fallbacks. `--remote`, `--remote-token`, `--remote-read-only`, `--strip`. |
| Q4 | Who writes | Writes require a bearer token (`--token` on the server, `--remote-token` on the client). CI holds the token as a secret; developers without a token read only. |
| Q5 | `ZIVID_*` values | No stripping needed; hashed verbatim as today. |
| Q6 | Blob sizes | Unknown, not important. `--max-blob-size` defaults to 1 GiB; `--size` default 40 GiB. Revisit if `/v1/stats` shows churn. |
| Q7 | Builder identity | CMake passes `--builder-id <conan ref + revision>`; hashed as a string. The builder path is still stripped and hashed as today. |
| Q8 | Packaging | Static `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` server binaries added to the GitHub release workflow, plus the systemd unit in the repo. |
| Q9 | Build dir | Always `repo/sdk/build`, under the repo root, so `base_dir` stripping applies. Remaining gap is arguments where the path is embedded (`--flag=/abs/path`), handled by substring stripping. The local cache dir stays `~/.cache/halide-cache` because the build dir is wiped between CI runs. |

Section 4 is to be read with these decisions applied: `ZIVID_*` values are not
stripped, and the builder is identified by `--builder-id` rather than by hashing
its contents.

## 10. Implementation notes

* Client: `halide-cache/src/remote.rs` (ureq, rustls) and the `--remote*`
  flags in `main.rs`. Local lookup first, then remote, then build and upload.
* Server: `halide-cache-server/` (axum): `blobs.rs` for the blob endpoints,
  `eviction.rs` for the LRU pass, `stats.rs` for the JSON, Prometheus and
  dashboard endpoints, `metrics.rs` for the in-memory counters, `extract.rs`
  for the shared request extractors. Systemd unit in
  `halide-cache-server/contrib/`, shipped inside the release archive.
* Shared: `lager::Kind`, `Lager::open_raw` / `store_raw` move the compressed
  blob as-is; `LRU::scan` ignores `*.tmp-*` files from in-progress writes.
* End-to-end test in `halide-cache/tests/run` starts a server and covers
  populate, cross-cache hit, read-only, rejected upload, and server down.

## 11. Dashboard

The server serves a self-contained HTML dashboard at `/` (no external assets,
so it works on an isolated network). Data comes from three JSON endpoints that
are also usable from scripts or a monitoring system:

| Endpoint | Content |
|---|---|
| `GET /v1/stats` | Totals since start: hits, misses, hit rate, uploads, duplicates, rejected uploads, evictions, bytes served/received, size, capacity, entries, uptime |
| `GET /v1/history?seconds=N` | One-minute buckets for the last N seconds (max 24 h): hits, misses, uploads, bytes, and the cache size measured by the last eviction pass in that minute |
| `GET /v1/clients` | Per source IP: hits, misses, uploads, rejected uploads, bytes, first/last seen. Behind a reverse proxy the first `X-Forwarded-For` entry is used |

The page shows stat tiles (hit rate, size against capacity with a meter that
turns amber above 95 %, uploads, evictions, traffic, clients, uptime), three
charts with a 1h/6h/24h switch, hover crosshair and a table view (requests per
minute, transfer per minute, cache size against capacity), and a client table.
It refreshes every 15 s.

Everything is in memory and resets when the server restarts; history is capped
at 24 h and client tracking at 4096 addresses.

Also shown:

* **Client hostnames.** The client sends its hostname in an
  `X-Halide-Cache-Client` header; the table shows hostname and address.
* **Unused time at eviction.** For every entry the LRU removes, the server
  records how long it had gone unused. A tile shows the median and minimum,
  and a histogram shows the distribution (under 1 h, 6 h, 1 d, 3 d, 7 d, over
  7 d). If the median is hours rather than days, `--size` is too small for the
  working set.
* **Prometheus** text format at `/metrics`: the same counters plus the
  eviction age histogram, for scraping into existing monitoring.

Decided against: persistence of history across restarts, and authentication
for the dashboard (put it behind a reverse proxy if the network is not trusted).
