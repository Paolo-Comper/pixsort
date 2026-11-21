# Pixsort – Project Specification

## Objective

Develop a native desktop application written in Rust for organizing and deduplicating large photo collections.

The application must analyze existing photo archives, import new photographs, detect exact and perceptual duplicates, recommend which version to keep, and prepare a complete modification plan before touching the original files.

The application must be fast, reliable, resumable, and entirely local.

No AI or machine-learning models should be used for image comparison.

## 1. Technology stack

Use the following technologies:

- **Language:** Rust
- **GUI:** egui + eframe
- **Database:** SQLite using rusqlite
- **Image decoding:** image crate
- **Image resizing:** fast_image_resize, if needed
- **EXIF metadata:** kamadak-exif
- **SHA-256:** sha2
- **Parallel processing:** rayon

Choose compatible and maintained versions of dependencies.

Keep the architecture modular, with a clear separation between:
- Image processing.
- Image comparison.
- Database management.
- Import planning.
- Filesystem operations.
- GUI.

Avoid unnecessary abstractions and overengineering.

## 2. Image formats

The application handles photographs only.

Support, where practical:
- JPEG
- PNG
- WebP
- BMP
- TIFF
- HEIC / HEIF

Do not process videos.

Animated image formats should not be treated as a separate animation-management feature. Define a consistent policy for decoding animated formats and document it.

## 3. Image identification

Implement exactly two identification levels.

### Level 1: Exact pixel comparison

Calculate SHA-256 over a normalized decoded pixel representation, not over the original file bytes.

The normalization process must:
- Apply EXIF orientation.
- Convert images into a consistent pixel format.
- Handle alpha consistently.
- Define and document color-space handling.
- Include image dimensions in the hashed representation.

The purpose is to identify images that produce exactly the same normalized pixels despite differences in metadata, file names, or file encoding.

Images with matching pixel hashes are exact visual duplicates.

Do not confuse this with byte-for-byte file equality.

### Level 2: Perceptual comparison

Implement perceptual hashing using pHash.

Use Hamming distance to compare perceptual hashes.

The system must detect visually equivalent photographs that have been:
- Resized.
- Compressed.
- Re-encoded.
- Passed through messaging platforms such as WhatsApp or Discord.

Do not implement:
- Crop matching.
- Object recognition.
- Semantic similarity.
- Advanced feature matching.
- AI-based image recognition.

The goal is strictly to identify different technical versions of the same photograph.

### Similarity thresholds

Make thresholds configurable.

Initial proposed behavior:

- Exact pixel hash match: confirmed duplicate.
- Approximately 95–100% perceptual similarity: probable duplicate.
- Approximately 70–95%: manual review.
- Below approximately 70%: treat as a new photograph.

These percentages are UI indicators, not mathematically guaranteed probabilities.

Calibrate the similarity calculation against actual pHash Hamming distances and document the conversion.

Avoid presenting similarity percentages as statistically reliable confidence values.

## 4. Image metadata and quality evaluation

Extract and store:

- Original filename.
- Original path.
- Image dimensions.
- File size.
- Image format.
- EXIF DateTimeOriginal.
- Other relevant EXIF timestamps.
- Filesystem modification time.
- Normalized pixel hash.
- Perceptual hash.

When comparing duplicates, calculate a recommendation score.

Use simple, explainable criteria:

1. Prefer higher image resolution.
2. Prefer older and more reliable original capture metadata.
3. Prefer more complete metadata.
4. Consider filename patterns that may contain original dates.
5. Use file size as a secondary indicator.

Do not assume that higher resolution always means better image quality.

The application must show why it recommends one image over another.

The user always has the final decision for perceptually similar images.

For exact pixel duplicates, use metadata to recommend which physical file to retain.

## 5. Database architecture

Use two separate temporary SQLite databases.

Neither database should be stored inside the photo archive.

### Database A: Image index

Stores the information required for efficient comparisons:

- Image paths.
- Normalized pixel hashes.
- Perceptual hashes.
- Dimensions.
- File sizes.
- Formats.
- EXIF metadata.
- Filesystem metadata.
- Processing status.

This database is an operational cache used during the current project/session.

It must support resuming interrupted scans.

