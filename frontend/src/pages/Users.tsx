// 用户管理:列表/新建/编辑/启停/批量创建/调账/跨用户密钥(users.js 迁移)
import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, errMsg, type Page } from '@/lib/api'
import { copyText, fmtCost, fmtNum, fmtTime } from '@/lib/utils'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { Dialog, DialogContent } from '@/components/ui/dialog'
import { Input, Textarea } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Table, TBody, TD, TH, THead, TR } from '@/components/ui/table'
import { sonnerToast } from '@/lib/toast'

interface UserRow {
  id: string
  username: string
  display_name: string
  role: string
  is_active: boolean
  balance: number
  created_at: string
}
interface UserKey {
  id: string
  name: string
  key: string
  is_active: boolean
  rate_limit_rpm: number
  quota_daily_tokens: number
  expires_at: string | null
}
interface BatchResult { username: string; ok: boolean; password?: string; code?: string }

function UserModal({ open, onOpenChange, editing }: { open: boolean; onOpenChange: (v: boolean) => void; editing: UserRow | null }) {
  const qc = useQueryClient()
  const [username, setUsername] = useState(editing?.username ?? '')
  const [name, setName] = useState(editing?.display_name ?? '')
  const [pass, setPass] = useState('')
  const [role, setRole] = useState(editing?.role ?? 'user')
  const [busy, setBusy] = useState(false)

  const save = async () => {
    setBusy(true)
    try {
      if (editing) {
        const upd: Record<string, unknown> = {}
        if (name !== editing.display_name) upd.display_name = name
        if (pass) upd.password = pass
        if (role !== editing.role) upd.role = role
        if (Object.keys(upd).length === 0) {
          sonnerToast('没有修改')
          return
        }
        const r = await api('PATCH', `/api/users/${editing.id}`, upd)
        if (!r.ok) return sonnerToast.error(errMsg(r))
        sonnerToast.success('用户已更新')
      } else {
        if (!username || !pass) return sonnerToast.error('请填写用户名和密码')
        const r = await api('POST', '/api/users', { username, password: pass, display_name: name || username, role })
        if (!r.ok) return sonnerToast.error(errMsg(r))
        sonnerToast.success('用户已创建')
      }
      onOpenChange(false)
      void qc.invalidateQueries({ queryKey: ['users'] })
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={editing ? '编辑用户' : '添加用户'}>
        <form className="space-y-3.5" onSubmit={(e) => { e.preventDefault(); void save() }}>
          <div className="space-y-1.5"><Label>用户名</Label><Input value={username} onChange={(e) => setUsername(e.target.value)} disabled={!!editing} /></div>
          <div className="space-y-1.5"><Label>显示名称</Label><Input value={name} onChange={(e) => setName(e.target.value)} /></div>
          <div className="space-y-1.5"><Label>密码{editing ? '（留空不修改）' : ''}</Label><Input type="password" value={pass} onChange={(e) => setPass(e.target.value)} required={!editing} /></div>
          <div className="space-y-1.5">
            <Label>角色</Label>
            <select className="h-9 w-full rounded-md border border-border bg-card px-3 text-sm" value={role} onChange={(e) => setRole(e.target.value)}>
              <option value="user">User</option>
              <option value="admin">Admin</option>
            </select>
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

function BatchModal({ open, onOpenChange }: { open: boolean; onOpenChange: (v: boolean) => void }) {
  const qc = useQueryClient()
  const [text, setText] = useState('')
  const [role, setRole] = useState('user')
  const [results, setResults] = useState<BatchResult[] | null>(null)
  const [busy, setBusy] = useState(false)

  const create = async () => {
    const lines = text.split('\n').map((l) => l.trim()).filter((l) => l && !l.startsWith('#'))
    if (lines.length === 0) return sonnerToast.error('请输入至少一行用户')
    const users = lines.map((line) => {
      const [username, password, display_name] = line.split(',').map((s) => s.trim())
      const u: Record<string, string> = { username, role }
      if (password) u.password = password
      if (display_name) u.display_name = display_name
      return u
    })
    if (users.some((u) => !u.username)) return sonnerToast.error('格式错误:存在空用户名行')
    setBusy(true)
    try {
      const r = await api<{ created: number; failed: number; results: BatchResult[] }>('POST', '/api/users/batch', { users })
      if (!r.ok) return sonnerToast.error(errMsg(r))
      sonnerToast.success(`创建完成:成功 ${r.data.created} 个,失败 ${r.data.failed} 个`)
      setResults(r.data.results)
      setText('')
      void qc.invalidateQueries({ queryKey: ['users'] })
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title="批量创建用户" className="w-[min(94vw,640px)]">
        <p className="mb-2.5 text-xs text-muted-foreground">
          每行一个用户，格式：<code>用户名,密码,显示名</code>。密码可留空（自动生成并只显示一次），显示名可省略。<br />
          示例：<code>alice,,爱丽丝</code> 或 <code>bob,pass123</code>
        </p>
        <Textarea rows={8} className="font-mono text-xs" value={text} onChange={(e) => setText(e.target.value)} placeholder={'alice,,爱丽丝\nbob,pass123\ncarol'} />
        <div className="mt-3 space-y-1.5">
          <Label>角色（应用于全部）</Label>
          <select className="h-9 w-full rounded-md border border-border bg-card px-3 text-sm" value={role} onChange={(e) => setRole(e.target.value)}>
            <option value="user">User</option>
            <option value="admin">Admin</option>
          </select>
        </div>
        {results && (
          <div className="mt-3 max-h-60 overflow-auto rounded-md border border-border p-2.5 text-xs">
            {results.map((x, i) =>
              x.ok ? (
                <div key={i}>
                  ✅ <strong>{x.username}</strong>
                  {x.password && (
                    <> — 密码:<code className="cursor-pointer" onClick={() => copyText(x.password!, (m) => sonnerToast.success(m))}>{x.password}</code></>
                  )}
                </div>
              ) : (
                <div key={i}>❌ <strong>{x.username}</strong> — {x.code}</div>
              ),
            )}
            {results.some((x) => x.ok && x.password) && (
              <p className="mt-1.5 text-destructive">自动生成的密码仅在此显示一次,请立即保存。</p>
            )}
          </div>
        )}
        <div className="mt-4 flex justify-end gap-2">
          <Button variant="outline" onClick={() => onOpenChange(false)}>关闭</Button>
          <Button onClick={() => void create()} disabled={busy}>{busy ? '创建中…' : '创建'}</Button>
        </div>
      </DialogContent>
    </Dialog>
  )
}

function AdjustModal({ user, onOpenChange }: { user: UserRow; onOpenChange: (v: boolean) => void }) {
  const qc = useQueryClient()
  const [amount, setAmount] = useState('')
  const [note, setNote] = useState('')
  const [busy, setBusy] = useState(false)
  const submit = async () => {
    const v = parseFloat(amount)
    if (!Number.isFinite(v) || v === 0) return sonnerToast.error('请输入非零金额(正=充值,负=扣减)')
    setBusy(true)
    try {
      const r = await api<{ balance: number }>('POST', `/api/users/${user.id}/balance-transactions`, { amount: v, ...(note ? { note } : {}) })
      if (r.ok) {
        sonnerToast.success(`操作成功,当前余额 ¥${fmtCost(r.data.balance)}`)
        onOpenChange(false)
        void qc.invalidateQueries({ queryKey: ['users'] })
      } else sonnerToast.error(errMsg(r))
    } finally {
      setBusy(false)
    }
  }
  return (
    <Dialog open onOpenChange={onOpenChange}>
      <DialogContent title={`调账 — ${user.username}(当前 ¥${fmtCost(user.balance ?? 0)})`}>
        <form className="space-y-3.5" onSubmit={(e) => { e.preventDefault(); void submit() }}>
          <div className="space-y-1.5"><Label>金额(元,正=充值 recharge,负=扣减 adjust)</Label><Input type="number" step="0.000001" value={amount} onChange={(e) => setAmount(e.target.value)} placeholder="10 或 -5" /></div>
          <div className="space-y-1.5"><Label>备注(可选)</Label><Input value={note} onChange={(e) => setNote(e.target.value)} placeholder="调账原因" /></div>
          <div className="flex justify-end gap-2 pt-2">
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>取消</Button>
            <Button type="submit" disabled={busy}>确认</Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  )
}

function UserKeysModal({ user, onOpenChange }: { user: UserRow; onOpenChange: (v: boolean) => void }) {
  const qc = useQueryClient()
  const list = useQuery({
    queryKey: ['user-keys', user.id],
    queryFn: async () => (await api<Page<UserKey>>('GET', `/api/api-keys?user_id=${encodeURIComponent(user.id)}`)).data.items ?? [],
  })
  const invalidate = () => qc.invalidateQueries({ queryKey: ['user-keys', user.id] })
  const toggle = async (k: UserKey) => {
    const r = await api('PATCH', `/api/api-keys/${k.id}`, { is_active: !k.is_active })
    if (r.ok) { sonnerToast.success(k.is_active ? '密钥已禁用' : '密钥已启用'); void invalidate() } else sonnerToast.error(errMsg(r))
  }
  const del = async (k: UserKey) => {
    if (!confirm('确定要删除此密钥吗？')) return
    const r = await api('DELETE', `/api/api-keys/${k.id}`)
    if (r.ok) { sonnerToast.success('密钥已删除'); void invalidate() } else sonnerToast.error(errMsg(r))
  }
  const isExpired = (k: UserKey) => k.expires_at && new Date(k.expires_at) < new Date()
  return (
    <Dialog open onOpenChange={onOpenChange}>
      <DialogContent title={`用户密钥 — ${user.username}`} className="w-[min(96vw,760px)]">
        {(list.data ?? []).length === 0 ? (
          <div className="py-8 text-center text-sm text-muted-foreground">{list.isLoading ? '加载中...' : '该用户暂无密钥'}</div>
        ) : (
          <Table>
            <THead><TR><TH>名称</TH><TH>密钥</TH><TH>状态</TH><TH>限额</TH><TH>过期时间</TH><TH>操作</TH></TR></THead>
            <TBody>
              {(list.data ?? []).map((k) => {
                const limits = [
                  k.rate_limit_rpm > 0 ? `${fmtNum(k.rate_limit_rpm)}/分` : null,
                  k.quota_daily_tokens > 0 ? `${fmtNum(k.quota_daily_tokens)}/日` : null,
                ].filter(Boolean).join(' · ')
                return (
                  <TR key={k.id}>
                    <TD><strong>{k.name || '-'}</strong></TD>
                    <TD className="font-mono text-xs text-muted-foreground">{k.key}</TD>
                    <TD><Badge tone={k.is_active && !isExpired(k) ? 'success' : 'destructive'}>{!k.is_active ? '已禁用' : isExpired(k) ? '已过期' : '活跃'}</Badge></TD>
                    <TD className="text-xs">{limits || <span className="text-muted-foreground">不限</span>}</TD>
                    <TD className="text-xs text-muted-foreground">{k.expires_at ? fmtTime(k.expires_at) : '永不'}</TD>
                    <TD>
                      <div className="flex gap-1.5">
                        <Button size="sm" variant="outline" onClick={() => void toggle(k)}>{k.is_active ? '禁用' : '启用'}</Button>
                        <Button size="sm" variant="destructive" onClick={() => void del(k)}>删除</Button>
                      </div>
                    </TD>
                  </TR>
                )
              })}
            </TBody>
          </Table>
        )}
      </DialogContent>
    </Dialog>
  )
}

export default function Users() {
  const qc = useQueryClient()
  const [userModal, setUserModal] = useState<{ open: boolean; editing: UserRow | null }>({ open: false, editing: null })
  const [batchOpen, setBatchOpen] = useState(false)
  const [keysUser, setKeysUser] = useState<UserRow | null>(null)
  const [adjustUser, setAdjustUser] = useState<UserRow | null>(null)

  const list = useQuery({
    queryKey: ['users'],
    queryFn: async () => (await api<Page<UserRow>>('GET', '/api/users?limit=200')).data.items ?? [],
  })
  const toggle = useMutation({
    mutationFn: async (u: UserRow) => ({ r: await api('PATCH', `/api/users/${u.id}`, { is_active: !u.is_active }), active: !u.is_active }),
    onSuccess: ({ r, active }) => {
      if (r.ok) { sonnerToast.success(active ? '用户已启用' : '用户已禁用'); void qc.invalidateQueries({ queryKey: ['users'] }) } else sonnerToast.error(errMsg(r))
    },
  })

  const data = list.data ?? []
  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-xl font-bold">用户管理</h2>
          <p className="text-sm text-muted-foreground">管理系统用户与权限</p>
        </div>
        <div className="flex gap-2">
          <Button variant="outline" onClick={() => setBatchOpen(true)}>批量创建</Button>
          <Button onClick={() => setUserModal({ open: true, editing: null })}>+ 添加用户</Button>
        </div>
      </div>
      {userModal.open && <UserModal open={userModal.open} onOpenChange={(v) => setUserModal({ open: v, editing: null })} editing={userModal.editing} />}
      {batchOpen && <BatchModal open={batchOpen} onOpenChange={setBatchOpen} />}
      {keysUser && <UserKeysModal user={keysUser} onOpenChange={(v) => { if (!v) setKeysUser(null) }} />}
      {adjustUser && <AdjustModal user={adjustUser} onOpenChange={(v) => { if (!v) setAdjustUser(null) }} />}
      <Card>
        <CardContent className="p-0">
          {data.length === 0 ? (
            <div className="py-10 text-center text-sm text-muted-foreground">{list.isLoading ? '加载中...' : '暂无用户'}</div>
          ) : (
            <Table>
              <THead><TR><TH>用户名</TH><TH>显示名</TH><TH>角色</TH><TH>状态</TH><TH>余额(元)</TH><TH>创建时间</TH><TH>操作</TH></TR></THead>
              <TBody>
                {data.map((u) => (
                  <TR key={u.id}>
                    <TD><strong>{u.username}</strong></TD>
                    <TD className="text-sm">{u.display_name}</TD>
                    <TD><Badge tone={u.role === 'admin' ? 'default' : 'muted'}>{u.role === 'admin' ? 'Admin' : 'User'}</Badge></TD>
                    <TD><Badge tone={u.is_active ? 'success' : 'destructive'}>{u.is_active ? '活跃' : '禁用'}</Badge></TD>
                    <TD className={`text-xs ${(u.balance ?? 0) < 0 ? 'text-destructive' : ''}`}>{fmtCost(u.balance ?? 0)}</TD>
                    <TD className="text-xs text-muted-foreground">{fmtTime(u.created_at)}</TD>
                    <TD>
                      <div className="flex flex-wrap gap-1.5">
                        <Button size="sm" variant="outline" onClick={() => setUserModal({ open: true, editing: u })}>编辑</Button>
                        <Button size="sm" variant="outline" onClick={() => setKeysUser(u)}>密钥</Button>
                        <Button size="sm" variant="outline" onClick={() => setAdjustUser(u)}>调账</Button>
                        <Button size="sm" variant="destructive" onClick={() => toggle.mutate(u)}>{u.is_active ? '禁用' : '启用'}</Button>
                      </div>
                    </TD>
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
