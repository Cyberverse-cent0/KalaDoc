# Stirling-PDF Advanced Features — Target Documentation

Reference inventory of Stirling-PDF's advanced capabilities, captured for
archival by Applaicon and used as the feature-parity target for KalaDoc.

> **How to read this document.** This is a feature *reference*, not an
> implementation requirement. The goal is to establish reliable document
> primitives first, then build higher-level engines on top of them — not to
> map every Stirling endpoint one-to-one.

## 1. Advanced PDF editing

- Edit existing PDF text.
- Edit or replace images inside PDFs.
- Add text at arbitrary page coordinates.
- Add images, logos and watermarks.
- Draw shapes and freehand annotations.
- Highlight, underline and strike through text.
- Add comments and sticky notes.
- Edit document metadata.
- Edit bookmarks and table of contents.
- Edit interactive PDF forms.
- Fill existing forms.
- Detect and create form fields automatically.
- Flatten interactive forms.
- Unlock read-only form fields.
- Remove annotations.
- Remove selected images.
- Compare two PDF documents.
- Find differences between documents.

## 2. Advanced page manipulation

- Merge multiple PDFs.
- Split PDFs by page ranges.
- Split PDFs into individual pages.
- Split by page count.
- Split by bookmarks.
- Extract selected pages.
- Delete selected pages.
- Remove blank pages automatically.
- Detect page orientation and auto-rotate.
- Rotate individual pages.
- Rearrange pages using drag and drop.
- Duplicate pages.
- Insert blank pages.
- Crop pages manually.
- Resize pages.
- Scale page content.
- Add page numbers.
- Add headers and footers.
- Multi-page layouts (N-up).
- Booklet imposition.
- Poster printing / tile splitting.
- Convert PDF into a single large page.
- Overlay multiple PDFs.
- Interleave pages from multiple documents.

## 3. Document conversion engine

### To PDF

- DOC / DOCX → PDF
- XLS / XLSX → PDF
- PPT / PPTX → PDF
- ODT / ODS / ODP → PDF
- HTML → PDF
- Markdown → PDF
- TXT → PDF
- Images → PDF
- EPUB → PDF
- Email (EML) → PDF
- URL → PDF
- HTML files → PDF
- CBZ / CBR → PDF

### From PDF

- PDF → Word
- PDF → Excel
- PDF → PowerPoint
- PDF → CSV
- PDF → HTML
- PDF → Markdown
- PDF → XML
- PDF → JSON-oriented extraction workflows
- PDF → Images
- PDF → Plain text
- PDF → EPUB
- PDF → PDF/A
- PDF → PDF/UA
- PDF → RTF
- PDF → CBZ / CBR
- PDF → Single-page images

> Actual format availability depends on the installed conversion engines.

## 4. OCR and intelligent document processing

- OCR scanned documents.
- Make scanned PDFs searchable.
- Extract text from images.
- Support multiple OCR languages.
- Automatically rotate scanned pages.
- Deskew scanned documents.
- Remove scanning noise.
- Clean scanned backgrounds.
- Detect text orientation.
- Extract images from PDFs.
- Extract photographs from scanned pages.
- Detect and split multiple photographs.
- Automatically rename PDFs based on document content.
- Detect document structure.
- Extract document information.
- Automatic form detection.
- Document classification and processing workflows.

## 5. Advanced security and digital signatures

- Password-protect PDFs.
- Remove PDF passwords.
- Change PDF permissions.
- Restrict printing.
- Restrict copying.
- Restrict editing.
- Apply digital certificate signatures.
- Sign using PEM certificates.
- Sign using PKCS#12/P12 certificates.
- Validate digital signatures.
- Verify certificate chains.
- Remove certificate signatures.
- Add timestamp signatures.
- RFC 3161 timestamping.
- Permanently redact sensitive content.
- Redact selected text.
- Redact selected regions.
- Redact entire pages.
- Sanitize PDF documents.
- Remove embedded JavaScript.
- Remove potentially dangerous embedded files.
- Flatten forms and annotations.
- Validate PDF compliance.

> **Important:** Redaction must remove the underlying content, not merely
> draw a black rectangle over it.

## 6. Compression and optimization

- Compress PDF files.
- Optimize embedded images.
- Reduce image resolution.
- Adjust JPEG quality.
- Remove unnecessary objects.
- Remove unused resources.
- Optimize PDF structure.
- Reduce redundant content.
- Linearize PDFs for faster web viewing.
- Convert images to more efficient formats.
- Optimize documents for printing.
- Optimize documents for archival storage.

## 7. Advanced visual processing

- Adjust brightness.
- Adjust contrast.
- Adjust saturation.
- Adjust RGB channels independently.
- Invert document colors.
- Replace foreground and background colors.
- Create high-contrast documents.
- Convert colors to CMYK.
- Apply scanner effects.
- Simulate photocopy effects.
- Apply grayscale.
- Repair damaged PDFs.
- Overlay documents with foreground/background control.

