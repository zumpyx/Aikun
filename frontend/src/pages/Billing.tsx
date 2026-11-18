// 计费:价格 CRUD + 兑换码 + 调账流水(billing.js + redemption.js 迁移;
// 兑换码独立成 Redemption 页,与侧栏导航一致)
import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, errMsg, type Page } from '@/lib/api'
import { fmtCost, fmtTime } from '@/lib/utils'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { Dialog, DialogContent } from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Table, TBody, TD, TH, THead, TR } from '@/components/ui/table'
import { sonnerToast } from '@/lib/toast'

interface PriceRow {
  id: string
  model: string
  prompt_price: number
  completion_price: number
  cached_price: number | null
  updated_at: string
}
interface TxRow {
  id: string
  created_at: string
  username?: string
  user_id: string
  amount: number
  balance_after: number
  kind: string
  note?: string | null
}

function PriceModal({ open, onOpenChange, editing }: { open: boolean; onOpenChange: (v: boolean) => void; editing: PriceRow | null }) {
  const qc = useQueryClient()
  const [model, setModel] = useState(editing?.model ?? '')
  const [prompt, setPrompt] = useState(editing ? String(editing.prompt_price) : '')
  const [completion, setCompletion] = useState(editing ? String(editing.completion_price) : '')
  const [cached, setCached] = useState(editing?.cached_price != null ? String(editing.cached_price) : '')
  const [busy, setBusy] = useState(false)
  const save = async () => {
    setBusy(true)
    try {
      const body = {
        model: model.trim(),
        prompt_price: parseFloat(prompt) || 0,
        completion_price: parseFloat(completion) || 0,
        cached_price: cached.trim() === '' ? null : parseFloat(cached) || 0,
      }
      const r = editing ? await api('PATCH', `/api/prices/${editing.id}`, body) : await api('POST', '/api/prices', body)
      if (r.ok) {
        sonnerToast.success(editing ? '价格已更新' : '价格已创建')
        onOpenChange(false)
        void qc.invalidateQueries({ queryKey: ['prices'] })
      } else sonnerToast.error(errMsg(r))
    } finally {
      setBusy(false)
    }
  }
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={editing ? '编辑价格' : '添加价格'}>
        <form className="space-y-3.5" onSubmit={(e) => { e.preventDefault(); void save() }}>
          <div className="space-y-1.5"><Label>模型</Label><Input value={model} onChange={(e) => setModel(e.target.value)} placeholder="gpt-4 或 gpt-*" /></div>
          <div className="space-y-1.5"><Label>输入价(元/1M tokens)</Label><Input type="number" min={0} step="any" value={prompt} onChange={(e) => setPrompt(e.target.value)} /></div>
          <div className="space-y-1.5"><Label>输出价(元/1M tokens)</Label><Input type="number" min={0} step="any" value={completion} onChange={(e) => setCompletion(e.target.value)} /></div>
          <div className="space-y-1.5"><Label>缓存价(元/1M tokens,留空按输入价)</Label><Input type="number" min={0} step="any" value={cached} onChange={(e) => setCached(e.target.value)} /></div>
          <div className="flex justify-end gap-2 pt-2">
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>取消</Button>
            <Button type="submit" disabled={busy}>{editing ? '保存' : '创建'}</Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  )
}

