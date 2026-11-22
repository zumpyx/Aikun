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
  // 剪贴板 API 需文档聚焦;IAB/失焦场景静默失败(写入空或不生效),
  // 故先试标准 API 再回退 execCommand——两者都校验"写入是否真的是
  // 目标文本"(读回比对),防止"提示已复制但剪贴板是旧值/掩码"。
  const modern = () =>
    navigator.clipboard?.writeText(text).then(async () => {
      try {
        return (await navigator.clipboard.readText()) === text
      } catch {
        // 读回无权限时无法自证,按成功处理(桌面端常态)
        return true
      }
    })
  const legacy = () => {
    const ta = document.createElement('textarea')
    ta.value = text
    ta.style.position = 'fixed'
    ta.style.opacity = '0'
    document.body.appendChild(ta)
    ta.focus()
    ta.select()
    let ok = false
    try {
      ok = document.execCommand('copy')
    } catch {
      ok = false
    }
    document.body.removeChild(ta)
    return ok
  }
  try {
    if ((await modern()) || legacy()) toast?.('已复制')
    else toast?.('复制失败，请手动复制', 'error')
  } catch {
    if (legacy()) toast?.('已复制')
    else toast?.('复制失败，请手动复制', 'error')
  }
}
