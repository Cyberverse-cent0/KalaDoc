# KalaDoc CLI & MCP Interface Specification

**Status:** Proposed implementation specification\
**Scope:** Command-line interface (CLI), Model Context Protocol (MCP)
server, local automation, and secure remote access\
**Project:** KalaDoc\
**Design principle:** A powerful, scriptable document platform with
explicit permissions, predictable output, and safe-by-default execution.

------------------------------------------------------------------------

## 1. Purpose and product direction

KalaDoc should expose the same document-processing capabilities through
three complementary interfaces:

1.  **HTTP API** --- the stable application interface for web and
    desktop clients.
2.  **CLI** --- an ergonomic interface for humans, shell scripts,
    administrators, and automation.
3.  **MCP server** --- a structured tool interface for AI assistants and
    MCP-compatible clients.

These interfaces should call shared application services. They must not
each implement their own PDF logic, authorization rules, storage access,
or job orchestration.

``` text
                  Web / Desktop UI
                         |
CLI ---------------- HTTP/API ---------------- MCP Server
                         |
                Application Services
                         |
        +----------------+----------------+
        |                |                |
    Job Engine       AuthZ Engine     Engine Registry
        |                |                |
        +----------------+----------------+
                         |
                Storage Abstraction
                         |
            Local / Network / Object Store
                         |
                Processing Engines
```

**Critical rule:** CLI and MCP are adapters. The application service
layer owns business rules. All entry points must enforce the same
authentication, authorization, validation, quotas, audit logging, and
resource limits.

## 2. Design principles

-   **One capability, one application service:** CLI, HTTP, and MCP
    invoke the same service operation.
-   **Safe by default:** destructive operations require explicit intent;
    overwrite is never implicit.
-   **Scriptable:** stable exit codes, machine-readable JSON,
    deterministic output, and no progress noise on stdout in JSON mode.
-   **Human-friendly:** concise tables, progress indicators, useful
    errors, and shell completion.
-   **MCP-native:** expose narrowly scoped, typed tools rather than a
    generic shell or unrestricted filesystem tool.
-   **Asynchronous by design:** expensive processing returns a job
    identifier and can be monitored, cancelled, or retrieved.
-   **Permission-aware:** every operation runs as an authenticated
    principal and is authorized against the target resource.
-   **Portable:** avoid shell-specific behavior in the Rust
    implementation; support Linux, macOS, and Windows.
-   **Observable:** operations carry request IDs, job IDs, structured
    logs, metrics, and audit events.
-   **Versioned contracts:** CLI output schemas and MCP tool
    inputs/outputs are documented and tested.

## 3. CLI product experience

Binary name: `kaladoc`

Recommended global options:

``` text
kaladoc [GLOBAL OPTIONS] <COMMAND>

Global options:
  --config <PATH>          Use a specific configuration file
  --profile <NAME>         Select a named profile
  --server <URL>           Use a remote KalaDoc server
  --output <FORMAT>        table | json | yaml
  --quiet                  Suppress non-essential output
  --verbose                Increase diagnostic output
  --no-color               Disable terminal colors
  --request-timeout <DUR>  Set request timeout
  --version                Print version and build information
  --help                   Print help
```

Recommended configuration locations:

-   Linux: `$XDG_CONFIG_HOME/kaladoc/config.toml` or
    `~/.config/kaladoc/config.toml`
-   macOS: `~/Library/Application Support/KalaDoc/config.toml`
-   Windows: `%APPDATA%\\KalaDoc\\config.toml`

Profiles should support local and remote servers, but secrets should not
be stored as plaintext in configuration files. Prefer OS credential
stores where available, or an explicitly permission-restricted secret
file as a documented fallback.

## 4. Proposed command tree

The command tree is intentionally broad. Implement it in phases; do not
create fake commands or placeholder success responses.

