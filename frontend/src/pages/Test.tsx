// 模型测试:渠道×模型实测矩阵 + 请求 playground(test.js 迁移)
import { useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { api, errMsg, getToken, type Page } from '@/lib/api'
import { fmtTime } from '@/lib/utils'
import { useAuth } from '@/lib/auth'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Input, Textarea } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { sonnerToast } from '@/lib/toast'

interface ProviderRow {
  id: string
  name: string
  models: string[]
  protocols?: string[]
  provider_type: string
  default_protocol: string
  is_active: boolean
  health_status: string
  disabled_reason?: string | null
}
interface HealthRow {
  provider_id: string
  model: string
  status: string
  checked_at?: string
  latency_ms: number
  error?: string | null
}

const boxTone = (status?: string) =>
  status === 'healthy' ? 'border-emerald-500 bg-emerald-50 text-emerald-700'
  : status === 'unhealthy' ? 'border-red-500 bg-red-50 text-red-700'
  : 'border-gray-300 bg-muted text-muted-foreground'

const healthBadge = (p: ProviderRow) => {
  if (!p.is_active) return <Badge tone="destructive" title={p.disabled_reason || '已手动禁用'}>已禁用</Badge>
  const tone = p.health_status === 'healthy' ? 'success' : p.health_status === 'degraded' ? 'warn' : p.health_status === 'down' ? 'destructive' : 'muted'
  return <Badge tone={tone}>{p.health_status}</Badge>
}

