import { clsx, type ClassValue } from 'clsx'
import { twMerge } from 'tailwind-merge'

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}

// 千分位
export const fmtNum = (n: unknown) => Number(n ?? 0).toLocaleString('en-US')
// 金额:6 位小数去尾零
export const fmtCost = (n: unknown) => {
  const v = Number(n ?? 0)
  if (!v) return '0'
  return v.toFixed(6).replace(/\.?0+$/, '')
}
// 时间:兼容 RFC3339 与 'YYYY-MM-DD HH:MM:SS'
export const fmtTime = (v?: string | null) => {
  if (!v) return ''
  const s = String(v)
  const d = new Date(s.includes('T') ? s : s.replace(' ', 'T'))
  return Number.isNaN(d.getTime()) ? s : d.toLocaleString()
}
// 字节截断(昵称 8 字节)
export const truncBytes = (s: unknown, max: number) => {
  let bytes = 0
  let out = ''
  for (const ch of String(s ?? '')) {
    const b = new TextEncoder().encode(ch).length
    if (bytes + b > max) break
    bytes += b
    out += ch
  }
  return out || '?'
}
export async function copyText(text: string, toast?: (m: string, t?: 'success' | 'error') => void) {
  try {
    await navigator.clipboard.writeText(text)
    toast?.('已复制')
  } catch {
    toast?.('复制失败，请手动复制', 'error')
  }
}
