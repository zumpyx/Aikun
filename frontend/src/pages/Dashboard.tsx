// 仪表盘:统计卡 + 30 天趋势(纯 SVG)+ 模型分布 + 渠道状态表(dashboard.js 迁移)
import { useQuery } from '@tanstack/react-query'
import { api, type Page } from '@/lib/api'
import { useAuth } from '@/lib/auth'
import { fmtNum } from '@/lib/utils'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Table, TBody, TD, TH, THead, TR } from '@/components/ui/table'
import { Activity, Boxes, KeyRound, MessageSquare, Users, Zap } from 'lucide-react'

interface ProviderRow {
  id: string
  name: string
  provider_type: string
  models: string[]
  is_active: boolean
  health_status: string
  latency_ms: number
  priority: number
}
interface DailyPoint { date: string; requests: number; tokens: number; success_rate?: number; avg_latency_ms?: number }
interface ModelUsage { model: string; requests: number; tokens: number }
interface UsageStats {
  today: { requests: number; tokens: number }
  week: { requests: number; tokens: number }
  month: { requests: number; tokens: number }
  daily: DailyPoint[]
  top_models: ModelUsage[]
}

function Stat({ icon: Icon, value, label, color }: { icon: typeof Zap; value: string; label: string; color: string }) {
  return (
    <div className="flex items-center gap-3 rounded-lg border border-border bg-card p-4">
      <div className="flex size-10 items-center justify-center rounded-lg" style={{ color, background: `${color}1a` }}>
        <Icon className="size-5" />
      </div>
      <div className="min-w-0">
        <div className="text-xl font-bold leading-tight">{value}</div>
        <div className="truncate text-xs text-muted-foreground">{label}</div>
      </div>
    </div>
  )
}

function UsageBarChart({ daily }: { daily: DailyPoint[] }) {
  if (daily.length === 0) return <div className="py-10 text-center text-sm text-muted-foreground">暂无调用数据</div>
  const W = 920, H = 220, PT = 16, PB = 26, PL = 48, PR = 8
  const iw = W - PL - PR, ih = H - PT - PB
  const max = Math.max(1, ...daily.map((d) => d.requests))
  const bw = iw / daily.length
  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="block w-full" role="img" aria-label="近 30 天调用趋势">
      <defs>
        <linearGradient id="usage-bar-grad" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0" stopColor="#6366f1" />
          <stop offset="1" stopColor="#8b5cf6" />
        </linearGradient>
      </defs>
      {[0, 0.5, 1].map((f) => {
        const y = (PT + ih * (1 - f)).toFixed(1)
        return (
          <g key={f}>
            <line x1={PL} y1={y} x2={W - PR} y2={y} stroke="#e2e8f0" strokeWidth={1} strokeDasharray={f === 0 ? undefined : '3 4'} />
            <text x={PL - 6} y={+y + 3.5} textAnchor="end" fontSize={10} fill="#94a3b8">{fmtNum(Math.round(max * f))}</text>
          </g>
        )
      })}
      {daily.map((d, i) => {
        const h = (d.requests / max) * ih
        const x = PL + i * bw
        const y = PT + (ih - h)
        const tip = `${d.date}\n请求 ${fmtNum(d.requests)} · Token ${fmtNum(d.tokens)}\n成功率 ${(d.success_rate ?? 0).toFixed(1)}% · 平均延迟 ${fmtNum(d.avg_latency_ms)}ms`
        return <rect key={d.date} x={(x + 1).toFixed(1)} y={y.toFixed(1)} width={Math.max(1, bw - 3).toFixed(1)} height={Math.max(0, h).toFixed(1)} rx={2.5} fill="url(#usage-bar-grad)"><title>{tip}</title></rect>
      })}
      {[0, Math.floor(daily.length / 2), daily.length - 1].map((i) => {
        const d = daily[i]
        if (!d) return null
        return (
          <text key={d.date} x={(PL + i * bw + bw / 2).toFixed(1)} y={H - 8} textAnchor="middle" fontSize={10} fill="#94a3b8">
            {d.date.slice(5)}
          </text>
        )
      })}
    </svg>
  )
}

function ModelBars({ models }: { models: ModelUsage[] }) {
  if (models.length === 0) return <div className="py-10 text-center text-sm text-muted-foreground">暂无调用数据</div>
  const max = Math.max(1, ...models.map((m) => m.requests))
  return (
    <div className="space-y-3">
      {models.map((m) => (
        <div key={m.model}>
          <div className="mb-1 flex justify-between gap-2.5 text-xs">
            <code className="truncate">{m.model}</code>
            <span className="shrink-0 text-muted-foreground">{fmtNum(m.requests)} 次 · {fmtNum(m.tokens)} tokens</span>
          </div>
          <div className="h-2 overflow-hidden rounded-full bg-muted">
            <div className="h-full rounded-full bg-gradient-to-r from-indigo-500 to-violet-500" style={{ width: `${((m.requests / max) * 100).toFixed(1)}%` }} />
          </div>
        </div>
      ))}
    </div>
  )
}

