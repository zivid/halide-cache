# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0] - 2026-10-06

First stable release. Cache entries can now be shared between machines
through a new `halide-cache-server`.

### Added

- `halide-cache-server`: an HTTP server sharing cache entries between
  machines (`GET`/`PUT /v1/blobs/{address}`), storing them in the same
  layout as the local cache with LRU eviction. Configured with `--listen`,
  `--data-dir`, `--size`, `--evict-interval` and `--max-blob-size`.
- `halide-cache-server`: monitoring. `/v1/stats` with eviction-age
  histogram, `/v1/history` with 24 hours of per-minute counters,
  `/v1/clients` with per-client counters, `/metrics` in Prometheus format,
  and a self-contained dashboard at `/`.
- `halide-cache-server`: a systemd unit (`contrib/halide-cache-server.service`)
  and static musl binaries for x86_64 and aarch64 in releases.
- `halide-cache`: `--remote <URL>`. Entries missing locally are fetched
  from the server, and new entries are uploaded. The remote is fail-open:
  connection errors, failed uploads and corrupt entries are warnings and
  the build proceeds locally.
- `halide-cache`: Windows clients are supported and tested in CI.
- End-to-end tests covering hits across machines, a server that is down,
  corrupt entries, oversized uploads and concurrent clients.

### Changed

- **Breaking:** the cache key scheme changed, so existing local caches are
  invalidated. Keys are now stable across machines: the base directory is
  stripped from arguments and path separators normalised, and the key
  includes a scheme version, the host OS and architecture, a hash of the
  builder executable, the Python interpreter and the native libraries of
  the `halide` package it imports.
- **Breaking:** the on-disk cache format changed. The generated object
  and header are stored together as one zstd-compressed tar entry, and
  entries in the older format are removed on the next scan.
- A missing or unusable `halide` Python package is now an error instead
  of producing a weaker key.
- Non-UTF-8 output paths are rejected instead of being hashed lossily.
- Local cache cleanup is best effort, since Windows refuses to delete an
  entry another parallel build has open.
- More compact cache-hit output.

### Fixed

- The system test had been broken since `--cache-dir` replaced
  `HALIDE_CACHE_DIR`. It is replaced by integration tests in Rust that run
  the `halide-cache` binary around a small Python generator.

## [0.2.1] - 2026-05-19

### Changed

- Stop using environment variables to configure `halide-cache`. The cache
  directory is now set with `--cache-dir`.

## [0.2.0] - 2026-05-18

### Added

- Hash the builder script invocation.
- `--base-dir` option.

### Changed

- License changed from MIT to BSD 3-Clause.

## [0.1.0] - 2026-03-09

First release.

[Unreleased]: https://github.com/zivid/halide-cache/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/zivid/halide-cache/compare/v0.2.1...v1.0.0
[0.2.1]: https://github.com/zivid/halide-cache/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/zivid/halide-cache/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/zivid/halide-cache/releases/tag/v0.1.0