export default function Billing() {
  const qc = useQueryClient()
  const [showAll, setShowAll] = useState(false)
  const [modal, setModal] = useState<{ open: boolean; editing: PriceRow | null }>({ open: false, editing: null })
  const [txUser, setTxUser] = useState('')

  const prices = useQuery({
    queryKey: ['prices', showAll],
    queryFn: async () => (await api<Page<PriceRow>>('GET', `/api/prices?limit=200${showAll ? '' : '&in_use=1'}`)).data.items ?? [],
  })
  const users = useQuery({
    queryKey: ['users'],
    queryFn: async () => (await api<Page<{ id: string; username: string }>>('GET', '/api/users?limit=200')).data.items ?? [],
  })
  const txs = useQuery({
    queryKey: ['transactions', txUser],
    queryFn: async () => (await api<Page<TxRow>>('GET', `/api/balance-transactions?limit=200${txUser ? `&user_id=${encodeURIComponent(txUser)}` : ''}`)).data.items ?? [],
  })
  const del = useMutation({
    mutationFn: async (id: string) => await api('DELETE', `/api/prices/${id}`),
    onSuccess: (r) => {
      if (r.ok) { sonnerToast.success('价格已删除'); void qc.invalidateQueries({ queryKey: ['prices'] }) } else sonnerToast.error(errMsg(r))
    },
  })

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-xl font-bold">计费</h2>
          <p className="text-sm text-muted-foreground">模型价格与调账记录,价格单位为每 1M tokens(元)</p>
        </div>
        <div className="flex items-center gap-3.5">
          <label className="flex cursor-pointer select-none items-center gap-1.5 text-xs text-muted-foreground">
            <input type="checkbox" checked={showAll} onChange={(e) => setShowAll(e.target.checked)} /> 显示全部价格
          </label>
          <Button onClick={() => setModal({ open: true, editing: null })}>+ 添加价格</Button>
        </div>
      </div>
      {modal.open && <PriceModal open={modal.open} onOpenChange={(v) => setModal({ open: v, editing: null })} editing={modal.editing} />}
      <Card>
        <CardContent className="p-0">
          {(prices.data ?? []).length === 0 ? (
            <div className="py-10 text-center text-sm text-muted-foreground">
              {prices.isLoading ? '加载中...' : showAll ? '暂无价格,未计费的请求按 0 元记账' : '渠道模型暂无价格条目,可切换"显示全部价格"或添加'}
            </div>
          ) : (
            <Table>
              <THead><TR><TH>模型</TH><TH>输入价(元/1M)</TH><TH>输出价(元/1M)</TH><TH>缓存价(元/1M)</TH><TH>更新时间</TH><TH>操作</TH></TR></THead>
              <TBody>
                {(prices.data ?? []).map((p) => (
                  <TR key={p.id}>
                    <TD><strong>{p.model}</strong></TD>
                    <TD>{fmtCost(p.prompt_price)}</TD>
                    <TD>{fmtCost(p.completion_price)}</TD>
                    <TD>{p.cached_price == null ? <span className="text-muted-foreground">同输入价</span> : fmtCost(p.cached_price)}</TD>
                    <TD className="text-xs text-muted-foreground">{fmtTime(p.updated_at)}</TD>
                    <TD>
                      <div className="flex gap-1.5">
                        <Button size="sm" variant="outline" onClick={() => setModal({ open: true, editing: p })}>编辑</Button>
                        <Button size="sm" variant="destructive" onClick={() => { if (confirm('确定要删除此价格吗?')) del.mutate(p.id) }}>删除</Button>
                      </div>
                    </TD>
                  </TR>
                ))}
              </TBody>
            </Table>
          )}
        </CardContent>
      </Card>

      <div className="pt-4"><h2 className="text-xl font-bold">调账记录</h2></div>
      <Card>
        <CardContent className="p-4">
          <div className="max-w-56 space-y-1">
            <Label>用户</Label>
            <select className="h-8 w-full rounded-md border border-border bg-card px-2 text-sm" value={txUser} onChange={(e) => setTxUser(e.target.value)}>
              <option value="">全部用户</option>
              {(users.data ?? []).map((u) => <option key={u.id} value={u.id}>{u.username}</option>)}
            </select>
          </div>
        </CardContent>
      </Card>
      <Card>
        <CardContent className="p-0">
          {(txs.data ?? []).length === 0 ? (
            <div className="py-10 text-center text-sm text-muted-foreground">{txs.isLoading ? '加载中...' : '暂无调账记录'}</div>
          ) : (
            <Table>
              <THead><TR><TH>时间</TH><TH>用户</TH><TH>金额(元)</TH><TH>调后余额(元)</TH><TH>类型</TH><TH>备注</TH></TR></THead>
              <TBody>
                {(txs.data ?? []).map((t) => (
                  <TR key={t.id}>
                    <TD className="text-xs text-muted-foreground">{fmtTime(t.created_at)}</TD>
                    <TD><strong>{t.username || t.user_id}</strong></TD>
                    <TD className={t.amount >= 0 ? 'text-emerald-600' : 'text-destructive'}>{t.amount >= 0 ? '+' : ''}{fmtCost(t.amount)}</TD>
                    <TD>{fmtCost(t.balance_after)}</TD>
                    <TD><Badge tone={t.kind === 'recharge' ? 'success' : 'muted'}>{t.kind === 'recharge' ? '充值' : '调整'}</Badge></TD>
                    <TD className="text-xs">{t.note || <span className="text-muted-foreground">-</span>}</TD>
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