``` text
kaladoc
├── version
├── doctor
├── init
├── config
│   ├── show
│   ├── validate
│   ├── get
│   ├── set
│   └── profile
│       ├── list
│       ├── add
│       ├── use
│       └── remove
├── server
│   ├── start
│   ├── stop
│   ├── status
│   ├── info
│   └── logs
├── auth
│   ├── login
│   ├── logout
│   ├── status
│   ├── whoami
│   ├── token
│   │   ├── create
│   │   ├── list
│   │   ├── revoke
│   │   └── rotate
│   ├── session
│   │   ├── list
│   │   └── revoke
│   ├── mfa
│   │   ├── setup
│   │   ├── verify
│   │   └── disable
│   └── provider
│       ├── list
│       └── status
├── user
│   ├── create
│   ├── list
│   ├── get
│   ├── update
│   ├── disable
│   └── delete
├── organization
│   ├── create
│   ├── list
│   ├── get
│   ├── update
│   ├── members
│   └── delete
├── workspace
│   ├── create
│   ├── list
│   ├── get
│   ├── update
│   ├── members
│   ├── share
│   └── delete
├── storage
│   ├── status
│   ├── backend
│   │   ├── list
│   │   ├── inspect
│   │   └── test
│   ├── quota
│   │   ├── show
│   │   └── set
│   ├── usage
│   ├── verify
│   ├── cleanup
│   └── migrate
├── document
│   ├── upload
│   ├── download
│   ├── list
│   ├── get
│   ├── inspect
│   ├── rename
│   ├── move
│   ├── copy
│   ├── delete
│   ├── restore
│   ├── versions
│   ├── share
│   ├── unshare
│   ├── permissions
│   ├── checksum
│   └── verify
├── folder
│   ├── create
│   ├── list
│   ├── get
│   ├── move
│   ├── rename
│   ├── delete
│   └── permissions
├── pdf
│   ├── info
│   ├── validate
│   ├── merge
│   ├── split
│   ├── extract-pages
│   ├── remove-pages
│   ├── rotate
│   ├── reorder
│   ├── crop
│   ├── resize
│   ├── compress
│   ├── repair
│   ├── flatten
│   ├── watermark
│   ├── stamp
│   ├── encrypt
│   ├── decrypt
│   ├── redact
│   ├── sanitize
│   ├── sign
│   ├── verify-signature
│   ├── extract-images
│   ├── extract-text
│   ├── extract-metadata
│   ├── compare
│   ├── page-count
│   └── render
├── convert
│   ├── to-pdf
│   ├── from-pdf
│   ├── image
│   ├── office
│   ├── html
│   ├── markdown
│   ├── text
│   ├── csv
│   ├── xlsx
│   ├── epub
│   └── pdfa
├── image
│   ├── info
│   ├── convert
│   ├── resize
│   ├── compress
│   ├── rotate
│   ├── optimize
│   └── inspect
├── ocr
│   ├── run
│   ├── languages
│   └── inspect
├── pipeline
│   ├── validate
│   ├── run
│   ├── inspect
│   ├── list
│   └── cancel
├── job
│   ├── list
│   ├── get
│   ├── wait
│   ├── logs
│   ├── cancel
│   ├── retry
│   └── result
├── engine
│   ├── list
│   ├── inspect
│   ├── health
│   ├── capabilities
│   └── diagnose
├── audit
│   ├── events
│   ├── export
│   └── verify
├── backup
│   ├── create
│   ├── list
│   ├── inspect
│   ├── restore
│   └── verify
├── benchmark
│   ├── list
│   ├── run
│   └── compare
├── completion
└── mcp
    ├── serve
    ├── inspect
    └── tools
```

The command tree is a product target, not a requirement to implement
every branch in the first release.

## 5. CLI behavior and conventions

### 5.1 Input and output

-   Accept file paths, document IDs, and stdin where meaningful.
-   Support `-` for stdin/stdout only for operations where streaming is
    safe and unambiguous.
-   Never write binary PDF output to stdout when human-readable status
    output is also enabled.
-   Prefer `--output-file` for generated artifacts.
-   Require `--overwrite` before replacing an existing file.
-   Support `--dry-run` for operations that can provide a meaningful
    execution plan.
-   Support `--wait` for users who want synchronous CLI behavior while
    the server still executes a job.
-   Support `--timeout` independently from server-side job deadlines.

Example:

``` bash
kaladoc pdf merge \
  --input ./a.pdf \
  --input ./b.pdf \
  --output-file ./merged.pdf \
  --wait
```

For repeatable scripts:

``` bash
kaladoc --output json pdf info ./document.pdf
```

JSON mode must emit a documented JSON object to stdout. Progress and
diagnostics go to stderr. Do not change JSON field names casually
between releases.

### 5.2 Exit codes

