export interface DocumentSource {
  origin: 'upload' | 'import' | 'generated' | 'external' | 'other' | string
  uri: string | null
  external_reference: string | null
  imported_at: string | null
}

export interface Document {
  id: string
  name: string
  document_type: 'pdf' | 'image' | 'spreadsheet' | 'presentation' | 'text' | 'archive' | 'other' | string
  status: 'active' | 'archived' | 'trashed' | 'deleted' | string
  source: DocumentSource
  folder_id: string | null
  owner_id: string
  created_at: string
  updated_at: string
}

export interface DocumentChecksum {
  algorithm: 'sha256' | 'sha512' | 'blake3' | 'other' | string
  value: string
}

export interface DocumentArtifact {
  id: string
  document_id: string
  storage_uri: string
  file_name: string
  mime_type: string | null
  size_bytes: number
  checksum: DocumentChecksum | null
  created_at: string
}

export interface DocumentMetadata {
  document_id: string
  title: string | null
  author: string | null
  subject: string | null
  language: string | null
  keywords: string[]
  custom_fields: Record<string, unknown>
}

export interface DocumentTag {
  id: string
  name: string
  color: string | null
  created_at: string
}

export type JobStatus =
  | 'pending'
  | 'running'
  | 'succeeded'
  | 'failed'
  | 'cancelled'
  | 'skipped'
  | string

export interface Job {
  id: number
  kind: string
  payload: Record<string, unknown>
  status: JobStatus
  priority: number
  attempts: number
  max_attempts: number
  run_at: number
  created_at: number
  started_at: number | null
  finished_at: number | null
  last_error: string | null
  worker_id: string | null
  batch_id: number | null
  progress: unknown | null
}

export interface Folder {
  id: string
  name: string
  parent_id: string | null
  owner_id: string
  created_at: string
  updated_at: string
}

export interface EngineInfo {
  name: string
  version: string
  kind: string
  operations: string[]
  input_formats: string[]
  output_formats: string[]
}

export interface EngineHealthEntry {
  name: string
  status: 'healthy' | 'degraded' | 'unavailable' | string
  message: string | null
}

export interface HealthComponent {
  name: string
  status: 'healthy' | 'degraded' | 'unavailable' | string
  message?: string
}

export interface HealthResponse {
  status: 'healthy' | 'degraded' | 'unavailable' | string
  components: HealthComponent[]
}

export interface StorageStatus {
  backend: string
  root_configured: boolean
  used_bytes: number | null
  quota_bytes: number | null
}

export interface PipelineStep {
  name: string
  engine: string
  operation: string
  params: Record<string, unknown>
}

export interface PipelineDefinition {
  name: string
  description?: string
  steps: PipelineStep[]
}
