import * as React from 'react'
import { cn } from '@/lib/utils'

export function Badge({
  className,
  tone = 'default',
  ...props
}: React.HTMLAttributes<HTMLSpanElement> & { tone?: 'default' | 'success' | 'destructive' | 'muted' | 'warn' }) {
  const tones: Record<string, string> = {
    default: 'bg-primary/10 text-primary',
    success: 'bg-emerald-100 text-emerald-700',
    destructive: 'bg-red-100 text-red-700',
    muted: 'bg-muted text-muted-foreground',
    warn: 'bg-amber-100 text-amber-700',
  }
  return (
    <span
      className={cn('inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium', tones[tone], className)}
      {...props}
    />
  )
}