Use stable, documented exit codes:

    Code Meaning
  ------ ------------------------------------
       0 Success
       1 General/unclassified failure
       2 Invalid arguments or configuration
       3 Authentication required or failed
       4 Permission denied
       5 Resource not found
       6 Conflict or unsafe overwrite
       7 Validation failure
       8 Processing/engine failure
       9 Timeout
      10 Job cancelled
      11 Resource/quota limit exceeded
      12 Remote service unavailable
      13 Unsupported operation or format

Do not leak secrets, local credentials, internal paths, or stack traces
in normal user-facing errors. Detailed diagnostics may be available
through an explicit debug mode with sensitive values redacted.

### 5.3 Error format

A machine-readable error should have a stable structure:

``` json
{
  "error": {
    "code": "permission_denied",
    "message": "You do not have permission to download this document.",
    "request_id": "req_...",
    "retryable": false
  }
}
```

## 6. Authentication and account commands

The CLI should support:

``` bash
kaladoc auth login --server https://docs.example.test
kaladoc auth status
kaladoc auth whoami
kaladoc auth logout

kaladoc auth token create \
  --name "backup-script" \
  --scope document:read,document:upload

kaladoc auth token list
kaladoc auth token revoke <token-id>
```

Requirements:

-   Interactive login should avoid echoing passwords.
-   Support device authorization flow when an interactive browser login
    is appropriate.
-   Never put passwords or bearer tokens in shell history or process
    arguments.
-   Prefer prompting, stdin with explicit safeguards, or OS credential
    storage.
-   Tokens must be scoped, revocable, expirable, and attributable to a
    user or service identity.
-   Display a newly created token only once.
-   Do not allow an API token to silently inherit administrator
    privileges.
-   Require elevated authorization for user, organization, role, and
    provider administration.
-   Audit login failures, token creation/revocation, and privilege
    changes.

## 7. Storage and document commands

### 7.1 Storage inspection

``` bash
kaladoc storage status
kaladoc storage backend list
kaladoc storage backend inspect <backend-id>
kaladoc storage usage --workspace <workspace-id>
kaladoc storage quota show --workspace <workspace-id>
kaladoc storage verify --scope workspace
```

### 7.2 Document operations

``` bash
kaladoc document upload ./report.pdf --workspace <workspace-id>
kaladoc document list --workspace <workspace-id>
kaladoc document inspect <document-id>
kaladoc document download <document-id> --output-file ./report.pdf
kaladoc document versions <document-id>
kaladoc document checksum <document-id>
kaladoc document share <document-id> --permission read --expires-in 2h
kaladoc document unshare <share-id>
```

The CLI should distinguish:

-   A local filesystem path.
-   A KalaDoc document ID.
-   A storage backend ID.
-   A workspace ID.
-   A share ID.

Never accept a user-supplied document path as proof of authorization.
Resolve document IDs through the authenticated application service and
enforce resource-level permissions there.

### 7.3 External storage providers

Potential provider support:

-   Local filesystem.
-   SMB/CIFS.
-   NFS.
-   S3-compatible storage.
-   MinIO.
-   Other object-storage providers through explicit adapters.

Provider commands should manage configuration references and connection
tests, not print stored credentials.

``` bash
kaladoc storage backend list
kaladoc storage backend inspect <backend-id>
kaladoc storage backend test <backend-id>
```

Do not make the CLI responsible for implementing SMB, NFS, or S3
protocols. Provider adapters belong in the storage subsystem.

## 8. PDF and conversion command design

Use predictable operation syntax:

``` bash
kaladoc pdf merge --input a.pdf --input b.pdf --output-file merged.pdf

kaladoc pdf split input.pdf \
  --mode fixed-size \
  --pages-per-file 10 \
  --output-dir ./split/

kaladoc pdf rotate input.pdf \
  --pages all \
  --degrees 90 \
  --output-file rotated.pdf

kaladoc pdf compress input.pdf \
  --preset balanced \
  --output-file compressed.pdf

kaladoc pdf extract-pages input.pdf \
  --pages 1-3,8,10-12 \
  --output-file selected.pdf

kaladoc pdf watermark input.pdf \
  --text "CONFIDENTIAL" \
  --position center \
  --output-file marked.pdf

kaladoc pdf validate input.pdf
kaladoc pdf info input.pdf
```

Design conventions:

-   Use explicit units where ambiguity is possible.
-   Document page numbering (recommend 1-based CLI page numbers).
-   Define range syntax once and reuse it.
-   Reject invalid page ranges before starting expensive work.
-   Keep operation options typed and validated.
-   Never silently downgrade encryption, signature, or validation
    behavior.
