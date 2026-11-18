// 代理池管理:渠道出网代理(socks5/http)的统一目录。
// 渠道的 proxy_url 是拷贝值——反代应用「批量添加」里从池中选用;
// 删除池条目不影响存量渠道。传输层已支持 socks5/socks5h/http/https。
import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, errMsg } from '@/lib/api'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { Dialog, DialogContent } from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { Table, TBody, TD, TH, THead, TR } from '@/components/ui/table'
import { sonnerToast } from '@/lib/toast'

type PoolItem = { id: string; name: string; url: string; note: string }

export default function ProxyPool() {
  const qc = useQueryClient()
  const [addOpen, setAddOpen] = useState(false)

  const list = useQuery({
    queryKey: ['proxy-pool'],
    queryFn: async () => (await api<{ items: PoolItem[] }>('GET', '/api/proxies')).data,
  })

  const del = useMutation({
    mutationFn: async (id: string) => await api('DELETE', `/api/proxies/${id}`),
    onSuccess: (r) => {
      if (r.ok) {
        sonnerToast.success('已删除')
        void qc.invalidateQueries({ queryKey: ['proxy-pool'] })
      } else sonnerToast.error(errMsg(r))
    },
  })

  const items = list.data?.items ?? []

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h2 className="text-xl font-bold">代理</h2>
          <p className="text-sm text-muted-foreground">
            渠道出网代理目录(socks5 / http);在反代应用「批量添加」里选用,删除不影响存量渠道
          </p>
        </div>
        <Button onClick={() => setAddOpen(true)}>+ 添加代理</Button>
      </div>

      <Card>
        <CardContent className="p-0">
          {items.length === 0 ? (
            <div className="py-10 text-center text-sm text-muted-foreground">
              {list.isLoading ? '加载中...' : '暂无代理——添加一个 socks5 或 http 代理,渠道即可通过它出网'}
            </div>
          ) : (
            <Table>
              <THead>
                <TR><TH>名称</TH><TH>URL</TH><TH>备注</TH><TH>操作</TH></TR>
              </THead>
              <TBody>
                {items.map((p) => (
                  <TR key={p.id}>
                    <TD><strong>{p.name}</strong></TD>
                    <TD className="font-mono text-xs">{maskUrl(p.url)}</TD>
                    <TD className="text-xs text-muted-foreground">{p.note || '-'}</TD>
                    <TD>
                      <Button size="sm" variant="outline"
                        onClick={() => { if (confirm(`确定删除代理「${p.name}」吗?(存量渠道不受影响)`)) del.mutate(p.id) }}>
                        删除
                      </Button>
                    </TD>
                  </TR>
                ))}
              </TBody>
            </Table>
          )}
        </CardContent>
      </Card>

      {addOpen && <AddModal onOpenChange={setAddOpen} />}
    </div>
  )
}

/// URL 里的 userinfo 密码打码展示。
function maskUrl(url: string): string {
  return url.replace(/(:\/\/[^:\/]+:)[^@]+@/, '$1***@')
}

function AddModal({ onOpenChange }: { onOpenChange: (v: boolean) => void }) {
  const qc = useQueryClient()
  const [name, setName] = useState('')
  const [url, setUrl] = useState('')
  const [note, setNote] = useState('')
  const [busy, setBusy] = useState(false)

  const submit = async () => {
    if (!name.trim()) return sonnerToast.error('请填写名称')
    if (!url.trim()) return sonnerToast.error('请填写 URL')
    setBusy(true)
    try {
      const r = await api<PoolItem>('POST', '/api/proxies', {
        name: name.trim(), url: url.trim(), note: note.trim(),
      })
      if (r.ok) {
        sonnerToast.success(`代理「${r.data.name}」已添加`)
        onOpenChange(false)
        void qc.invalidateQueries({ queryKey: ['proxy-pool'] })
      } else sonnerToast.error(errMsg(r))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open onOpenChange={onOpenChange}>
      <DialogContent title="添加代理">
        <form className="space-y-3.5" onSubmit={(e) => { e.preventDefault(); void submit() }}>
          <div className="space-y-1.5">
            <label className="text-xs font-medium text-muted-foreground">名称</label>
            <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="如 香港中转 01" />
          </div>
          <div className="space-y-1.5">
            <label className="text-xs font-medium text-muted-foreground">URL</label>
            <Input className="font-mono text-xs" value={url} onChange={(e) => setUrl(e.target.value)}
              placeholder="socks5://user:pass@host:port" />
            <p className="text-xs text-muted-foreground">支持 socks5:// socks5h:// http:// https://;认证信息放在 userinfo 里</p>
          </div>
          <div className="space-y-1.5">
            <label className="text-xs font-medium text-muted-foreground">备注(可选)</label>
            <Input value={note} onChange={(e) => setNote(e.target.value)} />
          </div>
          <div className="flex justify-end gap-2 pt-2">
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>取消</Button>
            <Button type="submit" disabled={busy}>{busy ? '添加中…' : '添加'}</Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  )
}
