// 钱包:余额/消费统计 + 兑换码充值 + 消费趋势/分布 + 在售价格(wallet.js 迁移)
import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, errMsg, type Page } from '@/lib/api'
import { fmtNum } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Table, TBody, TD, TH, THead, TR } from '@/components/ui/table'
import { CircleDollarSign, Coins, Gift } from 'lucide-react'
import { sonnerToast } from '@/lib/toast'

interface DailyCost { date: string; cost: number; tokens: number; requests: number }
interface ModelCost { model: string; cost: number; requests: number }
interface WalletData {
  balance: number
  today: { cost: number; requests: number }
  week: { cost: number; requests: number }
  month: { cost: number; requests: number; tokens: number }
  daily: DailyCost[]
  top_models: ModelCost[]
}
interface PublicPrice {
  model: string
  prompt_price: number | null
  completion_price: number | null
  cached_price: number | null
}

const fmtWalletCost = (n: unknown) => {
  const v = Number(n ?? 0)
  if (!Number.isFinite(v) || Math.abs(v) < 0.005) return '0'
  return v.toFixed(2).replace(/\.?0+$/, '')
}
const fmtPrice = (n: unknown) => {
  const v = Number(n ?? 0)
  if (!v) return '0'
  return v.toFixed(6).replace(/\.?0+$/, '')
}

function Stat({ icon: Icon, value, label, color }: { icon: typeof Coins; value: string; label: string; color: string }) {
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

function CostBarChart({ daily }: { daily: DailyCost[] }) {
  if (daily.length === 0) return <div className="py-10 text-center text-sm text-muted-foreground">暂无消费数据</div>
  const W = 920, H = 220, PT = 16, PB = 26, PL = 48, PR = 8
  const iw = W - PL - PR, ih = H - PT - PB
  const max = Math.max(0.000001, ...daily.map((d) => d.cost))
  const bw = iw / daily.length
  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="block w-full" role="img" aria-label="近 30 天消费趋势">
      <defs>
        <linearGradient id="cost-bar-grad" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0" stopColor="#f43f5e" />
          <stop offset="1" stopColor="#f97316" />
        </linearGradient>
      </defs>
      {[0, 0.5, 1].map((f) => {
        const y = (PT + ih * (1 - f)).toFixed(1)
        return (
          <g key={f}>
            <line x1={PL} y1={y} x2={W - PR} y2={y} stroke="#e2e8f0" strokeWidth={1} strokeDasharray={f === 0 ? undefined : '3 4'} />
            <text x={PL - 6} y={+y + 3.5} textAnchor="end" fontSize={10} fill="#94a3b8">{fmtWalletCost(max * f)}</text>
          </g>
        )
      })}
      {daily.map((d) => {
        const h = (d.cost / max) * ih
        const x = PL + daily.indexOf(d) * bw
        const tip = `${d.date}\n消耗 ¥${fmtWalletCost(d.cost)} · Token ${fmtNum(d.tokens)}\n请求 ${fmtNum(d.requests)} 次`
        return <rect key={d.date} x={(x + 1).toFixed(1)} y={(PT + (ih - h)).toFixed(1)} width={Math.max(1, bw - 3).toFixed(1)} height={Math.max(0, h).toFixed(1)} rx={2.5} fill="url(#cost-bar-grad)"><title>{tip}</title></rect>
      })}
      {[0, Math.floor(daily.length / 2), daily.length - 1].map((i) => {
        const d = daily[i]
        if (!d) return null
        return <text key={d.date} x={(PL + i * bw + bw / 2).toFixed(1)} y={H - 8} textAnchor="middle" fontSize={10} fill="#94a3b8">{d.date.slice(5)}</text>
      })}
    </svg>
  )
}

