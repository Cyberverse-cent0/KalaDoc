import { useQuery } from '@tanstack/react-query'
import { api } from '../lib/api'
import { isConfigured, loadSettings } from '../lib/settings'
import type { Job } from '../lib/types'

interface StorageResponse {
  backend?: unknown
  quota_bytes?: number
  used_bytes?: number
  [key: string]: unknown
}

function storageText(data: StorageResponse | undefined): string | null {
  if (!data) return null
  const num = (v: unknown) => (typeof v === 'number' ? v : null)
  const used = num(data.used_bytes) ?? num((data.usage as Record<string, unknown> | undefined)?.bytes)
  const quota =
    num(data.quota_bytes) ?? num((data.quota as Record<string, unknown> | undefined)?.bytes)
  if (used == null && quota == null) {
    return typeof data.backend === 'string' ? `storage: ${data.backend}` : null
  }
  const mb = (n: number) => `${(n / 1_048_576).toFixed(0)} MB`
  return `storage: ${used != null ? mb(used) : '?'} / ${quota != null ? mb(quota) : '∞'}`
}

export default function StatusBar() {
  const configured = isConfigured()
  const { apiUrl } = loadSettings()

  const jobsQuery = useQuery({
    queryKey: ['statusbar-jobs'],
    queryFn: () => api.get<{ jobs: Job[] }>('/jobs', { limit: 100 }),
    enabled: configured,
    refetchInterval: 15_000,
    retry: false,
  })
  const storageQuery = useQuery({
    queryKey: ['statusbar-storage'],
    queryFn: () => api.get<StorageResponse>('/storage'),
    enabled: configured,
    refetchInterval: 60_000,
    retry: false,
  })

  const active = (jobsQuery.data?.jobs ?? []).filter(
    (j) => j.status === 'pending' || j.status === 'running',
  ).length
  const host = (() => {
    try {
      return new URL(apiUrl).host
    } catch {
      return apiUrl
    }
  })()

  return (
    <footer className="flex h-7 shrink-0 items-center gap-4 border-t border-border bg-card px-3 text-[11px] text-faint-foreground">
      <span className="flex items-center gap-1.5">
        <span
          className={`h-1.5 w-1.5 rounded-full ${configured ? 'bg-emerald-400' : 'bg-slate-500'}`}
        />
        {configured ? host : 'not connected'}
      </span>
      {configured && <span>{active} active job{active === 1 ? '' : 's'}</span>}
      {configured && storageText(storageQuery.data) && (
        <span className="hidden sm:inline">{storageText(storageQuery.data)}</span>
      )}
      <span className="ml-auto hidden items-center gap-1 md:flex">
        <kbd className="rounded border border-border bg-muted px-1 font-mono text-[10px]">⌃K</kbd>
        commands
        <span className="mx-1">·</span>
        <kbd className="rounded border border-border bg-muted px-1 font-mono text-[10px]">?</kbd>
        shortcuts
      </span>
    </footer>
  )
}