function TestMatrix() {
  const qc = useQueryClient()
  const [message, setMessage] = useState(localStorage.getItem('test-message') || 'Say OK')
  const [results, setResults] = useState<Record<string, { status: string; tip: string } | 'testing'>>({})
  const providers = useQuery({
    queryKey: ['providers'],
    queryFn: async () => (await api<Page<ProviderRow>>('GET', '/api/providers')).data.items ?? [],
  })
  const health = useQuery({
    queryKey: ['model-health'],
    queryFn: async () => (await api<Page<HealthRow>>('GET', '/api/model-health')).data.items ?? [],
  })

  const healthMap: Record<string, HealthRow> = {}
  for (const h of health.data ?? []) healthMap[`${h.provider_id}|${h.model}`] = h
  const provs = (providers.data ?? []).filter((p) => (p.models ?? []).length > 0)

  const runCell = async (pid: string, model: string) => {
    const key = `${pid}|${model}`
    setResults((r) => ({ ...r, [key]: 'testing' }))
    const r = await api<{ ok?: boolean; latency_ms?: number; snippet?: string; error?: string; detail?: string; code?: string; status?: number }>(
      'POST', `/api/providers/${pid}/model-tests`, { model, message: message.trim() || 'Say OK' },
    )
    if (r.ok && r.data.ok) {
      setResults((prev) => ({ ...prev, [key]: { status: 'healthy', tip: `${model} ✓ ${Math.round(r.data.latency_ms ?? 0)}ms${r.data.snippet ? ' · ' + r.data.snippet : ''}` } }))
    } else {
      const err = String(r.data.error || r.data.detail || r.data.code || '请求失败').slice(0, 120)
      setResults((prev) => ({ ...prev, [key]: { status: 'unhealthy', tip: `${model} ✗${r.data.status ? ` [${r.data.status}]` : ''} ${err}` } }))
    }
  }

  const testAll = async () => {
    localStorage.setItem('test-message', message.trim() || 'Say OK')
    const cells = provs.flatMap((p) => (p.models ?? []).map((m) => [p.id, m] as const))
    if (cells.length === 0) return
    let failed = 0
    const queue = [...cells]
    const worker = async () => {
      while (queue.length > 0) {
        const [pid, m] = queue.shift()!
        await runCell(pid, m)
      }
    }
    await Promise.all([worker(), worker(), worker(), worker()])
    for (const [, r] of Object.entries(results)) if (r !== 'testing' && r.status === 'unhealthy') failed++
    void qc.invalidateQueries({ queryKey: ['model-health'] })
    if (failed === 0) sonnerToast.success('全部测试通过')
    else sonnerToast.error(`测试完成，${failed} 个失败`)
  }

  return (
    <Card className="mb-4">
      <CardContent className="pt-4">
        <div className="flex flex-wrap items-end gap-2.5">
          <div className="min-w-64 flex-1 space-y-1">
            <Label>测试消息（对所有渠道模型发送同一条）</Label>
            <Input value={message} onChange={(e) => setMessage(e.target.value)} placeholder="例如: Say OK" />
          </div>
          <Button onClick={() => void testAll()}>一键测试全部</Button>
        </div>
        <div className="mt-3.5">
          {providers.isLoading ? (
            <div className="py-8 text-center text-sm text-muted-foreground">加载中...</div>
          ) : provs.length === 0 ? (
            <div className="py-8 text-center text-sm text-muted-foreground">暂无配置了模型的渠道，请先在渠道页添加</div>
          ) : (
            <div className="space-y-2">
              {provs.map((p) => (
                <div key={p.id} className="flex items-start gap-3">
                  <div className="flex w-52 shrink-0 flex-col items-start gap-1 pt-1" title={p.name}>
                    <strong className="max-w-full truncate text-sm">{p.name}</strong>
                    {healthBadge(p)}
                  </div>
                  <div className="flex flex-wrap gap-1.5">
                    {(p.models ?? []).map((m) => {
                      const key = `${p.id}|${m}`
                      const live = results[key]
                      const h = healthMap[key]
                      const status = live === 'testing' ? 'testing' : live ? live.status : h?.status
                      const tip = live && live !== 'testing' ? live.tip
                        : h ? `${m} — ${h.status === 'healthy' ? '✓' : h.status === 'unhealthy' ? '✗' : '?'} ${h.checked_at ? fmtTime(h.checked_at) : ''}${h.latency_ms > 0 ? ` · ${Math.round(h.latency_ms)}ms` : ''}${h.error ? ' · ' + h.error : ''}`.trim()
                        : `${m} — 未测试，点击测试`
                      return (
                        <button
                          key={m}
                          title={tip}
                          onClick={() => void runCell(p.id, m)}
                          className={`h-11 w-24 truncate rounded-md border px-1 text-[11px] font-medium transition-colors ${boxTone(status)} ${live === 'testing' ? 'animate-pulse' : 'hover:opacity-80'}`}
                        >
                          {m.length > 12 ? m.slice(0, 10) + '..' : m}
                        </button>
                      )
                    })}
                  </div>
                </div>
              ))}
              <div className="flex flex-wrap items-center gap-4 pt-1 text-xs text-muted-foreground">
                <span className="flex items-center gap-1.5"><i className="size-2.5 rounded-sm bg-gray-300" />未测试</span>
                <span className="flex items-center gap-1.5"><i className="size-2.5 rounded-sm bg-emerald-500" />通过</span>
                <span className="flex items-center gap-1.5"><i className="size-2.5 rounded-sm bg-red-500" />失败</span>
                <span className="ml-auto">每 30 分钟自动测试更新，点击方框单独测试</span>
              </div>
            </div>
          )}
        </div>
      </CardContent>
    </Card>
  )
}

