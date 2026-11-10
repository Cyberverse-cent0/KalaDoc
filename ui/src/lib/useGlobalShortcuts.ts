import { useEffect, useRef } from 'react'

interface ShortcutHandlers {
  onPalette?: () => void
  onShortcuts?: () => void
  onToggleSidebar?: () => void
  onNavigate?: (path: string) => void
}

function isEditableTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false
  return (
    target.isContentEditable ||
    target instanceof HTMLInputElement ||
    target instanceof HTMLTextAreaElement ||
    target instanceof HTMLSelectElement
  )
}

/**
 * Global keyboard layer:
 *  - Ctrl/Cmd+K      command palette (works even inside inputs)
 *  - Ctrl+/ or ?     shortcuts cheat sheet
 *  - Ctrl+B          toggle sidebar
 *  - g then <key>    navigation chords: d=documents j=jobs p=pipelines e=engines s=settings
 */
export function useGlobalShortcuts(handlers: ShortcutHandlers) {
  const ref = useRef(handlers)
  ref.current = handlers

  useEffect(() => {
    let pendingG = 0
    let gTimer: ReturnType<typeof setTimeout> | null = null

    function clearG() {
      pendingG = 0
      if (gTimer) clearTimeout(gTimer)
      gTimer = null
    }

    function onKeyDown(e: KeyboardEvent) {
      const mod = e.ctrlKey || e.metaKey

      if (mod && e.key.toLowerCase() === 'k') {
        e.preventDefault()
        ref.current.onPalette?.()
        return
      }

      if (isEditableTarget(e.target)) return

      if (mod && e.key === '/') {
        e.preventDefault()
        ref.current.onShortcuts?.()
        return
      }
      if (!mod && e.key === '?' && !e.ctrlKey && !e.metaKey && !e.altKey) {
        e.preventDefault()
        ref.current.onShortcuts?.()
        return
      }
      if (mod && e.key.toLowerCase() === 'b') {
        e.preventDefault()
        ref.current.onToggleSidebar?.()
        return
      }

      // g-chords
      if (!mod && !e.altKey && e.key.toLowerCase() === 'g') {
        pendingG = Date.now()
        gTimer = setTimeout(clearG, 900)
        return
      }
      if (pendingG && Date.now() - pendingG < 900) {
        clearG()
        const routes: Record<string, string> = {
          d: '/documents',
          j: '/jobs',
          p: '/pipelines',
          e: '/engines',
          s: '/settings',
        }
        const path = routes[e.key.toLowerCase()]
        if (path) {
          e.preventDefault()
          ref.current.onNavigate?.(path)
        }
      }
    }

    window.addEventListener('keydown', onKeyDown)
    return () => {
      window.removeEventListener('keydown', onKeyDown)
      clearG()
    }
  }, [])
}
