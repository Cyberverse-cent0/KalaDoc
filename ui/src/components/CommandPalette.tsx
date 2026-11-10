import { useMemo } from 'react'
import { Command } from 'cmdk'
import { Search } from 'lucide-react'
import { buildCommands, type CommandDeps } from '../lib/commands'

export default function CommandPalette({
  open,
  onOpenChange,
  deps,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  deps: CommandDeps
}) {
  const commands = useMemo(() => buildCommands(deps), [deps])
  const groups = ['Navigate', 'Actions', 'View'] as const

  return (
    <Command.Dialog
      open={open}
      onOpenChange={onOpenChange}
      label="Command palette"
      className="fixed inset-0 z-[60] flex items-start justify-center bg-black/50 pt-[15vh]"
    >
      <div className="w-full max-w-lg overflow-hidden rounded-xl border border-border bg-card text-card-foreground shadow-2xl">
        <div className="flex items-center gap-2 border-b border-border px-3">
          <Search className="h-4 w-4 shrink-0 text-faint-foreground" />
          <Command.Input
            autoFocus
            placeholder="Search commands…"
            className="h-11 w-full bg-transparent text-sm outline-none placeholder:text-faint-foreground"
          />
        </div>
        <Command.List className="max-h-80 overflow-y-auto p-2">
          <Command.Empty className="px-3 py-6 text-center text-sm text-muted-foreground">
            No results.
          </Command.Empty>
          {groups.map((group) => {
            const items = commands.filter((c) => c.group === group)
            if (items.length === 0) return null
            return (
              <Command.Group
                key={group}
                heading={group}
                className="px-1 pb-1 text-xs font-medium text-faint-foreground [&_[cmdk-group-heading]]:px-2 [&_[cmdk-group-heading]]:py-1.5"
              >
                {items.map((cmd) => {
                  const Icon = cmd.icon
                  return (
                    <Command.Item
                      key={cmd.id}
                      value={`${cmd.title} ${cmd.keywords ?? ''}`}
                      onSelect={() => {
                        onOpenChange(false)
                        cmd.run()
                      }}
                      className="flex cursor-pointer items-center gap-2.5 rounded-md px-2 py-2 text-sm text-card-foreground aria-selected:bg-primary-subtle aria-selected:text-primary"
                    >
                      <Icon className="h-4 w-4 shrink-0 text-faint-foreground" />
                      <span className="flex-1 truncate">{cmd.title}</span>
                      {cmd.shortcut && (
                        <kbd className="rounded border border-border bg-muted px-1.5 py-0.5 font-mono text-[10px] text-faint-foreground">
                          {cmd.shortcut}
                        </kbd>
                      )}
                    </Command.Item>
                  )
                })}
              </Command.Group>
            )
          })}
        </Command.List>
      </div>
    </Command.Dialog>
  )
}
