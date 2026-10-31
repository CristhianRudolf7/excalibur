# Ingestion guard

[Español](ingestion.es.md)

`excalibur-ingest` is the shared, deterministic file-control library for architecture section 20.7. It is a library only: it starts no antivirus engine, opens no network connection and writes nothing except the quarantine directory a host explicitly configures. The document library uses it behind the existing `IngestGuard` trait, so `library_ingest` callers are unchanged.

## What the guard checks

`ContentGuard::examine(name, declared_type, bytes)` returns an `Inspection` or a `Rejection` with a stable `Code`. In order:

1. **Size.** Empty files and files over `max_input_bytes` (64 MiB by default) are rejected. Declared text types (`text/*`, JSON, XML, YAML, TOML and `+json`/`+xml`) must be UTF-8 without NUL bytes. These are the rules the previous built-in guard enforced.
2. **Type by content.** The kind is detected from magic bytes only: PDF, PNG, JPEG, GIF, WebP, WAV, MP3 with ID3, Ogg, FLAC, MP4/QuickTime `ftyp`, zip, gzip, zstd, ustar, ELF, PE, Mach-O, WebAssembly, text or unknown binary. ELF, PE, Mach-O and WebAssembly are always rejected, whatever the name or declared type. The declared media type and, when the caller has one, the file name extension are compared with the detected kind. PDF, PNG, JPEG, GIF, WebP, zip-based office formats, gzip, zstd and tar require their signature. Audio and video types without a reliable signature (MP3 frames without ID3, for example) are rejected only when the content carries a different positive signature; other `image/*`, `audio/*` and `video/*` types reject documents, archives and executables. With `MismatchPolicy::Flag` disagreements are recorded in `Inspection.flags` and accepted; the default is `Reject`.
3. **Bounded expansion.** zip (stored and deflate), gzip (including multi-member), zstd and POSIX ustar are expanded in memory. Limits cover total bytes produced by every decompressor across all levels (256 MiB), ratio of output to compressed input per stream and for the whole file (100:1, with output below 1 MiB exempt), total entries (10,000) and nesting depth (3; a `.tar.gz` is depth 2). Decoders produce output in 64 KiB steps and stop as soon as the next step would cross a limit, so a bomb never expands past it. zip entry counts are checked against the central directory before any entry is inflated. The zstd window is capped at 32 MiB.
4. **Entry safety.** Absolute paths, drive-qualified paths, `..` components, control characters and names that normalize to nothing are rejected for zip, tar and the gzip `FNAME` field. Symbolic links, hard links and device or FIFO entries are rejected. Encrypted zip entries, zip64, multi-disk archives, other zip methods, pax and GNU extension headers and base-256 tar sizes are rejected as unsupported rather than skipped. CRC, declared size, duplicate names, local/central name disagreement, overlapping zip records and trailing data are rejected as malformed. Executable content inside an archive is rejected, and an entry name whose extension disagrees with its content is a mismatch.
5. **Scanner hook.** An optional `Scanner` sees the file and every expanded entry, including intermediate decompressed containers. `Infected` and `Failed` verdicts reject the file; with `require_scanner` a guard with no scanner rejects everything. `SignatureScanner` is a deterministic byte-pattern scanner; `SignatureScanner::eicar()` recognises the standard EICAR test string, which `eicar_test_string()` assembles at run time.
6. **Quarantine.** When a `Quarantine` is configured, every rejected non-empty file is stored as `objects/<sha256>.bin` and a JSON line is appended to `manifest.jsonl` with hash, size, reason code, reason, original name, declared type, detected type, object path and timestamp. Directories are mode 0700 and files 0600. Objects are written to a temporary file, synced and renamed; manifest appends hold an OS file lock, are synced and repair a torn final line left by an interrupted append. A symlinked quarantine directory is refused. A failure to quarantine is reported in the rejection and never turns it into acceptance.

Nothing expanded is written to disk, executed or given a mode: archive modes with execute bits are reported as `executable_mode_ignored` flags only.

## Integration

`BasicIngestGuard { max_bytes }` keeps its type and default and now delegates to `ContentGuard` with no scanner and no quarantine. `ContentGuard` also implements `IngestGuard`, so a host installs a configured guard through `LibraryServices.guard`. `excalibur_memory::ingest` re-exports the crate. The trait passes no file name, so library ingestion compares the declared media type with the content; callers with a name use `examine` directly. Rejections surface as `Error::Rejected("<code>: <reason>")`.

## Verification

`cargo test -p excalibur-ingest` covers fake extensions and declared types, executables under any name, flag policy, ordinary nested archives, high-ratio gzip/zstd/zip bombs, total expansion, nested depth and cumulative bombs, entry limits across levels, damaged archives, traversal, link and special entries, ignored executable modes, scanner hit, miss, failure, required scanner, nested EICAR samples, quarantine manifest, permissions, reopening, torn-tail repair and concurrent appends from two handles. `cargo test -p excalibur-memory --test ingest_guard` drives `library_ingest` through the default and a configured guard. All fixtures are synthetic and generated during the test.

## Limits

- No real antivirus, ClamAV adapter or network scanner is included; the `file-scanner` capability remains a host integration. Signature matching is a test double, not malware detection.
- Section 20.7 step 5 (analysis and conversion in the `file-analysis` sandbox profile) and the "quarantine until passing" staging flow for attachments, downloads and imports are not wired here; only the document library calls the guard, and accepted files are not kept in quarantine.
- Detection covers the listed signatures. Old v7 tar without the ustar magic, bzip2, xz, 7z, RAR and zip64 are not expanded (zip64 is rejected; the others are treated as unknown binary). Office documents are zip containers inspected as such, not parsed for macros.
- Expansion happens in memory within the configured total; hosts with tight memory should lower `max_expanded_bytes`.
- File modes and permission checks are Unix behaviour verified on Linux. Other platforms skip the mode changes by conditional compilation; that path has not been built or run.
