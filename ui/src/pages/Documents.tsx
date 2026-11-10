import { useRef, useState } from 'react'
import { Link } from 'react-router-dom'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, ApiError } from '../lib/api'
import { formatDateTime } from '../lib/format'
import type { Document } from '../lib/types'
import StatusBadge from '../components/StatusBadge'

interface FoldersResponse {
  folders?: unknown[]
}

function folderField(f: unknown, key: string): string | null {
  if (f == null || typeof f !== 'object') return null
  const v = (f as Record<string, unknown>)[key]
  if (v == null) return null
  if (typeof v === 'object') return JSON.stringify(v)
  return String(v)
}

function folderLabel(f: unknown): string {
  return folderField(f, 'name') ?? folderField(f, 'id') ?? JSON.stringify(f)
}

function folderValue(f: unknown): string {
  return folderField(f, 'id') ?? folderLabel(f)
}

export default function DocumentsPage() {
  const [input, setInput] = useState('')
  const [query, setQuery] = useState('')
  const [folderId, setFolderId] = useState('')
  const [includeDeleted, setIncludeDeleted] = useState(false)
  const [uploadError, setUploadError] = useState<string | null>(null)
  const [uploading, setUploading] = useState(false)
  const fileInput = useRef<HTMLInputElement>(null)
  const queryClient = useQueryClient()

  const foldersQuery = useQuery({
    queryKey: ['folders'],
    queryFn: () => api.get<FoldersResponse>('/folders'),
    retry: false,
  })
  const folders = Array.isArray(foldersQuery.data?.folders) ? foldersQuery.data.folders : []

  const documentsQuery = useQuery({
    queryKey: ['documents', query, folderId, includeDeleted],
    queryFn: () =>
      api.get<{ documents: Document[] }>('/documents', {
        q: query || undefined,
        folder_id: folderId || undefined,
        include_deleted: includeDeleted ? 'true' : undefined,
      }),
  })

  const documents = documentsQuery.data?.documents ?? []
  const error = documentsQuery.error
  const errMessage =
    error instanceof ApiError ? `${error.message} (${error.code})` : error ? String(error) : null

  function search(e: React.FormEvent) {
    e.preventDefault()
    setQuery(input.trim())
  }

  function clear() {
    setInput('')
    setQuery('')
  }

  const upload = useMutation({
    mutationFn: (file: File) => api.upload(file),
    onSuccess: () => {
      setUploadError(null)
      void queryClient.invalidateQueries({ queryKey: ['documents'] })
    },
    onError: (err) => {
      setUploadError(err instanceof ApiError ? `${err.message} (${err.code})` : String(err))
    },
    onSettled: () => {
      setUploading(false)
      if (fileInput.current) fileInput.current.value = ''
    },
  })

  function onFileSelected(e: React.ChangeEvent<HTMLInputElement>) {
    const file = e.target.files?.[0]
    if (!file) return
    setUploadError(null)
    setUploading(true)
    upload.mutate(file)
  }

  return (
    <div className="mx-auto max-w-6xl px-4 py-8">
      <div className="flex items-center justify-between">
        <h1 className="text-xl font-semibold">Documents</h1>
        <div>
          <input
            ref={fileInput}
            type="file"
            className="hidden"
            onChange={onFileSelected}
          />
          <button
            type="button"
            disabled={uploading}
            onClick={() => fileInput.current?.click()}
            className="rounded-md bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
          >
            {uploading ? 'Uploading…' : 'Upload'}
          </button>
        </div>
      </div>
      {uploadError && (
        <p className="mt-3 rounded-md border border-red-900 bg-red-950/50 px-3 py-2 text-sm text-red-300">
          {uploadError}
        </p>
      )}

      <form onSubmit={search} className="mt-4 flex flex-wrap items-center gap-3">
        <input
          className="w-64 rounded-md border border-slate-700 bg-slate-900 px-3 py-2 text-sm outline-none focus:border-sky-600"
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder="Search documents…"
        />
        <button
          type="submit"
          className="rounded-md bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500"
        >
          Search
        </button>
        <button
          type="button"
          onClick={clear}
          className="rounded-md border border-slate-700 bg-slate-900 px-4 py-2 text-sm font-medium text-slate-300 hover:bg-slate-800"
        >
          Clear
        </button>

        {folders.length > 0 && (
          <select
            className="rounded-md border border-slate-700 bg-slate-900 px-3 py-2 text-sm outline-none focus:border-sky-600"
            value={folderId}
            onChange={(e) => setFolderId(e.target.value)}
          >
            <option value="">All folders</option>
            {folders.map((f, i) => (
              <option key={folderValue(f) || i} value={folderValue(f)}>
                {folderLabel(f)}
              </option>
            ))}
          </select>
        )}

        <label className="flex items-center gap-2 text-sm text-slate-300">
          <input
            type="checkbox"
            checked={includeDeleted}
            onChange={(e) => setIncludeDeleted(e.target.checked)}
            className="accent-sky-500"
          />
          Include trashed
        </label>
      </form>

      {errMessage && (
        <p className="mt-4 rounded-md border border-red-900 bg-red-950/50 px-3 py-2 text-sm text-red-300">
          {errMessage}
        </p>
      )}

      <div className="mt-4 overflow-hidden rounded-xl border border-slate-800 bg-slate-900">
        {documentsQuery.isPending ? (
          <p className="p-6 text-sm text-slate-400">Loading…</p>
        ) : documents.length === 0 ? (
          <p className="p-6 text-sm text-slate-400">No documents found.</p>
        ) : (
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-slate-800 text-left text-xs uppercase text-slate-500">
                <th className="px-4 py-3 font-medium">Name</th>
                <th className="px-4 py-3 font-medium">Type</th>
                <th className="px-4 py-3 font-medium">Status</th>
                <th className="px-4 py-3 font-medium">Folder</th>
                <th className="px-4 py-3 font-medium">Updated</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-slate-800">
              {documents.map((d) => (
                <tr key={d.id} className="hover:bg-slate-800/50">
                  <td className="px-4 py-3">
                    <Link
                      to={`/documents/${d.id}`}
                      className="text-sky-400 hover:text-sky-300 hover:underline"
                    >
                      {d.name}
                    </Link>
                  </td>
                  <td className="px-4 py-3 text-slate-300">{d.document_type}</td>
                  <td className="px-4 py-3">
                    <StatusBadge status={d.status} />
                  </td>
                  <td className="px-4 py-3 text-slate-400">{d.folder_id ?? '—'}</td>
                  <td className="px-4 py-3 text-slate-400">{formatDateTime(d.updated_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  )
}
