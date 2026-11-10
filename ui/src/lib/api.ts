import { loadSettings } from './settings'

export class ApiError extends Error {
  readonly status: number
  readonly code: string
  readonly retryable: boolean
  readonly requestId?: string

  constructor(
    message: string,
    opts: { status: number; code?: string; retryable?: boolean; requestId?: string },
  ) {
    super(message)
    this.name = 'ApiError'
    this.status = opts.status
    this.code = opts.code ?? 'unknown'
    this.retryable = opts.retryable ?? false
    this.requestId = opts.requestId
  }
}

interface ErrorEnvelope {
  error?: {
    code?: string
    message?: string
    request_id?: string
    retryable?: boolean
  }
}

export interface RequestOptions {
  method?: string
  body?: unknown
  query?: Record<string, string | number | boolean | undefined>
  /** Skip the Authorization header (public endpoints). */
  public?: boolean
}

async function request<T>(path: string, opts: RequestOptions = {}): Promise<T> {
  const { apiUrl, token } = loadSettings()
  const url = new URL(apiUrl.replace(/\/+$/, '') + path)
  for (const [k, v] of Object.entries(opts.query ?? {})) {
    if (v !== undefined && v !== '') url.searchParams.set(k, String(v))
  }

  const headers: Record<string, string> = { Accept: 'application/json' }
  if (!opts.public) {
    if (!token) throw new ApiError('No API token configured', { status: 0, code: 'not_configured' })
    headers.Authorization = `Bearer ${token}`
  }
  let body: BodyInit | undefined
  if (opts.body !== undefined) {
    headers['Content-Type'] = 'application/json'
    body = JSON.stringify(opts.body)
  }

  let res: Response
  try {
    res = await fetch(url, { method: opts.method ?? 'GET', headers, body })
  } catch (e) {
    throw new ApiError(`Cannot reach API at ${url.origin}: ${(e as Error).message}`, {
      status: 0,
      code: 'network',
      retryable: true,
    })
  }

  if (!res.ok) {
    let env: ErrorEnvelope = {}
    try {
      env = (await res.json()) as ErrorEnvelope
    } catch {
      // non-JSON error body
    }
    throw new ApiError(env.error?.message ?? `HTTP ${res.status}`, {
      status: res.status,
      code: env.error?.code,
      retryable: env.error?.retryable,
      requestId: env.error?.request_id,
    })
  }

  if (res.status === 204) return undefined as T
  return (await res.json()) as T
}

export const api = {
  get: <T>(path: string, query?: RequestOptions['query'], opts?: { public?: boolean }) =>
    request<T>(path, { query, public: opts?.public }),
  post: <T>(path: string, body?: unknown) => request<T>(path, { method: 'POST', body }),
  put: <T>(path: string, body?: unknown) => request<T>(path, { method: 'PUT', body }),
  patch: <T>(path: string, body?: unknown) => request<T>(path, { method: 'PATCH', body }),
  del: <T>(path: string) => request<T>(path, { method: 'DELETE' }),
  /** Raw-bytes upload: POST /documents?filename=<name> with the file body. */
  async upload(file: File, opts?: { filename?: string }): Promise<unknown> {
    const { apiUrl, token } = loadSettings()
    if (!token) throw new ApiError('No API token configured', { status: 0, code: 'not_configured' })
    const filename = opts?.filename ?? file.name
    const url = new URL(apiUrl.replace(/\/+$/, '') + '/documents')
    url.searchParams.set('filename', filename)
    const res = await fetch(url, {
      method: 'POST',
      headers: {
        Authorization: `Bearer ${token}`,
        'Content-Type': file.type || 'application/octet-stream',
      },
      body: file,
    })
    if (!res.ok) {
      let env: ErrorEnvelope = {}
      try {
        env = (await res.json()) as ErrorEnvelope
      } catch {
        // non-JSON error body
      }
      throw new ApiError(env.error?.message ?? `Upload failed (HTTP ${res.status})`, {
        status: res.status,
        code: env.error?.code,
        retryable: env.error?.retryable,
        requestId: env.error?.request_id,
      })
    }
    return res.json()
  },
  /** Authenticated fetch returning raw bytes (artifact downloads). */
  async download(path: string): Promise<Blob> {
    const { apiUrl, token } = loadSettings()
    const res = await fetch(apiUrl.replace(/\/+$/, '') + path, {
      headers: { Authorization: `Bearer ${token}` },
    })
    if (!res.ok) throw new ApiError(`Download failed (HTTP ${res.status})`, { status: res.status })
    return res.blob()
  },
}
