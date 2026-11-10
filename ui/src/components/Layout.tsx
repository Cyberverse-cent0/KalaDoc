import { useCallback, useMemo, useState } from 'react'
import { Outlet, useNavigate } from 'react-router-dom'
import Sidebar from './Sidebar'
import Topbar from './Topbar'
import StatusBar from './StatusBar'
import CommandPalette from './CommandPalette'
import ShortcutsDialog from './ShortcutsDialog'
import ConnectionMenu from './ConnectionMenu'
import { useGlobalShortcuts } from '../lib/useGlobalShortcuts'
import { applyTheme, loadPrefs, savePrefs } from '../lib/prefs'
import type { CommandDeps } from '../lib/commands'

export default function Layout() {
  const navigate = useNavigate()
  const [prefs, setPrefs] = useState(loadPrefs)
  const [paletteOpen, setPaletteOpen] = useState(false)
  const [shortcutsOpen, setShortcutsOpen] = useState(false)
  const [mobileNavOpen, setMobileNavOpen] = useState(false)

  const isDark = prefs.theme === 'dark' ||
    (prefs.theme === 'system' &&
      typeof window !== 'undefined' &&
      window.matchMedia('(prefers-color-scheme: dark)').matches)

  const toggleTheme = useCallback(() => {
    setPrefs((p) => {
      const next = p.theme === 'dark' ? 'light' : 'dark'
      applyTheme(next)
      return savePrefs({ theme: next })
    })
  }, [])

  const toggleSidebar = useCallback(() => {
    setPrefs((p) => savePrefs({ sidebarCollapsed: !p.sidebarCollapsed }))
  }, [])

  const openPalette = useCallback(() => setPaletteOpen((v) => !v), [])
  const openShortcuts = useCallback(() => setShortcutsOpen((v) => !v), [])

  useGlobalShortcuts({
    onPalette: openPalette,
    onShortcuts: openShortcuts,
    onToggleSidebar: toggleSidebar,
    onNavigate: (path) => navigate(path),
  })

  const commandDeps: CommandDeps = useMemo(
    () => ({
      navigate,
      togglePalette: openPalette,
      toggleShortcuts: openShortcuts,
      toggleSidebar,
      toggleTheme,
      isDark,
    }),
    [navigate, openPalette, openShortcuts, toggleSidebar, toggleTheme, isDark],
  )

  return (
    <div className="flex h-screen flex-col bg-background text-foreground">
      <a
        href="#main-content"
        className="sr-only focus:not-sr-only focus:absolute focus:left-2 focus:top-2 focus:z-[70] focus:rounded-md focus:bg-primary focus:px-3 focus:py-1.5 focus:text-sm focus:text-primary-foreground"
      >
        Skip to content
      </a>
      <div className="flex min-h-0 flex-1">
        <Sidebar
          collapsed={prefs.sidebarCollapsed}
          mobileOpen={mobileNavOpen}
          onCloseMobile={() => setMobileNavOpen(false)}
        />
        <div className="flex min-w-0 flex-1 flex-col">
          <Topbar
            collapsed={prefs.sidebarCollapsed}
            isDark={isDark}
            onToggleSidebar={toggleSidebar}
            onOpenMobileNav={() => setMobileNavOpen(true)}
            onOpenPalette={openPalette}
            onToggleTheme={toggleTheme}
          >
            <ConnectionMenu />
          </Topbar>
          <main id="main-content" className="min-h-0 flex-1 overflow-y-auto">
            <Outlet />
          </main>
          <StatusBar />
        </div>
      </div>
      <CommandPalette open={paletteOpen} onOpenChange={setPaletteOpen} deps={commandDeps} />
      <ShortcutsDialog open={shortcutsOpen} onClose={() => setShortcutsOpen(false)} />
    </div>
  )
}
