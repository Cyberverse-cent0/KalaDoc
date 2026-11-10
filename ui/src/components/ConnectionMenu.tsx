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

interface MeResponse {
  kind: 'session' | 'api_key'
  actor_id?: string
  user?: { user_name: string; email: string; first_name: string; last_name: string }
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

  const meQuery = useQuery({
    queryKey: ['auth-me', apiUrl],
    queryFn: () => api.get<MeResponse>('/auth/me'),
    enabled: configured,
    staleTime: 60_000,
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

  async function disconnect() {
    // Best-effort server-side session revocation; local logout always proceeds.
    try {
      await api.post('/auth/logout')
    } catch {
      // token may already be invalid — clearing locally is enough
    }
    clearSettings()
    setOpen(false)
    setShowForm(false)
    void queryClient.invalidateQueries()
    navigate('/login')
  }

  function hostOf(url: string): string {
    try {
      return new URL(url).host
    } catch {
      return url
    }
  }

  return (
    <div ref={ref} className="relative">
      {open && (
        <div className="absolute right-0 top-full z-50 mt-2 w-80 rounded-xl border border-border bg-card p-4 text-card-foreground shadow-2xl">
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
                <p className="text-xs font-medium uppercase tracking-wide text-faint-foreground">
                  Connection
                </p>
                <p className="mt-1 break-all text-sm text-card-foreground">{apiUrl}</p>
                <p className="mt-0.5 break-all font-mono text-xs text-faint-foreground">
                  {token ? `Bearer ${token.slice(0, 10)}…` : 'no token'}
                </p>
                {meQuery.data?.kind === 'session' && meQuery.data.user && (
                  <p className="mt-1 text-xs text-muted-foreground">
                    Signed in as{' '}
                    <span className="font-medium text-card-foreground">
                      {meQuery.data.user.user_name}
                    </span>
                  </p>
                )}
                {meQuery.data?.kind === 'api_key' && (
                  <p className="mt-1 text-xs text-muted-foreground">
                    API key:{' '}
                    <span className="font-mono">{meQuery.data.actor_id}</span>
                  </p>
                )}
              </div>
              {state === 'error' && statusQuery.error instanceof ApiError && (
                <p className="rounded-md border border-red-800 bg-red-950/50 px-2 py-1.5 text-xs text-red-300">
                  {statusQuery.error.message}
                </p>
              )}
              <div className="flex gap-2">
                <button
                  type="button"
                  onClick={() => setShowForm(true)}
                  className="flex-1 rounded-md bg-muted px-3 py-1.5 text-sm text-card-foreground hover:bg-border-strong"
                >
                  Change
                </button>
                <button
                  type="button"
                  onClick={disconnect}
                  className="rounded-md border border-red-800 bg-red-950/60 px-3 py-1.5 text-sm text-red-300 hover:bg-red-900/60"
                >
                  Sign out
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
        className="relative flex h-8 w-8 items-center justify-center rounded-md border border-border bg-muted text-muted-foreground hover:bg-border-strong"
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
          className={`absolute -right-0.5 -top-0.5 h-3 w-3 rounded-full border-2 border-card ${DOT[state]}`}
        />
      </button>
    </div>
  )
}
