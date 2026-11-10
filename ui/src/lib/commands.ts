import {
  FileText,
  ListChecks,
  Cpu,
  Workflow,
  Settings,
  Moon,
  Sun,
  PanelLeft,
  Keyboard,
  Upload,
  type LucideIcon,
} from 'lucide-react'

export interface AppCommand {
  id: string
  title: string
  group: 'Navigate' | 'Actions' | 'View'
  keywords?: string
  shortcut?: string
  icon: LucideIcon
  run: () => void
}

export interface CommandDeps {
  navigate: (path: string) => void
  togglePalette: () => void
  toggleShortcuts: () => void
  toggleSidebar: () => void
  toggleTheme: () => void
  isDark: boolean
}

export function buildCommands(deps: CommandDeps): AppCommand[] {
  const { navigate } = deps
  return [
    {
      id: 'nav-documents',
      title: 'Go to Documents',
      group: 'Navigate',
      keywords: 'files library',
      shortcut: 'g d',
      icon: FileText,
      run: () => navigate('/documents'),
    },
    {
      id: 'nav-jobs',
      title: 'Go to Jobs',
      group: 'Navigate',
      keywords: 'queue processing batch',
      shortcut: 'g j',
      icon: ListChecks,
      run: () => navigate('/jobs'),
    },
    {
      id: 'nav-engines',
      title: 'Go to Engines',
      group: 'Navigate',
      keywords: 'system health status',
      shortcut: 'g e',
      icon: Cpu,
      run: () => navigate('/engines'),
    },
    {
      id: 'nav-pipelines',
      title: 'Go to Pipelines',
      group: 'Navigate',
      keywords: 'workflows automation',
      shortcut: 'g p',
      icon: Workflow,
      run: () => navigate('/pipelines'),
    },
    {
      id: 'nav-settings',
      title: 'Go to Settings',
      group: 'Navigate',
      keywords: 'preferences account api',
      shortcut: 'g s',
      icon: Settings,
      run: () => navigate('/settings'),
    },
    {
      id: 'action-upload',
      title: 'Upload document',
      group: 'Actions',
      keywords: 'file new add',
      icon: Upload,
      run: () => navigate('/documents'),
    },
    {
      id: 'view-theme',
      title: deps.isDark ? 'Switch to light theme' : 'Switch to dark theme',
      group: 'View',
      keywords: 'appearance dark light mode',
      icon: deps.isDark ? Sun : Moon,
      run: deps.toggleTheme,
    },
    {
      id: 'view-sidebar',
      title: 'Toggle sidebar',
      group: 'View',
      shortcut: 'Ctrl B',
      icon: PanelLeft,
      run: deps.toggleSidebar,
    },
    {
      id: 'view-shortcuts',
      title: 'Keyboard shortcuts',
      group: 'View',
      keywords: 'help keys hotkeys',
      shortcut: 'Ctrl /',
      icon: Keyboard,
      run: deps.toggleShortcuts,
    },
  ]
}

/** Static list for the shortcuts cheat sheet (Ctrl+/ or ?). */
export const SHORTCUT_LIST: { keys: string; description: string; group: string }[] = [
  { group: 'General', keys: 'Ctrl K', description: 'Command palette' },
  { group: 'General', keys: 'Ctrl /  or  ?', description: 'Keyboard shortcuts' },
  { group: 'General', keys: 'Ctrl B', description: 'Toggle sidebar' },
  { group: 'General', keys: 'Esc', description: 'Close dialog / palette' },
  { group: 'Navigate', keys: 'g then d', description: 'Documents' },
  { group: 'Navigate', keys: 'g then j', description: 'Jobs' },
  { group: 'Navigate', keys: 'g then p', description: 'Pipelines' },
  { group: 'Navigate', keys: 'g then e', description: 'Engines' },
  { group: 'Navigate', keys: 'g then s', description: 'Settings' },
]
