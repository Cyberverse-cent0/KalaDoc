# Conversion Engine

`src/crates/conversion-engine/` converts documents between formats in
both directions — markup/office formats to PDF, and PDF back out to
text, markdown, and images — through pluggable backends.

## Architecture

```
CLI / MCP / jobs  →  ConversionEngine  →  ConversionBackend(s)
                          │                     │
                          │            ┌────────┴─────────┐
                          │       RustBackend      ExternalBackendSet
                          │       (lopdf, pure     (pandoc, libreoffice,
                          │        Rust, always     poppler — detected
                          │        available)       on PATH at startup)
```

- `ProcessingEngine` impl: `name = "convert"`,
  `kind = EngineKind::Conversion`, operation `EngineOperation::Convert`.
- Dispatch: first backend (in registration order — externals before
  rust) that is `Available` and `supports(from, to)` wins; a
  `BackendFailed`/`InvalidInput` result falls through to the next
  supporting backend. Deterministic failures (`TooLarge`, `Timeout`,
  `Io`) do not retry.
- `EngineRegistry::builtin()` registers all six engines: `convert`,
  `image`, `multimedia`, `pdf`, `pipeline`, and `security`.

## Formats

`Format`: `pdf`, `txt`, `markdown`, `html`, `png`, `jpeg`, `docx`,
`odt` (+ parse aliases `md`, `text`, `jpg`, `odf`, ...).

## Backends

| Backend | Source | Pairs |
|---|---|---|
| `rust` | in-process lopdf + pdf_engine | txt/markdown → pdf; pdf → txt |
| `pandoc` | external (optional LaTeX engine for PDF out) | markup/office → pdf, → txt/html/markdown |
| `libreoffice` | external `soffice` | docx/odt → pdf |
| `poppler` | external `pdftoppm` | pdf → png/jpeg (first page) |

Backends report honest `supports()` matrices — a pair with no backend
fails with `UnsupportedConversion`, never approximated.

## Contracts

- `ConvertRequest { input_format, output_format, input, options }` →
  `ConvertOutput { bytes, output_format, backend, warnings }`.
- Options (`dpi`, `quality`, `page_size`, `params`) are
  backend-interpreted; unhonorable options produce `warnings`, never
  silent ignores.
- External commands run with a 120 s timeout, unique temp dirs, and
  guaranteed cleanup.
- Input cap: 256 MiB (`TooLarge`); empty input → `InvalidInput`.
- Sync CPU/process-bound API — async callers use `spawn_blocking`.
