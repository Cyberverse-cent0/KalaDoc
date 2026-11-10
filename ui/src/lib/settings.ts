const SETTINGS_KEY = 'kaladoc.settings'

export interface ApiSettings {
  apiUrl: string
  token: string
}

const DEFAULT_API_URL =
  (import.meta.env.VITE_API_URL as string | undefined) ?? 'http://localhost:8080'

export function loadSettings(): ApiSettings {
  try {
    const raw = localStorage.getItem(SETTINGS_KEY)
    if (raw) {
      const parsed = JSON.parse(raw) as Partial<ApiSettings>
      return {
        apiUrl: parsed.apiUrl || DEFAULT_API_URL,
        token: parsed.token ?? '',
      }
    }
  } catch {
    // fall through to defaults
  }
  return { apiUrl: DEFAULT_API_URL, token: '' }
}

export function saveSettings(settings: ApiSettings): void {
  localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings))
}

export function clearSettings(): void {
  localStorage.removeItem(SETTINGS_KEY)
}

export function isConfigured(): boolean {
  const { token } = loadSettings()
  return token.trim().length > 0
}
