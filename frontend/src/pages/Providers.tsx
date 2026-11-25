// 渠道管理:预设点选 + 单建/批量共用表单 + 列表操作(providers.js 迁移;
// 旧版"单建/批量两份重复表单"在此合并为一个组件,roadmap 阶段 4 的
// providers.js 弹窗去重随之完成)
import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, errMsg, type Page } from '@/lib/api'
import { healthLabel } from '@/lib/utils'
import { sonnerToast } from '@/lib/toast'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { Dialog, DialogContent } from '@/components/ui/dialog'
import { Input, Textarea } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Table, TBody, TD, TH, THead, TR } from '@/components/ui/table'

// ---- 预设(providers.js PROVIDER_PRESETS 原样迁移)----
const PRESETS = [
  {
    id: 'orcarouter', name: 'OrcaRouter', website_url: 'https://www.orcarouter.ai',
    docs_url: 'https://docs.orcarouter.ai', protocols: ['openai', 'anthropic', 'responses'],
    default_protocol: 'openai', openai_base_url: 'https://api.orcarouter.ai',
    anthropic_base_url: 'https://api.orcarouter.ai', key_placeholder: 'sk-orca-...',
  },
  {
    id: 'deepseek', name: 'DeepSeek', website_url: 'https://platform.deepseek.com',
    docs_url: 'https://api-docs.deepseek.com/zh-cn/', protocols: ['responses'],
    default_protocol: 'responses', openai_base_url: 'https://api.deepseek.com',
    anthropic_base_url: '', key_placeholder: 'DeepSeek API Key', kind: 'deepseek',
  },
  {
    id: 'glm', name: '智谱 GLM', website_url: 'https://bigmodel.cn',
    docs_url: 'https://docs.bigmodel.cn/cn/api/introduction', protocols: ['openai'],
    default_protocol: 'openai', openai_base_url: 'https://open.bigmodel.cn/api/paas/v4',
    anthropic_base_url: '', key_placeholder: '智谱 API Key',
  },
  {
    id: 'minimax', name: 'MiniMax', website_url: 'https://platform.minimaxi.com',
    docs_url: 'https://platform.minimaxi.com/docs/api-reference/text-chat-openai',
    protocols: ['openai', 'anthropic', 'responses'], default_protocol: 'openai',
    openai_base_url: 'https://api.minimaxi.com/v1', anthropic_base_url: 'https://api.minimaxi.com/anthropic',
    key_placeholder: 'MiniMax API Key',
  },
] as const

export interface ProviderDetail {
  id: string
  name: string
  note?: string | null
  website_url?: string | null
  channel_kind?: string | null
  protocols?: string[]
  provider_type: string
  default_protocol: string
  openai_base_url: string
  anthropic_base_url: string
  api_key?: string
  models: string[]
  priority: number
  weight: number
  max_retries: number
  timeout_secs: number
  proxy_url?: string | null
  model_mapping?: Record<string, string> | null
  is_active: boolean
  health_status: string
  disabled_reason?: string | null
  latency_ms: number
}
export interface KindRow { kind: string; label: string; description?: string; experimental?: boolean }

const norm = (u: string | null | undefined) => (u ?? '').trim().replace(/\/+$/, '').toLowerCase()
const presetByUrls = (o?: string | null, a?: string | null) =>
  PRESETS.find((p) => norm(p.openai_base_url) === norm(o) && norm(p.anthropic_base_url) === norm(a)) ?? null
const toInt = (v: string, d: number) => { const n = parseInt(v, 10); return Number.isNaN(n) ? d : n }
const toNum = (v: string, d: number) => { const n = parseFloat(v); return Number.isNaN(n) ? d : n }

// ---- 共用表单(单建/批量同一形状;batch 用多行 keys + api_keys)----
interface FormState {
  preset: string
  name: string
  note: string
  website: string
  kind: string
  protocols: string[]
  defaultProtocol: string
  urlOpenai: string
  urlAnthropic: string
  key: string
  models: string
  priority: string
  weight: string
  retries: string
  timeout: string
  proxy: string
  mapping: string
  modelsAuto: string
  modelsStatus: string
}

function initialForm(provider: ProviderDetail | null): FormState {
  const protocols = provider?.protocols?.length ? provider.protocols : ['openai']
  return {
    preset: provider ? (presetByUrls(provider.openai_base_url, provider.anthropic_base_url)?.id ?? 'custom') : 'custom',
    name: provider?.name ?? '',
    note: provider?.note ?? '',
    website: provider?.website_url ?? '',
    kind: provider?.channel_kind && provider.channel_kind !== 'generic' ? provider.channel_kind : '',
    protocols: [...protocols],
    defaultProtocol: provider?.default_protocol || protocols[0],
    urlOpenai: provider?.openai_base_url ?? '',
    urlAnthropic: provider?.anthropic_base_url ?? '',
    key: '',
    models: (provider?.models ?? []).join(', '),
    priority: String(provider?.priority ?? 0),
    weight: String(provider?.weight ?? 1.0),
    retries: String(provider?.max_retries ?? 3),
    timeout: String(provider?.timeout_secs ?? 120),
    proxy: '',
    mapping: provider?.model_mapping && Object.keys(provider.model_mapping).length > 0 ? JSON.stringify(provider.model_mapping) : '',
    modelsAuto: '',
    modelsStatus: '',
  }
}

