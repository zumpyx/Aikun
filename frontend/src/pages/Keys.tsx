// API 密钥:列表/创建/编辑/启停/删除(keys.js 迁移)
import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, errMsg, type Page } from '@/lib/api'
import { copyText, fmtNum, fmtTime } from '@/lib/utils'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { Dialog, DialogContent } from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Table, TBody, TD, TH, THead, TR } from '@/components/ui/table'
import { sonnerToast } from '@/lib/toast'

interface ApiKey {
  id: string
  name: string
  key: string
  is_active: boolean
  models: string[]
  rate_limit_rpm: number
  quota_daily_tokens: number
  max_concurrent: number
  expires_at: string | null
  last_used_at: string | null
}

const isExpired = (k: ApiKey) => k.expires_at && new Date(k.expires_at) < new Date()

function toLocalInput(v?: string | null) {
  if (!v) return ''
  const d = new Date(v)
  if (Number.isNaN(d.getTime())) return ''
  const p = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`
}

function KeyModal({ open, onOpenChange, editing }: { open: boolean; onOpenChange: (v: boolean) => void; editing: ApiKey | null }) {
  const qc = useQueryClient()
  const [name, setName] = useState(editing?.name ?? '')
  const [expires, setExpires] = useState(toLocalInput(editing?.expires_at))
  const [models, setModels] = useState((editing?.models ?? []).join(', '))
  const [rpm, setRpm] = useState(String(editing?.rate_limit_rpm ?? 0))
  const [quota, setQuota] = useState(String(editing?.quota_daily_tokens ?? 0))
  const [concurrent, setConcurrent] = useState(String(editing?.max_concurrent ?? 0))
  const [busy, setBusy] = useState(false)

  const save = async () => {
    setBusy(true)
    try {
      const expDate = expires ? new Date(expires) : null
      const body = {
        name,
        expires_at: expDate && !Number.isNaN(expDate.getTime()) ? expDate.toISOString() : '',
        models: models.split(',').map((s) => s.trim()).filter(Boolean),
        rate_limit_rpm: Math.max(0, parseInt(rpm, 10) || 0),
        quota_daily_tokens: Math.max(0, parseInt(quota, 10) || 0),
        max_concurrent: Math.max(0, parseInt(concurrent, 10) || 0),
      }
      const r = editing
        ? await api('PATCH', `/api/api-keys/${editing.id}`, body)
        : await api('POST', '/api/api-keys', body)
      if (r.ok) {
        sonnerToast.success(editing ? '已保存' : '密钥已创建')
        onOpenChange(false)
        void qc.invalidateQueries({ queryKey: ['api-keys'] })
      } else sonnerToast.error(errMsg(r))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={editing ? '编辑密钥' : '创建密钥'}>
        <form className="space-y-3.5" onSubmit={(e) => { e.preventDefault(); void save() }}>
          <div className="space-y-1.5"><Label>名称</Label><Input value={name} onChange={(e) => setName(e.target.value)} placeholder="例如: 我的应用" /></div>
          <div className="space-y-1.5"><Label>过期时间（留空永不过期）</Label><Input type="datetime-local" value={expires} onChange={(e) => setExpires(e.target.value)} /></div>
          <div className="space-y-1.5"><Label>模型限制（逗号分隔，留空不限制）</Label><Input value={models} onChange={(e) => setModels(e.target.value)} placeholder="gpt-4, claude-*" /></div>
          <div className="grid grid-cols-3 gap-3">
            <div className="space-y-1.5"><Label>每分钟请求上限</Label><Input type="number" min={0} step={1} value={rpm} onChange={(e) => setRpm(e.target.value)} /></div>
            <div className="space-y-1.5"><Label>每日 Token 额度</Label><Input type="number" min={0} step={1} value={quota} onChange={(e) => setQuota(e.target.value)} /></div>
            <div className="space-y-1.5"><Label>并发请求上限</Label><Input type="number" min={0} step={1} value={concurrent} onChange={(e) => setConcurrent(e.target.value)} /></div>
          </div>
          <div className="flex justify-end gap-2 pt-2">
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>取消</Button>
            <Button type="submit" disabled={busy}>{editing ? '保存' : '创建'}</Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  )
}

export default function Keys() {
  const qc = useQueryClient()
  const [modal, setModal] = useState<{ open: boolean; editing: ApiKey | null }>({ open: false, editing: null })
  const list = useQuery({
    queryKey: ['api-keys'],
    queryFn: async () => (await api<Page<ApiKey>>('GET', '/api/api-keys')).data.items ?? [],
  })
  const invalidate = () => qc.invalidateQueries({ queryKey: ['api-keys'] })

  const toggle = useMutation({
    mutationFn: async (k: ApiKey) => ({ r: await api('PATCH', `/api/api-keys/${k.id}`, { is_active: !k.is_active }), active: !k.is_active }),
    onSuccess: ({ r, active }) => {
      if (r.ok) { sonnerToast.success(active ? '密钥已启用' : '密钥已禁用'); void invalidate() } else sonnerToast.error(errMsg(r))
    },
  })
  const del = useMutation({
    mutationFn: async (id: string) => await api('DELETE', `/api/api-keys/${id}`),
    onSuccess: (r) => {
      if (r.ok) { sonnerToast.success('密钥已删除'); void invalidate() } else sonnerToast.error(errMsg(r))
    },
  })

  const data = list.data ?? []
  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-xl font-bold">API 密钥</h2>
          <p className="text-sm text-muted-foreground">创建和管理访问密钥，密钥可直接用于 /v1/* 接口调用</p>
        </div>
        <Button onClick={() => setModal({ open: true, editing: null })}>+ 创建密钥</Button>
      </div>
      {modal.open && <KeyModal open={modal.open} onOpenChange={(v) => setModal({ open: v, editing: null })} editing={modal.editing} />}
      <Card>
        <CardContent className="p-0">
          {data.length === 0 ? (
            <div className="py-10 text-center text-sm text-muted-foreground">暂无密钥，点击上方创建</div>
          ) : (
            <Table>
              <THead><TR><TH>名称</TH><TH>密钥</TH><TH>状态</TH><TH>模型限制</TH><TH>限额</TH><TH>过期时间</TH><TH>最后使用</TH><TH>操作</TH></TR></THead>
              <TBody>
                {data.map((k) => {
                  const limits = [
                    k.rate_limit_rpm > 0 ? `${fmtNum(k.rate_limit_rpm)}/分` : null,
                    k.quota_daily_tokens > 0 ? `${fmtNum(k.quota_daily_tokens)}/日` : null,
                    k.max_concurrent > 0 ? `${fmtNum(k.max_concurrent)} 并发` : null,
                  ].filter(Boolean).join(' · ')
                  return (
                    <TR key={k.id}>
                      <TD><strong>{k.name || '-'}</strong></TD>
                      <TD>
                        <button className="font-mono text-xs text-muted-foreground hover:text-foreground" onClick={() => copyText(k.key, (m) => sonnerToast.success(m))} title="点击复制">
                          {k.key}
                        </button>
                      </TD>
                      <TD><Badge tone={k.is_active && !isExpired(k) ? 'success' : 'destructive'}>{!k.is_active ? '已禁用' : isExpired(k) ? '已过期' : '活跃'}</Badge></TD>
                      <TD className="text-xs">{(k.models ?? []).length > 0 ? k.models.join(', ') : <span className="text-muted-foreground">全部</span>}</TD>
                      <TD className="text-xs">{limits || <span className="text-muted-foreground">不限</span>}</TD>
                      <TD className="text-xs text-muted-foreground">{k.expires_at ? fmtTime(k.expires_at) : <span className="text-muted-foreground">永不</span>}</TD>
                      <TD className="text-xs text-muted-foreground">{k.last_used_at ? fmtTime(k.last_used_at) : <span className="text-muted-foreground">未使用</span>}</TD>
                      <TD>
                        <div className="flex gap-1.5">
                          <Button size="sm" variant="outline" onClick={() => setModal({ open: true, editing: k })}>编辑</Button>
                          <Button size="sm" variant="outline" onClick={() => toggle.mutate(k)}>{k.is_active ? '禁用' : '启用'}</Button>
                          <Button size="sm" variant="destructive" onClick={() => { if (confirm('确定要删除此密钥吗？')) del.mutate(k.id) }}>删除</Button>
                        </div>
                      </TD>
                    </TR>
                  )
                })}
              </TBody>
            </Table>
          )}
        </CardContent>
      </Card>
    </div>
  )
}
