# Architecture

Pixsort is a single Rust binary with a library core. The GUI is a thin layer: it
reads SQLite state, starts background jobs, and never manipulates the archive
itself.

## Module dependency flow

```
model/config
    │
    ▼
imaging ──────────────┐
    │                 │
    ▼                 │
db (index + plan)     │
    │                 │
    ▼                 ▼
scan        compare / recommend / planner
    │                 │
    ▼                 ▼
jobs ─────────── import / analysis
                      │
                      ▼
                  executor (fsops)
                      │
                      ▼
                     ui
```

| Module | Responsibility |
| --- | --- |
| `config` | Four independent paths, thresholds, platform config directory, validation. |
| `model` | Plain data types, plan/group states, progress and pause/stop control. |
| `imaging` | Decoding, EXIF, normalization, exact pixel hash, pHash, previews, optional libheif. |
| `db::index_db` | Database A: operational image index and resumable scan state. |
| `db::plan_db` | Database B: sessions, groups, members, decisions and plan items. |
| `scan` | Directory enumeration, cache-aware parallel indexing, progress. |
| `compare` | Exact hash classes, BK-tree perceptual candidate search, bounded complete-linkage grouping. |
| `recommend` | Explainable keeper scoring. |
| `planner` | Groups → final-state plan items, destination naming, review decisions. |
| `import` / `analysis` | Workflow orchestration. |
| `fsops` | Verified copies, no-overwrite publishing, cross-filesystem moves, trash paths. |
| `executor` | Idempotent plan application and interrupted-operation recovery. |
| `jobs` | Background thread plumbing and throttled progress reporting. |
| `dirpicker` | Zenity/KDialog/macOS abstraction; in-app browser fallback lives in the UI. |
| `ui` | egui/eframe screens, image cache and dialogs. |

## Database A — image index

A temporary, rebuildable cache. Default: `<config>/sessions/index.sqlite`.

`photos` stores one row per file:

- absolute path, root (`archive` / `input`),
- file size and modification time in nanoseconds (change detection),
- actual format, normalized dimensions, orientation,
- normalized pixel hash, 63-bit pHash,
- capture date (`taken_on` ISO date, timestamp, source/reliability),
- extracted EXIF summary as JSON,
- processing status (`pending` / `indexed` / `failed`) and error,
- index timestamp.

Every file is inserted as `pending` before hashing. If the application crashes
or the user stops the job, completed rows stay `indexed`; the rest stay
`pending`. A later scan reuses rows whose size and mtime are unchanged and
re-hashes only changed/new files. Rows for files that no longer exist are pruned
after a complete enumeration.

Indexes exist on path (unique), root, status, pixel hash and pHash.

## Database B — modification plan

A durable working database. Default: `<config>/sessions/plan.sqlite`.

- `sessions` — mode (`import` / `analysis`), input, destination, status,
  timestamps, serialized settings, and a `plan_dirty` recovery flag.
- `groups` — exact/perceptual/single groups, similarity, distance, state,
  decision, keeper and recommended path, explanation, title.
- `group_members` — full serialized photo records, so review still works if the
  index is unavailable.
- `plan_items` — operation type, source/destination/existing/preferred paths,
  reason, detail, execution status, result path, error, and recovery metadata.
- `reviews` — an append-only audit trail of user decisions.

`plan_items` is regenerated from group decisions while the plan is still being
prepared. Once execution starts (any item is running/applied/failed), decisions
can no longer trigger a silent regeneration.

## Import flow

1. Validate all four locations and refuse to start while an unfinished plan
   exists.
2. Scan/index the archive root, reusing cached hashes.
3. Scan/index the input folder. Unchanged files are not decoded again.
4. Load indexed photos. Group exact pixel duplicates first (hash map).
5. Build BK-trees of archive and input pHashes; query only the input side, so
   archive photos are never compared with each other during import.
6. Merge exact classes with bounded complete-linkage clustering (strongest edges first, every pair within the review radius, hard group-size cap); create persisted groups and a metadata recommendation.
7. Classify each input photo as new, exact duplicate, probable or ambiguous.
8. Create a session and groups, then rebuild final-state plan items.
9. Stop before any filesystem modification.

One `single` group is stored per unmatched imported photograph so the plan can
be regenerated from the database after a restart without re-running comparison.

## Archive analysis flow

1. Scan/index the archive (read-only).
2. Collapse exact-pixel classes.
3. Build a BK-tree over one representative per exact class and query it with the
   review radius.
4. Merge classes with the same bounded complete-linkage rule and compute the worst cross-class distance per group.
5. Persist groups and generate a plan; nothing is modified until approval.

No image is decoded during comparison. The index is the only input.

## Comparison thresholds

The pHash is 63 bits. Similarity is:

```
similarity_percent = (1 - hamming_distance / 63) × 100
```

Default thresholds: probable at 95% (distance ≤ 3), grouping/review at 85%
(distance ≤ 9), and a hard cap of 10 photographs per perceptual group. The
thresholds and cap are configurable in Settings. The original proposal used 70%
for review, but at that radius a transitive chain of weak matches can merge
hundreds of unrelated photos; 85% is the conservative default, and grouped
matches must be mutually coherent at the radius. See
[policies.md](policies.md).

A plan session records the grouping-policy version that produced it. Sessions
created before the bounded complete-linkage algorithm (or by hand) are detected
and refused for review/apply; they must be discarded and the analysis rerun.

## Threading and responsiveness

- Indexing runs in a dedicated background thread with a Rayon pool sized by the
  worker-thread setting; files are processed in bounded chunks so full-resolution
  images are not all resident at once.
- Results are written to SQLite in batches. Progress events are throttled to
  100 ms.
- Pause blocks workers between items; stop leaves unprocessed rows pending.
- Preview images are decoded on a separate single worker thread and uploaded as
  egui textures on the UI thread, with a small LRU cache.
- Plan execution is sequential, which makes recovery and status reporting much
  simpler than parallel destructive operations.

## Session lifecycle and restarts

- Starting a new session supersedes the previous active one, but refuses if the
  previous plan still has unfinished operations.
- Settings, session paths and database locations persist in JSON; databases
  persist independently.
- On startup the app loads the newest session. If `plan_dirty` is set on an
  active session, the plan is rebuilt from persisted groups and decisions.
- If the configured destination/input no longer matches the session, planning
  and execution are refused with a reconnect message.
- Discarding a session removes its rows (explicitly, never automatically) but
  leaves the database files and all photographs.

## Platform notes

- Linux is the primary target. No hardcoded mount points or absolute paths are
  used by the core.
- Paths are handled with `Path`/`PathBuf`; spaces and Unicode (UTF-8) are
  supported. Non-UTF-8 filenames are skipped with a warning because SQLite text
  storage cannot round-trip them safely.
- Windows builds use the same core. The native folder dialog is not implemented
  on Windows yet; the in-app browser is used. HEIC uses `heif.dll` when present.
  Case-insensitive destination conflicts are handled by name comparison.
- macOS folder selection uses `osascript`; otherwise the in-app browser appears.