function ProviderForm({
  form,
  setForm,
  batch,
  kinds,
  editing,
}: {
  form: FormState
  setForm: (updater: (f: FormState) => FormState) => void
  batch: boolean
  kinds: KindRow[]
  editing: ProviderDetail | null
}) {
  const preset = PRESETS.find((p) => p.id === form.preset)
  const urlFor = (proto: string) => (proto === 'anthropic' ? form.urlAnthropic.trim() || form.urlOpenai.trim() : form.urlOpenai.trim() || form.urlAnthropic.trim())

  const selectPreset = (id: string) => {
    setForm((f) => {
      const p = PRESETS.find((x) => x.id === id)
      if (!p) return { ...f, preset: 'custom' }
      const isPresetName = !f.name.trim() || PRESETS.some((x) => x.name === f.name.trim())
      const isPresetSite = !f.website.trim() || PRESETS.some((x) => x.website_url === f.website.trim())
      return {
        ...f,
        preset: id,
        name: isPresetName ? p.name : f.name,
        website: isPresetSite ? p.website_url : f.website,
        protocols: [...p.protocols],
        defaultProtocol: p.default_protocol,
        urlOpenai: p.openai_base_url,
        urlAnthropic: p.anthropic_base_url,
        kind: 'kind' in p ? p.kind : '',
        models: '',
        modelsAuto: '',
        modelsStatus: '填入 API Key 后会自动向官方拉取最新模型',
      }
    })
  }

  const pullModels = async () => {
    const proto = form.protocols.includes(form.defaultProtocol) ? form.defaultProtocol : 'openai'
    const apiKey = batch ? form.key.split('\n').map((s) => s.trim()).filter(Boolean)[0] || '' : form.key.trim()
    if (apiKey.length < 8 && !editing?.id) {
      setForm((f) => ({ ...f, modelsStatus: preset ? '填入 API Key 后会自动向官方拉取最新模型' : '' }))
      return
    }
    if (!apiKey && !editing?.id) return
    setForm((f) => ({ ...f, modelsStatus: '正在从官方获取最新模型…' }))
    const r = await api<{ models?: string[] }>('POST', '/api/providers/fetch-models', {
      base_url: urlFor(proto),
      api_key: apiKey,
      protocol: proto,
      proxy_url: form.proxy.trim(),
      channel_kind: form.kind,
      provider_id: editing?.id ?? null,
    })
    if (r.ok && (r.data.models ?? []).length > 0) {
      const text = r.data.models!.join(', ')
      setForm((f) => ({ ...f, models: text, modelsAuto: text, modelsStatus: `已获取 ${r.data.models!.length} 个模型（官方当前列表）` }))
    } else {
      setForm((f) => ({ ...f, modelsStatus: errMsg(r) || '未能获取模型列表，可点「获取模型列表」重试' }))
    }
  }

  const onKeyChange = (v: string) => {
    setForm((f) => ({ ...f, key: v }))
    // 填入 Key 后防抖自动拉取(人手改过模型框则不覆盖)
    window.clearTimeout((window as unknown as { __kTimer?: number }).__kTimer)
    ;(window as unknown as { __kTimer?: number }).__kTimer = window.setTimeout(() => {
      const manual = form.models.trim() && form.models.trim() !== form.modelsAuto
      if (!manual) void pullModels()
    }, 600)
  }

  const toggleProto = (t: string, on: boolean) => {
    setForm((f) => {
      const protocols = on ? [...new Set([...f.protocols, t])] : f.protocols.filter((x) => x !== t)
      let defaultProtocol = f.defaultProtocol
      if (defaultProtocol === t || !protocols.includes(defaultProtocol)) {
        defaultProtocol = ['openai', 'anthropic'].find((x) => protocols.includes(x)) ?? protocols[0] ?? 'openai'
      }
      return { ...f, protocols, defaultProtocol }
    })
  }

  const kindRow = kinds.find((k) => (k.kind === 'generic' ? '' : k.kind) === form.kind)

  return (
    <div className="space-y-3.5">
      <div className="space-y-1.5">
        <Label>渠道</Label>
        <div className="flex flex-wrap gap-1.5">
          {[...PRESETS.map((p) => ({ id: p.id, label: p.name })), { id: 'custom', label: '自定义' }].map((it) => (
            <button
              key={it.id}
              type="button"
              onClick={() => selectPreset(it.id)}
              className={`rounded-full border px-3 py-1 text-xs font-medium transition-colors ${
                form.preset === it.id ? 'border-primary bg-primary/10 text-primary' : 'border-border text-muted-foreground hover:bg-muted'
              }`}
            >
              {it.label}
            </button>
          ))}
        </div>
        <p className="text-xs text-muted-foreground">
          {preset ? (
            <>只需填写 API Key，不用写 URL。模型在填入 Key 后自动向官方拉取。 <a className="text-primary hover:underline" href={preset.docs_url} target="_blank" rel="noopener">文档</a></>
          ) : (
            '自定义渠道需要填写 Base URL。模型仍可点「获取模型列表」从该地址拉取。'
          )}
        </p>
      </div>
      <div className="grid grid-cols-2 gap-3">
        <div className="space-y-1.5"><Label>名称</Label><Input value={form.name} onChange={(e) => setForm((f) => ({ ...f, name: e.target.value }))} placeholder={batch ? '基础名称(自动编号 -01..)' : '例如: OpenAI'} /></div>
        <div className="space-y-1.5"><Label>备注</Label><Input value={form.note} onChange={(e) => setForm((f) => ({ ...f, note: e.target.value }))} /></div>
      </div>
      <div className="space-y-1.5"><Label>官网地址</Label><Input value={form.website} onChange={(e) => setForm((f) => ({ ...f, website: e.target.value }))} placeholder="https://..." /></div>
      <div className="space-y-1.5">
        <Label>渠道类型(适配器)</Label>
        <select className="h-9 w-full rounded-md border border-border bg-card px-3 text-sm" value={form.kind} onChange={(e) => setForm((f) => ({ ...f, kind: e.target.value }))}>
          {kinds.map((k) => <option key={k.kind} value={k.kind === 'generic' ? '' : k.kind}>{k.label}</option>)}
        </select>
        {kindRow?.description && <p className="text-xs text-muted-foreground">{kindRow.experimental ? '⚠ ' : ''}{kindRow.description}</p>}
      </div>
      {!preset ? (
        <div className="space-y-1.5">
          <Label>API 协议与 Base URL</Label>
          <div className="grid grid-cols-[auto_auto_1fr] items-center gap-2.5 gap-x-3.5">
            <span className="text-center text-xs font-semibold text-muted-foreground">默认</span>
            <span className="text-xs font-semibold text-muted-foreground">API 协议</span>
            <span className="text-xs font-semibold text-muted-foreground">Base URL</span>
            {(['openai', 'anthropic'] as const).map((t) => (
              <div key={t} className="contents">
                <input
                  type="radio"
                  className="mx-auto"
                  checked={form.defaultProtocol === t}
                  disabled={!form.protocols.includes(t)}
                  onChange={() => setForm((f) => ({ ...f, defaultProtocol: t }))}
                />
                <label className="flex items-center gap-1.5 text-sm">
                  <input type="checkbox" checked={form.protocols.includes(t)} onChange={(e) => toggleProto(t, e.target.checked)} /> {t === 'openai' ? 'OpenAI' : 'Anthropic'}
                </label>
                <Input
                  value={t === 'openai' ? form.urlOpenai : form.urlAnthropic}
                  disabled={!form.protocols.includes(t)}
                  onChange={(e) => setForm((f) => (t === 'openai' ? { ...f, urlOpenai: e.target.value } : { ...f, urlAnthropic: e.target.value }))}
                  placeholder={t === 'openai' ? 'https://api.openai.com' : 'https://api.anthropic.com'}
                />
              </div>
            ))}
            <span />
            <label className="flex items-center gap-1.5 text-sm">
              <input type="checkbox" checked={form.protocols.includes('responses')} onChange={(e) => toggleProto('responses', e.target.checked)} /> Responses
            </label>
          </div>
        </div>
      ) : form.preset === 'deepseek' ? (
        <div className="space-y-1.5">
          <Label>协议</Label>
          <p className="text-xs text-muted-foreground">DeepSeek 固定使用 OpenAI Responses（POST /responses）。健康和模型列表走 api.deepseek.com/models。</p>
        </div>
      ) : (
        <div className="space-y-1.5">
          <Label>协议</Label>
          <div className="flex gap-3 text-sm">
            {(['openai', 'anthropic'] as const).map((t) => (
              <label key={t} className="flex items-center gap-1.5">
                <input
                  type="radio"
                  checked={form.defaultProtocol === t}
                  onChange={() => setForm((f) => ({
                    ...f,
                    protocols: [t],
                    defaultProtocol: t,
                    urlOpenai: t === 'openai' ? (preset.openai_base_url || f.urlOpenai) : '',
                    urlAnthropic: t === 'anthropic' ? (preset.anthropic_base_url || f.urlAnthropic) : '',
                  }))}
                />
                {t === 'openai' ? 'OpenAI' : 'Anthropic'}
              </label>
            ))}
          </div>
          <p className="text-xs text-muted-foreground">标准渠道选一个协议即可。</p>
        </div>
      )}
      {batch ? (
        <div className="space-y-1.5">
          <Label>API Keys(每行一个,每个 key 建一个渠道)</Label>
          <Textarea rows={4} className="font-mono text-xs" value={form.key} onChange={(e) => onKeyChange(e.target.value)} placeholder={'sk-aaa\nsk-bbb\nsk-ccc'} />
        </div>
      ) : (
        <div className="space-y-1.5">
          <Label>API Key{editing ? '（留空不修改）' : ''}</Label>
          <Input type="password" value={form.key} onChange={(e) => onKeyChange(e.target.value)} placeholder={preset ? preset.key_placeholder : 'sk-...'} />
        </div>
      )}
      <div className="space-y-1.5">
        <Label>模型（逗号分隔）</Label>
        <div className="flex gap-2">
          <Input className="flex-1" value={form.models} onChange={(e) => setForm((f) => ({ ...f, models: e.target.value }))} placeholder="留空则在填写 Key 后自动获取" />
          <Button type="button" variant="outline" className="shrink-0" onClick={() => void pullModels()}>获取模型列表</Button>
        </div>
        {form.modelsStatus && <p className="text-xs text-muted-foreground">{form.modelsStatus}</p>}
      </div>
      <div className="grid grid-cols-4 gap-3">
        <div className="space-y-1.5"><Label>优先级</Label><Input type="number" value={form.priority} onChange={(e) => setForm((f) => ({ ...f, priority: e.target.value }))} /></div>
        <div className="space-y-1.5"><Label>权重</Label><Input type="number" step="0.1" value={form.weight} onChange={(e) => setForm((f) => ({ ...f, weight: e.target.value }))} /></div>
        <div className="space-y-1.5"><Label>最大重试</Label><Input type="number" value={form.retries} onChange={(e) => setForm((f) => ({ ...f, retries: e.target.value }))} /></div>
        <div className="space-y-1.5"><Label>超时(秒)</Label><Input type="number" value={form.timeout} onChange={(e) => setForm((f) => ({ ...f, timeout: e.target.value }))} /></div>
      </div>
      <div className="space-y-1.5"><Label>代理{editing ? '（留空不修改）' : '（可选）'}</Label><Input value={form.proxy} onChange={(e) => setForm((f) => ({ ...f, proxy: e.target.value }))} placeholder="socks5://127.0.0.1:1080 或 http://127.0.0.1:8080" /></div>
      <div className="space-y-1.5"><Label>模型重定向（可选，JSON 对象）</Label><Textarea value={form.mapping} onChange={(e) => setForm((f) => ({ ...f, mapping: e.target.value }))} placeholder='{"gpt-4": "gpt-4-turbo"}' /></div>
    </div>
  )
}

