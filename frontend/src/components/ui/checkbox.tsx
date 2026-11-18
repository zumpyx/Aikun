import * as React from 'react'
import { cn } from '@/lib/utils'

// 行多选用的原生 checkbox(tailwind 样式;无 radix 依赖)
export const Checkbox = React.forwardRef<
  HTMLInputElement,
  React.InputHTMLAttributes<HTMLInputElement>
>(({ className, ...props }, ref) => (
  <input
    type="checkbox"
    ref={ref}
    className={cn(
      'size-4 shrink-0 cursor-pointer rounded border border-border accent-[hsl(221_83%_53%)]',
      className,
    )}
    {...props}
  />
))
Checkbox.displayName = 'Checkbox'
