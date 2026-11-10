# KalaDoc UI Design Proposal

Synthesized from ~40 parallel research agents covering ilovepdf.com,
smallpdf.com, sejda, pdfescape, Adobe Acrobat online, CloudConvert,
DocuSign/DocuSeal, Google Drive/Dropbox/Figma workspace patterns,
design systems (Linear/Notion/Stripe/Geist), accessibility research,
and a full audit of KalaDoc's backend capabilities (`src/crates/*`)
and existing SPA (`ui/src`).

References: ilovepdf.com and smallpdf.com are the primary benchmarks;
Sejda is the power-user benchmark; Linear/Notion are the app-shell
benchmarks.

---

## 1. Product framing

KalaDoc is **not** a marketing-site tool catalog — it already has a
real backend: ~160 PDF ops, ~80 media ops, ~80 video jobs, conversion,
OCR, signing, pipelines, folders, versions, permissions, tags,
schedules, and a durable job queue. The UI should therefore be a
**document workspace** (desktop-app feel) with a **tool layer** on top,
rather than a clone of ilovepdf's per-tool landing pages.

Recommended product shape:

- **Workspace shell** — file manager + viewer + jobs + pipelines
  (what exists today, redesigned).
- **Tool surface** — every engine operation reachable as a guided
  3-step flow (upload → configure → process → result), reusing the
  same components as the workspace.
- **Object-first actions** — "what do you want to do with this file?"
  on any document, mirroring Acrobat Home / iLovePDF mobile.

This gives the ilovepdf/smallpdf familiarity users expect while
exploiting what those sites can't: persistent documents, versions,
pipelines, schedules, and a real job system.

---

## 2. Information architecture

### 2.1 App shell: sidebar + topbar (desktop-app feel)

Replace the current top-nav `Layout.tsx` with a **persistent left
sidebar** (240–280px, collapsible to a 56–64px icon rail), per
Linear/Vercel/Notion conventions:

```
┌──────────────┬────────────────────────────────────────┐
│ ◇ KalaDoc    │  Topbar: breadcrumbs · ⌘K search · bell │
│ [⌘K search]  ├────────────────────────────────────────┤
│              │                                        │
│  ▸ Home      │         main content                   │
│  ▸ Documents │                                        │
│  ▸ Jobs      │                                        │
│  ▸ Tools     │                                        │
│  ▸ Pipelines │                                        │
│  ─────────   │                                        │
│  FOLDERS     │                                        │
│   ▸ …        │                                        │
│  ─────────   │                                        │
│  ▸ Settings  │                                        │
│  ▸ Activity  │                                        │
└──────────────┴────────────────────────────────────────┘
│ status bar (bottom): connection · storage · job counts      │
└────────────────────────────────────────────────────────────┘
```

Sidebar sections:

- **Primary**: Home, Documents, Jobs, Tools, Pipelines, Activity
- **Folders**: collapsible folder tree (backend already supports
  `parent_id` hierarchy via `/folders`)
- **Secondary**: Settings, connection status

Desktop-app signals to include:

- **⌘K / Ctrl+K command palette** (`cmdk` ≥1.0.4) — the single most
  recognizable "pro app" affordance. One command registry feeds the
  palette, context menus, toolbar tooltips, and a `?`/`Ctrl+/`
  shortcut cheat-sheet. Toggle on re-press, restore focus on close,
  show keybinding chips per row.
- **Status bar** at bottom: API connection state, storage usage
  (`GET /storage`), running-job count, active document.
- **Context menus** (right-click / long-press) on documents, pages,
  jobs — same actions as palette/menus (NN/g: never bury actions
  behind right-click only).
- **Keyboard-first**: j/k or arrows to move, x/Space to select,
  Shift+arrows range, Ctrl+A all, Enter open, F2 rename, Delete
  trash, Ctrl+X/C/V cut-copy-paste, `g`-chords for nav jumps
  (g+d documents, g+j jobs). Never fire single-key shortcuts while
  typing in inputs.