function validate(form: FormState, batch: boolean, editing: ProviderDetail | null): string | null {
  if (form.protocols.length === 0) return '请至少勾选一个支持协议'
  if (form.protocols.includes('responses') && form.preset !== 'deepseek' && !form.protocols.includes('openai') && !form.protocols.includes('anthropic'))
    return 'Responses 为附加协议，需同时勾选 OpenAI 或 Anthropic'
  if (!form.name.trim()) return batch ? '请填写基础名称' : '请填写名称'
  if (batch && form.key.split('\n').map((s) => s.trim()).filter(Boolean).length === 0) return '请至少填写一个 API Key(每行一个)'
  if (!batch && !editing && !form.key.trim()) return '请填写名称和 API Key'
  const hasOpenai = form.protocols.includes('openai') || form.protocols.includes('responses')
  if (hasOpenai && !form.urlOpenai.trim()) return '已勾选 OpenAI/Responses 协议，请填写 OPENAI_BASE_URL'
  if (form.protocols.includes('anthropic') && !form.urlAnthropic.trim()) return '已勾选 Anthropic 协议，请填写 ANTHROPIC_BASE_URL'
  return null
}

function buildBody(form: FormState, batch: boolean): Record<string, unknown> {
  const body: Record<string, unknown> = {
    name: form.name.trim(),
    note: form.note.trim(),
    website_url: form.website.trim(),
    channel_kind: form.kind,
    protocols: form.protocols,
    default_protocol: ['openai', 'anthropic'].includes(form.defaultProtocol)
      ? form.defaultProtocol
      : ['openai', 'anthropic'].find((t) => form.protocols.includes(t)) ?? form.protocols[0],
    openai_base_url: form.urlOpenai.trim(),
    anthropic_base_url: form.urlAnthropic.trim(),
    models: form.models.split(',').map((s) => s.trim()).filter(Boolean),
    priority: toInt(form.priority, 0),
    weight: toNum(form.weight, 1.0),
    max_retries: toInt(form.retries, 3),
    timeout_secs: toInt(form.timeout, 120),
    proxy_url: form.proxy.trim(),
  }
  if (batch) body.api_keys = form.key.split('\n').map((s) => s.trim()).filter(Boolean)
  else body.api_key = form.key
  if (form.mapping.trim()) body.model_mapping = JSON.parse(form.mapping)
  else body.model_mapping = {}
  return body
}

