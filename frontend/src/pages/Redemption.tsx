// 兑换码:批量生成/筛选/分页/禁用(redemption.js 迁移)
import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, errMsg, type Page } from '@/lib/api'
import { copyText, fmtCost, fmtTime } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { Dialog, DialogContent } from '@/components/ui/dialog'
import { Input, Textarea } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Table, TBody, TD, TH, THead, TR } from '@/components/ui/table'
import { sonnerToast } from '@/lib/toast'

const PAGE_SIZE = 50

interface CodeRow {
  id: string
  code_masked: string
  amount: number
  batch: string
  status: string
  expired?: boolean
  used_by?: string | null
  created_at: string
  expires_at: string | null
  used_at: string | null
  note?: string | null
}

function GenerateModal({ open, onOpenChange }: { open: boolean; onOpenChange: (v: boolean) => void }) {
  const qc = useQueryClient()
  const [count, setCount] = useState('10')
  const [amount, setAmount] = useState('')
  const [batch, setBatch] = useState('')
  const [expires, setExpires] = useState('')
  const [note, setNote] = useState('')
  const [busy, setBusy] = useState(false)
  const [result, setResult] = useState<{ codes: string[]; count: number; amount: number } | null>(null)

  const generate = async () => {
    setBusy(true)
    try {
      const body: Record<string, unknown> = {
        count: parseInt(count, 10) || 0,
        amount: parseFloat(amount) || 0,
        batch: batch.trim(),
        note: note.trim(),
      }
      if (expires.trim()) body.expires_at = expires.trim()
      const r = await api<{ codes: string[]; count: number; amount: number }>('POST', '/api/redemption-codes', body)
      if (r.ok) {
        setResult(r.data)
        void qc.invalidateQueries({ queryKey: ['codes'] })
      } else sonnerToast.error(errMsg(r) || '生成失败')
    } finally {
      setBusy(false)
    }
  }

  if (result) {
    const text = (result.codes ?? []).join('\n')
    return (
      <Dialog open onOpenChange={(v) => { if (!v) { setResult(null); onOpenChange(false) } }}>
        <DialogContent title={`已生成 ${result.count} 张兑换码(面值 ¥${fmtCost(result.amount)})`}>
          <p className="mb-2.5 text-xs text-destructive">明文仅本次可见,请立即复制保存;关闭后无法找回。</p>
          <Textarea readOnly rows={9} className="font-mono text-xs" value={text} />
          <div className="mt-4 flex justify-end gap-2">
            <Button onClick={() => copyText(text, (m) => sonnerToast.success(m))}>复制全部</Button>
            <Button variant="outline" onClick={() => { setResult(null); onOpenChange(false) }}>关闭</Button>
          </div>
        </DialogContent>
      </Dialog>
    )
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title="批量生成兑换码">
        <form className="space-y-3.5" onSubmit={(e) => { e.preventDefault(); void generate() }}>
          <div className="space-y-1.5"><Label>数量(1-500)</Label><Input type="number" min={1} max={500} step={1} value={count} onChange={(e) => setCount(e.target.value)} /></div>
          <div className="space-y-1.5"><Label>单张面值(元)</Label><Input type="number" min={0} step="any" value={amount} onChange={(e) => setAmount(e.target.value)} placeholder="如 10" /></div>
          <div className="space-y-1.5"><Label>批次名</Label><Input value={batch} onChange={(e) => setBatch(e.target.value)} placeholder="如 2026-08 活动" /></div>
          <div className="space-y-1.5"><Label>过期时间(UTC,留空永不过期)</Label><Input type="date" value={expires} onChange={(e) => setExpires(e.target.value)} /></div>
          <div className="space-y-1.5"><Label>备注(可选)</Label><Input value={note} onChange={(e) => setNote(e.target.value)} placeholder="仅管理员可见" /></div>
          <div className="flex justify-end gap-2 pt-2">
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>取消</Button>
            <Button type="submit" disabled={busy}>生成</Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  )
}

