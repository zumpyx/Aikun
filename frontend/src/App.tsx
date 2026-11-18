// 应用外壳:登录门卫 + 侧栏布局 + 路由(admin 页面按角色守卫)
import { Suspense, lazy } from 'react'
import { Navigate, Route, Routes, useLocation } from 'react-router-dom'
import { LoginForm } from './pages/Login'
import { Shell } from './components/Shell'
import { useAuth } from './lib/auth'
import { Loader2 } from 'lucide-react'

const Dashboard = lazy(() => import('./pages/Dashboard'))
const Wallet = lazy(() => import('./pages/Wallet'))
const Providers = lazy(() => import('./pages/Providers'))
const ProxyProviders = lazy(() => import('./pages/ProxyProviders'))
const Users = lazy(() => import('./pages/Users'))
const Billing = lazy(() => import('./pages/Billing'))
const Keys = lazy(() => import('./pages/Keys'))
const Models = lazy(() => import('./pages/Models'))
const Test = lazy(() => import('./pages/Test'))
const Logs = lazy(() => import('./pages/Logs'))
const Docs = lazy(() => import('./pages/Docs'))

function Loading() {
  return (
    <div className="flex h-64 items-center justify-center">
      <Loader2 className="size-6 animate-spin text-muted-foreground" />
    </div>
  )
}

function AdminRoute({ children }: { children: React.ReactNode }) {
  const { isAdmin } = useAuth()
  if (!isAdmin) return <Navigate to="/" replace />
  return <>{children}</>
}

function LoginGate() {
  const { ready, user } = useAuth()
  const loc = useLocation()
  if (!ready) return <Loading />
  if (!user)
    return (
      <div className="flex min-h-screen items-center justify-center">
        <LoginForm />
      </div>
    )
  return (
    <Shell>
      <Suspense fallback={<Loading />}>
        <Routes key={loc.pathname}>
          <Route path="/" element={<Dashboard />} />
          <Route path="/wallet" element={<Wallet />} />
          <Route path="/keys" element={<Keys />} />
          <Route path="/logs" element={<Logs />} />
          <Route path="/docs" element={<Docs />} />
          <Route path="/providers" element={<AdminRoute><Providers /></AdminRoute>} />
          <Route path="/proxies" element={<AdminRoute><ProxyProviders /></AdminRoute>} />
          <Route path="/proxies/:app" element={<AdminRoute><ProxyProviders /></AdminRoute>} />
          <Route path="/users" element={<AdminRoute><Users /></AdminRoute>} />
          <Route path="/billing" element={<AdminRoute><Billing /></AdminRoute>} />
          <Route path="/models" element={<AdminRoute><Models /></AdminRoute>} />
          <Route path="/test" element={<AdminRoute><Test /></AdminRoute>} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </Suspense>
    </Shell>
  )
}

export default function App() {
  return (
    <Routes>
      <Route path="/*" element={<LoginGate />} />
    </Routes>
  )
}