function ProviderModal({ editing, onOpenChange }: { editing: ProviderDetail | null; onOpenChange: (v: boolean) => void }) {
  const appGroup = (editing as any)?._appGroup as ProviderDetail[] | undefined
  if (appGroup) {
    const enabled = appGroup.filter((p) => p.is_active).length
    const maxPrio = Math.max(...appGroup.map((p) => p.priority), 0)
    const models = Array.from(new Set(appGroup.flatMap((p) => p.models ?? [])))
    const recentErrors = appGroup
      .filter((p) => p.disabled_reason || p.health_status === 'down' || p.health_status === 'degraded')
      .slice(0, 5)
      .map((p) => `${p.name}: ${p.disabled_reason || healthLabel(p.health_status)}`)
    return (
      <Dialog open onOpenChange={onOpenChange}>
        <DialogContent title={`${editing?.name} 摘要`}>
          <div className="space-y-3 text-sm">
            <div>账号数量：{appGroup.length}（启用 {enabled}）</div>
            <div>组内最高优先级：{maxPrio}</div>
            <div>健康度：{appGroup.filter((p) => p.health_status === 'healthy').length}/{appGroup.length} 健康</div>
            <div>可用模型：{models.join(', ') || '-'}</div>
            <div>
              最近错误：
              {recentErrors.length === 0 ? ' 无' : (
                <ul className="mt-1 list-disc pl-4 text-xs text-destructive">
                  {recentErrors.map((e, i) => <li key={i}>{e}</li>)}
                </ul>
              )}
            </div>
            <p className="text-xs text-muted-foreground">账号级配置与优先级请到「反代」页管理。此处优先级决定本应用整体与标准渠道的选路顺序。</p>
          </div>
          <div className="mt-4 flex justify-end">
            <Button onClick={() => onOpenChange(false)}>关闭</Button>
          </div>
        </DialogContent>
      </Dialog>
    )
  }
  const qc = useQueryClient()
  const [form, setForm] = useState(() => initialForm(editing))
  const [busy, setBusy] = useState(false)
  const kinds = useQuery({
    queryKey: ['channel-kinds'],
    queryFn: async () => {
      const r = await api<Page<KindRow>>('GET', '/api/channel-kinds')
      return r.ok && r.data.items ? r.data.items : [{ kind: 'generic', label: '通用', description: '' }]
    },
    staleTime: Infinity,
  })
  const save = async () => {
    let body: Record<string, unknown>
    try {
      body = buildBody(form, false)
    } catch {
      return sonnerToast.error('模型重定向不是有效的 JSON')
    }
    const v = validate(form, false, editing)
    if (v) return sonnerToast.error(v)
    setBusy(true)
    try {
      if (editing) {
        // proxy 留空 = 不修改(后端返回脱敏值,不回填);空串不参与 diff
        if (!body.proxy_url) delete body.proxy_url
        // 只发变更字段(与旧实现同一 diff 口径)
        const upd: Record<string, unknown> = {}
        for (const k of Object.keys(body)) {
          const nv = typeof body[k] === 'object' && body[k] !== null ? JSON.stringify(body[k]) : String(body[k])
          let old: unknown = (editing as unknown as Record<string, unknown>)[k]
          if (old === null || old === undefined || old === '') {
            old = typeof body[k] === 'object' && body[k] !== null ? (Array.isArray(body[k]) ? [] : {}) : ''
          }
          const ov = typeof old === 'object' && old !== null ? JSON.stringify(old) : String(old)
          if (nv !== ov) upd[k] = body[k]
        }
        if (Object.keys(upd).length === 0) {
          sonnerToast('没有修改')
          return
        }
        const r = await api('PATCH', `/api/providers/${editing.id}`, upd)
        if (!r.ok) return sonnerToast.error(errMsg(r))
        sonnerToast.success('渠道已更新')
      } else {
        const r = await api('POST', '/api/providers', body)
        if (!r.ok) return sonnerToast.error(errMsg(r))
        sonnerToast.success('渠道已创建')
      }
      onOpenChange(false)
      void qc.invalidateQueries({ queryKey: ['providers'] })
    } finally {
      setBusy(false)
    }
  }
  return (
    <Dialog open onOpenChange={onOpenChange}>
      <DialogContent title={editing ? '编辑渠道' : '添加渠道'}>
        <form onSubmit={(e) => { e.preventDefault(); void save() }}>
          <ProviderForm form={form} setForm={setForm} batch={false} kinds={kinds.data ?? [{ kind: 'generic', label: '通用', description: '' }]} editing={editing} />
          <div className="mt-4 flex justify-end gap-2">
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>取消</Button>
            <Button type="submit" disabled={busy}>{editing ? '保存' : '创建'}</Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  )
}