- **Persisted layout state**: sidebar collapsed, view mode
  (grid/list), sort, filters, pane widths — desktop apps feel native
  because they remember.

### 2.2 Routes

```
/                  → Home (dashboard: recent files, recent jobs,
                    suggested actions, quick tools)
/documents         → file library (grid/list, folders, search, tags)
/documents/:id     → document workspace (viewer + actions + artifacts
                    + versions + history)
/documents/:id/tools/:op → guided tool flow for that document
/tools             → tool catalog (verb-grouped grid)
/tools/:op         → generic tool page (drop files → configure → run)
/jobs              → job queue (upgraded, see §6)
/jobs/batch/:id    → batch progress
/pipelines         → pipeline list + builder
/pipelines/:name   → pipeline detail / run
/activity          → audit/activity feed (document history + job events)
/schedules         → recurring jobs (interval-based)
/settings          → account, preferences, developer/API keys, security
/login, /connect   → unchanged conceptually
```

Migration notes: `/engines` stays but becomes a diagnostic view under
Settings → System (it's an operator surface, not a user surface).
`/pipelines` run-by-name becomes a real builder.

---

## 3. Home / dashboard

Adopt the **"What would you like to work on?"** intent surface
(Adobe Acrobat Home pattern) rather than a bare file list:

- **Drop zone hero**: "Drop any document — PDF, Office, image, audio,
  video" → sniffs MIME type → routes to relevant actions or the
  matching tool. This is the file-first entry; the tool grid is the
  fallback.
- **Continue where you left off**: recent documents row (thumbnail +
  name + "last opened"), each with contextual action chips derived
  from `document_type` (PDF → Compress/Merge/Sign/OCR…).
- **Recent jobs strip**: last ~5 jobs with status badge and download
  for succeeded ones (CloudConvert recents pattern).
- **Quick tools**: 6–8 popular tools as cards (verb + one-line
  outcome): Merge, Compress, Convert, Organize, Sign, OCR, Edit,
  Watermark. "See all tools" expands the catalog.
- **Storage widget**: `GET /storage` quota bar in the sidebar/status
  bar, not a hero card.

Empty states follow Stripe/Carbon rules: title states what's missing,
one sentence on when data appears, one primary CTA into the upload
flow. Never a blank page.

---

## 4. Documents library

Upgrade `Documents.tsx` from a plain table to a real file manager
(Google Drive / Dropbox patterns):

### Views
- **List** (default — dense scanning) and **Grid** (thumbnails for
  PDFs/images via rendered first page). Toggle persisted per user.
- Preserve sort/filter/selection when switching views.

### Columns / sorting
Name (folders first, optional), Type, Status, Size, Folder, Modified,
Owner. Click-to-sort headers with `aria-sort`. Filter chips
(removable pills) for type/status/folder + free-text `?q=` search —
keep "filter this view" distinct from "search everywhere".

### Selection model (implement first — everything derives from it)
- `Set<id>` selected + anchor + focus. Click = select one,
  Ctrl/Cmd-click = toggle, Shift-click = range, marquee on empty
  space, Space toggles focused item.
- Selection drives a **contextual action bar** (Share/Move/Tag/
  Archive/Trash/Run tool) — the primary discovery path for batch ops.
- `role="grid"`/`listbox` ARIA, `aria-selected`, live-region
  announcements, full keyboard parity (WCAG 2.5.7 requires a
  non-drag path for everything).

### Upload
- Global **drag-and-drop dropzone**: dotted border, icon,
  "Drag files here or browse", helper text stating accepted types +
  size limits *before* upload. Full-viewport overlay when it's the
  only uploader on the page; valid/invalid tint on `dragover`
  (guard with `dataTransfer.types.includes('Files')`).
- Multi-file drops → per-file queue rows with independent states
  (queued → uploading % → processing → done/failed), per-file cancel,
  byte-weighted aggregate bar, "9 of 10 files" count label. Never a
  single spinner for a batch; never fake progress.
- Paste-from-clipboard support (`paste` listener, filter
  `clipboardData.items` for `kind==="file"`), ⌘V hint near the zone.
- Client-side validation first (type/size/count) with specific errors:
  "File too large (140 MB). Maximum is 100 MB" — never generic.
- Upload API needs progress: current `api.upload()` buffers the whole
  response; switch to XHR `upload.onprogress` or fetch streaming.

### Document detail → workspace
`/documents/:id` becomes a **three-pane workspace**:

```
┌──────────┬──────────────────────────────┬─────────────┐
│ thumbs   │   PDF viewer (pdf.js)        │ inspector / │
│ sidebar  │   continuous scroll, zoom,   │ tool options│
│ (pages)  │   find, page jump            │             │
└──────────┴──────────────────────────────┴─────────────┘
```

- **Left**: page-thumbnail sidebar (virtualized, ~126px thumbs,
  canvas→img→free GPU memory). Doubles as the organize-pages surface.
- **Center**: pdf.js viewer — continuous scroll default (fit-width),
  paged/spread modes, Ctrl+scroll zoom with scroll compensation,
  find-in-document searching *extracted text* (not DOM — virtualization
  breaks naive Ctrl+F), page-number input honoring page labels,
  Home/End/PgUp/PgDn.
- **Right**: inspector — metadata fields (not raw JSON), tags,
  versions, permissions, history; and the **tool options panel** when
  a tool is active.
- **Top**: document tabs if multi-doc opening is added later; for now
  breadcrumb + action toolbar (Rename, Move, Tag, Archive, Tools ▾,
  Download, Share).
- Replace `window.prompt`/`confirm` in `DocumentActions` with real
  dialogs (`<dialog>` element — free focus trap + focus return).

Artifact downloads: keep blob approach for small files; add
`expires_at` display ("Expires in 3 days" / live countdown <1h) once
the backend persists artifact deadlines (machinery exists:
`retention_secs` GC, `retention_days` setting — needs an
`expires_at` column).

---

## 5. Tool surface — the core UX

### 5.1 Universal tool-page grammar

Every operation — the ~160 pdf ops, ~80 media ops, conversions,
image ops — uses **one identical skeleton** (iLovePDF/Smallpdf
consensus; consistency is the feature):

```
┌─────────────────────────────────────────────────┐
│ Tool title + one-line outcome                   │
│                                                 │
│ ① Upload zone (drop / browse / cloud import)    │
│    accepted formats + limits stated upfront     │
│                                                 │
│ ② Options panel (appears AFTER files selected — │
│    progressive disclosure, never before)        │
│                                                 │
│ ③ Primary CTA — verb-labeled: "Merge PDF",      │
│    "Compress", "Organize" — never "Submit"      │
│                                                 │
│ ④ Result: per-file status → Download primary,   │
│    cloud-save/share secondary, "run another     │
│    tool" chaining row                           │
└─────────────────────────────────────────────────┘
```

Key rules from research:

- **Options appear only after upload** — the empty state stays calm.
- **Named presets over raw numbers**: compression =
  `Recommended / Extreme / Less` (iLovePDF) or `Basic / Strong`
  (Smallpdf) with one-line consequences ("may soften images; text
  stays crisp"). Middle preset is default. No continuous sliders for
  consumers; DPI shown as named presets (72/150/300) where exposed.
- **Estimate before commit** where feasible (Smallpdf shows expected
  output size per preset — iLovePDF doesn't; it's a differentiator).
- **Pre-flight summary line** above the CTA:
  "3 files · 48 pages → 1 merged PDF" / "Source unchanged · 9 pages ·
  1 new PDF". States the output contract (one file vs N files vs ZIP)
  before processing — the #1 confusion point in competitors.
- **Result screen hierarchy**: Download (primary) → save/share →
  delete-now → related-tools row. Show size delta for compress-type
  ops ("12.4 MB → 3.1 MB, −75%").
- **Tool chaining**: after a result, offer the next logical tools on
  the *output* document — turns one-off visits into sessions.
- **Trust copy near the dropzone**: retention promise, TLS, "no
  signup needed" where true. Specific numbers ("files deleted after
  processing") beat vague claims.

### 5.2 Tool catalog organization

Group by **verb/intent**, not format (Smallpdf/iLovePDF consensus):

| Group | Tools (map to existing ops) |
|---|---|
| **Organize** | Merge, Split, Organize pages, Extract pages, Remove pages, Rotate, Reorder, Crop, N-up, Booklet, Reverse, Interleave |
| **Convert** | PDF→Word/Excel/PPT/JPG/PNG/HTML/MD/TXT, Office/images→PDF, PDF/A, EPUB, URL→PDF (conversion-engine + convert.run) |
| **Edit** | Edit PDF (text/shapes/images via draw_text/draw_shapes), Annotate (comments/freetext/ink), Forms (fill/create), Whiteout/Redact |
| **Optimize** | Compress, Recompress, Garbage collect, Linearize, Repair, Grayscale, Deskew, Clean scan |
| **Security** | Protect (encrypt AES-256), Unlock/decrypt, Permissions report, Sanitize, Redact (terms/regions), Sign, Timestamp (RFC 3161), Flatten |
| **Create/Enhance** | Watermark, Page numbers, Headers/footers, Bates, Stamps, Portfolio, Blank PDF |
| **Text/AI-adjacent** | Extract text, OCR, Searchable PDF, Extract tables/key-values |
| **Media** | Video: trim/convert/GIF/audio-extract/compress/subtitles/thumbnail; Audio: convert/trim/normalize; Image: resize/convert/crop/compress/OCR |
| **Pipelines** | Saved workflows, schedules |

The catalog is driven by `GET /engines/{name}/operations` — render
grouped pickers dynamically, with a curated "popular" ordering on
top. With ~300 ops total, **grouping + search (⌘K) is mandatory**;
never a flat 300-item grid.

### 5.3 Multi-input semantics — be explicit

Three modes users constantly confuse; expose as a **mode selector**
when >1 file is dropped on a non-merge tool:

- **Combine into one** (merge ops)
- **Process each file separately** → N outputs (batch — show
  per-file status, partial success, ZIP download)
- **Apply pipeline** → chained ops

Merge workspace = file queue (drag-reorder rows, page-count/size
previews) with a **Files ⇄ Pages view toggle** (Smallpdf "Show pages"
pattern): file-level ordering by default, page-level thumbnail grid
as progressive disclosure. Per-file rotate, A–Z sort, "add more"
after upload, insert-blank-page affordance between thumbnails
(iLovePDF).

### 5.4 Page-range selection

Reuse the existing `parse_page_range` grammar (`"1-5,8,11-13"`,
`"all"`) — it's already strict and validated:

- Text input with **live resolution**: "3 ranges → 9 pages selected";
  surface out-of-range errors inline (backend rejects rather than
  clamps — show that before submit).
- Quick-fill chips: All / Even / Odd / Reverse — generate the spec
  text, don't extend the grammar.
- Thumbnail picker mode for "select pages" (click-toggle,
  Ctrl/Cmd multi-select, selected-count badge) — selection state must
  survive virtualization.
- Keep the three intents distinct: **Extract** (new file, source
  kept), **Remove pages** (new file minus selection), **Split** (N
  outputs). Never conflate; label outputs honestly.

### 5.5 Specialized flows

- **Compress**: preset radio cards (Recommended default / Extreme /
  Less) with consequence microcopy; per-preset estimated size if
  backend can estimate; post-run size delta + before/after page
  thumbnail comparison (nobody does visual compare — cheap
  differentiator via rendered page images).
- **OCR**: auto-detect image-only PDFs (no text layer) → prompt
  "Looks like a scan — run OCR?"; language dropdown defaulting to
  auto; output choice searchable-PDF vs text/Word; show file/page
  caps upfront.
- **Sign**: fork first — "Sign it myself" vs "Request signatures"
  (backend has sign/verify ops; request-flow needs recipient model —
  phase 2). Self-sign: create signature once (draw/type/upload —
  three equal tabs), then click-to-place + drag/resize on the page
  overlay; auto-fill date at completion.
- **Redact**: highest-risk UX — two selection modes (drag box vs
  select-text), marked-vs-applied visual states, explicit
  "cannot be undone" confirmation, and label true redaction vs
  cosmetic whiteout honestly.
- **Protect/Unlock**: paired password + confirm fields; offer open
  vs permissions password (engine supports both).
- **Watermark/Page numbers/Headers**: options panel with position
  grid or presets, font/size/opacity, page-range selector,
  layer (over/under) where supported; `{page}`/`{total}` placeholder
  syntax for headers (Sejda pattern).
- **Organize pages**: thumbnail grid with hover actions
  (rotate/delete/duplicate), drag-reorder (dnd-kit), insert-blank
  gap affordance, multi-file color framing, undo/redo (Sejda),
  magnifier hover for dense grids, "Apply changes" commit step.
  Mobile: dedicated organize mode — tap-select + action bar +
  "Move to position" dialog (drag is not accessible on touch).
- **Convert**: direction-specific pages (PDF→Word etc.) with a
  "Convert to" format picker; OCR checkbox appears contextually for
  scanned inputs; per-format options panel (DPI, page range,
  margins) revealed progressively.
- **Multimedia**: same 3-step grammar; presets by intent
  ("Smaller file / Balanced / Best quality"); GIF flow = time-range
  picker + width/fps with inline size guidance; video→audio treated
  as a format pair, not a separate product; probe via
  `POST /media/probe` to show duration/streams/resolution before
  committing; thumbnail via `/media/thumbnail` for preview.

### 5.6 Jobs & batch UX

Upgrade `Jobs.tsx` into a real queue UI:

- Per-row state chips: queued → uploading → processing → done /
  failed / cancelled / retrying, with phase labels ("Audio received;
  waiting to encode") — honest status vocabulary, no fake progress.
- Progress: `job.progress` JSON exists — render determinate bars
  where the backend reports them, indeterminate + label otherwise.
- Batch: `POST /jobs/batch` + `GET /jobs/batch/{id}` already exist —
  show aggregate bar (byte-weighted), per-file rows, retry-failed-
  only, cancel-all, "N succeeded / M failed" summary.
- Cancel semantics honest: "Stop now" vs "stop after this step";
  show "Cancel requested" intermediate state.
- Retry only failed; show attempts/max_attempts (already in schema).
- Live updates: SSE if added, else keep conditional 3s polling —
  status must survive page reload (job IDs are durable).
- Job → document linkage: render `payload.document_id` as a link to
  the document workspace instead of raw JSON tooltips.
- Schedules page: interval-based list ("every N seconds" — no cron),
  enable/pause, run-now via `/schedules/tick`.

---

### 5.7 Error & edge-case contract (backend-informed)

Research across iLovePDF/Smallpdf/CloudConvert plus an audit of
KalaDoc's job handlers surfaced concrete gaps the UI cannot fix
alone — the design assumes this backend contract:

- **Structured per-op outcomes**: extend `pdf_ops.*`/`media_ops.*`/
  `convert.*` metadata entries with `code`, `retryable`, `action`,
  `details` alongside the existing `status`/`error`. Suggested codes:
  `encrypted_pdf`, `password_required`, `password_incorrect`
  (retryable-with-new-password), `malformed_pdf`, `file_too_large`,
  `page_limit_exceeded`, `unsupported_format`, `missing_tool`,
  `backend_unavailable`, `content_type_mismatch`, `partial_failure`.
- **Password states are first-class**: encrypted input to non-decrypt
  ops → `password_required`/`encrypted_pdf` with action "run decrypt
  first"; wrong decrypt password → `password_incorrect`. UI renders a
  password field scoped to that file, never echoes passwords.
- **Honest batch semantics**: today deterministic op failures are
  recorded in document metadata while the *job* still reports
  `succeeded` — `/jobs/batch` can show "50 succeeded" when 4 files
  actually failed. Batch aggregates must count operation outcomes,
  not just job status; return per-item rows (file, op, code, message,
  retryable, action, output artifact ids) and preserve successful
  outputs.
- **Repair emits a real artifact**: `RepairReport.bytes` currently
  serializes into a `*-result.json` instead of a downloadable PDF —
  emit it as a normal output artifact plus a warnings report.
- **Sniff conversion inputs**: `convert.run` trusts declared
  `from_format`; compare against detected bytes and return
  `content_type_mismatch` with both formats.
- **Warnings channel**: `warnings[]` separate from errors — repair
  normalized rewrites, conversion fallback backends, partial OCR,
  skipped outputs. Visible but non-fatal.
- **Upload cap alignment**: body reader caps at `max_upload_bytes`
  (100 MiB) before sniffing, so legitimate videos under the 2 GiB
  video cap get a misleading `payload_too_large` — stream or raise
  the initial cap and let `validate_upload` enforce per-format limits.
- **`storage.scan_inbox`**: record per-file failures in job `progress`
  (currently log-only).

UI rendering contract for all of the above: every failure shows
*what happened* (code + message), *why* (details), *what to do*
(action — e.g. "enter password", "retry", "run repair first"), and
*retryability*. Errors are inline per-file, never batch-fatal;
successful siblings still offer downloads.

## 6. Pipelines

`/pipelines` evolves from a run-by-name list into a **linear builder**
(iLovePDF Workflows / Acrobat Action Wizard pattern — keep it linear,
not a node graph):

- Ordered step cards: `{engine, operation, params}` with per-step
  options; compatibility hints from the documented
  `(engine, op)` matrix in `pipeline-engine/src/model.rs`.
- Save named pipelines (backend currently accepts inline definitions
  only — needs a `pipelines` store or reuse schedules).
- Run against document(s): mode = each-file vs merged vs selection.
- DAG definitions (`DagDefinition` exists) stay an advanced/API
  surface — expose later behind "advanced".
- Built-ins (`pdf.intake`, `pdf.extract-text`) ship as curated
  starting templates.

---

## 7. Design system

Adopt tokens (Tailwind v4 `@theme` + CSS vars, light + dark from one
source — current UI is hardcoded dark `slate-*`):

- **Color**: one neutral family (slate — matches existing) + **one
  accent**. Keep `sky`→ shift to a Linear-style indigo `#5E6AD2` or
  keep sky-600; either way, accent = interactive only, never surface
  fills. Semantic status colors separate: success/warn/danger/info.
- **Dark theme** (current): near-black warm-neutral `#0A0A0B`-ish,
  elevation by lightness not shadow, borders at ~8% white, muted text
  bumped to `slate-400` minimum (current `slate-500` fails AA).
- **Light theme**: warm paper `#FAFAF9`-ish canvas, white cards,
  hairline borders — the document-tool audience expects light mode;
  default to `prefers-color-scheme` with persisted toggle.
- **Type**: Inter Variable (`@fontsource-variable/inter`), scale
  11–14px dense UI, 18–24px headings, `tabular-nums` on all
  sizes/dates/IDs, mono only for IDs/hashes.
- **Spacing**: 4px base; card padding 16–24px; list rows ~36px.
- **Components**: shadcn/ui-style owned components (Radix primitives
  under Tailwind) — Dialog, DropdownMenu, ContextMenu, Tooltip, Tabs,
  Popover, Command (cmdk). Plus `lucide-react` icons (16/20px, one
  stroke weight). No MUI — too heavy.
- **Libraries to add**: `cmdk` (palette), `@dnd-kit/react` or classic
  v6 (sortable lists/grids — react-beautiful-dnd is dead),
  `@tanstack/react-virtual` (`useFlushSync: false` on React 19),
  `react-pdf` v11 (React 19 peer, pdfjs-dist ~5MB — lazy-load the
  viewer chunk) or pdfjs-dist directly, `@fontsource-variable/inter`.

---

## 8. Cross-cutting patterns

- **Auth**: keep session-token login; guest/anonymous mode is a
  product decision — backend currently requires auth for everything,
  so guest mode = backend work, not just UI. Magic-link + OAuth are
  phase-2. Session/device management page is a differentiator (none
  of the competitors have it; `sessions_tbl` already supports revoke).
- **Activity feed**: `/documents/:id/history` + job events →
  object-centric activity sentences with thumbnails (Drive pattern).
  Cheap to build — data exists.
- **Notifications**: bell only if a real event source exists —
  job failed/succeeded transitions qualify; badge = unread capped
  `9+`, panel with mark-all-read.
- **Settings restructure**: sections — Profile / Preferences
  (defaults, density, theme) / Developer (API keys list with scopes
  + revoke — `KALADOC_API_KEYS` exists but needs UI management) /
  Security (change password, sessions, 2FA later) / System
  (engines health, storage).
- **Empty/loading/error discipline**: skeletons for lists, spinner
  only for indeterminate waits, distinct visuals for empty vs
  no-results vs error (never conflated).
- **Accessibility floor**: skip-link, focus-to-h1 on route change,
  `aria-live` announcer for uploads/jobs/drag-moves, 4.5:1 contrast,
  44px touch targets under `pointer: coarse`, `prefers-reduced-motion`
  respected, keyboard parity for every drag action, axe-core smoke
  tests.
- **Mobile**: 3-tier breakpoints (<600 compact / 600–1023 medium /
  ≥1024 expanded per EmbedPDF). Bottom tab bar on compact (iLovePDF's
  proven fix), side panels → bottom sheets, tool flows → dedicated
  full-screen modes, Jobs page is the natural "results inbox" since
  processing is already async server-side.

---

## 9. Phased implementation plan

**Phase 1 — shell & foundations**
- Design tokens (light+dark), Inter, icon set, shadcn-style
  primitives (Dialog/Popover/Tooltip/Tabs/Command).
- New sidebar Layout + status bar + ⌘K palette scaffold + shortcut
  registry + `?` cheat sheet.
- Documents library: grid/list, selection model, filter chips,
  multi-file drag-drop upload with per-file progress.

**Phase 2 — viewer & document workspace**
- pdf.js viewer pane (continuous scroll, thumbnails sidebar, zoom,
  find, page jump), inspector panel, real dialogs (replace prompts),
  versions/tags/permissions/history UI, artifact expiry display.

**Phase 3 — tool framework + top tools**
- Generic `/tools/:op` 3-step shell driven by `/engines/{name}/operations`.
- Curated tool set: Merge, Split, Organize pages, Compress,
  Convert (PDF↔Office/JPG), Rotate, Watermark, Page numbers,
  Protect/Unlock, Extract text, OCR.
- Organize-pages thumbnail editor (dnd-kit, Files⇄Pages toggle).
- Jobs/batch UI upgrade; pipeline linear builder.

**Phase 4 — depth**
- Sign flow, Redact, Bates/headers, N-up/booklet, image ops,
  multimedia ops, schedules UI, activity feed, notifications,
  settings sections, mobile adaptations, a11y audit.
- **Backend error contract (§5.7)**: structured op outcomes,
  honest batch aggregates, password states, repair artifacts,
  conversion sniffing, warnings channel, upload-cap alignment —
  schedule alongside or before the tool surfaces that depend on them.

---

## 10. Key risks / open questions

- **Auth model**: guest mode needs backend support; decide whether
  anonymous processing is in scope.
- **Pipeline persistence**: backend accepts inline definitions only;
  saved pipelines need storage.
- **Op metadata**: `/engines/{name}/operations` returns capabilities —
  verify it carries enough param schema for auto-generated options
  panels; otherwise curate param forms per tool.
- **pdf.js bundle**: ~5MB engine — code-split the viewer; consider
  server-rendered thumbnails (`thumbnail.generate` job already
  exists) for the library grid instead of client-side rendering.
- **Image/multimedia engines degraded on the deploy target**
  (tesseract/ffmpeg missing per health check) — UI should surface
  engine health and disable ops whose engine is `unavailable`.
