import { useEffect, useRef, useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { api, ApiError } from '../lib/api'
import { clearSettings, isConfigured, loadSettings } from '../lib/settings'
import ConnectForm from './ConnectForm'

type ConnState = 'unknown' | 'ok' | 'error' | 'unconfigured'

const DOT: Record<ConnState, string> = {
  unknown: 'bg-slate-500',
  ok: 'bg-emerald-400',
  error: 'bg-red-400',
  unconfigured: 'bg-slate-600',
}

export default function ConnectionMenu() {
  const navigate = useNavigate()
  const queryClient = useQueryClient()
  const [open, setOpen] = useState(false)
  const [showForm, setShowForm] = useState(false)
  const ref = useRef<HTMLDivElement>(null)
  const configured = isConfigured()
  const { apiUrl, token } = loadSettings()

  const statusQuery = useQuery({
    queryKey: ['conn-status', apiUrl],
    queryFn: () => api.get('/engines'),
    enabled: configured,
    refetchInterval: 30_000,
    retry: false,
  })

  const state: ConnState = !configured
    ? 'unconfigured'
    : statusQuery.isPending
      ? 'unknown'
      : statusQuery.isError
        ? 'error'
        : 'ok'

  useEffect(() => {
    function onClickOutside(e: MouseEvent) {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener('mousedown', onClickOutside)
    return () => document.removeEventListener('mousedown', onClickOutside)
  }, [])

  function disconnect() {
    clearSettings()
    setOpen(false)
    setShowForm(false)
    void queryClient.invalidateQueries()
    navigate('/connect')
  }

  function hostOf(url: string): string {
    try {
      return new URL(url).host
    } catch {
      return url
    }
  }

  return (
    <div ref={ref} className="fixed bottom-4 right-4 z-50">
      {open && (
        <div className="absolute bottom-14 right-0 w-80 rounded-xl border border-slate-700 bg-slate-900 p-4 shadow-2xl">
          {showForm ? (
            <ConnectForm
              onCancel={() => setShowForm(false)}
              onSuccess={() => {
                setShowForm(false)
                setOpen(false)
                void queryClient.invalidateQueries()
              }}
            />
          ) : (
            <div className="space-y-3">
              <div>
                <p className="text-xs font-medium uppercase tracking-wide text-slate-500">
                  Connection
                </p>
                <p className="mt-1 break-all text-sm text-slate-200">{apiUrl}</p>
                <p className="mt-0.5 break-all font-mono text-xs text-slate-500">
                  {token ? `Bearer ${token.slice(0, 10)}…` : 'no token'}
                </p>
              </div>
              {state === 'error' && statusQuery.error instanceof ApiError && (
                <p className="rounded-md border border-red-900 bg-red-950/50 px-2 py-1.5 text-xs text-red-300">
                  {statusQuery.error.message}
                </p>
              )}
              <div className="flex gap-2">
                <button
                  type="button"
                  onClick={() => setShowForm(true)}
                  className="flex-1 rounded-md bg-slate-800 px-3 py-1.5 text-sm text-slate-200 hover:bg-slate-700"
                >
                  Change
                </button>
                <button
                  type="button"
                  onClick={disconnect}
                  className="rounded-md border border-red-900 bg-red-950/60 px-3 py-1.5 text-sm text-red-300 hover:bg-red-900/60"
                >
                  Disconnect
                </button>
              </div>
            </div>
          )}
        </div>
      )}

      <button
        type="button"
        title={configured ? `Connected to ${hostOf(apiUrl)}` : 'Not connected'}
        onClick={() => {
          setOpen((v) => !v)
          if (!configured) setShowForm(true)
        }}
        className="relative flex h-11 w-11 items-center justify-center rounded-full border border-slate-700 bg-slate-900 text-slate-300 shadow-lg hover:bg-slate-800"
      >
        <svg
          xmlns="http://www.w3.org/2000/svg"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
          className="h-5 w-5"
        >
          <path d="M12 22v-5" />
          <path d="M9 8V2" />
          <path d="M15 8V2" />
          <path d="M18 8v5a4 4 0 0 1-4 4h-4a4 4 0 0 1-4-4V8Z" />
        </svg>
        <span
          className={`absolute -right-0.5 -top-0.5 h-3 w-3 rounded-full border-2 border-slate-950 ${DOT[state]}`}
        />
      </button>
    </div>
  )
}
