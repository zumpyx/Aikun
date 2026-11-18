// 日志审计:筛选 + 统计 chips + 分页列表(logs.js 迁移)
import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { api, type Page } from '@/lib/api'
import { fmtCost, fmtNum, fmtTime } from '@/lib/utils'
import { useAuth } from '@/lib/auth'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { Label } from '@/components/ui/label'
import { Table, TBody, TD, TH, THead, TR } from '@/components/ui/table'

const LOG_PAGE_SIZE = 200

interface LogRow {
  id: string
  created_at: string
  username?: string
  provider_name?: string
  model: string
  total_tokens: number
  cost: number
  latency_ms: number
  success: boolean
  error_message?: string | null
}
interface LogStats {
  total_requests: number
  total_tokens: number
  total_cost: number
  avg_latency_ms: number
  success_rate: number
}

const TIME_RANGES: [string, string, number][] = [
  ['', '全部时间', 0],
  ['1h', '最近 1 小时', 3600_000],
  ['24h', '最近 24 小时', 86_400_000],
  ['7d', '最近 7 天', 604_800_000],
  ['30d', '最近 30 天', 2_592_000_000],
]

function useFilterQuery(model: string, user: string, status: string, time: string, isAdmin: boolean) {
  let q = ''
  if (model) q += '&model=' + encodeURIComponent(model)
  if (status) q += '&success=' + status
  if (isAdmin && user) q += '&user_id=' + encodeURIComponent(user)
  const ms = TIME_RANGES.find(([v]) => v === time)?.[2] ?? 0
  if (ms > 0) q += '&since=' + encodeURIComponent(new Date(Date.now() - ms).toISOString())
  return q
}

export default function Logs() {
  const { isAdmin } = useAuth()
  const [model, setModel] = useState('')
  const [user, setUser] = useState('')
  const [status, setStatus] = useState('')
  const [time, setTime] = useState('')
  const [applied, setApplied] = useState({ model: '', user: '', status: '', time: '' })
  const fq = useFilterQuery(applied.model, applied.user, applied.status, applied.time, isAdmin)

  const models = useQuery({
    queryKey: ['v1-models'],
    queryFn: async () => ((await api<{ data?: { id: string }[] }>('GET', '/v1/models')).data.data ?? []),
    staleTime: 5 * 60_000,
  })
  const users = useQuery({
    queryKey: ['users'],
    queryFn: async () => (await api<Page<{ id: string; username: string }>>('GET', '/api/users')).data.items ?? [],
    enabled: isAdmin,
  })
  const stats = useQuery({
    queryKey: ['log-stats', fq],
    queryFn: async () => (await api<LogStats>('GET', `/api/logs/stats?${'x'.repeat(0)}${fq.replace(/^&/, '')}`)).data,
  })
  const logs = useQuery({
    queryKey: ['logs', fq],
    queryFn: async () => (await api<Page<LogRow>>('GET', `/api/logs?limit=${LOG_PAGE_SIZE}${fq}`)).data,
  })

  const chip = (label: string, value: string) => (
    <span key={label} className="inline-flex items-center gap-1.5 rounded-lg border border-border bg-card px-3.5 py-1.5 text-xs text-muted-foreground shadow-sm">
      {label} <strong className="text-sm text-foreground">{value}</strong>
    </span>
  )
  const s = stats.data

  return (
    <div className="space-y-3.5">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-xl font-bold">日志审计</h2>
          <p className="text-sm text-muted-foreground">请求调用记录与统计</p>
        </div>
        <Button variant="outline" size="sm" onClick={() => { void stats.refetch(); void logs.refetch() }}>刷新</Button>
      </div>
      <Card>
        <CardContent className="flex flex-wrap items-end gap-3 p-4">
          <div className="min-w-40 flex-1 space-y-1">
            <Label>模型</Label>
            <select className="h-8 w-full rounded-md border border-border bg-card px-2 text-sm" value={model} onChange={(e) => setModel(e.target.value)}>
              <option value="">全部模型</option>
              {(models.data ?? []).map((m) => <option key={m.id} value={m.id}>{m.id}</option>)}
            </select>
          </div>
          {isAdmin && (
            <div className="min-w-40 flex-1 space-y-1">
              <Label>用户</Label>
              <select className="h-8 w-full rounded-md border border-border bg-card px-2 text-sm" value={user} onChange={(e) => setUser(e.target.value)}>
                <option value="">全部用户</option>
                {(users.data ?? []).map((u) => <option key={u.id} value={u.id}>{u.username}</option>)}
              </select>
            </div>
          )}
          <div className="min-w-32 space-y-1">
            <Label>状态</Label>
            <select className="h-8 w-full rounded-md border border-border bg-card px-2 text-sm" value={status} onChange={(e) => setStatus(e.target.value)}>
              <option value="">全部状态</option>
              <option value="1">成功</option>
              <option value="0">失败</option>
            </select>
          </div>
          <div className="min-w-36 space-y-1">
            <Label>时间范围</Label>
            <select className="h-8 w-full rounded-md border border-border bg-card px-2 text-sm" value={time} onChange={(e) => setTime(e.target.value)}>
              {TIME_RANGES.map(([v, label]) => <option key={v} value={v}>{label}</option>)}
            </select>
          </div>
          <Button size="sm" className="h-8" onClick={() => setApplied({ model, user, status, time })}>筛选</Button>
        </CardContent>
      </Card>
      {s && (
        <div className="flex flex-wrap gap-2.5">
          {chip('请求数量', fmtNum(s.total_requests ?? 0))}
          {chip('消耗 Token', fmtNum(s.total_tokens ?? 0))}
          {chip('消费(元)', fmtCost(s.total_cost))}
          {chip('平均延迟', `${Math.round(s.avg_latency_ms ?? 0)}ms`)}
          {chip('成功率', `${(s.success_rate ?? 0).toFixed(1)}%`)}
        </div>
      )}
      <Card>
        <CardContent className="p-0">
          {(logs.data?.items ?? []).length === 0 ? (
            <div className="py-10 text-center text-sm text-muted-foreground">{logs.isLoading ? '加载中...' : '暂无匹配的调用记录'}</div>
          ) : (
            <Table>
              <THead>
                <TR>
                  <TH>时间</TH>
                  {isAdmin && <TH>用户</TH>}
                  <TH>渠道</TH><TH>模型</TH><TH>Token</TH><TH>费用(元)</TH><TH>延迟</TH><TH>状态</TH><TH>错误</TH>
                </TR>
              </THead>
              <TBody>
                {(logs.data?.items ?? []).map((l) => (
                  <TR key={l.id}>
                    <TD className="whitespace-nowrap text-xs text-muted-foreground">{fmtTime(l.created_at)}</TD>
                    {isAdmin && <TD className="text-xs">{l.username || '已删除'}</TD>}
                    <TD className="text-xs">{l.provider_name || '已删除'}</TD>
                    <TD><code className="text-xs">{l.model}</code></TD>
                    <TD>{fmtNum(l.total_tokens)}</TD>
                    <TD>{fmtCost(l.cost)}</TD>
                    <TD>{l.latency_ms}ms</TD>
                    <TD><Badge tone={l.success ? 'success' : 'destructive'}>{l.success ? '成功' : '失败'}</Badge></TD>
                    <TD className="max-w-40 truncate text-xs text-destructive">{l.error_message || '-'}</TD>
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
