import { SHORTCUT_LIST } from '../lib/commands'

export default function ShortcutsDialog({
  open,
  onClose,
}: {
  open: boolean
  onClose: () => void
}) {
  if (!open) return null
  const groups = [...new Set(SHORTCUT_LIST.map((s) => s.group))]
  return (
    <div
      className="fixed inset-0 z-[60] flex items-center justify-center bg-black/50 p-4"
      onClick={onClose}
      role="dialog"
      aria-modal="true"
      aria-label="Keyboard shortcuts"
    >
      <div
        className="w-full max-w-md rounded-xl border border-border bg-card p-5 text-card-foreground shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between">
          <h2 className="text-base font-semibold">Keyboard shortcuts</h2>
          <button
            type="button"
            onClick={onClose}
            className="rounded-md px-2 py-1 text-sm text-muted-foreground hover:bg-muted"
            aria-label="Close"
          >
            Esc
          </button>
        </div>
        <div className="mt-4 space-y-4">
          {groups.map((group) => (
            <div key={group}>
              <p className="mb-1.5 text-xs font-medium uppercase tracking-wide text-faint-foreground">
                {group}
              </p>
              <ul className="space-y-1">
                {SHORTCUT_LIST.filter((s) => s.group === group).map((s) => (
                  <li key={s.keys + s.description} className="flex items-center justify-between text-sm">
                    <span className="text-muted-foreground">{s.description}</span>
                    <kbd className="rounded border border-border bg-muted px-1.5 py-0.5 font-mono text-[10px] text-faint-foreground">
                      {s.keys}
                    </kbd>
                  </li>
                ))}
              </ul>
            </div>
          ))}
        </div>
      </div>
    </div>
  )
}
