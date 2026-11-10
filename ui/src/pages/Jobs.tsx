import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, ApiError } from '../lib/api'
import type { Job } from '../lib/types'
import { formatUnix } from '../lib/format'
import StatusBadge from '../components/StatusBadge'

interface JobsResponse {
  jobs: Job[]
}

const STATUSES = ['', 'pending', 'running', 'succeeded', 'failed', 'cancelled'] as const

function truncate(s: string, max: number): string {
  return s.length > max ? s.slice(0, max - 1) + '…' : s
}

function JobRow({ job }: { job: Job }) {
  const queryClient = useQueryClient()
  const cancel = useMutation({
    mutationFn: () => api.post<{ id: number; status: string }>(`/jobs/${job.id}/cancel`),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['jobs'] }),
  })
  const retry = useMutation({
    mutationFn: () => api.post<{ id: number; status: string }>(`/jobs/${job.id}/retry`),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['jobs'] }),
  })

  const error = cancel.error ?? retry.error
  const active = job.status === 'pending' || job.status === 'running'

  return (
    <tr className="border-t border-slate-800 align-top">
      <td className="px-3 py-2 font-mono text-xs text-slate-400">{job.id}</td>
      <td className="px-3 py-2" title={JSON.stringify(job.payload)}>
        <span className="text-sm font-medium text-slate-200">{job.kind}</span>
      </td>
      <td className="px-3 py-2">
        <StatusBadge status={job.status} />
      </td>
      <td className="px-3 py-2 text-sm text-slate-300">
        {job.attempts}/{job.max_attempts}
      </td>
      <td className="px-3 py-2 text-sm text-slate-400">{formatUnix(job.created_at)}</td>
      <td className="px-3 py-2 text-sm text-slate-400">{formatUnix(job.finished_at)}</td>
      <td className="max-w-64 px-3 py-2 text-sm text-red-300" title={job.last_error ?? undefined}>
        {job.last_error ? truncate(job.last_error, 60) : '—'}
        {error && (
          <p className="mt-1 text-xs text-red-400">
            {error instanceof ApiError ? `${error.message} (${error.code})` : String(error)}
          </p>
        )}
      </td>
      <td className="px-3 py-2">
        <div className="flex gap-2">
          {active && (
            <button
              type="button"
              disabled={cancel.isPending}
              onClick={() => cancel.mutate()}
              className="rounded-md border border-slate-700 bg-slate-800 px-2 py-1 text-xs font-medium text-slate-200 hover:bg-slate-700 disabled:opacity-50"
            >
              {cancel.isPending ? 'Cancelling…' : 'Cancel'}
            </button>
          )}
          {job.status === 'failed' && (
            <button
              type="button"
              disabled={retry.isPending}
              onClick={() => retry.mutate()}
              className="rounded-md border border-slate-700 bg-slate-800 px-2 py-1 text-xs font-medium text-slate-200 hover:bg-slate-700 disabled:opacity-50"
            >
              {retry.isPending ? 'Retrying…' : 'Retry'}
            </button>
          )}
        </div>
      </td>
    </tr>
  )
}

export default function JobsPage() {
  const [status, setStatus] = useState<string>('')

  const { data, isPending, error } = useQuery<JobsResponse>({
    queryKey: ['jobs', status],
    queryFn: () =>
      api.get<JobsResponse>('/jobs', { status: status || undefined, limit: 100 }),
    refetchInterval: (query) =>
      query.state.data?.jobs.some((j) => j.status === 'pending' || j.status === 'running')
        ? 3000
        : false,
  })

  const jobs = data?.jobs ?? []

  return (
    <div>
      <div className="flex items-center justify-between">
        <h1 className="text-xl font-semibold">Jobs</h1>
        <select
          value={status}
          onChange={(e) => setStatus(e.target.value)}
          className="rounded-md border border-slate-700 bg-slate-900 px-2 py-1.5 text-sm text-slate-200 outline-none focus:border-sky-600"
        >
          {STATUSES.map((s) => (
            <option key={s} value={s}>
              {s === '' ? 'All' : s}
            </option>
          ))}
        </select>
      </div>

      {isPending && <p className="mt-6 text-sm text-slate-400">Loading jobs…</p>}

      {error && (
        <p className="mt-6 rounded-md border border-red-900 bg-red-950/50 px-3 py-2 text-sm text-red-300">
          {error instanceof ApiError ? `${error.message} (${error.code})` : String(error)}
        </p>
      )}

      {!isPending && !error && (
        <div className="mt-4 overflow-x-auto rounded-xl border border-slate-800 bg-slate-900">
          {jobs.length === 0 ? (
            <p className="px-4 py-8 text-center text-sm text-slate-400">No jobs</p>
          ) : (
            <table className="w-full text-left">
              <thead>
                <tr className="border-b border-slate-800 text-xs uppercase tracking-wide text-slate-500">
                  <th className="px-3 py-2 font-medium">ID</th>
                  <th className="px-3 py-2 font-medium">Kind</th>
                  <th className="px-3 py-2 font-medium">Status</th>
                  <th className="px-3 py-2 font-medium">Attempts</th>
                  <th className="px-3 py-2 font-medium">Created</th>
                  <th className="px-3 py-2 font-medium">Finished</th>
                  <th className="px-3 py-2 font-medium">Error</th>
                  <th className="px-3 py-2 font-medium">Actions</th>
                </tr>
              </thead>
              <tbody>
                {jobs.map((job) => (
                  <JobRow key={job.id} job={job} />
                ))}
              </tbody>
            </table>
          )}
        </div>
      )}
    </div>
  )
}
