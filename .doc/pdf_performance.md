# PDF Engine Performance

How `src/crates/pdf-engine/` spends its time, what already keeps it
safe, and where the real wins are. Every claim below is grounded in
the current code (`ops/load.rs`, `ops/watermark.rs`, `ops/merge.rs`,
`ops/compress.rs`, `ops/garbage_collect.rs`, `ops/raster.rs`, ...).

## Pipeline: full parse + serialize per operation

Every op has the same shape — bytes in, bytes out, no shared state:

```
input &[u8] → load_pdf() → lopdf::Document::load_mem (full parse,
catch_unwind-wrapped) → op mutates doc.objects → save_pdf() /
save_with_options() (full serialize, always rewrites the file)
```

- `load_pdf` (`ops/load.rs:76`) parses the whole document and calls
  `doc.get_pages()` once just to enforce `max_pages` — a page-tree
  walk on every load.
- `save_pdf` (`ops/load.rs:97`) serializes every object; there is no
  incremental write path in `lopdf` 0.47, so a one-field metadata edit
  costs the same serialize as a full rewrite.
- `PdfEngine` methods (`engine.rs`) take `&[u8]` and return `Vec<u8>`;
  there is no API to hand a parsed `Document` between ops, so chaining
  N ops costs N parses + N serializes.

This full-parse-per-op model is the dominant cost for any pipeline
that runs several ops on one document.

## Where time goes per op class

- **Content-stream rewriting** (watermark, stamp, draw_text, overlay,
  crop_view, ...): `merge_page_content` (`ops/watermark.rs:532`)
  decodes *every* `/Contents` stream of the page — bounded by a 64 MiB
  `MAX_CONTENT_STREAM` decompression-bomb guard — concatenates them
  with the overlay bytes into one new **uncompressed** stream, then
  `doc.compress()` flate-recompresses it (and every other unfiltered
  stream) at save time. The old streams are left orphaned in the
  object graph (`watermark.rs:30-33`). Per-page cost is
  decompress + copy + recompress of all page content.
- **Object remapping** (`merge.rs`): each input document's objects are
  cloned and every `Object::Reference` recursively rewritten
  (`remap_object`, `merge.rs:138`) — O(total objects). `get_pages()`
  is called twice per input (lines 40 and 77).
- **Whole-document passes**: `compress` runs `Document::compress`
  (FlateDecode level 9 on unfiltered streams only where smaller) plus
  an object-stream/xref-stream save (`compress.rs:46-56`);
  `garbage_collect` adds `prune_objects` + `renumber_objects`
  (`garbage_collect.rs:72-100`). These are O(objects) and O(stream
  bytes) respectively, on top of parse+save.
- **Raster** (`rasterize`, `ops/raster.rs:147`): `load_pdf` validates
  limits, then the parsed doc is **dropped** and `pdftoppm` re-parses
  the file from disk — two full parses plus a subprocess. `render()`
  (`render.rs:69-87`) additionally loads once just to resolve the
  first selected page before calling `rasterize`, which loads again.

## Existing mitigations

- `EngineLimits` (`core/engine.rs:119`): `max_input_bytes`,
  `max_pages`, `max_pixels`, `max_outputs`, enforced at load and per-op.
- `catch_unwind` around `Document::load_mem` (`load.rs:78`) — parser
  panics surface as `PdfError::InvalidInput`.
- Shared helpers: `load.rs` is the single input-limit enforcement
  point; `watermark.rs` hosts the shared page/resource/content helpers
  (`media_box`, `ensure_*_resource`, `wrap_page_content`) reused by
  stamp/overlay and friends.
- Byte budgets: 64 MiB decompressed content-stream cap, 64/512 MiB
  per-page/total PNG budgets in `raster.rs:48-50`.

## Optimization opportunities (impact × effort)

1. **Op chaining / document reuse — highest impact, highest effort.**
   Add an internal `Document`-level API (or a batch op) so a pipeline
   pays one parse and one save. Today N ops = N full parse+save
   cycles.
2. **Cache `get_pages()` — small effort, real win.** It walks the
   page tree each call; `merge.rs` calls it twice per input and
   `load_pdf` calls it once before every op does it again. Cache the
   page map on the document (or a wrapper) per load.
3. **Append `/Contents` as an array instead of merging bytes —
   medium effort, big win for draw ops.** PDF already allows
   `/Contents` to be an array of streams; pushing the overlay stream
   id avoids decoding, copying, and recompressing all existing page
   content. Trade-off: draw order is fixed (array order) and slightly
   larger object count.
4. **Scope `doc.compress()` to touched streams — small-medium.** Many
   ops call `doc.compress()` on the whole document though only the
   new/merged streams need it; untouched already-filtered streams are
   skipped by lopdf anyway, but unfiltered untouched streams still pay
   level-9 flate.
5. **Stream dedup — medium.** Identical content streams across pages
   (blank pages, repeated marks) can be deduplicated by content hash
   before save; `lopdf` has no built-in dedup.
6. **Merge fast paths — small.** Reserve `out.objects` capacity,
   compute the page map once per input, and skip `remap_object` for
   scalar-only objects.
7. **Raster: skip the validation re-parse — small.** `rasterize`
   needs only page count + encryption flag; a cheaper page-count probe
   (or letting `pdftoppm` report errors) would remove one full parse
   per render.

## Measuring

There is **no `tests/pdf_perf.rs` yet** — the suite is still
all-correctness (`tests/advanced_ops.rs`, `src/tests.rs`,
`src/crates/pdf-engine/tests/advanced_ops.rs`). Suggested first step:
a `pdf_perf.rs` smoke test that builds `test_utils`-style fixtures at
two scales (e.g. 5 vs. 200 pages), times each op class with
`Instant`, asserts output correctness, and prints a table — cheap
regression signal for the pipeline cost dominating everything else.
For real profiling add `criterion` as a dev-dependency (currently only
`tower` is present) with benches over `load_pdf`+`save_pdf`,
`watermark`, `merge` (N inputs), `compress`, and `garbage_collect`.

## lopdf 0.47 limitations to keep in mind

- **No linearization writer**: `SaveOptions::linearize` is stored but
  never read; `ops/linearize.rs` re-parses output and honestly reports
  `linearized: false`.
- **Save always rewrites** the entire file — no incremental/xref-append
  update, so small edits pay full serialization.
- `Document::compress` only touches *unfiltered* streams; already-
  filtered streams (`/DCTDecode`, `/JPXDecode`, ...) are never
  recompressed — `compress`/`garbage_collect` cannot shrink images.
- Object-stream packing exists only via `save_with_options`
  (`compress`, `garbage_collect`); default `save_pdf` writes classic
  xref tables.
- `Document::objects` is a `BTreeMap<ObjectId, Object>`; merge's
  per-object insert + recursive reference rewrite is O(objects) with
  constant-factor overhead worth shaving before reaching for
  `HashMap` (ordering is load-bearing for deterministic output).