export default function Redemption() {
  const qc = useQueryClient()
  const [batch, setBatch] = useState('')
  const [status, setStatus] = useState('')
  const [applied, setApplied] = useState({ batch: '', status: '' })
  const [page, setPage] = useState(0)
  const [genOpen, setGenOpen] = useState(false)

  const qs = new URLSearchParams()
  if (applied.batch) qs.set('batch', applied.batch)
  if (applied.status) qs.set('status', applied.status)
  qs.set('limit', String(PAGE_SIZE))
  qs.set('offset', String(page * PAGE_SIZE))

  const list = useQuery({
    queryKey: ['codes', applied, page],
    queryFn: async () => await api<Page<CodeRow>>('GET', `/api/redemption-codes?${qs}`),
  })
  const disable = useMutation({
    mutationFn: async (id: string) => await api('DELETE', `/api/redemption-codes/${id}`),
    onSuccess: (r) => {
      if (r.ok) { sonnerToast.success('已禁用'); void qc.invalidateQueries({ queryKey: ['codes'] }) } else sonnerToast.error(errMsg(r) || '禁用失败')
    },
  })

  const data = list.data?.data.items ?? []
  const total = list.data?.data.total ?? 0
  const totalPages = Math.max(1, Math.ceil(total / PAGE_SIZE))

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-xl font-bold">兑换码</h2>
          <p className="text-sm text-muted-foreground">批量生成兑换码,用户在钱包页兑换充值</p>
        </div>
        <Button onClick={() => setGenOpen(true)}>+ 批量生成</Button>
      </div>
      {genOpen && <GenerateModal open={genOpen} onOpenChange={setGenOpen} />}
      <Card>
        <CardContent className="flex flex-wrap items-end gap-3.5 p-4">
          <div className="max-w-52 space-y-1">
            <Label>批次</Label>
            <Input value={batch} onChange={(e) => setBatch(e.target.value)} placeholder="全部批次" />
          </div>
          <div className="max-w-40 space-y-1">
            <Label>状态</Label>
            <select className="h-8 w-full rounded-md border border-border bg-card px-2 text-sm" value={status} onChange={(e) => setStatus(e.target.value)}>
              <option value="">全部</option>
              <option value="unused">未使用</option>
              <option value="used">已使用</option>
              <option value="disabled">已禁用</option>
              <option value="expired">已过期</option>
            </select>
          </div>
          <Button variant="outline" onClick={() => { setPage(0); setApplied({ batch: batch.trim(), status }) }}>筛选</Button>
        </CardContent>
      </Card>
      <Card>
        <CardContent className="p-0">
          {data.length === 0 && page === 0 ? (
            <div className="py-10 text-center text-sm text-muted-foreground">{list.isLoading ? '加载中...' : '暂无兑换码,点击右上角批量生成'}</div>
          ) : (
            <Table>
              <THead><TR><TH>兑换码</TH><TH>面值(元)</TH><TH>批次</TH><TH>状态</TH><TH>使用者</TH><TH>创建时间</TH><TH>过期时间 (UTC)</TH><TH>使用时间</TH><TH>备注</TH><TH>操作</TH></TR></THead>
              <TBody>
                {data.map((c) => (
                  <TR key={c.id}>
                    <TD><code className="text-xs">{c.code_masked}</code></TD>
                    <TD>{fmtCost(c.amount)}</TD>
                    <TD className="text-xs">{c.batch}</TD>
                    <TD className="text-xs">
                      {c.status === 'unused' && c.expired ? <span className="text-amber-600">已过期</span>
                        : c.status === 'unused' ? <span className="text-teal-600">未使用</span>
                        : c.status === 'used' ? <span className="text-muted-foreground">已使用</span>
                        : <span className="text-destructive">已禁用</span>}
                    </TD>
                    <TD className="text-xs">{c.used_by || <span className="text-muted-foreground">—</span>}</TD>
                    <TD className="text-xs text-muted-foreground">{fmtTime(c.created_at)}</TD>
                    <TD className="text-xs text-muted-foreground">{c.expires_at ? fmtTime(c.expires_at) : '永不'}</TD>
                    <TD className="text-xs text-muted-foreground">{c.used_at ? fmtTime(c.used_at) : '—'}</TD>
                    <TD className="text-xs text-muted-foreground">{c.note || ''}</TD>
                    <TD>
                      {c.status === 'unused' && <Button size="sm" variant="destructive" onClick={() => disable.mutate(c.id)}>禁用</Button>}
                    </TD>
                  </TR>
                ))}
              </TBody>
            </Table>
          )}
          <div className="flex items-center justify-between p-4">
            <span className="text-sm text-muted-foreground">共 {total} 条 · 第 {page + 1}/{totalPages} 页</span>
            <div className="flex gap-2">
              <Button size="sm" variant="outline" disabled={page === 0} onClick={() => setPage((p) => p - 1)}>上一页</Button>
              <Button size="sm" variant="outline" disabled={page + 1 >= totalPages} onClick={() => setPage((p) => p + 1)}>下一页</Button>
            </div>
          </div>
        </CardContent>
      </Card>
    </div>
  )
}
