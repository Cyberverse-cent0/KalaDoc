import { useState } from 'react'
import { Link, useParams } from 'react-router-dom'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { api, ApiError } from '../lib/api'
import { formatBytes, formatDateTime } from '../lib/format'
import type { Document, DocumentArtifact } from '../lib/types'
import StatusBadge from '../components/StatusBadge'
import DocumentActions from '../components/DocumentActions'

interface DocumentDetailResponse {
  document: Document
  artifacts: DocumentArtifact[]
  metadata: unknown
}

export default function DocumentDetailPage() {
  const { id = '' } = useParams<{ id: string }>()
  const queryClient = useQueryClient()
  const [downloadError, setDownloadError] = useState<string | null>(null)
  const [downloadingId, setDownloadingId] = useState<string | null>(null)

  const { data, isPending, isError, error } = useQuery<DocumentDetailResponse>({
    queryKey: ['document', id],
    queryFn: () => api.get<DocumentDetailResponse>(`/documents/${id}`),
    enabled: id !== '',
  })

  async function downloadArtifact(a: DocumentArtifact) {
    setDownloadError(null)
    setDownloadingId(a.id)
    try {
      const blob = await api.download(`/documents/${id}/artifacts/${a.id}/download`)
      const url = URL.createObjectURL(blob)
      const link = document.createElement('a')
      link.href = url
      link.download = a.file_name
      document.body.appendChild(link)
      link.click()
      link.remove()
      setTimeout(() => URL.revokeObjectURL(url), 1000)
    } catch (err) {
      setDownloadError(err instanceof ApiError ? err.message : String(err))
    } finally {
      setDownloadingId(null)
    }
  }

  if (isPending) {
    return <p className="p-6 text-sm text-slate-400">Loading…</p>
  }

  if (isError || !data) {
    const message = error instanceof Error ? error.message : 'Failed to load document'
    return (
      <div className="m-6 rounded-md border border-red-900 bg-red-950/50 px-4 py-3 text-sm text-red-300">
        {message}
      </div>
    )
  }

  const { document: doc, artifacts, metadata } = data

  return (
    <div className="mx-auto max-w-4xl space-y-6 p-6">
      <Link to="/documents" className="text-sm text-sky-400 hover:text-sky-300">
        ← Back to documents
      </Link>

      <section className="rounded-xl border border-slate-800 bg-slate-900 p-6">
        <div className="flex items-center gap-3">
          <h1 className="text-xl font-semibold text-slate-100">{doc.name}</h1>
          <StatusBadge status={doc.status} />
        </div>
        <dl className="mt-5 grid grid-cols-1 gap-4 text-sm sm:grid-cols-2">
          <div>
            <dt className="text-slate-400">Type</dt>
            <dd className="mt-1 text-slate-100">{doc.document_type}</dd>
          </div>
          <div>
            <dt className="text-slate-400">Owner</dt>
            <dd className="mt-1 text-slate-100">{doc.owner_id}</dd>
          </div>
          <div>
            <dt className="text-slate-400">Folder</dt>
            <dd className="mt-1 text-slate-100">{doc.folder_id ?? '—'}</dd>
          </div>
          <div>
            <dt className="text-slate-400">Created</dt>
            <dd className="mt-1 text-slate-100">{formatDateTime(doc.created_at)}</dd>
          </div>
          <div>
            <dt className="text-slate-400">Updated</dt>
            <dd className="mt-1 text-slate-100">{formatDateTime(doc.updated_at)}</dd>
          </div>
          <div>
            <dt className="text-slate-400">Source</dt>
            <dd className="mt-1 break-all text-slate-100">
              {doc.source != null ? JSON.stringify(doc.source) : '—'}
            </dd>
          </div>
        </dl>
        <div className="mt-6 border-t border-slate-800 pt-4">
          <DocumentActions
            document={doc}
            onChanged={() =>
              void queryClient.invalidateQueries({ queryKey: ['document', id] })
            }
          />
        </div>
      </section>

      <section className="rounded-xl border border-slate-800 bg-slate-900 p-6">
        <h2 className="text-lg font-semibold text-slate-100">Artifacts</h2>
        {artifacts.length === 0 ? (
          <p className="mt-4 text-sm text-slate-400">No artifacts yet.</p>
        ) : (
          <div className="mt-4 overflow-x-auto">
            <table className="w-full text-left text-sm">
              <thead>
                <tr className="border-b border-slate-800 text-slate-400">
                  <th className="pb-2 pr-4 font-medium">File</th>
                  <th className="pb-2 pr-4 font-medium">Type</th>
                  <th className="pb-2 pr-4 font-medium">Size</th>
                  <th className="pb-2 pr-4 font-medium">Created</th>
                  <th className="pb-2 font-medium"> </th>
                </tr>
              </thead>
              <tbody>
                {artifacts.map((a) => (
                  <tr key={a.id} className="border-b border-slate-800/60 text-slate-300">
                    <td className="py-2.5 pr-4 text-slate-100">{a.file_name}</td>
                    <td className="py-2.5 pr-4">{a.mime_type ?? '—'}</td>
                    <td className="py-2.5 pr-4">{formatBytes(a.size_bytes)}</td>
                    <td className="py-2.5 pr-4">{formatDateTime(a.created_at)}</td>
                    <td className="py-2.5 text-right">
                      <button
                        type="button"
                        disabled={downloadingId === a.id}
                        onClick={() => void downloadArtifact(a)}
                        className="rounded-md bg-slate-800 px-3 py-1.5 text-sm text-slate-200 hover:bg-slate-700 disabled:opacity-50"
                      >
                        {downloadingId === a.id ? 'Downloading…' : 'Download'}
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        {downloadError && <p className="mt-3 text-sm text-red-300">{downloadError}</p>}
      </section>

      {metadata != null && (
        <details className="rounded-xl border border-slate-800 bg-slate-900 p-6">
          <summary className="cursor-pointer text-lg font-semibold text-slate-100">Metadata</summary>
          <pre className="mt-4 max-h-96 overflow-auto rounded-md bg-slate-950 p-3 text-xs text-slate-300">
            {JSON.stringify(metadata, null, 2)}
          </pre>
        </details>
      )}
    </div>
  )
}