-   Make unsupported engine capabilities explicit.

Conversion commands should expose format-specific options without
turning the CLI into an unstructured bag of flags. Use subcommands and
documented option groups.

## 9. Jobs and pipelines

Expensive tasks should return a job ID:

``` bash
kaladoc pdf compress large.pdf --output-file compact.pdf
```

Example response:

``` text
Job: job_01...
Status: queued
Use `kaladoc job get job_01...` to inspect progress.
```

Job commands:

``` bash
kaladoc job list --status running
kaladoc job get <job-id>
kaladoc job wait <job-id> --timeout 10m
kaladoc job logs <job-id>
kaladoc job cancel <job-id>
kaladoc job result <job-id> --output-dir ./results/
```

Pipeline example:

``` bash
kaladoc pipeline run \
  --file ./incoming.pdf \
  --step validate \
  --step ocr \
  --step compress \
  --step watermark \
  --output-file ./processed.pdf
```

Do not treat arbitrary shell strings as pipeline steps. Define a typed
pipeline schema, validate it before execution, and enforce permissions
for every step.

Pipelines should eventually support:

-   Typed input/output artifacts.
-   Dependency graphs.
-   Parallel independent steps.
-   Resource estimates and limits.
-   Per-step progress.
-   Cancellation propagation.
-   Failure policies.
-   Reproducible execution metadata.

## 10. MCP server design

MCP should expose structured application capabilities to AI clients. It
should not be a generic terminal, SQL, or filesystem bridge.

Recommended command:

``` bash
kaladoc mcp serve --transport stdio
```

Initial transports:

1.  **stdio** --- best starting point for local MCP clients.
2.  **Streamable HTTP** --- for authenticated remote deployments, after
    transport security and session handling are designed.

Do not expose an unauthenticated network MCP listener by default. Bind
local development listeners to loopback unless the operator explicitly
configures a secured remote deployment.

### 10.1 MCP tool design rules

-   One tool should perform one coherent application action.
-   Tool names should be stable, descriptive, and namespaced.
-   Inputs must use JSON Schema with clear constraints and descriptions.
-   Return structured content in addition to concise human-readable text
    where supported.
-   Return job IDs for long-running operations rather than keeping the
    tool call open indefinitely.
-   Include document IDs, job IDs, request IDs, and relevant status
    fields.
-   Enforce the authenticated principal's permissions on every
    invocation.
-   Treat all document text and metadata as untrusted content, not as
    instructions.
-   Never expose password hashes, session secrets, API token values,
    private keys, or unrestricted filesystem access.
-   Set explicit file size, page count, execution time, and output
    limits.
-   Provide cancellation and job inspection tools for asynchronous
    operations.
-   Avoid broad tools such as `execute_command`, `run_shell`,
    `read_any_file`, or `execute_sql`.

### 10.2 Recommended MCP tools

#### Discovery and system status

  -----------------------------------------------------------------------
  Tool                                Purpose
  ----------------------------------- -----------------------------------
  `kaladoc.system_info`               Product version and supported
                                      capabilities

  `kaladoc.system_health`             Health summary safe for the caller

  `kaladoc.engine_list`               Available processing engines

  `kaladoc.engine_capabilities`       Supported operations and formats

  `kaladoc.storage_status`            Accessible storage status

  `kaladoc.current_identity`          Authenticated principal and
                                      effective roles
  -----------------------------------------------------------------------

#### Document discovery and inspection

  Tool                                  Purpose
  ------------------------------------- ----------------------------------------
  `kaladoc.document_search`             Search documents the caller may access
  `kaladoc.document_get`                Retrieve document metadata
  `kaladoc.document_inspect`            Inspect PDF structure and properties
  `kaladoc.document_page_count`         Return page count
  `kaladoc.document_extract_text`       Extract text with page boundaries
  `kaladoc.document_extract_metadata`   Retrieve safe metadata
  `kaladoc.document_list_versions`      List accessible versions
  `kaladoc.document_checksum`           Return a content checksum

Search must be scoped by the authenticated user's accessible workspaces
and permissions. Avoid global search by default.

