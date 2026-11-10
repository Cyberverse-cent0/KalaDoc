import { useState } from 'react'
import { api, ApiError } from '../lib/api'
import type { Document } from '../lib/types'

const baseBtn =
  'rounded bg-slate-800 px-3 py-1.5 text-sm text-slate-200 hover:bg-slate-700 disabled:opacity-50'
const dangerBtn =
  'rounded border border-red-900 bg-red-950/60 px-3 py-1.5 text-sm text-red-300 hover:bg-red-900/60 disabled:opacity-50'

export default function DocumentActions({
  document,
  onChanged,
}: {
  document: Document
  onChanged?: () => void
}) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const status = document.status.toLowerCase()
  const trashed = status.includes('trash') || status.includes('deleted')
  const archived = status.includes('archiv')

  async function run(fn: () => Promise<unknown>) {
    setBusy(true)
    setError(null)
    try {
      await fn()
      onChanged?.()
    } catch (err) {
      setError(err instanceof ApiError ? `${err.message} (${err.code})` : String(err))
    } finally {
      setBusy(false)
    }
  }

  const path = `/documents/${document.id}`

  const rename = () =>
    run(async () => {
      const name = window.prompt('Rename document', document.name)
      if (!name || name.trim() === '' || name.trim() === document.name) return
      await api.patch(`${path}`, { name: name.trim() })
    })

  const archiveToggle = () =>
    run(() =>
      api.post(`${path}/${archived ? 'unarchive' : 'archive'}`),
    )

  const copy = () => run(() => api.post(`${path}/copy`))
  const trash = () => run(() => api.del(path))
  const restore = () => run(() => api.post(`${path}/restore`))
  const purge = () =>
    run(async () => {
      if (!window.confirm('Permanently delete this document? This cannot be undone.')) return
      await api.post(`${path}/purge`)
    })
  const addTag = () =>
    run(async () => {
      const name = window.prompt('Tag name')
      if (!name || name.trim() === '') return
      await api.put(`${path}/tags`, { name: name.trim() })
    })

  return (
    <div className="flex flex-wrap items-center gap-2">
      <button className={baseBtn} disabled={busy} onClick={rename}>
        Rename
      </button>
      <button className={baseBtn} disabled={busy} onClick={archiveToggle}>
        {archived ? 'Unarchive' : 'Archive'}
      </button>
      <button className={baseBtn} disabled={busy} onClick={copy}>
        Copy
      </button>
      {!trashed && (
        <button className={dangerBtn} disabled={busy} onClick={trash}>
          Trash
        </button>
      )}
      {trashed && (
        <>
          <button className={baseBtn} disabled={busy} onClick={restore}>
            Restore
          </button>
          <button className={dangerBtn} disabled={busy} onClick={purge}>
            Purge
          </button>
        </>
      )}
      <button className={baseBtn} disabled={busy} onClick={addTag}>
        Add tag
      </button>
      {error && <span className="text-sm text-red-300">{error}</span>}
    </div>
  )
}
