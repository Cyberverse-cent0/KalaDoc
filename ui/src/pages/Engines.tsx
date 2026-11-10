import { useQuery } from '@tanstack/react-query'
import { api, ApiError } from '../lib/api'
import { formatBytes } from '../lib/format'
import StatusBadge from '../components/StatusBadge'

type JsonObject = Record<string, unknown>

function asRecord(v: unknown): JsonObject | null {
  return v !== null && typeof v === 'object' && !Array.isArray(v) ? (v as JsonObject) : null
}

function asArray(v: unknown): unknown[] {
  return Array.isArray(v) ? v : []
}

function asString(v: unknown): string | null {
  return typeof v === 'string' && v.length > 0 ? v : null
}

function asNumber(v: unknown): number | null {
  return typeof v === 'number' && Number.isFinite(v) ? v : null
}

/** First string value among the given keys, or null. */
function pickString(obj: JsonObject, keys: string[]): string | null {
  for (const k of keys) {
    const s = asString(obj[k])
    if (s !== null) return s
  }
  return null
}

/** Best-effort status string for an entry: status/health/state, or available/healthy booleans. */
function pickStatus(obj: JsonObject): string | null {
  const s = pickString(obj, ['status', 'health', 'state'])
  if (s !== null) return s
  const avail = obj.available ?? obj.healthy
  if (typeof avail === 'boolean') return avail ? 'available' : 'unavailable'
  return null
}

function titleFor(entry: unknown): string {
  const rec = asRecord(entry)
  if (!rec) return String(entry)
  return pickString(rec, ['name', 'id', 'kind', 'type']) ?? JSON.stringify(entry)
}

function rawJson(entry: unknown): string {
  try {
    return JSON.stringify(entry, null, 2)
  } catch {
    return String(entry)
  }
}

function EntryCard({ entry, fallbackStatus }: { entry: unknown; fallbackStatus?: string | null }) {
  const rec = asRecord(entry)
  const status = rec ? pickStatus(rec) ?? fallbackStatus ?? null : (fallbackStatus ?? null)
  return (
    <div className="rounded-xl border border-slate-800 bg-slate-900 p-4">
      <div className="flex items-start justify-between gap-3">
        <h3 className="min-w-0 break-words text-sm font-semibold text-slate-100">{titleFor(entry)}</h3>
        {status && <StatusBadge status={status} />}
      </div>
      <details className="mt-3">
        <summary className="cursor-pointer text-xs text-slate-400 hover:text-slate-200">
          raw JSON
        </summary>
        <pre className="mt-2 max-h-56 overflow-auto rounded-md bg-slate-950 p-3 text-xs text-slate-400">
          {rawJson(entry)}
        </pre>
      </details>
    </div>
  )
}

interface StorageStatus {
  backend: string | null
  rootConfigured: boolean | null
  usedBytes: number | null
  quotaBytes: number | null
}

function parseStorage(raw: unknown): StorageStatus {
  const rec = asRecord(raw) ?? {}
  return {
    backend: pickString(rec, ['backend', 'type', 'driver']),
    rootConfigured:
      typeof rec.root_configured === 'boolean'
        ? rec.root_configured
        : typeof rec.rootConfigured === 'boolean'
          ? rec.rootConfigured
          : null,
    usedBytes: asNumber(rec.used_bytes) ?? asNumber(rec.usedBytes),
    quotaBytes: asNumber(rec.quota_bytes) ?? asNumber(rec.quotaBytes),
  }
}

function StorageCard({ storage }: { storage: StorageStatus }) {
  return (
    <div className="rounded-xl border border-slate-800 bg-slate-900 p-4">
      <h2 className="text-sm font-semibold text-slate-100">Storage</h2>
      <dl className="mt-3 grid grid-cols-2 gap-x-4 gap-y-2 text-sm sm:grid-cols-4">
        <div>
          <dt className="text-xs text-slate-500">Backend</dt>
          <dd className="mt-0.5 text-slate-200">{storage.backend ?? '—'}</dd>
        </div>
        <div>
          <dt className="text-xs text-slate-500">Root configured</dt>
          <dd className="mt-0.5">
            {storage.rootConfigured == null ? (
              <span className="text-slate-400">—</span>
            ) : (
              <StatusBadge status={storage.rootConfigured ? 'yes' : 'no'} />
            )}
          </dd>
        </div>
        <div>
          <dt className="text-xs text-slate-500">Used</dt>
          <dd className="mt-0.5 text-slate-200">
            {storage.usedBytes != null ? formatBytes(storage.usedBytes) : '—'}
          </dd>
        </div>
        <div>
          <dt className="text-xs text-slate-500">Quota</dt>
          <dd className="mt-0.5 text-slate-200">
            {storage.quotaBytes != null ? formatBytes(storage.quotaBytes) : '—'}
          </dd>
        </div>
      </dl>
    </div>
  )
}

function Loading() {
  return <p className="text-sm text-slate-400">Loading…</p>
}

function ErrorBox({ error }: { error: unknown }) {
  const message = error instanceof ApiError ? `${error.message} (${error.code})` : String(error)
  return (
    <p className="rounded-md border border-red-900 bg-red-950/50 px-3 py-2 text-sm text-red-300">
      {message}
    </p>
  )
}

export default function EnginesPage() {
  const enginesQuery = useQuery({
    queryKey: ['engines'],
    queryFn: () => api.get<unknown>('/engines'),
    refetchInterval: 10000,
  })

  const storageQuery = useQuery({
    queryKey: ['storage'],
    queryFn: () => api.get<unknown>('/storage'),
    refetchInterval: 10000,
  })

  const root = asRecord(enginesQuery.data)
  const engines = asArray(root?.engines ?? enginesQuery.data)
  const health = asArray(root?.health)

  const storage = parseStorage(storageQuery.data)

  return (
    <div className="mx-auto max-w-6xl px-4 py-6">
      <h1 className="text-xl font-semibold">Engines</h1>
      <p className="mt-1 text-sm text-slate-400">
        Registered engines and their reported health. Refreshes every 10 seconds.
      </p>

      {storageQuery.data !== undefined && <div className="mt-6"><StorageCard storage={storage} /></div>}

      {enginesQuery.isPending && <div className="mt-6"><Loading /></div>}
      {enginesQuery.isError && (
        <div className="mt-6">
          <ErrorBox error={enginesQuery.error} />
        </div>
      )}

      {enginesQuery.data !== undefined && (
        <>
          <h2 className="mt-6 text-sm font-semibold uppercase tracking-wide text-slate-400">
            Registry ({engines.length})
          </h2>
          {engines.length === 0 ? (
            <p className="mt-3 text-sm text-slate-400">No engines registered.</p>
          ) : (
            <div className="mt-3 grid grid-cols-1 gap-4 md:grid-cols-2 lg:grid-cols-3">
              {engines.map((e, i) => (
                <EntryCard key={i} entry={e} />
              ))}
            </div>
          )}

          {health.length > 0 && (
            <>
              <h2 className="mt-8 text-sm font-semibold uppercase tracking-wide text-slate-400">
                Health ({health.length})
              </h2>
              <div className="mt-3 grid grid-cols-1 gap-4 md:grid-cols-2 lg:grid-cols-3">
                {health.map((h, i) => (
                  <EntryCard key={i} entry={h} />
                ))}
              </div>
            </>
          )}
        </>
      )}
    </div>
  )
}