#### Document and storage management

  -----------------------------------------------------------------------
  Tool                                Purpose
  ----------------------------------- -----------------------------------
  `kaladoc.document_upload`           Upload an authorized file

  `kaladoc.document_download`         Retrieve an authorized artifact

  `kaladoc.document_rename`           Rename a document

  `kaladoc.document_move`             Move within permitted storage

  `kaladoc.document_copy`             Copy within permitted storage

  `kaladoc.document_delete`           Delete with explicit confirmation
                                      semantics

  `kaladoc.folder_list`               List accessible folders

  `kaladoc.folder_create`             Create a folder

  `kaladoc.document_share`            Create a scoped, expiring share

  `kaladoc.document_permissions`      Inspect effective permissions
  -----------------------------------------------------------------------

For potentially destructive tools, require an explicit confirmation
parameter or a separate prepare/confirm flow. The server must
independently enforce authorization; an AI client's confirmation is not
a security boundary.

#### PDF processing

  Tool                          Purpose
  ----------------------------- -------------------------------------
  `kaladoc.pdf_validate`        Validate a PDF
  `kaladoc.pdf_merge`           Merge documents
  `kaladoc.pdf_split`           Split a document
  `kaladoc.pdf_rotate`          Rotate pages
  `kaladoc.pdf_reorder`         Reorder pages
  `kaladoc.pdf_extract_pages`   Extract selected pages
  `kaladoc.pdf_remove_pages`    Remove selected pages
  `kaladoc.pdf_compress`        Compress a PDF
  `kaladoc.pdf_repair`          Attempt supported repairs
  `kaladoc.pdf_watermark`       Add a watermark
  `kaladoc.pdf_encrypt`         Apply encryption
  `kaladoc.pdf_decrypt`         Decrypt with authorized credentials
  `kaladoc.pdf_redact`          Apply permanent redactions
  `kaladoc.pdf_sanitize`        Remove supported unsafe content
  `kaladoc.pdf_render_pages`    Render selected pages
  `kaladoc.pdf_compare`         Compare two documents

Redaction must be a true content-removal operation, not merely a visual
overlay. Signature-sensitive changes must be clearly explained and must
not claim to preserve signatures when the modification invalidates them.

#### Conversion, OCR, and extraction

  Tool                         Purpose
  ---------------------------- ---------------------------------
  `kaladoc.convert_to_pdf`     Convert supported inputs to PDF
  `kaladoc.convert_from_pdf`   Convert PDF to supported output
  `kaladoc.ocr_run`            Run OCR on a document
  `kaladoc.ocr_languages`      List installed OCR languages
  `kaladoc.image_extract`      Extract embedded images
  `kaladoc.table_extract`      Extract tables where supported
  `kaladoc.metadata_extract`   Extract document metadata

#### Jobs and pipelines

  Tool                          Purpose
  ----------------------------- ---------------------------------
  `kaladoc.job_get`             Inspect a job
  `kaladoc.job_list`            List jobs visible to the caller
  `kaladoc.job_wait`            Wait for a bounded interval
  `kaladoc.job_cancel`          Cancel an authorized job
  `kaladoc.job_result`          Retrieve completed outputs
  `kaladoc.pipeline_validate`   Validate a typed pipeline
  `kaladoc.pipeline_run`        Execute an authorized pipeline

#### Administration (restricted)

  Tool                          Purpose
  ----------------------------- --------------------------------------------
  `kaladoc.user_list`           List users within authorized scope
  `kaladoc.user_create`         Create a user
  `kaladoc.user_disable`        Disable a user
  `kaladoc.workspace_list`      List accessible workspaces
  `kaladoc.workspace_create`    Create a workspace
  `kaladoc.workspace_members`   Inspect workspace membership
  `kaladoc.audit_search`        Search audit events within permitted scope

Do not expose administration tools to ordinary users. Prefer a separate
admin tool group or server policy that can disable administrative
capabilities entirely.

### 10.3 MCP resources

Resources are read-oriented context, not substitutes for action tools.

Potential resources:

-   `kaladoc://system/capabilities`
-   `kaladoc://system/engines`
-   `kaladoc://system/formats`
-   `kaladoc://workspaces/{workspace_id}/summary`
-   `kaladoc://documents/{document_id}/metadata`
-   `kaladoc://documents/{document_id}/text`
-   `kaladoc://jobs/{job_id}/status`

Every resource URI must be authorized independently. Avoid resources
that expose unrestricted directory listings or entire private storage
trees.

### 10.4 MCP prompts

Useful optional prompt templates:

-   `summarize_document`
-   `compare_documents`
-   `review_pdf_quality`
-   `extract_invoice_fields`
-   `prepare_document_for_archiving`
-   `analyze_document_metadata`