### Database B: Modification plan

Stores all planned changes to the archive.

Possible operation types:

- INSERT: add a new photograph.
- REPLACE: replace an existing photograph with a better version.
- SKIP: do not import a redundant photograph.
- MOVE_TO_TRASH: move a discarded version to a temporary trash directory.
- REVIEW: wait for a manual decision.

Each operation must include sufficient information to identify:
- The source file.
- The destination file or directory.
- The affected existing file, if applicable.
- The reason for the operation.
- The selected preferred image.
- Its execution status.

The plan must represent the desired final state of the archive, rather than merely a sequence of independent decisions.

This is important when multiple imported photographs belong to the same duplicate group.

The application must preserve the plan across application restarts.

## 6. Import workflow

The import workflow consists of distinct stages.

### Stage 1: Source selection

The user selects:
- The source directory containing new photographs.
- The existing photo archive.
- The desired destination configuration.

### Stage 2: Indexing

Scan the existing archive and the new photographs.

Reuse previously calculated hashes whenever the file has not changed.

Detect changes using stored filesystem information and verify cached information when necessary.

### Stage 3: Comparison

Compare all newly imported photographs:
- Against the existing archive.
- Against other photographs in the same import batch.

Do not compare existing archive photographs against one another during normal import.

Do not calculate perceptual hashes unnecessarily when an exact pixel hash match has already established a duplicate.

### Stage 4: Classification

Classify each photograph as:
- New photograph.
- Exact duplicate.
- Probable perceptual duplicate.
- Ambiguous match requiring review.

Determine the appropriate destination year using:

1. EXIF DateTimeOriginal.
2. Other reliable image timestamps.
3. Filesystem timestamps as fallback.
4. An Unknown directory when no reliable date exists.

### Stage 5: Manual review

Present ambiguous matches side by side.

Always use a consistent layout, with the application's recommended photograph on the same side.

Show:
- Both photographs.
- Resolution.
- File size.
- Format.
- Capture date.
- Modification date.
- Original path.
- Similarity indicator.
- Recommended choice.
- Explanation of the recommendation.

Available user decisions:
- Keep the first image.
- Keep the second image.
- Keep both.
- Ignore the match.

All decisions must update the modification plan.

### Stage 6: Preview

Before applying changes, show a complete summary of the planned final archive state.

The user must be able to inspect pending operations and their consequences.

### Stage 7: Apply

Only after explicit approval should the application modify the original archive.

Use safe filesystem operations.

Avoid deleting photographs permanently.

Move discarded versions into a configurable trash directory.

Make operations recoverable and idempotent where possible.

If execution is interrupted, detect completed operations and resume safely without duplicating or losing files.

## 7. Archive analysis mode

Implement a separate mode for analyzing an existing archive.

This mode must:
- Index all photographs.
- Identify exact pixel duplicates.
- Identify perceptual duplicates.
- Group related photographs.
- Allow manual review.
- Generate a modification plan.

It must not automatically modify the archive.

Do not compare every archive image against every other image using expensive image decoding.

Use the stored hashes and efficient candidate filtering.

This feature must remain separate from the normal import workflow.

## 8. Performance and resumability

Performance is a primary requirement.

Implement:
- Parallel image decoding and hashing.
- Efficient database queries.
- Cached hash reuse.
- Incremental indexing.
- Configurable worker count.
- Efficient perceptual hash comparisons.
- Batched database writes where appropriate.

The application must support large collections containing tens of thousands of photographs.

The user must be able to:
- Pause processing.
- Resume processing.
- Stop processing.
- Close and reopen the application without losing progress.

Do not restart completed processing unnecessarily.

Display:
- Total photographs.
- Processed photographs.
- Remaining photographs.
- Processing speed.
- Estimated remaining time.
- Current operation.

Avoid loading all full-resolution photographs into memory simultaneously.

## 9. Desktop interface

Use egui + eframe.

The application should have a simple, functional desktop interface.

Main sections:

1. Dashboard.
2. Import.
3. Archive analysis.
4. Manual review.
5. Planned modifications.
6. Settings.

The interface must include progress indicators, clear status messages, and side-by-side image comparison.

