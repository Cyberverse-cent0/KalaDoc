import { useState } from 'react'
import { api, ApiError } from '../lib/api'
import { loadSettings, saveSettings } from '../lib/settings'

export default function ConnectForm({
  onSuccess,
  onCancel,
}: {
  onSuccess?: () => void
  onCancel?: () => void
}) {
  const initial = loadSettings()
  const [apiUrl, setApiUrl] = useState(initial.apiUrl)
  const [token, setToken] = useState(initial.token)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  async function connect(e: React.FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      saveSettings({ apiUrl: apiUrl.trim(), token: token.trim() })
      // /engines is protected — proves both reachability and the token.
      await api.get('/engines')
      onSuccess?.()
    } catch (err) {
      setError(err instanceof ApiError ? `${err.message} (${err.code})` : String(err))
    } finally {
      setBusy(false)
    }
  }

  return (
    <form onSubmit={connect} className="space-y-4">
      <p className="text-sm text-slate-400">
        API URL of your <code>kaladoc serve</code> instance and a session token or API key
        (from <code>kaladoc auth login</code> credentials or <code>KALADOC_API_KEYS</code>).
      </p>

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

      <label className="block text-sm font-medium text-slate-300">
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

      {error && (
        <p className="rounded-md border border-red-900 bg-red-950/50 px-3 py-2 text-sm text-red-300">
          {error}
        </p>
      )}

      <div className="flex gap-2">
        <button
          type="submit"
          disabled={busy}
          className="flex-1 rounded-md bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
        >
          {busy ? 'Connecting…' : 'Connect'}
        </button>
        {onCancel && (
          <button
            type="button"
            onClick={onCancel}
            className="rounded-md border border-slate-700 px-4 py-2 text-sm font-medium text-slate-300 hover:bg-slate-800"
          >
            Cancel
          </button>
        )}
      </div>
    </form>
  )
}