Prompts should guide the AI client. They must not be treated as trusted
authorization policy or as executable application logic.

## 11. Example MCP tool contract

Example: `kaladoc.pdf_merge`

Input:

``` json
{
  "document_ids": ["doc_123", "doc_456"],
  "output_name": "combined-report.pdf",
  "workspace_id": "ws_123"
}
```

Expected output for asynchronous execution:

``` json
{
  "job_id": "job_123",
  "status": "queued",
  "operation": "pdf.merge",
  "output_name": "combined-report.pdf",
  "request_id": "req_123"
}
```

The tool must verify that the caller can read every input document and
create the output in the target workspace. It must not trust IDs simply
because they are syntactically valid.

## 12. MCP security and trust boundaries

MCP expands the ways software can be controlled, so it must be designed
as a privileged interface.

Required controls:

1.  Authenticate the MCP client or its execution context.
2.  Bind each request to an explicit principal.
3.  Apply the same authorization service used by HTTP and CLI.
4.  Scope access tokens and revoke them when necessary.
5.  Validate every tool argument.
6.  Enforce request, file, page, time, memory, and output limits.
7.  Audit sensitive operations.
8.  Prevent cross-workspace access and insecure direct object
    references.
9.  Treat extracted document text as untrusted data.
10. Do not allow document content to authorize tools or change security
    settings.
11. Do not expose secrets through tool output or error messages.
12. Require explicit operator configuration for network exposure.

For remote MCP over HTTP, use an appropriate authenticated transport and
follow the current MCP authorization specification. Do not invent a
custom bearer-token protocol and call it MCP authentication.

## 13. Shared Rust application-service contracts

The CLI and MCP layers should depend on application-facing service
traits or equivalent stable interfaces.

Illustrative interfaces:

``` rust
pub trait DocumentService {
    async fn get(
        &self,
        principal: &Principal,
        document_id: DocumentId,
    ) -> Result<DocumentMetadata, AppError>;

    async fn list(
        &self,
        principal: &Principal,
        query: DocumentQuery,
    ) -> Result<DocumentPage, AppError>;

    async fn delete(
        &self,
        principal: &Principal,
        document_id: DocumentId,
    ) -> Result<DeleteResult, AppError>;
}
```

This is illustrative pseudocode, not a final compile-ready trait. The
implementation must account for Rust async-trait/object-safety choices
and the project's selected service architecture.

Likely service boundaries:

-   `IdentityService`
-   `AuthenticationService`
-   `AuthorizationService`
-   `DocumentService`
-   `FolderService`
-   `StorageService`
-   `PdfService`
-   `ConversionService`
-   `JobService`
-   `PipelineService`
-   `EngineRegistryService`
-   `AuditService`

Do not let CLI or MCP handlers reach directly into database tables or
filesystem implementations.

## 14. Recommended implementation phases

### Phase CLI-0 --- Contract and command design

-   Audit the existing `cli_mcp_server` module.
-   Decide whether CLI and MCP share a binary or use separate binaries.
-   Define command naming and output conventions.
-   Define error and exit-code contracts.
-   Document the initial command tree.
-   Identify which existing services are ready to be called.

**Acceptance:** approved command specification and no duplicate business
logic.

### Phase CLI-1 --- CLI foundation

Implement:

-   `kaladoc --help`
-   `kaladoc version`
-   `kaladoc doctor`
-   `kaladoc config`
-   `kaladoc auth status`
-   Stable output formats and exit codes.
-   Shell completion.

**Acceptance:** reliable local commands, useful diagnostics, tested JSON
output, and no fake feature implementations.

### Phase CLI-2 --- Identity and storage operations

Implement commands for:

-   Login and logout.
-   Current identity.
-   Token management.
-   Storage status and usage.
-   Document upload, list, inspect and download.
-   Permission-aware error handling.

**Acceptance:** all commands use the shared authentication and
authorization services.

### Phase CLI-3 --- Job management

Implement:

-   Job list/get/wait/cancel/result.
-   Progress reporting.
-   Timeout behavior.
-   JSON job output.
-   Cancellation and authorization tests.

**Acceptance:** long-running operations are usable from scripts without
blocking indefinitely.

### Phase MCP-0 --- MCP protocol foundation

