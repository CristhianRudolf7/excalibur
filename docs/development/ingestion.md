# Ingestion guard

[Español](ingestion.es.md)

`excalibur-ingest` is the shared, deterministic file-control library for architecture section 20.7. It is a library only: it starts no antivirus engine, opens no network connection and writes nothing except the quarantine directory a host explicitly configures. The document library uses it behind the existing `IngestGuard` trait, so `library_ingest` callers are unchanged.

## What the guard checks

`ContentGuard::examine(name, declared_type, bytes)` returns an `Inspection` or a `Rejection` with a stable `Code`. In order:

1. **Size.** Empty files and files over `max_input_bytes` (64 MiB by default) are rejected. Declared text types (`text/*`, JSON, XML, YAML, TOML and `+json`/`+xml`) must be UTF-8 without NUL bytes. These are the rules the previous built-in guard enforced.
2. **Type by content.** The kind is detected from magic bytes: PDF, PNG, JPEG, GIF, WebP, WAV, MP3 with ID3, Ogg, FLAC, MP4/QuickTime `ftyp`, zip, gzip, zstd, ustar, ELF, PE, Mach-O, WebAssembly, text or unknown binary. ELF, PE, Mach-O and WebAssembly are always rejected, whatever the name or declared type. The declared media type and, when the caller has one, the file name extension are compared with the detected kind. One exception keeps the previous behaviour: valid UTF-8 text that the declared type or name promises as text stays text even if it begins with another format's magic bytes (for example a Markdown note starting with `%PDF` or `ID3`). No executable signature is valid NUL-free UTF-8, so this never admits code. PDF, PNG, JPEG, GIF, WebP, zip-based office formats, gzip, zstd and tar require their signature. Audio and video types without a reliable signature (MP3 frames without ID3, for example) are rejected only when the content carries a different positive signature; other `image/*`, `audio/*` and `video/*` types reject documents, archives and executables. With `MismatchPolicy::Flag` disagreements are recorded in `Inspection.flags` and accepted; the default is `Reject`.
3. **Bounded expansion.** zip (stored and deflate), gzip (including multi-member), zstd and POSIX ustar are expanded in memory. The whole container structure is validated before any entry is decoded or handed out, and stored zip and tar entries borrow the input instead of being copied. Limits cover:
   - the expansion budget (256 MiB), counting every decoder's output plus the bytes of stored zip and tar entries handed out for inspection, across all levels. A container nested in another is counted at each level;
   - the ratio of output to compressed input, per stream and for the whole file (100:1, with output below 1 MiB exempt);
   - total entries (10,000);
   - gzip members or zstd frames per stream (4,096);
   - nesting depth (3; a `.tar.gz` is depth 2).

   Decoders produce output in 64 KiB steps and stop as soon as the next step would cross a limit. zip entry counts are checked against the central directory before any entry is inflated. The zstd window is capped at 32 MiB. Legacy (pre-1.0) zstd frames are refused anywhere in the stream before decoding, so the cap always applies.
4. **Entry safety.** Absolute paths, drive-qualified paths, `..` components, control characters and names that normalize to nothing are rejected for zip, tar and the gzip `FNAME` field. Symbolic links, hard links and device or FIFO entries are rejected. For zip, the high 16 bits of the external attributes are read as a Unix mode for every host system, as Info-ZIP does for Unix, VMS, BeOS, AtheOS and others. Encrypted zip entries, zip64, multi-disk archives, other zip methods, pax and GNU extension headers and base-256 tar sizes are rejected as unsupported rather than skipped. The following zip conditions are rejected as malformed:
   - a central directory holding more data than its declared entries;
   - a local header whose flags, method, CRC, sizes or name differ from the central directory (data descriptors must also agree);
   - local records that overlap, leave gaps or do not reach the central directory exactly;
   - bytes after an entry's deflate stream;
   - CRC or size mismatches and duplicate names.

   Also rejected as malformed: non-zero data after the tar end-of-archive block, data after the last gzip member and data after the last zstd frame. Executable content inside an archive is rejected, and an entry name whose extension disagrees with its content is a mismatch.