const healthTone = (s: string) =>
  s === 'healthy' ? 'success' : s === 'degraded' ? 'warn' : s === 'unhealthy' ? 'destructive' : 'muted'

export default function Dashboard() {
  const { isAdmin } = useAuth()
  const providers = useQuery({
    queryKey: ['dashboard', 'providers'],
    queryFn: async () => (await api<Page<ProviderRow>>('GET', '/api/providers')).data.items ?? [],
  })
  const users = useQuery({
    queryKey: ['dashboard', 'users'],
    queryFn: async () => (await api<Page<{ id: string }>>('GET', '/api/users')).data.items ?? [],
  })
  const keys = useQuery({
    queryKey: ['dashboard', 'keys'],
    queryFn: async () => {
      if (isAdmin) return (await api<{ api_keys?: number }>('GET', '/api/system/stats')).data.api_keys ?? 0
      const r = await api<Page<unknown>>('GET', '/api/api-keys')
      return r.data.total ?? r.data.items?.length ?? 0
    },
  })
  const usage = useQuery({
    queryKey: ['dashboard', 'usage'],
    queryFn: async () => (await api<UsageStats>('GET', '/api/usage-stats')).data,
  })

  const pData = providers.data ?? []
  const u = usage.data
  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-xl font-bold">数据总览</h2>
        <p className="text-sm text-muted-foreground">系统运行状态与渠道健康一览</p>
      </div>
      <div className="grid grid-cols-2 gap-3 md:grid-cols-3 xl:grid-cols-4">
        <Stat icon={Boxes} value={fmtNum(pData.length)} label="上游渠道" color="#6366f1" />
        <Stat icon={Activity} value={fmtNum(pData.filter((p) => p.is_active && p.health_status === 'healthy').length)} label="健康渠道" color="#10b981" />
        <Stat icon={Zap} value={fmtNum(new Set(pData.flatMap((p) => p.models ?? [])).size)} label="可用模型" color="#8b5cf6" />
        <Stat icon={Users} value={fmtNum(users.data?.length ?? 0)} label="用户" color="#0ea5e9" />
        <Stat icon={KeyRound} value={fmtNum(keys.data ?? 0)} label={isAdmin ? 'API 密钥' : '我的密钥'} color="#f59e0b" />
        {u && (
          <>
            <Stat icon={MessageSquare} value={fmtNum(u.today.requests)} label={`今日调用 (UTC) · ${fmtNum(u.today.tokens)} tokens`} color="#f43f5e" />
            <Stat icon={MessageSquare} value={fmtNum(u.week.requests)} label={`近 7 天调用 (UTC) · ${fmtNum(u.week.tokens)} tokens`} color="#f97316" />
            <Stat icon={MessageSquare} value={fmtNum(u.month.requests)} label={`近 30 天调用 (UTC) · ${fmtNum(u.month.tokens)} tokens`} color="#14b8a6" />
          </>
        )}
      </div>
      {u && (
        <div className="grid gap-4 lg:grid-cols-5">
          <Card className="lg:col-span-3">
            <CardHeader className="flex-row items-center justify-between">
              <CardTitle>近 30 天调用趋势</CardTitle>
              <span className="text-xs text-muted-foreground">每日请求数</span>
            </CardHeader>
            <CardContent>
              <UsageBarChart daily={u.daily ?? []} />
            </CardContent>
          </Card>
          <Card className="lg:col-span-2">
            <CardHeader className="flex-row items-center justify-between">
              <CardTitle>模型调用分布</CardTitle>
              <span className="text-xs text-muted-foreground">近 30 天</span>
            </CardHeader>
            <CardContent>
              <ModelBars models={u.top_models ?? []} />
            </CardContent>
          </Card>
        </div>
      )}
      <Card>
        <CardHeader>
          <CardTitle>渠道状态</CardTitle>
        </CardHeader>
        <CardContent>
          {pData.length === 0 ? (
            <div className="py-10 text-center text-sm text-muted-foreground">暂无渠道，请先在「渠道」页添加</div>
          ) : (
            <Table>
              <THead>
                <TR>
                  <TH>名称</TH><TH>类型</TH><TH>模型</TH><TH>状态</TH><TH>延迟</TH><TH>优先级</TH>
                </TR>
              </THead>
              <TBody>
                {pData.map((p) => (
                  <TR key={p.id}>
                    <TD><strong>{p.name}</strong></TD>
                    <TD><Badge>{p.provider_type}</Badge></TD>
                    <TD className="max-w-64 truncate">{(p.models ?? []).slice(0, 3).join(', ')}{(p.models?.length ?? 0) > 3 ? '...' : ''}</TD>
                    <TD><Badge tone={healthTone(p.health_status)}>{p.health_status}</Badge></TD>
                    <TD>{p.latency_ms > 0 ? `${Math.round(p.latency_ms)}ms` : '-'}</TD>
                    <TD>{p.priority}</TD>
                  </TR>
                ))}
              </TBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  )
}
