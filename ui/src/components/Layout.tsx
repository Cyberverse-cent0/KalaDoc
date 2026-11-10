import { NavLink, Outlet } from 'react-router-dom'
import { loadSettings } from '../lib/settings'
import ConnectionMenu from './ConnectionMenu'

const linkClass = ({ isActive }: { isActive: boolean }) =>
  `px-3 py-1.5 rounded-md text-sm font-medium ${
    isActive ? 'bg-slate-800 text-white' : 'text-slate-300 hover:bg-slate-800/60'
  }`

export default function Layout() {
  const { apiUrl } = loadSettings()
  return (
    <div className="min-h-screen bg-slate-950 text-slate-100">
      <header className="border-b border-slate-800 bg-slate-900">
        <div className="mx-auto flex max-w-6xl items-center gap-6 px-4 py-3">
          <span className="text-lg font-semibold tracking-tight">KalaDoc</span>
          <nav className="flex gap-1">
            <NavLink to="/documents" className={linkClass}>
              Documents
            </NavLink>
            <NavLink to="/jobs" className={linkClass}>
              Jobs
            </NavLink>
            <NavLink to="/engines" className={linkClass}>
              Engines
            </NavLink>
            <NavLink to="/pipelines" className={linkClass}>
              Pipelines
            </NavLink>
            <NavLink to="/settings" className={linkClass}>
              Settings
            </NavLink>
          </nav>
          <span className="ml-auto truncate text-xs text-slate-500" title={apiUrl}>
            {apiUrl}
          </span>
        </div>
      </header>
      <main className="mx-auto max-w-6xl px-4 py-6">
        <Outlet />
      </main>
      <ConnectionMenu />
    </div>
  )
}
