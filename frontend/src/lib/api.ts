// API 客户端:与管理面 problem+json 错误约定对齐(core.js 迁移)
import { sonnerToast } from './toast'

export interface ApiResult<T = Record<string, unknown>> {
  ok: boolean
  status: number
  data: T & { error?: string; message?: string; code?: string; detail?: string }
}

export const TOKEN_KEY = 'token'
export const getToken = () => localStorage.getItem(TOKEN_KEY)
export const clearToken = () => localStorage.removeItem(TOKEN_KEY)

export class UnauthorizedError extends Error {}

export async function api<T = Record<string, unknown>>(
  method: string,
  path: string,
  body?: unknown,
): Promise<ApiResult<T>> {
  const h: Record<string, string> = { 'Content-Type': 'application/json' }
  const token = getToken()
  if (token) h['Authorization'] = `Bearer ${token}`
  try {
    const res = await fetch(path, {
      method,
      headers: h,
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(30000),
    })
    let data: Record<string, unknown>
    try {
      data = (await res.json()) as Record<string, unknown>
    } catch {
      data = {}
    }
    if (data === null || typeof data !== 'object') data = {}
    if (!res.ok && !data.error && !data.message && !data.code && !data.detail)
      data = { error: 'unknown_error', code: 'unknown_error' }
    if (res.status === 401 && path !== '/api/auth/login') {
      clearToken()
      throw new UnauthorizedError('session expired')
    }
    return { ok: res.ok, status: res.status, data: data as ApiResult<T>['data'] }
  } catch (e) {
    if (e instanceof UnauthorizedError) throw e
    return { ok: false, status: 0, data: { error: 'network_error' } as ApiResult<T>['data'] }
  }
}

// 错误消息提取(problem+json 的 detail 优先)
export function errMsg(r: { data: { message?: string; detail?: string; error?: string; code?: string } }) {
  return r.data.detail || r.data.message || r.data.error || r.data.code || '操作失败'
}

// 查询列表信封
export interface Page<T> { items: T[]; total: number }

export function toastErr(r: ApiResult, prefix = '') {
  sonnerToast.error(prefix + errMsg(r))
}