Prioritize usability and responsiveness over visual complexity.

Do not implement a web interface or network services.

## 10. Safety requirements

The original archive must remain untouched during scanning and review.

No destructive operation may happen without explicit user approval.

Implement safeguards against:
- Interrupted file operations.
- Missing source files.
- Changed files between indexing and execution.
- Duplicate destination filenames.
- Database corruption.
- Incomplete import operations.

Never silently overwrite an existing photograph.

Preserve original filenames whenever possible.

When filename conflicts occur, resolve them safely and transparently.

## 11. Development approach

Implement the project incrementally.

Recommended development stages:

1. Project structure and basic application startup.
2. Image decoding and normalized pixel hashing.
3. Perceptual hashing and comparison.
4. SQLite image index.
5. Incremental and resumable scanning.
6. Modification-plan database.
7. Import classification and recommendation engine.
8. Manual review interface.
9. Safe filesystem execution.
10. Existing archive analysis mode.
11. Performance improvements and testing.

Write automated tests for:
- Exact pixel duplicates with different metadata.
- Images with different resolutions.
- Recompressed images.
- EXIF orientation.
- Different image formats.
- Missing metadata.
- Multiple duplicates in one import batch.
- Interrupted operations.
- Resuming scans.
- Filesystem conflicts.

Do not implement the entire application as one large module.

Keep code readable and maintainable.

Document important architectural decisions, especially image normalization, similarity thresholds, and filesystem recovery.

## 12. Final objective

Deliver a working native desktop application called **Pixsort**.

It should be a focused, fast and reliable photo archive organizer.

The application must prioritize correctness and data safety over aggressive automatic deduplication.

Do not introduce unnecessary AI, cloud services, web interfaces, video processing, or unrelated functionality.

If a technical requirement is ambiguous, document the ambiguity and choose the simplest safe implementation rather than silently making destructive assumptions.

## 13. Input, destination and storage configuration

The application must allow the user to independently select and configure:

1. **Input directory:** contains the photographs to import. It does not need to follow any particular directory structure.
2. **Destination directory:** the root of the existing photo archive. This directory contains subdirectories organized by year (e.g. `2007`, `2008`, ..., `2026`).
3. **Temporary image-index database location.**
4. **Temporary modification-plan database location.**

All four locations must be independently configurable and may reside on different disks or filesystems.

The input directory must never be assumed to have year-based subdirectories.

The destination directory is the archive root. Pixsort must determine the appropriate year subdirectory automatically using the image metadata and the configured fallback rules.

Do not assume that the destination archive or temporary databases are located on the same filesystem.

Handle cross-filesystem operations safely. When an atomic rename is not possible, use a safe copy-and-verify procedure followed by removal of the source only after successful verification.

### Directory selection

Provide graphical directory selection dialogs for input, destination and database locations.

On Linux:
- Prefer Zenity when available for native directory selection dialogs.
- If Zenity is unavailable, provide a suitable fallback.
- Never require the user to manually type filesystem paths.

On Windows:
- Use the native Windows folder selection dialog when Windows support is implemented.

Keep platform-specific directory selection behind a small abstraction so that the GUI and application logic remain platform-independent.

### Platform priorities

Linux is the primary development and testing target.

Windows is a secondary target and does not need to be fully functional in the first release.

However:
- Prefer cross-platform Rust libraries whenever possible.
- Avoid unnecessary Linux-only dependencies in the core application.
- Keep filesystem operations and path handling compatible with both Linux and Windows.
- Do not hardcode mount points, drive letters or absolute paths.
- Support paths containing spaces and non-ASCII characters.

The application should be designed so Windows support can be added without redesigning its architecture.

## 14. Temporary database lifecycle

The two SQLite databases are temporary working databases, not permanent archive metadata.

Their locations must be independently configurable and must not be forced inside the destination archive.

The application must:
- Allow databases to be stored on different disks.
- Preserve their contents when the application is closed.
- Resume interrupted sessions.
- Allow the user to explicitly discard completed temporary sessions.
- Never delete temporary databases automatically while operations remain pending.

When reopening a project, allow the user to reconnect the required input, destination and database locations.

Validate that the referenced directories and files are still accessible before resuming.