function BatchModal({ onOpenChange }: { onOpenChange: (v: boolean) => void }) {
  const qc = useQueryClient()
  const [form, setForm] = useState(() => initialForm(null))
  const [busy, setBusy] = useState(false)
  const [result, setResult] = useState<{ providers: { id: string; name: string }[]; failed: { index: number; message?: string; code?: string }[] } | null>(null)
  const [testResults, setTestResults] = useState<Record<string, string>>({})
  const kinds = useQuery({
    queryKey: ['channel-kinds'],
    queryFn: async () => {
      const r = await api<Page<KindRow>>('GET', '/api/channel-kinds')
      return r.ok && r.data.items ? r.data.items : [{ kind: 'generic', label: '通用', description: '' }]
    },
    staleTime: Infinity,
  })

  const save = async () => {
    let body: Record<string, unknown>
    try {
      body = buildBody(form, true)
    } catch {
      return sonnerToast.error('模型重定向不是有效的 JSON')
    }
    const v = validate(form, true, null)
    if (v) return sonnerToast.error(v)
    setBusy(true)
    try {
      const r = await api<{ providers?: { id: string; name: string }[]; failed?: { index: number; message?: string; code?: string }[] }>('POST', '/api/providers/batch', body)
      if (!r.ok && !r.data.providers) return sonnerToast.error(errMsg(r) || '批量创建失败')
      setResult({ providers: r.data.providers ?? [], failed: r.data.failed ?? [] })
      void qc.invalidateQueries({ queryKey: ['providers'] })
      // 逐个串行连通性测试,避免瞬时打满上游
      for (const p of r.data.providers ?? []) {
        setTestResults((t) => ({ ...t, [p.id]: '测试中…' }))
        const tr = await api<{ status?: string; message?: string }>('POST', `/api/providers/${p.id}/tests`)
        const ok = tr.ok && tr.data.status === 'healthy'
        setTestResults((t) => ({ ...t, [p.id]: `${ok ? '✓ ' : '✗ '}${tr.data.message || tr.data.status || '测试失败'}` }))
      }
      void qc.invalidateQueries({ queryKey: ['providers'] })
    } finally {
      setBusy(false)
    }
  }

  if (result) {
    return (
      <Dialog open onOpenChange={(v) => { if (!v) { onOpenChange(false) } }}>
        <DialogContent title={`批量创建完成:成功 ${result.providers.length} 个${result.failed.length ? `,失败 ${result.failed.length} 个` : ''}`}>
          {result.failed.length > 0 && (
            <div className="mb-2.5">
              {result.failed.map((f) => <div key={f.index} className="text-xs text-destructive">第 {f.index + 1} 个 key:{f.message || f.code || '创建失败'}</div>)}
            </div>
          )}
          <div className="max-h-72 overflow-auto text-xs">
            {result.providers.map((p) => (
              <div key={p.id} className={`py-1 ${testResults[p.id]?.startsWith('✓') ? 'text-emerald-600' : ''}`}>
                {p.name} <span className={testResults[p.id]?.startsWith('✓') ? 'text-emerald-600' : testResults[p.id]?.startsWith('✗') ? 'text-destructive' : 'text-muted-foreground'}>{testResults[p.id] ?? '测试排队中…'}</span>
              </div>
            ))}
          </div>
          <div className="mt-4 flex justify-end">
            <Button onClick={() => onOpenChange(false)}>关闭</Button>
          </div>
        </DialogContent>
      </Dialog>
    )
  }

  return (
    <Dialog open onOpenChange={onOpenChange}>
      <DialogContent title="批量添加渠道">
        <form onSubmit={(e) => { e.preventDefault(); void save() }}>
          <ProviderForm form={form} setForm={setForm} batch kinds={kinds.data ?? [{ kind: 'generic', label: '通用', description: '' }]} editing={null} />
          <div className="mt-4 flex justify-end gap-2">
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>取消</Button>
            <Button type="submit" disabled={busy}>{busy ? '创建中…' : '批量创建'}</Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  )
}