function Playground() {
  const { isAdmin } = useAuth()
  const providers = useQuery({
    queryKey: ['providers'],
    queryFn: async () => (await api<Page<ProviderRow>>('GET', '/api/providers')).data.items ?? [],
    enabled: isAdmin,
  })
  const v1models = useQuery({
    queryKey: ['v1-models'],
    queryFn: async () => ((await api<{ data?: { id: string }[] }>('GET', '/v1/models')).data.data ?? []),
    enabled: !isAdmin,
  })
  const [pid, setPid] = useState('')
  const [model, setModel] = useState('')
  const [protocol, setProtocol] = useState('')
  const [stream, setStream] = useState(false)
  const [messages, setMessages] = useState('[{"role":"user","content":"你好，请用中文回复"}]')
  const [busy, setBusy] = useState(false)
  const [resp, setResp] = useState('')

  const provs = (providers.data ?? []).filter((p) => (p.models ?? []).length > 0)
  const prov = provs.find((p) => p.id === pid)
  const provProtos = ((prov?.protocols?.length ? prov.protocols : [prov?.provider_type ?? 'openai']) as string[]).filter((t) => t !== 'responses')

  const send = async () => {
    let msgs: unknown
    try {
      msgs = JSON.parse(messages)
    } catch {
      return sonnerToast.error('消息格式错误，请输入有效的 JSON')
    }
    if (!Array.isArray(msgs) || msgs.length === 0) return sonnerToast.error('消息必须是非空 JSON 数组')
    setBusy(true)
    setResp('')
    try {
      if (isAdmin) {
        if (!pid || !model) return sonnerToast.error('请选择协议、渠道和模型')
        const body = { model, messages: msgs, protocol, stream }
        if (!stream) {
          const r = await api<{ response?: unknown; ok?: boolean; latency_ms?: number; error?: string }>('POST', `/api/providers/${pid}/model-tests`, body)
          if (r.ok) {
            setResp(typeof r.data.response === 'string' ? r.data.response : JSON.stringify(r.data.response ?? r.data, null, 2))
            if (r.data.ok) sonnerToast.success(`请求成功（${Math.round(r.data.latency_ms ?? 0)}ms）`)
            else sonnerToast.error(r.data.error || errMsg(r) || '请求失败')
          } else {
            setResp(JSON.stringify(r.data, null, 2))
            sonnerToast.error(errMsg(r))
          }
        } else {
          // 流式:手动解析 SSE(delta 渐进上屏,末条 result 补全)
          const headers: Record<string, string> = { 'Content-Type': 'application/json' }
          const token = getToken()
          if (token) headers['Authorization'] = `Bearer ${token}`
          const res = await fetch(`/api/providers/${pid}/model-tests`, {
            method: 'POST', headers, body: JSON.stringify(body), signal: AbortSignal.timeout(180_000),
          })
          if (!res.ok || res.status === 204 || !res.body) {
            const data = await res.clone().json().catch(() => ({ error: 'unknown_error' }))
            setResp(JSON.stringify(data, null, 2))
            sonnerToast.error(errMsg({ data }))
            return
          }
          const reader = res.body.getReader()
          const decoder = new TextDecoder()
          let buf = ''
          let text = ''
          const dispatch = (frame: string) => {
            let event = 'message'
            const dataLines: string[] = []
            for (const line of frame.split('\n')) {
              if (line.startsWith('event:')) event = line.slice(6).trim()
              else if (line.startsWith('data:')) dataLines.push(line.slice(5).trimStart())
            }
            if (!dataLines.length) return
            const payload = dataLines.join('\n')
            if (event === 'delta') {
              try { text += (JSON.parse(payload) as { text?: string }).text || '' } catch { /* 忽略坏帧 */ }
              setResp(text)
            } else if (event === 'result') {
              try {
                const r = JSON.parse(payload) as { ok?: boolean; latency_ms?: number; error?: string }
                if (r.ok) sonnerToast.success(`请求成功（${Math.round(r.latency_ms ?? 0)}ms）`)
                else sonnerToast.error(r.error || '请求失败')
              } catch { /* 末帧非 JSON */ }
            }
          }
          for (;;) {
            const { done, value } = await reader.read()
            if (done) break
            buf += decoder.decode(value, { stream: true })
            let idx: number
            while ((idx = buf.indexOf('\n\n')) >= 0) {
              const frame = buf.slice(0, idx)
              buf = buf.slice(idx + 2)
              if (frame.trim()) dispatch(frame)
            }
          }
          if (buf.trim()) dispatch(buf)
        }
      } else {
        if (!model) return sonnerToast.error('请选择模型')
        const r = await api('POST', '/v1/chat/completions', { model, messages: msgs })
        setResp(JSON.stringify(r.data, null, 2))
        if (r.ok) sonnerToast.success('请求成功')
        else sonnerToast.error(String((r.data as { error?: { message?: string } }).error?.message || (r.data as { error?: string }).error || '请求失败'))
      }
    } finally {
      setBusy(false)
    }
  }

  return (
    <Card>
      <CardHeader><CardTitle>请求测试{isAdmin ? '' : '（OpenAI 协议）'}</CardTitle></CardHeader>
      <CardContent className="space-y-3">
        {isAdmin ? (
          <div className="flex flex-wrap gap-2.5">
            <div className="min-w-44 flex-1 space-y-1">
              <Label>渠道</Label>
              <select className="h-9 w-full rounded-md border border-border bg-card px-2 text-sm" value={pid} onChange={(e) => { setPid(e.target.value); setModel(''); setProtocol('') }}>
                {provs.length === 0 ? <option value="">暂无可用渠道</option>
                  : provs.map((p) => <option key={p.id} value={p.id}>{p.name}</option>)}
              </select>
            </div>
            <div className="min-w-44 flex-1 space-y-1">
              <Label>模型</Label>
              <select className="h-9 w-full rounded-md border border-border bg-card px-2 text-sm" value={model} onChange={(e) => setModel(e.target.value)}>
                {(prov?.models ?? []).length === 0 ? <option value="">该渠道未配置模型</option>
                  : (prov?.models ?? []).map((m) => <option key={m} value={m}>{m}</option>)}
              </select>
            </div>
            <div className="min-w-32 flex-1 space-y-1">
              <Label>协议</Label>
              <select className="h-9 w-full rounded-md border border-border bg-card px-2 text-sm" value={protocol} onChange={(e) => setProtocol(e.target.value)}>
                {provProtos.length === 0 ? <option value="">无可用协议</option>
                  : provProtos.map((t) => <option key={t} value={t}>{t}{prov?.default_protocol === t ? '（默认）' : ''}</option>)}
              </select>
            </div>
            <div className="min-w-28 space-y-1">
              <Label>方式</Label>
              <select className="h-9 w-full rounded-md border border-border bg-card px-2 text-sm" value={stream ? '1' : ''} onChange={(e) => setStream(e.target.value === '1')}>
                <option value="">非流式</option>
                <option value="1">流式</option>
              </select>
            </div>
          </div>
        ) : (
          <div className="space-y-1">
            <Label>模型</Label>
            <select className="h-9 w-full rounded-md border border-border bg-card px-2 text-sm" value={model} onChange={(e) => setModel(e.target.value)}>
              {(v1models.data ?? []).length === 0 ? <option value="">-- 暂无可用模型, 请先配置渠道 --</option>
                : (v1models.data ?? []).map((m) => <option key={m.id} value={m.id}>{m.id}</option>)}
            </select>
          </div>
        )}
        <div className="space-y-1">
          <Label>消息 (JSON 数组)</Label>
          <Textarea rows={4} className="font-mono text-xs" value={messages} onChange={(e) => setMessages(e.target.value)} />
        </div>
        <Button onClick={() => void send()} disabled={busy}>{busy ? '发送中…' : '发送请求'}</Button>
        {resp && (
          <pre className="max-h-96 overflow-auto rounded-md bg-muted p-3.5 text-xs leading-relaxed">
            <code>{resp}</code>
          </pre>
        )}
      </CardContent>
    </Card>
  )
}

export default function Test() {
  const { isAdmin } = useAuth()
  return (
    <div className="space-y-4">
      <div>
        <h2 className="text-xl font-bold">模型测试</h2>
        <p className="text-sm text-muted-foreground">验证各渠道模型的连通性，或直接向网关发送测试请求</p>
      </div>
      {isAdmin && <TestMatrix />}
      <Playground />
    </div>
  )
}
