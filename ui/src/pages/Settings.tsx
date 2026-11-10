import { useEffect, useRef, useState, type FormEvent } from 'react'
import { useNavigate } from 'react-router-dom'
import { api, ApiError } from '../lib/api'
import { clearSettings, loadSettings, saveSettings } from '../lib/settings'

type Status =
  | { kind: 'idle' }
  | { kind: 'saved' }
  | { kind: 'ok'; engines: number }
  | { kind: 'err'; message: string }

/** Loosely interpret the /engines response to count registered engines. */
function countEngines(data: unknown): number {
  if (Array.isArray(data)) return data.length
  if (data && typeof data === 'object') {
    const engines = (data as { engines?: unknown }).engines
    if (Array.isArray(engines)) return engines.length
  }
  return 0
}

export default function SettingsPage() {
  const navigate = useNavigate()
  const initial = loadSettings()
  const [apiUrl, setApiUrl] = useState(initial.apiUrl)
  const [token, setToken] = useState(initial.token)
  const [status, setStatus] = useState<Status>({ kind: 'idle' })
  const [testing, setTesting] = useState(false)
  const savedTimer = useRef<ReturnType<typeof setTimeout> | null>(null)

  useEffect(() => {
    return () => {
      if (savedTimer.current) clearTimeout(savedTimer.current)
    }
  }, [])

  function save(e: FormEvent) {
    e.preventDefault()
    saveSettings({ apiUrl: apiUrl.trim(), token: token.trim() })
    setStatus({ kind: 'saved' })
    if (savedTimer.current) clearTimeout(savedTimer.current)
    savedTimer.current = setTimeout(() => {
      setStatus((s) => (s.kind === 'saved' ? { kind: 'idle' } : s))
    }, 2000)
  }

  async function testConnection() {
    setTesting(true)
    setStatus({ kind: 'idle' })
    try {
      const data = await api.get<unknown>('/engines')
      setStatus({ kind: 'ok', engines: countEngines(data) })
    } catch (err) {
      setStatus({
        kind: 'err',
        message: err instanceof ApiError ? `${err.message} (${err.code})` : String(err),
      })
    } finally {
      setTesting(false)
    }
  }

  function disconnect() {
    clearSettings()
    navigate('/connect')
  }

  return (
    <div className="mx-auto max-w-md">
      <h1 className="text-xl font-semibold">Settings</h1>
      <p className="mt-1 text-sm text-slate-400">
        Connection settings for your <code>kaladoc serve</code> instance.
      </p>

      <form
        onSubmit={save}
        className="mt-4 rounded-xl border border-slate-800 bg-slate-900 p-6 shadow-xl"
      >
        <label className="block text-sm font-medium text-slate-300">
          API URL
          <input
            className="mt-1 w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm outline-none focus:border-sky-600"
            value={apiUrl}
            onChange={(e) => setApiUrl(e.target.value)}
            placeholder="http://localhost:8080"
            required
          />
        </label>
        <p className="mt-1 truncate text-xs text-slate-500" title={apiUrl.trim() || '(empty)'}>
          Effective API URL: {apiUrl.trim() || '(empty)'}
        </p>

        <label className="mt-4 block text-sm font-medium text-slate-300">
          Token / API key
          <input
            className="mt-1 w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 font-mono text-sm outline-none focus:border-sky-600"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            placeholder="Bearer token"
            type="password"
            required
          />
        </label>

        {status.kind === 'saved' && (
          <p className="mt-4 rounded-md border border-emerald-900 bg-emerald-950/50 px-3 py-2 text-sm text-emerald-300">
            Saved.
          </p>
        )}
        {status.kind === 'ok' && (
          <p className="mt-4 rounded-md border border-emerald-900 bg-emerald-950/50 px-3 py-2 text-sm text-emerald-300">
            Connected — {status.engines} {status.engines === 1 ? 'engine' : 'engines'} registered.
          </p>
        )}
        {status.kind === 'err' && (
          <p className="mt-4 rounded-md border border-red-900 bg-red-950/50 px-3 py-2 text-sm text-red-300">
            {status.message}
          </p>
        )}

        <div className="mt-6 flex flex-col gap-2">
          <button
            type="submit"
            className="w-full rounded-md bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500"
          >
            Save
          </button>
          <button
            type="button"
            onClick={testConnection}
            disabled={testing}
            className="w-full rounded-md border border-slate-700 bg-slate-950 px-4 py-2 text-sm font-medium text-slate-300 hover:bg-slate-800 disabled:opacity-50"
          >
            {testing ? 'Testing…' : 'Test connection'}
          </button>
          <button
            type="button"
            onClick={disconnect}
            className="w-full rounded-md border border-red-900 bg-red-950/40 px-4 py-2 text-sm font-medium text-red-300 hover:bg-red-950/70"
          >
            Disconnect
          </button>
        </div>
      </form>
    </div>
  )
}
