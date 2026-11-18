// 登录页(core.js renderLogin 迁移)
import { useState } from 'react'
import { api, errMsg } from '@/lib/api'
import { useAuth } from '@/lib/auth'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Card, CardContent } from '@/components/ui/card'
import { Zap } from 'lucide-react'
import { sonnerToast } from '@/lib/toast'

export function LoginForm() {
  const { login } = useAuth()
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [busy, setBusy] = useState(false)

  const submit = async () => {
    setBusy(true)
    try {
      const r = await api<{ token?: string }>('POST', '/api/auth/login', { username, password })
      if (r.ok && r.data.token) {
        await login(r.data.token)
        sonnerToast.success('登录成功')
      } else {
        sonnerToast.error(errMsg(r) || '登录失败')
      }
    } finally {
      setBusy(false)
    }
  }

  return (
    <Card className="w-full max-w-sm">
      <CardContent className="p-8">
        <div className="mb-6 flex flex-col items-center gap-2 text-center">
          <div className="flex size-12 items-center justify-center rounded-xl bg-primary text-primary-foreground">
            <Zap className="size-6" />
          </div>
          <h1 className="text-xl font-bold">Aikun</h1>
          <p className="text-sm text-muted-foreground">统一管理你的 AI 服务渠道</p>
        </div>
        <form
          className="space-y-4"
          onSubmit={(e) => {
            e.preventDefault()
            void submit()
          }}
        >
          <div className="space-y-1.5">
            <Label htmlFor="login-user">用户名</Label>
            <Input id="login-user" value={username} onChange={(e) => setUsername(e.target.value)} placeholder="用户名" autoFocus />
          </div>
          <div className="space-y-1.5">
            <Label htmlFor="login-pass">密码</Label>
            <Input id="login-pass" type="password" value={password} onChange={(e) => setPassword(e.target.value)} placeholder="密码" />
          </div>
          <Button type="submit" className="w-full" disabled={busy}>
            {busy ? '登录中…' : '登 录'}
          </Button>
        </form>
        <p className="mt-4 text-center text-xs text-muted-foreground">账号由管理员创建，请联系管理员获取</p>
      </CardContent>
    </Card>
  )
}