5. **Scanner hook.** An optional `Scanner` sees the file and every expanded entry, including intermediate decompressed containers. `Infected` and `Failed` verdicts reject the file; with `require_scanner` a guard with no scanner rejects everything. `SignatureScanner` is a deterministic byte-pattern scanner; `SignatureScanner::eicar()` recognises the standard EICAR test string, which `eicar_test_string()` assembles at run time.
6. **Quarantine.** When a `Quarantine` is configured, every rejected non-empty file is stored as `objects/<sha256>.bin` and a JSON line is appended to `manifest.jsonl`. The line records hash, size, reason code, reason, original name, declared type, detected type, object path and timestamp, with text fields truncated to fixed bounds.
   - **Permissions:** directories are mode 0700 and files 0600, set through open handles.
   - **Writes:** an object is written to a temporary file, synced, renamed and its directory synced. An existing object is reused only if it is a regular file whose content still matches its hash; a planted symlink or corrupted object is replaced by the rename and its target is never touched.
   - **Leftovers:** temporary files older than ten minutes are removed when the quarantine is opened.
   - **Manifest:** appends hold an OS file lock, are synced and repair a torn final line left by an interrupted append. A symlinked quarantine directory, `objects` directory or manifest is refused.
   - **Failures:** a failure to quarantine is reported in the rejection and never turns it into acceptance.

Nothing expanded is written to disk, executed or given a mode: archive modes with execute bits are reported as `executable_mode_ignored` flags only. Names, declared types and scanner text echoed in rejection messages are quoted and truncated to 96 characters.

## Integration

`BasicIngestGuard { max_bytes }` keeps its type and default and now delegates to `ContentGuard` with no scanner and no quarantine. `ContentGuard` also implements `IngestGuard`, so a host installs a configured guard through `LibraryServices.guard`. `excalibur_memory::ingest` re-exports the crate. The trait passes no file name, so library ingestion compares the declared media type with the content; callers with a name use `examine` directly. Rejections surface as `Error::Rejected("<code>: <reason>")`.

## Verification

`cargo test -p excalibur-ingest` runs three suites:

- `guard` covers fake extensions and declared types, executables under any name, flag policy, ordinary nested archives, high-ratio gzip/zstd/zip bombs, total expansion, nested depth and cumulative bombs, entry limits across levels, damaged archives, traversal, link and special entries, ignored executable modes, scanner hit, miss, failure, required scanner, nested EICAR samples, quarantine manifest, permissions, reopening, torn-tail repair and concurrent appends from two handles.
- `audit_regressions` covers the findings of the independent audit:
  - stored entries charged to the budget;
  - hidden central-directory entries and unaccounted local records;
  - local/central disagreement;
  - symlink and executable modes for every host byte;
  - text with magic prefixes;
  - tar and deflate trailing data;
  - gzip member and zstd frame floods;
  - truncated echoes;
  - planted-symlink, corrupted-object, symlinked-manifest and stale-temporary quarantine cases;
  - legacy zstd frames.
- `zip_memory` checks with a counting allocator that overlapping stored entries are refused with bounded heap growth.

`cargo test -p excalibur-memory --test ingest_guard` drives `library_ingest` through the default and a configured guard. All fixtures are synthetic and generated during the test.

## Limits

- No real antivirus, ClamAV adapter or network scanner is included; the `file-scanner` capability remains a host integration. Signature matching is a test double, not malware detection.
- Section 20.7 step 5 (analysis and conversion in the `file-analysis` sandbox profile) and the "quarantine until passing" staging flow for attachments, downloads and imports are not wired here. Only the document library calls the guard, and accepted files are not kept in quarantine.
- Inspection runs synchronously on the calling thread, including inside the async `library_ingest`; its cost is bounded only by the limits above.
- Detection covers the listed signatures at the start of the file. Formats that are not expanded:
  - zip64 is rejected;
  - old v7 tar without the ustar marker, bzip2, xz, 7z and RAR are treated as unknown binary;
  - data appended after a non-archive signature (a PDF or GIF with a zip appended, for example) is not inspected;
  - Office documents are inspected as zip containers, without parsing macros;
  - Java class files share the Mach-O universal magic and are rejected as executables, so JAR contents with classes are refused.
- Shebang scripts are accepted as text. The guard never executes received files; it does not judge script content.
- Entry names are checked for traversal only, because nothing is extracted to disk. Unicode look-alikes, bidirectional overrides, Windows reserved names and case-only duplicates are not rejected; a future extractor must apply its own rules.
- Rejection messages and quarantine reasons contain bounded but untrusted text; marking them as untrusted content when shown to a model (section 20.8) is the caller's responsibility.
- Expansion happens in memory within the configured total; hosts with tight memory should lower `max_expanded_bytes`.
- The quarantine manifest and objects grow without bound, and `records()` reads the whole manifest. Parent directories of the quarantine are created as given and trusted. The symlink checks narrow, but do not close, races with another process running as the same user.
- File modes and permission checks are Unix behaviour verified on Linux. Other platforms skip the mode changes by conditional compilation; that path has not been built or run.