export default function Wallet() {
  const qc = useQueryClient()
  const [code, setCode] = useState('')
  const wallet = useQuery({ queryKey: ['wallet'], queryFn: async () => (await api<WalletData>('GET', '/api/users/me/wallet')).data })
  const prices = useQuery({
    queryKey: ['public-prices'],
    queryFn: async () => {
      const r = await api<Page<PublicPrice>>('GET', '/api/models/prices')
      return r.ok ? r.data.items ?? [] : null
    },
  })
  const redeem = useMutation({
    mutationFn: async (c: string) => await api<{ amount?: number; balance?: number }>('POST', '/api/users/me/redemptions', { code: c }),
    onSuccess: (r) => {
      if (r.ok) {
        sonnerToast.success(`充值成功 +¥${fmtWalletCost(r.data.amount)},当前余额 ¥${fmtWalletCost(r.data.balance)}`)
        setCode('')
        void qc.invalidateQueries({ queryKey: ['wallet'] })
      } else sonnerToast.error(errMsg(r) || '兑换失败')
    },
  })

  const w = wallet.data
  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-xl font-bold">钱包</h2>
        <p className="text-sm text-muted-foreground">余额、消费与模型价格</p>
      </div>
      {w && (
        <>
          <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
            <Stat icon={Coins} value={`¥${fmtWalletCost(w.balance ?? 0)}`} label="当前余额" color="#6366f1" />
            <Stat icon={CircleDollarSign} value={`¥${fmtWalletCost(w.today.cost)}`} label={`今日消耗 (UTC) · ${fmtNum(w.today.requests)} 次`} color="#f43f5e" />
            <Stat icon={CircleDollarSign} value={`¥${fmtWalletCost(w.week.cost)}`} label={`近 7 天消耗 (UTC) · ${fmtNum(w.week.requests)} 次`} color="#f97316" />
            <Stat icon={CircleDollarSign} value={`¥${fmtWalletCost(w.month.cost)}`} label={`近 30 天消耗 (UTC) · ${fmtNum(w.month.requests)} 次 · ${fmtNum(w.month.tokens)} tokens`} color="#14b8a6" />
          </div>
          <Card>
            <CardContent className="flex flex-col items-center gap-4 p-5 sm:flex-row">
              <div className="flex size-12 items-center justify-center rounded-full bg-primary/10 text-primary">
                <Gift className="size-6" />
              </div>
              <div className="flex-1">
                <h3 className="font-semibold">兑换码充值</h3>
                <p className="text-sm text-muted-foreground">输入管理员发放的兑换码,余额立即到账</p>
              </div>
              <div className="flex w-full max-w-md gap-2">
                <Input value={code} onChange={(e) => setCode(e.target.value)} placeholder="AK-XXXX-XXXX-XXXX-XXXX" autoComplete="off" spellCheck={false} />
                <Button disabled={redeem.isPending || !code.trim()} onClick={() => redeem.mutate(code.trim())}>立即兑换</Button>
              </div>
            </CardContent>
          </Card>
          <div className="grid gap-4 lg:grid-cols-5">
            <Card className="lg:col-span-3">
              <CardHeader className="flex-row items-center justify-between"><CardTitle>近 30 天消费趋势</CardTitle><span className="text-xs text-muted-foreground">每日消耗(元)</span></CardHeader>
              <CardContent><CostBarChart daily={w.daily ?? []} /></CardContent>
            </Card>
            <Card className="lg:col-span-2">
              <CardHeader className="flex-row items-center justify-between"><CardTitle>模型消费分布</CardTitle><span className="text-xs text-muted-foreground">近 30 天</span></CardHeader>
              <CardContent>
                {(w.top_models ?? []).length === 0 ? (
                  <div className="py-10 text-center text-sm text-muted-foreground">暂无消费数据</div>
                ) : (
                  <div className="space-y-3">
                    {w.top_models!.map((m) => {
                      const max = Math.max(0.000001, ...w.top_models!.map((x) => x.cost))
                      return (
                        <div key={m.model}>
                          <div className="mb-1 flex justify-between gap-2.5 text-xs">
                            <code className="truncate">{m.model}</code>
                            <span className="shrink-0 text-muted-foreground">¥{fmtWalletCost(m.cost)} · {fmtNum(m.requests)} 次</span>
                          </div>
                          <div className="h-2 overflow-hidden rounded-full bg-muted">
                            <div className="h-full rounded-full bg-gradient-to-r from-rose-500 to-orange-500" style={{ width: `${((m.cost / max) * 100).toFixed(1)}%` }} />
                          </div>
                        </div>
                      )
                    })}
                  </div>
                )}
              </CardContent>
            </Card>
          </div>
        </>
      )}
      <Card>
        <CardHeader className="flex-row items-center justify-between">
          <CardTitle>模型价格</CardTitle>
          <span className="text-xs text-muted-foreground">当前在售模型,只读。密钥若限制了可用模型,实际能调用的是其中一部分</span>
        </CardHeader>
        <CardContent>
          {(() => {
            const items = prices.data ?? null
            return items === null ? (
              <div className="py-10 text-center text-sm text-muted-foreground">价格加载失败</div>
            ) : items.length === 0 ? (
              <div className="py-10 text-center text-sm text-muted-foreground">暂无在售模型</div>
            ) : (
              <Table>
              <THead><TR><TH>模型</TH><TH>输入价(元/1M)</TH><TH>输出价(元/1M)</TH><TH>缓存价(元/1M)</TH></TR></THead>
              <TBody>
                {items.map((p) =>
                  p.prompt_price == null ? (
                    <TR key={p.model}><TD><strong>{p.model}</strong></TD><TD colSpan={3} className="text-muted-foreground">未定价,按 0 元记账</TD></TR>
                  ) : (
                    <TR key={p.model}>
                      <TD><strong>{p.model}</strong></TD>
                      <TD>{fmtPrice(p.prompt_price)}</TD>
                      <TD>{fmtPrice(p.completion_price)}</TD>
                      <TD>{p.cached_price == null ? <span className="text-muted-foreground">同输入价</span> : fmtPrice(p.cached_price)}</TD>
                    </TR>
                  ),
                )}
              </TBody>
            </Table>
            )
          })()}
        </CardContent>
      </Card>
    </div>
  )
}
