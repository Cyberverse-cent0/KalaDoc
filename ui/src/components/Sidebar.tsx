import { NavLink } from 'react-router-dom'
import { FileText, ListChecks, Cpu, Workflow, Settings, type LucideIcon } from 'lucide-react'

const NAV: { to: string; label: string; icon: LucideIcon; end?: boolean }[] = [
  { to: '/documents', label: 'Documents', icon: FileText },
  { to: '/jobs', label: 'Jobs', icon: ListChecks },
  { to: '/engines', label: 'Engines', icon: Cpu },
  { to: '/pipelines', label: 'Pipelines', icon: Workflow },
  { to: '/settings', label: 'Settings', icon: Settings },
]

function NavItems({ collapsed, onNavigate }: { collapsed: boolean; onNavigate?: () => void }) {
  return (
    <nav className="flex flex-col gap-0.5 px-2" aria-label="Primary">
      {NAV.map(({ to, label, icon: Icon }) => (
        <NavLink
          key={to}
          to={to}
          onClick={onNavigate}
          title={collapsed ? label : undefined}
          className={({ isActive }) =>
            `flex items-center gap-2.5 rounded-md px-2.5 py-1.5 text-[13px] font-medium motion-safe-transition ${
              collapsed ? 'justify-center' : ''
            } ${
              isActive
                ? 'bg-primary-subtle text-primary'
                : 'text-muted-foreground hover:bg-muted hover:text-foreground'
            }`
          }
        >
          <Icon className="h-4 w-4 shrink-0" />
          {!collapsed && <span className="truncate">{label}</span>}
        </NavLink>
      ))}
    </nav>
  )
}

export function SidebarBrand({ collapsed }: { collapsed: boolean }) {
  return (
    <div className={`flex items-center gap-2 px-3 py-3 ${collapsed ? 'justify-center' : ''}`}>
      <span className="flex h-7 w-7 shrink-0 items-center justify-center rounded-md bg-primary text-primary-foreground">
        <FileText className="h-4 w-4" />
      </span>
      {!collapsed && (
        <span className="text-[15px] font-semibold tracking-tight">KalaDoc</span>
      )}
    </div>
  )
}

export default function Sidebar({
  collapsed,
  mobileOpen,
  onCloseMobile,
}: {
  collapsed: boolean
  mobileOpen: boolean
  onCloseMobile: () => void
}) {
  const width = collapsed ? 'w-16' : 'w-60'
  return (
    <>
      {/* Desktop sidebar */}
      <aside
        className={`hidden shrink-0 flex-col border-r border-border bg-card transition-[width] duration-200 md:flex ${width}`}
      >
        <SidebarBrand collapsed={collapsed} />
        <div className="flex-1 overflow-y-auto py-2">
          <NavItems collapsed={collapsed} />
        </div>
        <div className="border-t border-border p-2">
          <p
            className={`px-2.5 py-1 text-[11px] text-faint-foreground ${collapsed ? 'hidden' : ''}`}
          >
            Ctrl K for commands
          </p>
        </div>
      </aside>

      {/* Mobile drawer */}
      {mobileOpen && (
        <div className="fixed inset-0 z-40 md:hidden">
          <div className="absolute inset-0 bg-black/50" onClick={onCloseMobile} />
          <aside className="absolute left-0 top-0 flex h-full w-64 flex-col border-r border-border bg-card">
            <SidebarBrand collapsed={false} />
            <div className="flex-1 overflow-y-auto py-2">
              <NavItems collapsed={false} onNavigate={onCloseMobile} />
            </div>
          </aside>
        </div>
      )}
    </>
  )
}
