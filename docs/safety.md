# Safety, execution and recovery

Data safety takes priority over aggressive automatic cleanup. The archive is
read-only during scanning, comparison and review.

## Grouping safety

Duplicate grouping must not be allowed to drift through weak similarity chains.
The comparator uses bounded complete-linkage clustering: every pair of exact
classes inside a perceptual group must be within the configured review radius,
and a group is capped at 10 photographs by default. Weak candidates are not
transitively merged. Perceptual groups are always REVIEW operations until the
user decides.

Plans store the grouping-policy version that created them. A session produced by
the old unbounded grouping algorithm is detected and refused for decisions and
execution; the safe action is to discard it (database files and photos are kept)
and rerun Archive analysis.

## Explicit approval

`executor::apply_plan` requires a `confirmed` argument. The GUI sets it only
after the user ticks a checkbox in the Apply dialog. There is no "auto-clean"
mode and no destructive action on startup.

## The plan as a final state

The plan represents what the archive should look like after execution:

- for every duplicate group, one keeper (or all versions when the user chose
  "keep both"/"ignore"),
- every new keeper is inserted at a computed year path,
- redundant archive versions are moved to trash,
- redundant imports are skips (the input file is left where it is),
- unresolved perceptual groups are REVIEW items and block execution.

If several imported photographs belong to one group, the decision is made once
and all derived operations are rebuilt together. Applied operations are never
silently replaced by a regenerated plan.

## Per-operation execution

Operations run sequentially in a stable order (trash first, then replace, then
insert, then skips). Each item is persisted as `running` before its first
destructive step.

| Operation | Behavior |
| --- | --- |
| INSERT | Verify source against the index, copy to a temp file in the destination folder, flush, re-read and verify SHA-256, publish without overwrite, update the index. Input file is untouched. |
| REPLACE | Persist the intended trash path, move the old archive file to trash, copy/verify the new file to the destination, update the index. |
| MOVE_TO_TRASH | Persist the trash path, move the file (atomic rename or verified copy + remove), remove its index row. |
| SKIP | No filesystem change; item becomes `noop`. |
| REVIEW | Never executed; blocks the whole plan until resolved. |

### Publishing without overwriting

Copies are written to `.pixsort-partial-<uuid>.tmp` in the destination
directory. After `sync_all`, the temporary file is re-read and hashed; the
expected SHA-256 is the hash computed while reading the source. Publication uses
a hard link (atomic, fails if the name exists); if hard links are unsupported,
an exclusive create-new copy is used. The temp file is removed only after
publication succeeds. Directory `fsync` is best-effort on platforms that allow
it.

### Cross-filesystem moves

`move_file_verified` first tries `rename`. On cross-device failure (or any
rename error), it falls back to the verified copy procedure and removes the
source **only after** the copy has been verified and the source size/mtime is
unchanged. The trash directory and destination may be on different filesystems;
Pixsort never assumes otherwise.

### Trash

The trash directory is configurable. The default is `pixsort-trash` as a sibling
of the archive root (outside the archive). Trash paths preserve the archive's
relative directory structure and get ` (1)` suffixes on collision. Files in
trash are never deleted by Pixsort.

The trash folder may be on a different filesystem from the archive (or even a
local disk when the archive is a NAS). Cross-filesystem moves use the verified
copy-and-remove procedure. Settings → Trash folder has a **Use local trash
folder** shortcut that selects a user-writable local path.

### Write-access preflight

Before the first operation is executed, Pixsort checks every archive folder it
will actually write to or remove files from, plus (when the plan contains
REPLACE or MOVE_TO_TRASH) the trash folder. It creates and removes a private
probe file in each. A move-only plan therefore needs write access to the source
folders, not necessarily to the archive root. If a preflight fails, the session
stays active, no plan item is marked running, and nothing is moved or copied.
The error names the exact path and tells you which folder to fix.

On a shared/server disk with `root:users` and setgid folders, the standard fix
is to put your user in the owning group or grant a write ACL on the specific
folders Pixsort needs. That is narrower than running the application as root.

Pixsort deliberately does **not** invoke `sudo`/`pkexec` or run as root. A
desktop photo application parses untrusted images and should not have root
privileges; elevating the whole process would also make settings and database
files root-owned. More importantly, privilege escalation cannot make a
read-only/NAS mount writable. Fix the mount options or folder ownership, or put
the trash on a writable local disk and keep the archive on a writable share.

