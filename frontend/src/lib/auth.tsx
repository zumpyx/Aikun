// 登录态:token + 当前用户 + 版本号(core.js state 迁移)
import { createContext, useCallback, useContext, useEffect, useState } from 'react'
import { api, clearToken, getToken } from './api'

export interface CurrentUser {
  id: string
  username: string
  display_name?: string
  role: string
  balance?: number
}

interface AuthCtx {
  user: CurrentUser | null
  version: string | null
  ready: boolean
  isAdmin: boolean
  login: (token: string) => Promise<void>
  logout: () => void
}

const Ctx = createContext<AuthCtx | null>(null)

export function AuthProvider({ children }: { children: React.ReactNode }) {
  const [user, setUser] = useState<CurrentUser | null>(null)
  const [version, setVersion] = useState<string | null>(null)
  const [ready, setReady] = useState(false)

  useEffect(() => {
    if (!getToken()) {
      setReady(true)
      return
    }
    ;(async () => {
      const r = await api('GET', '/api/users/me')
      if (r.ok) {
        setUser(r.data as unknown as CurrentUser)
        const v = await api<{ version?: string }>('GET', '/api/system/version')
        if (v.ok && v.data.version) setVersion(v.data.version)
      } else {
        clearToken()
      }
      setReady(true)
    })()
  }, [])

  const login = useCallback(async (token: string) => {
    localStorage.setItem('token', token)
    const r = await api('GET', '/api/users/me')
    if (r.ok) {
      setUser(r.data as unknown as CurrentUser)
      const v = await api<{ version?: string }>('GET', '/api/system/version')
      if (v.ok && v.data.version) setVersion(v.data.version)
    }
  }, [])

  const logout = useCallback(() => {
    clearToken()
    setUser(null)
  }, [])

  return (
    <Ctx.Provider value={{ user, version, ready, isAdmin: user?.role === 'admin', login, logout }}>
      {children}
    </Ctx.Provider>
  )
}

export function useAuth() {
  const ctx = useContext(Ctx)
  if (!ctx) throw new Error('useAuth outside AuthProvider')
  return ctx
}