export default function Providers() {
  const qc = useQueryClient()
  const [editModal, setEditModal] = useState<{ open: boolean; provider: ProviderDetail | null }>({ open: false, provider: null })
  const [batchOpen, setBatchOpen] = useState(false)
  const [testing, setTesting] = useState<string | null>(null)

  const list = useQuery({
    queryKey: ['providers'],
    queryFn: async () => (await api<Page<ProviderDetail>>('GET', '/api/providers?category=standard')).data.items ?? [],
  })
  const proxies = useQuery({
    queryKey: ['proxies-all'],
    queryFn: async () => (await api<Page<ProviderDetail>>('GET', '/api/providers?category=proxy&limit=200')).data.items ?? [],
  })
  const invalidate = () => {
    void qc.invalidateQueries({ queryKey: ['providers'] })
    void qc.invalidateQueries({ queryKey: ['proxies-all'] })
  }

  const toggle = useMutation({
    mutationFn: async (p: ProviderDetail) => ({ r: await api('PATCH', `/api/providers/${p.id}`, { is_active: !p.is_active }), active: !p.is_active }),
    onSuccess: ({ r, active }) => {
      if (r.ok) { sonnerToast.success(active ? '渠道已启用' : '渠道已禁用'); void invalidate() } else sonnerToast.error(errMsg(r))
    },
  })
  const duplicate = useMutation({
    mutationFn: async (id: string) => await api<{ name?: string }>('POST', `/api/providers/${id}/duplicates`),
    onSuccess: (r) => {
      if (r.ok) { sonnerToast.success(`已创建副本: ${r.data.name || '新渠道'}`); void invalidate() } else sonnerToast.error('复制失败: ' + errMsg(r))
    },
  })
  const del = useMutation({
    mutationFn: async (id: string) => await api('DELETE', `/api/providers/${id}`),
    onSuccess: (r) => {
      if (r.ok) { sonnerToast.success('渠道已删除'); void invalidate() } else sonnerToast.error(errMsg(r))
    },
  })
  const test = async (id: string) => {
    setTesting(id)
    try {
      const r = await api<{ status?: string; message?: string }>('POST', `/api/providers/${id}/tests`)
      if (r.ok) { if (r.data.status === 'healthy') sonnerToast.success(r.data.message || '测试完成'); else sonnerToast(r.data.message || '测试完成') }
      else sonnerToast.error('测试失败: ' + errMsg(r))
      void invalidate()
    } finally {
      setTesting(null)
    }
  }
  const openEdit = async (id: string) => {
    const r = await api<ProviderDetail>('GET', `/api/providers/${id}`)
    if (r.ok) setEditModal({ open: true, provider: r.data })
    else sonnerToast.error(errMsg(r) || '加载渠道详情失败')
  }

  const data = list.data ?? []
  const allProxies = proxies.data ?? []
  const isDomestic = (p: ProviderDetail) =>
    p.channel_kind === 'bigmodel-plan' || (p.note ?? '').trim().startsWith('[国内]')
  const cnGroup = allProxies.filter((p) => isDomestic(p) && p.channel_kind !== 'minimax-code')
  const intGroup = allProxies.filter((p) => !isDomestic(p) && (p.channel_kind === 'zcode' || p.channel_kind === 'zai-plan'))
  const cnMiniMax = allProxies.filter((p) =>
    p.channel_kind === 'minimax-code' && (
      (p.note ?? '').includes('国内') ||
      (p.anthropic_base_url ?? '').includes('minimax.cn') ||
      (p.openai_base_url ?? '').includes('minimax.cn')
    ))

  const renderAppRow = (label: string, items: ProviderDetail[], key: string) => {
    if (items.length === 0) return null
    const enabled = items.filter((p) => p.is_active)
    const maxPrio = Math.max(...items.map((p) => p.priority), 0)
    const models = Array.from(new Set(items.flatMap((p) => p.models ?? []))).slice(0, 4)
    const healthy = items.filter((p) => p.health_status === 'healthy').length
    return (
      <TR key={key} className="bg-muted/30">
        <TD><strong>{label}</strong> <Badge tone="muted">应用</Badge></TD>
        <TD className="text-xs text-muted-foreground">反代应用 · {items.length} 账号</TD>
        <TD className="text-xs">{models.join(', ') || '-'}</TD>
        <TD>
          <Badge tone={healthy === items.length ? 'success' : healthy > 0 ? 'warn' : 'destructive'}>
            {healthy}/{items.length} 健康
          </Badge>
        </TD>
        <TD>-</TD>
        <TD>{maxPrio}</TD>
        <TD>-</TD>
        <TD>
          <div className="flex flex-wrap gap-1.5">
            <Button size="sm" variant="outline" onClick={() => setEditModal({ open: true, provider: { id: key, name: label, is_active: enabled.length > 0, priority: maxPrio, models, health_status: healthy === items.length ? 'healthy' : 'degraded', _appGroup: items } as any })}>摘要</Button>
            <Button size="sm" variant="outline" onClick={async () => {
              const input = prompt(`设置 ${label} 组优先级（当前最高 ${maxPrio}，组内相对顺序保持）`, String(maxPrio))
              const target = parseInt(input ?? '', 10)
              if (Number.isNaN(target)) return
              for (const p of items) {
                const rel = maxPrio === 0 ? 0 : p.priority - maxPrio
                const np = target + rel
                if (np !== p.priority) await api('PATCH', `/api/providers/${p.id}`, { priority: np })
              }
              sonnerToast.success(`${label} 组优先级已更新`)
              invalidate()
            }}>优先级</Button>
          </div>
        </TD>
      </TR>
    )
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-xl font-bold">渠道管理</h2>
          <p className="text-sm text-muted-foreground">标准协议渠道与反代应用（CN-ZCode / INT-ZCode）同级，按优先级统一选路</p>
        </div>
        <div className="flex gap-2">
          <Button variant="outline" onClick={() => setBatchOpen(true)}>批量添加</Button>
          <Button onClick={() => setEditModal({ open: true, provider: null })}>+ 添加渠道</Button>
        </div>
      </div>
      {editModal.open && <ProviderModal editing={editModal.provider} onOpenChange={(v) => setEditModal({ open: v, provider: null })} />}
      {batchOpen && <BatchModal onOpenChange={setBatchOpen} />}
      <Card>
        <CardContent className="p-0">
          {data.length === 0 && cnGroup.length === 0 && intGroup.length === 0 && cnMiniMax.length === 0 ? (
            <div className="py-10 text-center text-sm text-muted-foreground">{list.isLoading ? '加载中...' : '暂无渠道，点击上方按钮添加'}</div>
          ) : (
            <Table>
              <THead><TR><TH>名称</TH><TH>地址</TH><TH>模型</TH><TH>状态</TH><TH>延迟</TH><TH>优先级</TH><TH>权重</TH><TH>操作</TH></TR></THead>
              <TBody>
                {renderAppRow('CN-ZCode', cnGroup, 'app-cn-zcode')}
                {renderAppRow('INT-ZCode', intGroup, 'app-int-zcode')}
                {renderAppRow('CN-MiniMax-Code', cnMiniMax, 'app-cn-minimax')}
                {data.map((p) => (
                  <TR key={p.id}>
                    <TD><strong>{p.name}</strong></TD>
                    <TD className="max-w-56">
                      <div className="flex items-center gap-1.5 truncate">
                        {p.website_url
                          ? /^https?:\/\//.test(p.website_url)
                            ? <a href={p.website_url} target="_blank" rel="noopener" className="truncate text-primary hover:underline" title="官网地址">{p.website_url}</a>
                            : <span className="truncate" title="官网地址">{p.website_url}</span>
                          : '-'}
                        {p.proxy_url && <Badge className="shrink-0" title={p.proxy_url}>代理</Badge>}
                      </div>
                    </TD>
                    <TD className="text-xs">{(p.models ?? []).slice(0, 2).join(', ')}{(p.models?.length ?? 0) > 2 && <Badge className="ml-1" tone="muted">+{(p.models?.length ?? 0) - 2}</Badge>}</TD>
                    <TD>
                      {!p.is_active
                        ? <Badge tone="destructive" title={p.disabled_reason || '已手动禁用'}>已禁用</Badge>
                        : <Badge tone={p.health_status === 'healthy' ? 'success' : p.health_status === 'degraded' ? 'warn' : p.health_status === 'down' ? 'destructive' : 'muted'} title={p.disabled_reason ?? ''}>{healthLabel(p.health_status)}</Badge>}
                    </TD>
                    <TD>{p.latency_ms > 0 ? `${Math.round(p.latency_ms)}ms` : '-'}</TD>
                    <TD>{p.priority}</TD>
                    <TD>{p.weight}</TD>
                    <TD>
                      <div className="flex flex-wrap gap-1.5">
                        <Button size="sm" variant="outline" onClick={() => void openEdit(p.id)}>编辑</Button>
                        <Button size="sm" variant="outline" onClick={() => duplicate.mutate(p.id)}>复制</Button>
                        <Button size="sm" variant="outline" disabled={testing === p.id} onClick={() => void test(p.id)}>{testing === p.id ? '…' : '测试'}</Button>
                        <Button size="sm" variant={p.is_active ? 'outline' : 'default'} onClick={() => toggle.mutate(p)}>{p.is_active ? '禁用' : '启用'}</Button>
                        <Button size="sm" variant="destructive" onClick={() => { if (confirm('确定要删除此渠道吗？')) del.mutate(p.id) }}>删除</Button>
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
