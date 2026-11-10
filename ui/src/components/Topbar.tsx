import { useLocation } from 'react-router-dom'
import { Menu, PanelLeft, Search, Moon, Sun } from 'lucide-react'

const TITLES: [prefix: string, title: string][] = [
  ['/documents', 'Documents'],
  ['/jobs', 'Jobs'],
  ['/engines', 'Engines'],
  ['/pipelines', 'Pipelines'],
  ['/settings', 'Settings'],
]

export function pageTitle(pathname: string): string {
  for (const [prefix, title] of TITLES) {
    if (pathname === prefix || pathname.startsWith(prefix + '/')) return title
  }
  return 'KalaDoc'
}

export default function Topbar({
  collapsed,
  isDark,
  onToggleSidebar,
  onOpenMobileNav,
  onOpenPalette,
  onToggleTheme,
  children,
}: {
  collapsed: boolean
  isDark: boolean
  onToggleSidebar: () => void
  onOpenMobileNav: () => void
  onOpenPalette: () => void
  onToggleTheme: () => void
  children?: React.ReactNode
}) {
  const { pathname } = useLocation()
  return (
    <header className="flex h-12 shrink-0 items-center gap-2 border-b border-border bg-card px-3">
      <button
        type="button"
        onClick={onOpenMobileNav}
        className="rounded-md p-1.5 text-muted-foreground hover:bg-muted md:hidden"
        aria-label="Open navigation"
      >
        <Menu className="h-4.5 w-4.5" />
      </button>
      <button
        type="button"
        onClick={onToggleSidebar}
        className="hidden rounded-md p-1.5 text-muted-foreground hover:bg-muted md:block"
        aria-label={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}
        title="Toggle sidebar (Ctrl+B)"
      >
        <PanelLeft className="h-4.5 w-4.5" />
      </button>
      <span className="text-sm font-medium">{pageTitle(pathname)}</span>
      <div className="ml-auto flex items-center gap-1.5">
        <button
          type="button"
          onClick={onOpenPalette}
          className="hidden items-center gap-2 rounded-md border border-border bg-muted px-2.5 py-1.5 text-xs text-faint-foreground hover:bg-border-strong sm:flex"
        >
          <Search className="h-3.5 w-3.5" />
          <span>Search or command</span>
          <kbd className="rounded border border-border bg-card px-1 font-mono text-[10px]">⌃K</kbd>
        </button>
        <button
          type="button"
          onClick={onOpenPalette}
          className="rounded-md p-1.5 text-muted-foreground hover:bg-muted sm:hidden"
          aria-label="Open command palette"
        >
          <Search className="h-4.5 w-4.5" />
        </button>
        <button
          type="button"
          onClick={onToggleTheme}
          className="rounded-md p-1.5 text-muted-foreground hover:bg-muted"
          aria-label={isDark ? 'Switch to light theme' : 'Switch to dark theme'}
          title={isDark ? 'Light theme' : 'Dark theme'}
        >
          {isDark ? <Sun className="h-4.5 w-4.5" /> : <Moon className="h-4.5 w-4.5" />}
        </button>
        {children}
      </div>
    </header>
  )
}
