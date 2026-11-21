# Image normalization, hashing and similarity policies

This document records the decisions the specification explicitly asks to
document. If something is ambiguous, the simplest safe interpretation was
chosen and is noted here.

## Supported formats and decoding

| Format | Decoder |
| --- | --- |
| JPEG | `image` crate |
| PNG | `image` crate |
| WebP | `image` crate |
| BMP | `image` crate |
| TIFF | `image` crate |
| HEIC / HEIF | optional runtime `libheif` (loaded with `libloading`) |

Format detection uses magic bytes, with the file extension as a fallback. GIF is
not considered a photograph and is not indexed; videos are ignored.

### Animated images

Pixsort does not provide animation management. For animated PNG and WebP files:

- only the **first frame** is decoded and analysed,
- the file is flagged as animated and this is visible in the review metadata,
- the flag is informational; there is no frame browser.

Animated GIF is outside the supported set. Animated WebP/APNG files that are
first-frame duplicates are therefore treated as duplicate stills, which is the
consistent, documented policy.

### HEIC

HEIC/HEIF is decoded through the system `libheif` at runtime. If the library is
missing, indexing marks the file failed with a message that explains how to
install or point to libheif. EXIF (including orientation) is read by
`kamadak-exif`, which understands the HEIF container independently of libheif.
HEIC alpha, when present, is decoded as RGBA and follows the same alpha policy.

## Normalized pixel representation

Exact-pixel equality must not depend on file metadata, encoding, filename or
container. The normalization pipeline is:

1. Decode the file to pixels. Animated inputs use frame 0.
2. Read EXIF and apply orientation 1–8 (rotate/flip/transpose).
3. Convert to 8-bit RGB.
4. Composite alpha over **opaque white**:
   `out = (c·a + 255·(255−a) + 127) / 255`.
   The alpha channel itself is not hashed; two images that differ only in an
   alpha channel but composite to the same RGB values are exact duplicates.
5. Convert higher-bit-depth samples to 8 bits deterministically through the
   image decoder; Pixsort stores and hashes 8-bit samples.
6. Treat decoded samples as **sRGB**. Embedded ICC profiles are *not* applied.
   This is documented rather than silently normalized because applying ICC
   transforms would require a color-management dependency and introduce
   platform-dependent results.
7. The normalized dimensions are part of the hash input.

### Exact hash framing

```
SHA-256(
    "PIXSORT-PIXEL-HASH-v1"
    || width  (u32 little-endian)
    || height (u32 little-endian)
    || "RGB8"
    || width × height × 3 RGB bytes
)
```

This is a **visual** hash, not a byte-for-byte file hash. Re-encoding the same
pixels as PNG, BMP or TIFF yields the same value; a lossy JPEG re-encode does
not.

## Perceptual hash (pHash)

1. Resize the normalized RGB image to 32×32 with Lanczos3
   (`fast_image_resize`).
2. Convert to luma with BT.601 weights: `0.299R + 0.587G + 0.114B`.
3. Compute the low-frequency 8×8 block of the 2-D DCT-II.
4. Exclude the DC coefficient (it only describes average brightness), leaving
   **63 coefficients**.
5. Compute the median of those 63 coefficients; each coefficient above the
   median sets one bit.

The result is a 63-bit hash. Hamming distance is computed with popcount.

### Similarity conversion and calibration

```
similarity_percent = (1 − distance / 63) × 100
```

| Hamming distance | Indicator | Default class |
| ---: | ---: | --- |
| 0 | 100.0% | probable / exact if pixel hash matches |
| 1 | 98.4% | probable |
| 2 | 96.8% | probable |
| 3 | 95.2% | probable |
| 4 | 93.7% | review |
| 10 | 84.1% | review |
| 19 | 69.8% | review (boundary) |
| 20 | 68.3% | new photograph |

Default thresholds:

- **probable duplicate**: ≥ 95% (distance ≤ 3),
- **worth reviewing**: ≥ 70% (distance ≤ 19),
- below that: treated as a new photograph.

These percentages are **UI indicators, not calibrated probabilities**. Real
pHash distances depend on content, resizing, compression and messaging-app
pipelines. The initial thresholds were chosen conservatively so that a
re-encode is almost always surfaced, while clear differences fall below the
review radius. Tests cover downscaling, JPEG recompression and a
resize+recompress messaging-style pipeline. Thresholds are configurable, and
the actual distance is always displayed next to the percentage.

### Scope of perceptual matching

Deliberately not implemented: crop matching, rotation matching, object
recognition, semantic similarity, feature matching and AI/ML. A version that
was cropped or rotated is not guaranteed to match; aspect-preserving
resize/recompression is the target.

## Classification and plan behavior

For each imported photograph:

- **New** — no candidate within the review radius.
- **Exact duplicate** — another indexed photo has the same normalized pixel
  hash.
- **Probable duplicate** — best perceptual distance ≤ probable threshold.
- **Ambiguous** — best distance inside the review radius but above the probable
  threshold.

Exact groups are resolved automatically using the recommendation. Perceptual
groups are always placed in manual review, even when probable, because the user
has the final decision for perceptually similar photographs. A group may contain
exact and perceptual links; the UI shows the full group.

For an import, if the keeper is a new file and archive versions exist, the plan
uses REPLACE (old copy → trash, new copy verified and inserted). If the keeper is
the archive copy, imports are SKIP. In archive analysis, redundant archive
versions become MOVE_TO_TRASH.

## Recommendation scoring

The keeper recommendation is deterministic and explainable. Weights:

| Criterion | Max points | Notes |
| --- | ---: | --- |
| Higher resolution (pixel count) | 40 | Proportional to the largest in the group. Not proof of quality. |
| Reliable capture date | 15 | DateTimeOriginal 15, DateTimeDigitized 12, EXIF DateTime 8, mtime 3.75. |
| Older capture metadata | 10 | Proportional among EXIF timestamps. |
| Metadata completeness | 10 | Fraction of six useful fields. |
| Date in filename | 5 | `YYYYMMDD`, `YYYY-MM-DD`, `YYYY_MM_DD`. |
| File size | 5 | Secondary indicator; larger is preferred within the group. |
| Existing archive copy | 0.25 | Deterministic retention tie-break, not quality. |

Ties are broken by path, so repeated runs produce the same plan. The UI shows
every awarded factor with its points and the score difference to the next
candidate. For exact duplicates, resolution and pixels are identical; the
recommendation is about which physical file to retain.

## Capture-date resolution

Destination year priority:

1. EXIF `DateTimeOriginal`
2. EXIF `DateTimeDigitized`
3. EXIF `DateTime`
4. Filesystem modification time (configurable; local time zone)
5. `Unknown/` when no reliable date exists

EXIF timestamps are treated as local wall-clock values; time-zone offsets are
not used for ordering. This is deterministic and avoids needing external tzdata.
Invalid placeholder dates (`0000:00:00 …`) are ignored.

## Filenames and conflicts

Original filenames are preserved. Characters invalid on common filesystems are
replaced with `_`; Windows reserved names are prefixed with `_`. If a name is
already used in the target year folder (case-insensitive comparison), Pixsort
appends ` (1)`, ` (2)`, … before the extension. The plan displays the chosen
name, and execution re-checks for late conflicts and never overwrites.

## Databases and exact-match reuse

The pHash is computed once during indexing and cached. This is deliberate: it is
a required field of Database A and allows archive analysis without decoding
images again. When an exact pixel match is already established, **no perceptual
comparison is performed** for that pair; the cached pHash is simply not queried.
This satisfies the "do not calculate perceptual hashes unnecessarily" intent
without re-decoding files on every import.
