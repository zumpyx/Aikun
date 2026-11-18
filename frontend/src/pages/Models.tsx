// 模型列表:/v1/models + admin 渠道健康矩阵(models.js 迁移)
import { useQuery } from '@tanstack/react-query'
import { api, type Page } from '@/lib/api'
import { fmtTime } from '@/lib/utils'
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
}
interface HealthRow {
  provider_id: string
  model: string
  status: string
  checked_at?: string
  latency_ms: number
  error?: string | null
}

function cellTone(active: boolean, health?: string) {
  if (!active) return 'bg-gray-300'
  if (health === 'healthy') return 'bg-emerald-500'
  if (health === 'unhealthy') return 'bg-red-500'
  if (health) return 'bg-gray-300'
  return undefined
}
function fallbackTone(p: ProviderRow) {
  if (!p.is_active) return 'bg-gray-300'
  if (p.health_status === 'healthy') return 'bg-emerald-500'
  if (p.health_status === 'degraded') return 'bg-amber-400'
  if (p.health_status === 'down') return 'bg-red-500'
  return 'bg-gray-300'
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
        <span className="flex items-center gap-1.5"><i className="size-2.5 rounded-sm bg-emerald-500" />实测通过</span>
        <span className="flex items-center gap-1.5"><i className="size-2.5 rounded-sm bg-amber-400" />降级</span>
        <span className="flex items-center gap-1.5"><i className="size-2.5 rounded-sm bg-red-500" />实测失败</span>
        <span className="flex items-center gap-1.5"><i className="size-2.5 rounded-sm bg-gray-300" />未测试/已禁用</span>
        <span className="ml-auto">共 {data.length} 个模型 · 每 30 分钟自动实测更新</span>
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
                      <div className="flex flex-wrap gap-1.5">
                        {provs.map((p) => {
                          const h = healthMap[`${p.id}|${m.id}`]
                          const tone = h ? cellTone(p.is_active, h.status) : fallbackTone(p)
                          const tip = h
                            ? `${p.name} — 实测${h.status === 'healthy' ? '通过' : h.status === 'unhealthy' ? '失败' : '未知'}${h.checked_at ? ' · ' + fmtTime(h.checked_at) : ''}${h.latency_ms > 0 ? ` · ${Math.round(h.latency_ms)}ms` : ''}${h.error ? ' · ' + h.error : ''}`
                            : `${p.name} — ${p.health_status}（未实测）${p.latency_ms > 0 ? ` · ${Math.round(p.latency_ms)}ms` : ''}`
                          return <i key={p.id} title={tip} className={`size-3.5 rounded-sm ${tone ?? 'bg-gray-300'}`} />
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
