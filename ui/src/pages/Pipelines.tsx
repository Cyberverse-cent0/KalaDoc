import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, ApiError } from '../lib/api'
import type { Job } from '../lib/types'

// Pipeline shapes are server-defined and loosely documented; treat everything
// as unknown and narrow defensively.
type Loose = Record<string, unknown>

function isObject(v: unknown): v is Loose {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}

function asString(v: unknown): string | null {
  return typeof v === 'string' && v.length > 0 ? v : null
}

interface Pipeline {
  raw: unknown
  name: string
  description: string | null
  steps: number | null
}

function extractPipelines(data: unknown): Pipeline[] {
  let list: unknown[] = []
  if (Array.isArray(data)) {
    list = data
  } else if (isObject(data)) {
    // Accept { pipelines: [...] }, { items: [...] }, or an arbitrary key whose
    // value is an array of objects.
    const candidates = ['pipelines', 'items', 'data'].map((k) => data[k])
    const listCandidate = candidates.find((v) => Array.isArray(v) && v.length > 0)
    if (Array.isArray(listCandidate) && listCandidate.length > 0) {
      list = listCandidate
    } else {
      const arr = Object.values(data).find(
        (v): v is unknown[] => Array.isArray(v) && v.some(isObject),
      )
      if (arr) list = arr
    }
  }
  return list.map((entry) => {
    const obj = isObject(entry) ? entry : null
    const name =
      asString(obj?.name) ??
      asString(obj?.id) ??
      asString(obj?.slug) ??
      (obj ? JSON.stringify(obj).slice(0, 60) : String(entry))
    const description = obj
      ? (asString(obj.description) ?? asString(obj.summary) ?? asString(obj.desc))
      : null
    const stepsField = obj?.steps ?? obj?.stages ?? obj?.tasks
    const steps = Array.isArray(stepsField) ? stepsField.length : null
    return { raw: entry, name, description, steps }
  })
}

function extractJob(data: unknown): { job?: Job; raw: unknown } {
  if (isObject(data) && isObject(data.job)) {
    return { job: data.job as unknown as Job, raw: data }
  }
  // Some backends return the job object at the top level.
  if (isObject(data) && ('status' in data || 'id' in data)) {
    return { job: data as unknown as Job, raw: data }
  }
  return { raw: data }
}

interface RunSuccess {
  name: string
  message: string
}

export default function PipelinesPage() {
  const queryClient = useQueryClient()
  const [successes, setSuccesses] = useState<Record<string, RunSuccess>>({})
  const [runErrors, setRunErrors] = useState<Record<string, string>>({})

  const query = useQuery({
    queryKey: ['pipelines'],
    queryFn: () => api.get<unknown>('/pipelines'),
    refetchInterval: 15000,
  })

  const run = useMutation({
    mutationFn: (name: string) => api.post<unknown>('/pipelines/run', { name }),
    onSuccess: (data, name) => {
      const { job } = extractJob(data)
      const status = job ? `${job.kind ?? 'job'} #${job.id} (${job.status})` : 'accepted'
      setSuccesses((s) => ({
        ...s,
        [name]: { name, message: `Pipeline run queued: ${status}.` },
      }))
      setRunErrors((e) => {
        const next = { ...e }
        delete next[name]
        return next
      })
      // A run may create/affect jobs; refetch the list opportunistically.
      queryClient.invalidateQueries({ queryKey: ['pipelines'] })
    },
    onError: (err, name) => {
      setRunErrors((e) => ({
        ...e,
        [name]: err instanceof ApiError ? `${err.message} (${err.code})` : String(err),
      }))
      setSuccesses((s) => {
        const next = { ...s }
        delete next[name]
        return next
      })
    },
  })

  if (query.isPending) {
    return (
      <div className="mx-auto max-w-4xl px-6 py-8">
        <h1 className="text-xl font-semibold">Pipelines</h1>
        <p className="mt-4 text-sm text-slate-400">Loading pipelines…</p>
      </div>
    )
  }

  if (query.isError) {
    const err = query.error
    const msg = err instanceof ApiError ? `${err.message} (${err.code})` : String(err)
    return (
      <div className="mx-auto max-w-4xl px-6 py-8">
        <h1 className="text-xl font-semibold">Pipelines</h1>
        <p className="mt-4 rounded-md border border-red-900 bg-red-950/50 px-3 py-2 text-sm text-red-300">
          {msg}
        </p>
      </div>
    )
  }

  const pipelines = extractPipelines(query.data)

  return (
    <div className="mx-auto max-w-4xl px-6 py-8">
      <div className="flex items-center justify-between">
        <h1 className="text-xl font-semibold">Pipelines</h1>
        {query.isFetching && <span className="text-xs text-slate-500">refreshing…</span>}
      </div>
      <p className="mt-1 text-sm text-slate-400">
        Built-in pipeline definitions. Runs are dispatched to the job queue.
      </p>

      {pipelines.length === 0 ? (
        <p className="mt-6 rounded-xl border border-slate-800 bg-slate-900 p-6 text-sm text-slate-400">
          No pipelines defined.
        </p>
      ) : (
        <ul className="mt-6 space-y-4">
          {pipelines.map((p, i) => (
            <li
              key={p.name}
              className="rounded-xl border border-slate-800 bg-slate-900 p-4"
            >
              <div className="flex items-start justify-between gap-4">
                <div className="min-w-0">
                  <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
                    <h2 className="truncate text-sm font-semibold text-slate-100">{p.name}</h2>
                    {p.steps != null && (
                      <span className="text-xs text-slate-500">
                        {p.steps} step{p.steps === 1 ? '' : 's'}
                      </span>
                    )}
                  </div>
                  {p.description && (
                    <p className="mt-1 text-sm text-slate-400">{p.description}</p>
                  )}
                </div>
                <button
                  onClick={() => run.mutate(p.name)}
                  disabled={run.isPending && run.variables === p.name}
                  className="shrink-0 rounded-md bg-sky-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
                >
                  {run.isPending && run.variables === p.name ? 'Running…' : 'Run'}
                </button>
              </div>

              {successes[p.name] && (
                <p className="mt-3 rounded-md border border-emerald-900 bg-emerald-950/50 px-3 py-2 text-sm text-emerald-300">
                  {successes[p.name].message}
                </p>
              )}
              {runErrors[p.name] && (
                <p className="mt-3 rounded-md border border-red-900 bg-red-950/50 px-3 py-2 text-sm text-red-300">
                  {runErrors[p.name]}
                </p>
              )}

              <details className="mt-3">
                <summary className="cursor-pointer text-xs text-slate-500 hover:text-slate-300">
                  Raw definition
                </summary>
                <pre className="mt-2 max-h-64 overflow-auto rounded-md bg-slate-950 p-3 text-xs text-slate-400">
                  {JSON.stringify(p.raw ?? pipelines[i], null, 2)}
                </pre>
              </details>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}