## Interrupted execution and recovery

If the application or machine stops mid-plan, the item remains `running` with
recovery metadata. On the next Apply:

- **INSERT** — if the planned destination exists and its content matches the
  source (or the source is gone and the size matches), the copy is considered
  published; index bookkeeping is completed and the item becomes applied. No
  second copy is created.
- **REPLACE** — the persisted trash path is inspected. If the new copy is
  already published, the old file is retired if still present, then the item is
  finalized. If only the old file was trashed, the copy is completed.
- **MOVE_TO_TRASH** — if the source is gone and the recorded trash file exists,
  the item is applied. If both exist with identical hashes, the source is
  removed (the copy was completed but the source removal was interrupted).
- **Failed** items are retried on the next Apply, using the same persistent
  metadata; completed items are skipped. The Apply button changes to
  “Retry failed / apply remaining…” when failures are present.

Execution status, result paths, errors and recovery metadata are stored in
Database B. `PRAGMA synchronous=FULL` is used for the plan database because
losing a committed row could lose track of a filesystem operation that already
happened.

## Changed or missing inputs

Before any operation, the source (and for REPLACE the old archive file) is
looked up in the image index and its current size/mtime are compared with the
indexed values. A missing or changed file becomes `blocked`, with an explanation;
it is never guessed or overwritten. The user can rescan and retry.

Change detection intentionally uses size and nanosecond modification time rather
than hashing every file again: it is cheap enough for tens of thousands of
photos and avoids a second full read of the archive. The residual case — content
edited while both size and mtime are artificially preserved — is not detected
automatically; a rescan (or deleting the index cache) recomputes the hashes.

## Temporary files

If the process is killed during a copy, a hidden `.pixsort-partial-*.tmp` file
may remain in the destination folder. It is never treated as a photograph by
scans, and it is never referenced by the plan. It can be deleted manually once
no copy is running. Published files are always complete because publishing
happens only after verification.

## Destination conflicts

Conflicts are resolved twice:

1. at planning time, by scanning the year folders and appending a suffix,
2. at execution time, by checking again and choosing a new unique name if a
   file appeared in the meantime.

The final name is recorded in the plan item and report. Existing files are never
overwritten; a late conflict is reported, not hidden.

## Session/path validation

Each session stores its input and destination paths. Plan rebuilding and
execution refuse to proceed if the configured paths no longer match the session,
so accidentally pointing Pixsort at a different archive cannot apply an old
plan there. Reconnect the original folders or explicitly discard the session.

Changing database locations simply opens different databases; old files remain
on disk.

## Database integrity and lifecycle

- SQLite WAL is enabled; the index uses `synchronous=NORMAL`, the plan database
  uses `FULL`.
- Settings has a "Check database integrity" action (`PRAGMA quick_check`).
- A corrupt plan database is reported instead of silently recreated; a corrupt
  index can be rebuilt by deleting the index file (it is a cache) and rescanning.
- Both databases are temporary working data, not archive metadata. They are
  never deleted automatically, and never deleted while operations are pending.
- "Discard session" deletes only the session's rows. Database files, photographs
  and trash contents are untouched.

## Ambiguities and chosen safe defaults

| Ambiguity | Decision |
| --- | --- |
| Delete imported source files after a successful import? | No. Imports copy; SKIP leaves the source in place. The user manages the input folder. |
| Should probable perceptual duplicates auto-apply? | No. All perceptual groups require a human decision; exact duplicates are automatic. |
| What is "ignore match"? | Keep both versions and record the ignored relationship as a decided group (no dedupe operation). |
| Animated formats? | First frame only; flagged as animated; no animation management. |
| GIF? | Not supported as a photograph format. |
| Non-UTF-8 filenames? | Skipped with a warning (SQLite text paths cannot round-trip safely). |
| ICC profiles? | Not applied; samples are treated as sRGB. Documented. |
| Exact hash of transparent pixels? | Alpha is composited over white, consistent for all formats. |
| Time zones in EXIF? | Wall-clock ordering; mtime fallback uses the local zone. |
| Trash inside the archive? | Allowed with a warning and skipped during scans; outside is recommended. |
| Database files inside the archive? | Refused by validation. |
| Multiple archive duplicates matched by one import? | All matched archive copies can be retired to trash if the imported keeper wins. Archive photographs are still not compared with each other directly. |
| Stop during apply? | Finish the current item, leave the rest pending, mark the session active so Apply can resume. |
