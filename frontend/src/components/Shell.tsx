// 侧栏布局:导航 + 用户区(core.js renderLayout 迁移;admin 项按角色显隐)
import { NavLink, useNavigate } from 'react-router-dom'
import {
  Activity, ArrowLeftRight, BookOpen, Coins, Gauge, KeyRound, ScrollText,
  Settings2, Users, Zap, FlaskConical, Boxes,
} from 'lucide-react'
import { useAuth } from '@/lib/auth'
import { truncBytes } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { sonnerToast } from '@/lib/toast'

const NAV = [
  { to: '/', label: '面板', icon: Gauge, admin: false },
  { to: '/wallet', label: '钱包', icon: Coins, admin: false },
  { to: '/keys', label: '密钥', icon: KeyRound, admin: false },
  { to: '/logs', label: '日志', icon: ScrollText, admin: false },
  { to: '/docs', label: '文档', icon: BookOpen, admin: false },
  { to: '/providers', label: '渠道', icon: Boxes, admin: true },
  { to: '/proxies', label: '反代', icon: ArrowLeftRight, admin: true },
  { to: '/models', label: '模型', icon: Zap, admin: true },
  { to: '/test', label: '测试', icon: FlaskConical, admin: true },
  { to: '/users', label: '用户', icon: Users, admin: true },
  { to: '/billing', label: '价格', icon: Activity, admin: true },
]

export function Shell({ children }: { children: React.ReactNode }) {
  const { user, version, isAdmin, logout } = useAuth()
  const nav = useNavigate()
  return (
    <div className="flex min-h-screen">
      <aside className="flex w-52 shrink-0 flex-col border-r border-border bg-card">
        <div className="flex items-center gap-2 px-4 py-4 border-b border-border">
          <div className="flex size-8 items-center justify-center rounded-md bg-primary text-primary-foreground">
            <Settings2 className="size-4" />
          </div>
          <div>
            <div className="text-sm font-bold leading-tight">Aikun</div>
            <div className="text-[10px] text-muted-foreground">AI Gateway {version ? `v${version}` : ''}</div>
          </div>
        </div>
        <nav className="flex-1 overflow-y-auto p-2 space-y-0.5">
          {NAV.filter((n) => !n.admin || isAdmin).map((n) => (
            <NavLink
              key={n.to}
              to={n.to}
              end={n.to === '/'}
              className={({ isActive }) =>
                `flex items-center gap-2.5 rounded-md px-3 py-2 text-sm transition-colors ${
                  isActive ? 'bg-primary/10 text-primary font-medium' : 'text-muted-foreground hover:bg-muted hover:text-foreground'
                }`
              }
            >
              <n.icon className="size-4" />
              {n.label}
            </NavLink>
          ))}
        </nav>
        <div className="border-t border-border p-3">
          <div className="mb-2 truncate text-xs text-muted-foreground" title={user?.display_name || user?.username}>
            {truncBytes(user?.display_name || user?.username, 8)} · {user?.role}
          </div>
          <Button
            variant="outline"
            size="sm"
            className="w-full"
            onClick={() => {
              logout()
              sonnerToast.success('已退出登录')
              nav('/')
            }}
          >
            退出登录
          </Button>
        </div>
      </aside>
      <main className="flex-1 overflow-x-hidden p-6">{children}</main>
    </div>
  )
}
