const TONES: Record<string, string> = {
  active: 'bg-emerald-950 text-emerald-300 border-emerald-800',
  succeeded: 'bg-emerald-950 text-emerald-300 border-emerald-800',
  ok: 'bg-emerald-950 text-emerald-300 border-emerald-800',
  pending: 'bg-amber-950 text-amber-300 border-amber-800',
  running: 'bg-sky-950 text-sky-300 border-sky-800',
  trashed: 'bg-amber-950 text-amber-300 border-amber-800',
  archived: 'bg-slate-800 text-slate-300 border-slate-700',
  failed: 'bg-red-950 text-red-300 border-red-800',
  cancelled: 'bg-slate-800 text-slate-400 border-slate-700',
  deleted: 'bg-red-950 text-red-300 border-red-800',
}

export default function StatusBadge({ status }: { status: string }) {
  const key = status.toLowerCase()
  const tone = TONES[key] ?? 'bg-slate-800 text-slate-300 border-slate-700'
  return (
    <span
      className={`inline-block rounded-full border px-2 py-0.5 text-xs font-medium ${tone}`}
    >
      {status}
    </span>
  )
}