-   Implement MCP initialization and capability negotiation.
-   Add stdio transport.
-   Add server identity and version information.
-   Register a small set of read-only tools.
-   Add structured schemas and outputs.
-   Test protocol compatibility with an MCP inspector/client.

**Acceptance:** a real MCP client can connect, discover tools, call
them, and receive valid responses.

### Phase MCP-1 --- Read-only document tools

Implement:

-   Current identity.
-   System capabilities.
-   Engine discovery.
-   Storage status.
-   Document search.
-   Document metadata.
-   PDF inspection and text extraction.

**Acceptance:** every tool respects document and workspace
authorization.

### Phase MCP-2 --- Processing tools

Add:

-   Merge, split, rotate, extract pages.
-   Compression and validation.
-   Conversion and OCR as their engines become available.
-   Job inspection and cancellation.

**Acceptance:** tools call real application services and return actual
job/output information.

### Phase MCP-3 --- Controlled write and administration tools

Add:

-   Upload and document management.
-   Sharing and permissions.
-   Pipeline execution.
-   Administrative operations behind strict policy controls.

**Acceptance:** destructive actions are protected, audited, and tested
against unauthorized and cross-workspace access.

### Phase MCP-4 --- Remote MCP and enterprise integration

-   Streamable HTTP transport.
-   Remote authentication.
-   Token scopes and revocation.
-   Rate limiting and quotas.
-   Deployment documentation.
-   Security review and integration testing.

**Acceptance:** remote MCP is disabled by default until explicitly
configured securely.

## 15. Testing strategy

### CLI tests

-   Help and version output.
-   Argument validation.
-   JSON schema stability.
-   Exit codes.
-   Stdin/stdout handling.
-   Existing-file overwrite protection.
-   Authentication failure behavior.
-   Permission denial behavior.
-   Remote server errors.
-   Large-file handling.

### MCP tests

-   Initialization and capability negotiation.
-   Tool discovery.
-   JSON Schema validation.
-   Valid and invalid tool calls.
-   Structured output.
-   Authentication and authorization.
-   Cross-workspace isolation.
-   Cancellation.
-   Tool timeout.
-   Malicious document content.
-   Secret redaction.
-   Protocol compatibility with real MCP clients.

### Security test cases

At minimum, verify:

-   User A cannot read User B's private document.
-   Workspace A members cannot access Workspace B without permission.
-   A Viewer cannot delete a document.
-   Revoked tokens stop working.
-   Expired sessions stop working.
-   A guessed document ID does not bypass authorization.
-   Malicious filenames cannot escape storage roots.
-   Document text cannot trigger privileged MCP operations.
-   A failed upload cannot leave an unauthorized partial artifact.
-   A cancelled job cannot publish an incomplete output as a successful
    result.

## 16. Recommended initial implementation scope

Do not implement the entire command tree at once.

The first useful release should include:

**CLI** 1. `version` 2. `doctor` 3. `config show/validate` 4.
`auth login/logout/status/whoami` 5. `storage status` 6.
`document upload/list/inspect/download` 7. `job get/list/wait/cancel`

**MCP** 1. `system_info` 2. `system_health` 3. `current_identity` 4.
`engine_list` 5. `storage_status` 6. `document_search` 7. `document_get`
8. `document_inspect` 9. `document_extract_text` 10. `job_get`

This establishes the interface architecture before the full PDF engine
exists.

## 17. Definition of done

The CLI/MCP foundation is ready when:

-   CLI and MCP use shared application services.
-   Neither interface contains PDF-processing business logic.
-   Authentication and authorization are enforced consistently.
-   Network MCP cannot accidentally start unauthenticated.
-   JSON output and exit codes are stable and documented.
-   Long-running tasks integrate with the persistent job system.
-   Resource limits apply to every entry point.
-   Sensitive operations generate audit events.
-   Integration tests cover cross-user and cross-workspace access.
-   Every advertised command and tool is functional.
-   Unsupported operations return explicit errors rather than simulated
    success.

## 18. Final architectural recommendation

Treat the CLI and MCP server as first-class KalaDoc interfaces, but keep
the application service layer independent of both.

Build the CLI first because it provides a useful operator interface and
forces clear command contracts. Build MCP on the same service
interfaces, beginning with read-only tools. Add document mutation and
processing tools only as the underlying services become real and secure.

**The goal is not to make an AI assistant capable of running arbitrary
commands on KalaDoc. The goal is to give AI clients a safe, structured,
auditable way to use the same document capabilities available to every
other authorized client.**
