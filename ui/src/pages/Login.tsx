import { useState } from 'react'
import { useNavigate } from 'react-router-dom'
import { api, ApiError } from '../lib/api'
import { loadSettings, saveSettings } from '../lib/settings'

type Mode = 'login' | 'register'

interface SessionResponse {
  token: string
  expires_at: number
  user: {
    id: number
    first_name: string
    last_name: string
    user_name: string
    email: string
  }
}

const inputCls =
  'mt-1 w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm outline-none focus:border-sky-600'
const labelCls = 'block text-sm font-medium text-slate-300'

export default function LoginPage() {
  const navigate = useNavigate()
  const initial = loadSettings()
  const [mode, setMode] = useState<Mode>('login')
  const [apiUrl, setApiUrl] = useState(initial.apiUrl)
  const [userName, setUserName] = useState('')
  const [password, setPassword] = useState('')
  const [firstName, setFirstName] = useState('')
  const [lastName, setLastName] = useState('')
  const [email, setEmail] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  async function submit(e: React.FormEvent) {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      const res =
        mode === 'login'
          ? await api.post<SessionResponse>(
              '/auth/login',
              { user_name: userName.trim(), password },
              { public: true },
            )
          : await api.post<SessionResponse>(
              '/auth/register',
              {
                first_name: firstName.trim(),
                last_name: lastName.trim(),
                user_name: userName.trim(),
                email: email.trim(),
                password,
              },
              { public: true },
            )
      saveSettings({ apiUrl: apiUrl.trim(), token: res.token })
      navigate('/documents')
    } catch (err) {
      setError(err instanceof ApiError ? `${err.message} (${err.code})` : String(err))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="flex min-h-screen items-center justify-center bg-slate-950 px-4">
      <div className="w-full max-w-md rounded-xl border border-slate-800 bg-slate-900 p-6 shadow-xl">
        <div className="flex gap-1 rounded-lg bg-slate-950 p-1 text-sm">
          {(['login', 'register'] as const).map((m) => (
            <button
              key={m}
              type="button"
              onClick={() => {
                setMode(m)
                setError(null)
              }}
              className={`flex-1 rounded-md px-3 py-1.5 font-medium capitalize ${
                mode === m ? 'bg-sky-600 text-white' : 'text-slate-400 hover:text-slate-200'
              }`}
            >
              {m === 'login' ? 'Sign in' : 'Create account'}
            </button>
          ))}
        </div>

        <form onSubmit={submit} className="mt-5 space-y-4">
          <label className={labelCls}>
            API URL
            <input
              className={inputCls}
              value={apiUrl}
              onChange={(e) => setApiUrl(e.target.value)}
              placeholder="http://localhost:8080"
              required
            />
          </label>

          {mode === 'register' && (
            <div className="grid grid-cols-2 gap-3">
              <label className={labelCls}>
                First name
                <input className={inputCls} value={firstName} onChange={(e) => setFirstName(e.target.value)} required />
              </label>
              <label className={labelCls}>
                Last name
                <input className={inputCls} value={lastName} onChange={(e) => setLastName(e.target.value)} required />
              </label>
            </div>
          )}

          <label className={labelCls}>
            Username
            <input
              className={inputCls}
              value={userName}
              onChange={(e) => setUserName(e.target.value)}
              autoComplete="username"
              required
            />
          </label>

          {mode === 'register' && (
            <label className={labelCls}>
              Email
              <input
                className={inputCls}
                type="email"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                autoComplete="email"
                required
              />
            </label>
          )}

          <label className={labelCls}>
            Password
            <input
              className={inputCls}
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              autoComplete={mode === 'login' ? 'current-password' : 'new-password'}
              minLength={8}
              required
            />
          </label>

          {error && (
            <p className="rounded-md border border-red-900 bg-red-950/50 px-3 py-2 text-sm text-red-300">
              {error}
            </p>
          )}

          <button
            type="submit"
            disabled={busy}
            className="w-full rounded-md bg-sky-600 px-4 py-2 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
          >
            {busy ? 'Please wait…' : mode === 'login' ? 'Sign in' : 'Create account'}
          </button>

          <p className="text-center text-xs text-slate-500">
            Using an API key instead?{' '}
            <a href="/connect" className="text-sky-400 hover:underline">
              Connect with token
            </a>
          </p>
        </form>
      </div>
    </div>
  )
}
