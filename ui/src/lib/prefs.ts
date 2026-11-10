const PREFS_KEY = 'kaladoc.prefs'

export type ThemePref = 'dark' | 'light' | 'system'

export interface UiPrefs {
  theme: ThemePref
  sidebarCollapsed: boolean
}

const DEFAULT_PREFS: UiPrefs = { theme: 'dark', sidebarCollapsed: false }

export function loadPrefs(): UiPrefs {
  try {
    const raw = localStorage.getItem(PREFS_KEY)
    if (raw) return { ...DEFAULT_PREFS, ...(JSON.parse(raw) as Partial<UiPrefs>) }
  } catch {
    // fall through to defaults
  }
  return { ...DEFAULT_PREFS }
}

export function savePrefs(prefs: Partial<UiPrefs>): UiPrefs {
  const next = { ...loadPrefs(), ...prefs }
  localStorage.setItem(PREFS_KEY, JSON.stringify(next))
  return next
}

export function resolvedTheme(pref: ThemePref): 'dark' | 'light' {
  if (pref === 'system') {
    return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light'
  }
  return pref
}

export function applyTheme(pref: ThemePref): void {
  document.documentElement.dataset.theme = resolvedTheme(pref)
}