## 8. Attachments and embedded document management

- Add attachments to PDFs.
- Extract embedded attachments.
- List attachments.
- Remove attachments.
- Extract individual attachments.
- Process multiple attachments in batches.
- Create PDF portfolios.
- Manage embedded files.
- Inspect document metadata.
- Inspect PDF fonts.
- Inspect page dimensions.
- Inspect permissions and document properties.
- Inspect embedded JavaScript.

## 9. Automation and workflow processing

- Build reusable processing workflows.
- Chain multiple PDF operations.
- Save automation pipelines.
- Run workflows repeatedly.
- Process multiple documents.
- Batch-process files.
- Automate document conversion.
- Automate OCR.
- Automate compression.
- Automate document renaming.
- Process files through APIs.
- Automate folder scanning.
- Watch folders for new documents.
- Automatically process incoming files.
- Route processed documents into output folders.
- Configure pipeline processing stages.

## 10. Developer and integration features

- REST API.
- API documentation.
- API authentication.
- API endpoint configuration.
- OpenAPI integration.
- Batch processing through API.
- Automated folder scanning.
- MCP integration for AI agents.
- Workflow automation APIs.
- External engine integration.
- Configurable feature flags.
- Custom application configuration.
- Health and operational endpoints.

## 11. Enterprise and administration

- User management.
- Authentication.
- Role-based access control.
- Single Sign-On (SSO).
- OAuth/OIDC integrations.
- LDAP integrations where supported.
- Audit logging.
- API key management.
- Feature-level permissions.
- Endpoint enable/disable controls.
- User-specific settings.
- Server configuration.
- Custom branding.
- Multi-language interface.
- Docker deployment.
- Kubernetes deployment.
- Desktop deployment.
- Air-gapped deployment.
- On-premises hosting.

> Some administration and enterprise capabilities depend on the Stirling
> edition and deployment configuration.

## 12. AI and intelligent features

- AI assistant.
- Document ingestion.
- Document parsing.
- Document classification.
- AI-assisted document workflows.
- MCP-based AI tool invocation.
- Automated document processing.
- Structured document extraction.

> These should be distinguished from traditional PDF-engine operations:
> some require separate models, services or integrations.

## Target engine mapping for KalaDoc

For the Rust implementation, organize functionality into independent
engines rather than isolated endpoints:

| Engine | Responsibilities |
|---|---|
| Document Engine | PDF parsing, creation, modification |
| Page Engine | Merge, split, rotate, crop, rearrange |
| Conversion Engine | Office, HTML, images, EPUB, PDF/A |
| OCR Engine | Text recognition, cleanup, extraction |
| Security Engine | Encryption, signatures, redaction, sanitization |
| Form Engine | Form detection, filling, editing, flattening |
| Image Engine | Image extraction, compression, processing |
| Optimization Engine | Compression, repair, optimization |
| Metadata Engine | Metadata, bookmarks, document information |
| Comparison Engine | Text and visual comparison |
| Workflow Engine | Pipelines, batch jobs, automation |
| Ingestion Engine | Document ingestion, parsing, classification |

## Features that would make the implementation particularly powerful

Beyond basic Stirling-PDF parity:

- **Streaming document processing** — process large PDFs without loading
  entire documents into RAM.
- **Incremental processing** — process pages independently where
  operations permit.
- **Plugin-based conversion engines** — dynamically discover external
  tools.
- **Transactional document operations** — avoid corrupting source files
  when operations fail.
- **Document versioning** — retain previous versions and operation
  history.
- **Job orchestration** — asynchronous jobs, cancellation, retries and
  progress reporting.
- **Workflow DAGs** — conditional branches, parallel operations and
  dependencies.
- **Content-addressed storage** — deduplicate identical documents.
- **Document fingerprints** — detect duplicate or modified documents.
- **Structured extraction** — extract tables, key-value pairs,
  metadata and document sections.
- **Resource limits** — memory, CPU, processing time and file-size
  limits.
- **Sandboxed external engines** — isolate Ghostscript, LibreOffice and
  other conversion tools.
- **Content validation** — validate generated PDFs after every critical
  operation.
- **Event-driven processing** — publish job events and processing status.
- **Resumable jobs** — recover interrupted processing without
  unnecessarily restarting everything.

## Official references

- Stirling-PDF tool catalogue: <https://docs.stirlingpdf.com/Functionality/Recommended-Tools/>
- Stirling-PDF advanced tools: <https://docs.stirlingpdf.com/Functionality/Advanced-Tools/>
- Feature flags and endpoint configuration: <https://docs.stirlingpdf.com/Server/Customisation/Features-and-Endpoints/>
- Stirling-PDF GitHub repository: <https://github.com/Stirling-Tools/Stirling-PDF>
