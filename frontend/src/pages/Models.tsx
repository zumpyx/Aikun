// 模型列表:/v1/models + admin 渠道健康矩阵(models.js 迁移)
import { useQuery } from '@tanstack/react-query'
import { api, type Page } from '@/lib/api'
import { useAuth } from '@/lib/auth'
import { Badge } from '@/components/ui/badge'
import { Card, CardContent } from '@/components/ui/card'
import { Table, TBody, TD, TH, THead, TR } from '@/components/ui/table'

interface ProviderRow {
  id: string
  name: string
  models: string[]
  is_active: boolean
  health_status: string
  latency_ms: number
  error_rate?: number | null
  disabled_reason?: string | null
  channel_kind?: string | null
}
interface HealthRow {
  provider_id: string
  model: string
  status: string
  checked_at?: string
  latency_ms: number
  error?: string | null
}

type CellKind = 'ok' | 'bad' | 'limited' | 'off'

function cellKind(p: ProviderRow, h?: HealthRow): CellKind {
  if (!p.is_active) return 'off'
  const err = `${h?.error ?? ''} ${p.disabled_reason ?? ''}`.toLowerCase()
  if (err.includes('429') || err.includes('rate') || err.includes('限速') || p.health_status === 'degraded') return 'limited'
  if (h) return h.status === 'healthy' ? 'ok' : 'bad'
  if (p.health_status === 'healthy') return 'ok'
  if (p.health_status === 'down' || p.health_status === 'unhealthy') return 'bad'
  return 'off'
}

function cellColor(k: CellKind): string {
  if (k === 'ok') return 'bg-emerald-500'
  if (k === 'bad') return 'bg-red-500'
  if (k === 'limited') return 'bg-amber-400'
  return 'bg-gray-300'
}

function healthPct(p: ProviderRow, h?: HealthRow): number {
  if (typeof p.error_rate === 'number' && Number.isFinite(p.error_rate)) {
    return Math.round((1 - Math.min(1, Math.max(0, p.error_rate))) * 100)
  }
  const k = cellKind(p, h)
  if (k === 'ok') return 100
  if (k === 'limited') return 50
  if (k === 'bad') return 0
  return 0
}

export default function Models() {
  const { isAdmin } = useAuth()
  const models = useQuery({
    queryKey: ['v1-models'],
    queryFn: async () => ((await api<{ data?: { id: string; owned_by: string }[] }>('GET', '/v1/models')).data.data ?? []),
  })
  const providers = useQuery({
    queryKey: ['providers'],
    queryFn: async () => (await api<Page<ProviderRow>>('GET', '/api/providers')).data.items ?? [],
    enabled: isAdmin,
  })
  const health = useQuery({
    queryKey: ['model-health'],
    queryFn: async () => (await api<Page<HealthRow>>('GET', '/api/model-health')).data.items ?? [],
    enabled: isAdmin,
  })

  const data = models.data ?? []
  if (models.isLoading) return <div className="py-10 text-center text-sm text-muted-foreground">加载中...</div>
  if (!models.data) return <div className="py-10 text-center text-sm text-muted-foreground">加载失败</div>

  if (!isAdmin || !providers.data) {
    return (
      <div className="space-y-4">
        <div><h2 className="text-xl font-bold">模型列表</h2><p className="text-sm text-muted-foreground">网关上所有可用模型</p></div>
        <Card><CardContent className="p-0">
          {data.length === 0 ? (
            <div className="py-10 text-center text-sm text-muted-foreground">暂无可用模型，请先配置上游渠道</div>
          ) : (
            <Table>
              <THead><TR><TH>模型 ID</TH><TH>来源</TH></TR></THead>
              <TBody>
                {data.map((m) => <TR key={m.id}><TD><code>{m.id}</code></TD><TD><Badge>{m.owned_by}</Badge></TD></TR>)}
              </TBody>
            </Table>
          )}
        </CardContent></Card>
      </div>
    )
  }

  const provMap: Record<string, ProviderRow[]> = {}
  for (const p of providers.data) for (const m of p.models ?? []) (provMap[m] ??= []).push(p)
  const healthMap: Record<string, HealthRow> = {}
  for (const h of health.data ?? []) healthMap[`${h.provider_id}|${h.model}`] = h

  return (
    <div className="space-y-4">
      <div><h2 className="text-xl font-bold">模型列表</h2><p className="text-sm text-muted-foreground">网关上所有可用模型</p></div>
      <div className="flex flex-wrap items-center gap-4 text-xs text-muted-foreground">
        <span className="flex items-center gap-1.5"><i className="size-2.5 rounded-[2px] bg-emerald-500" />正常</span>
        <span className="flex items-center gap-1.5"><i className="size-2.5 rounded-[2px] bg-amber-400" />限速</span>
        <span className="flex items-center gap-1.5"><i className="size-2.5 rounded-[2px] bg-red-500" />故障</span>
        <span className="flex items-center gap-1.5"><i className="size-2.5 rounded-[2px] bg-gray-300" />未测/禁用</span>
        <span className="ml-auto">共 {data.length} 个模型 · 悬停查看健康度</span>
      </div>
      <Card><CardContent className="p-0">
        <Table>
          <THead><TR><TH className="w-[42%]">模型 ID</TH><TH>渠道健康（每个方块代表一个支持渠道）</TH></TR></THead>
          <TBody>
            {data.map((m) => {
              const provs = provMap[m.id] ?? []
              return (
                <TR key={m.id}>
                  <TD><code>{m.id}</code></TD>
                  <TD>
                    {provs.length === 0 ? (
                      <span className="text-xs text-muted-foreground">无渠道</span>
                    ) : (
                      <div
                        className="inline-grid gap-px rounded border bg-background p-1"
                        style={{ gridTemplateColumns: `repeat(${Math.min(12, Math.max(4, Math.ceil(Math.sqrt(provs.length))))}, 10px)` }}
                      >
                        {provs.map((p) => {
                          const h = healthMap[`${p.id}|${m.id}`]
                          const kind = cellKind(p, h)
                          const pct = healthPct(p, h)
                          const tip = `${p.name}\n健康度 ${pct}%\n${kind === 'ok' ? '正常' : kind === 'limited' ? '限速' : kind === 'bad' ? '故障' : '未测/禁用'}${h?.latency_ms ? ` · ${Math.round(h.latency_ms)}ms` : ''}${h?.error ? `\n${h.error}` : ''}`
                          return (
                            <i
                              key={p.id}
                              title={tip}
                              className={`size-2.5 rounded-[1px] ${cellColor(kind)}`}
                            />
                          )
                        })}
                      </div>
                    )}
                  </TD>
                </TR>
              )
            })}
          </TBody>
        </Table>
      </CardContent></Card>
    </div>
  )
}
