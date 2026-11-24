// 反代应用中心:每应用一个子页(/proxies/:app),ZCode 子页含
// 国际/国内 × 订阅JWT/APIKey 四种接入预设与免费额度领取任务面板;
// WorkBuddy/Qoder 为占位协议子页;Z.AI Chat 待接入。
// 批量启停/删除、导入导出随应用(kind 过滤);表单复用批量粘贴。
import { useRef, useState } from 'react'
import { Link, Navigate, useParams } from 'react-router-dom'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { api, errMsg, type Page } from '@/lib/api'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { Checkbox } from '@/components/ui/checkbox'
import { Dialog, DialogContent } from '@/components/ui/dialog'
import { Input, Textarea } from '@/components/ui/input'
import { Table, TBody, TD, TH, THead, TR } from '@/components/ui/table'
import { sonnerToast } from '@/lib/toast'
import type { ProviderDetail } from './Providers'

const PAGE_SIZE = 200

/**
 * 应用注册表(两行按区域分组,每应用一个页):
 * kinds = 归组的 channel_kind 集(查询 kind 过滤);regionFilter 只对
 * zcode 生效——JWT 形态两个区域共用 kind=zcode,按渠道 base 含
 * bigmodel 与否在前端分域(zai-plan / bigmodel-plan 自带区域属性)。
 */
const APPS = {
  zcode: {
    region: 'intl',
    label: 'ZCode',
    kinds: 'zcode,zai-plan',
    regionFilter: 'intl' as const,
    desc: 'GLM Coding Plan·国际(z.ai):订阅 JWT(zcode.z.ai)与 API Key(api.z.ai);支持免费额度领取',
  },
  workbuddy: {
    region: 'intl',
    label: 'WorkBuddy',
    kinds: 'workbuddy',
    desc: 'WorkBuddy 订阅上游(协议尚未接入,按 OpenAI 兼容转发)',
  },
  qoder: {
    region: 'intl',
    label: 'Qoder',
    kinds: 'qoder',
    desc: 'Qoder 订阅上游(协议尚未接入,按 OpenAI 兼容转发)',
  },
  'zcode-cn': {
    region: 'cn',
    label: 'ZCode',
    kinds: 'zcode,bigmodel-plan',
    regionFilter: 'cn' as const,
    desc: 'GLM Coding Plan·国内(智谱 BigModel):订阅 JWT(API 走 zcode.z.ai ultra)与 API Key(open.bigmodel.cn);凭证与国际 z.ai 互不通用',
  },
} as const

/** 两行区域分组(第一行国际,第二行国内)。 */
const REGION_ROWS: { label: string; apps: AppId[] }[] = [
  { label: '国际', apps: ['zcode', 'workbuddy', 'qoder'] },
  { label: '国内', apps: ['zcode-cn'] },
]

/** 渠道是否国内(API 统一走 zcode.z.ai,分域看 kind/note 标记:
 * bigmodel-plan 自带;zcode JWT 形态看 note 的 [国内] 前缀)。 */
const isDomestic = (p: ProviderDetail) =>
  p.channel_kind === 'bigmodel-plan' || (p.note ?? '').trim().startsWith('[国内]')

type AppId = keyof typeof APPS

export default function ProxyProviders() {
  const { app } = useParams<{ app?: string }>()
  if (!app || !(app in APPS)) return <Navigate to="/proxies/zcode" replace />
  return <AppPage appId={app as AppId} />
}

function AppPage({ appId }: { appId: AppId }) {
  const conf = APPS[appId]
  const qc = useQueryClient()
  const [selected, setSelected] = useState<Set<string>>(new Set())
  const [batchOpen, setBatchOpen] = useState(false)
  const [importOpen, setImportOpen] = useState(false)
  const [login, setLogin] = useState<{ state: 'idle' | 'waiting' | 'done' | 'error'; authorizeUrl?: string; message?: string }>({ state: 'idle' })
  const loginTimer = useRef<number>(0)

  const kindQ = conf.kinds ? `&kind=${encodeURIComponent(conf.kinds)}` : ''
  const list = useQuery({
    queryKey: ['proxies', appId],
    queryFn: async () => (await api<Page<ProviderDetail>>('GET', `/api/providers?category=proxy${kindQ}&limit=${PAGE_SIZE}`)).data,
  })
  const invalidate = () => {
    void qc.invalidateQueries({ queryKey: ['proxies'] })
    void qc.invalidateQueries({ queryKey: ['providers'] })
    void qc.invalidateQueries({ queryKey: ['zcode-tasks'] })
  }

  const bulk = useMutation({
    mutationFn: async (p: { action: 'enable' | 'disable' | 'delete'; ids: string[] }) =>
      await api<{ requested?: number; succeeded?: number; failed?: { code: string; message?: string }[] }>('POST', '/api/providers/bulk', p),
    onSuccess: (r) => {
      if (r.ok) {
        sonnerToast.success(`批量操作完成:成功 ${r.data.succeeded ?? 0}/${r.data.requested ?? 0}`)
        setSelected(new Set())
        invalidate()
      } else sonnerToast.error(errMsg(r))
    },
  })

  const doExport = async () => {
    const r = await fetch(`/api/providers/export?category=proxy${kindQ}`, { headers: { Authorization: `Bearer ${localStorage.getItem('token') ?? ''}` } })
    if (!r.ok) {
      sonnerToast.error('导出失败')
      return
    }
    const blob = await r.blob()
    const a = document.createElement('a')
    a.href = URL.createObjectURL(blob)
    a.download = `aikun-${appId}-${new Date().toISOString().slice(0, 10)}.json`
    a.click()
    URL.revokeObjectURL(a.href)
    sonnerToast.success('已导出(⚠ 文件含明文凭证,仅可信环境存储)')
  }

  // 在线登录(env 随页:intl = z.ai 账号 / cn = 智谱 BigModel 账号):
  // init → 开授权窗 → 每 3s 轮询,总超时 5 分钟
  const startLogin = async () => {
    // env 随应用页:zcode-cn = 智谱 BigModel 账号,否则 z.ai 国际账号
    const env = appId === 'zcode-cn' ? 'cn' : 'intl'
    setLogin({ state: 'waiting', message: '正在发起授权…' })
    const r = await api<{ flow_id: string; authorize_url: string; poll_token: string }>('POST', '/api/providers/zcode/login-init', { env })
    if (!r.ok) {
      setLogin({ state: 'error', message: errMsg(r) })
      return
    }
    window.open(r.data.authorize_url, '_blank', 'noopener')
    setLogin({ state: 'waiting', authorizeUrl: r.data.authorize_url, message: `已打开授权窗口(${env === 'cn' ? '智谱 BigModel 账号' : 'z.ai 国际账号'}),完成授权后自动创建渠道并领取免费额度…` })
    const started = Date.now()
    const tick = async () => {
      if (Date.now() - started > 5 * 60_000) {
        setLogin({ state: 'error', message: '授权超时(5 分钟),请重试' })
        return
      }
      const p = await api<{ status: string; email?: string }>('POST', '/api/providers/zcode/login-poll', {
        flow_id: r.data.flow_id,
        poll_token: r.data.poll_token,
        env,
      })
      if (!p.ok) {
        setLogin({ state: 'error', message: errMsg(p) })
        return
      }
      if (p.data.status === 'ready') {
        sonnerToast.success(`授权成功(${p.data.email ?? 'zcode'}),渠道已创建并在后台领取免费额度`)
        setLogin({ state: 'done' })
        invalidate()
        return
      }
      if (p.data.status === 'expired' || p.data.status === 'failed') {
        setLogin({ state: 'error', message: p.data.status === 'expired' ? '授权流已过期,请重试' : '授权失败' })
        return
      }
      loginTimer.current = window.setTimeout(() => void tick(), 3000)
    }
    loginTimer.current = window.setTimeout(() => void tick(), 3000)
  }

  // ---- 领取免费额度 / 账号信息 / 手动备注 ----
  const [claiming, setClaiming] = useState<Set<string>>(new Set())
  const [detailFor, setDetailFor] = useState<ProviderDetail | null>(null)
  const [editFor, setEditFor] = useState<{ id: string; name: string } | null>(null)
  const doClaim = async (id: string, name: string) => {
    setClaiming((prev) => new Set(prev).add(id))
    const r = await api<{ claimed: string[]; skipped: string[]; problems: string[] }>('POST', `/api/providers/${id}/zcode/claim`)
    setClaiming((prev) => {
      const next = new Set(prev)
      next.delete(id)
      return next
    })
    if (!r.ok) {
      sonnerToast.error(`${name} 领取失败:${errMsg(r)}`)
      return
    }
    if (r.data.problems.length > 0) sonnerToast.warning(`${name} 领取存在问题:${r.data.problems.join(';')}`)
    const parts: string[] = []
    if (r.data.claimed.length > 0) parts.push(`已领取 ${r.data.claimed.join('、')}`)
    if (r.data.skipped.length > 0) parts.push(`已持有 ${r.data.skipped.join('、')}`)
    if (parts.length > 0) sonnerToast.success(`${name}:${parts.join(' · ')}`)
    else if (r.data.problems.length === 0) sonnerToast.info(`${name}:当前没有可领的额度`)
    invalidate()
  }
  // 账号信息:弹窗内拉取聚合(订阅/额度/重置次数);服务端顺带回填
  // 名称与备注摘要(见 AccountInfoModal 的 name_updated toast)。

  const claimAll = async () => {
    const targets = (list.data?.items ?? []).filter((p) => p.channel_kind === 'zcode')
    if (targets.length === 0) return sonnerToast.info('没有可领取的国内 ZCode 订阅渠道')
    sonnerToast.info(`开始领取 ${targets.length} 个渠道的免费额度…`)
    for (const p of targets) await doClaim(p.id, p.name)
  }

  // zcode 双页分域:JWT 形态按 base;kind 自带区域的(plan 两种)顺带兜底
  const rf = 'regionFilter' in conf ? conf.regionFilter : undefined
  const data = (list.data?.items ?? []).filter((p) =>
    rf === 'intl' ? !isDomestic(p) : rf === 'cn' ? isDomestic(p) : true,
  )
  const allChecked = data.length > 0 && data.every((p) => selected.has(p.id))
  const toggleAll = () => setSelected(allChecked ? new Set() : new Set(data.map((p) => p.id)))
  const toggleOne = (id: string) => setSelected((prev) => {
    const next = new Set(prev)
    if (next.has(id)) next.delete(id)
    else next.add(id)
    return next
  })
  const selIds = [...selected]
  const bulkBtn = (action: 'enable' | 'disable' | 'delete', label: string, variant?: 'default' | 'destructive') => (
    <Button size="sm" variant={variant ?? 'outline'} disabled={selected.size === 0 || bulk.isPending}
      onClick={() => { if (action !== 'delete' || confirm(`确定删除所选 ${selected.size} 个渠道吗?(不可恢复)`)) bulk.mutate({ action, ids: selIds }) }}>
      {label}{selected.size > 0 ? `(${selected.size})` : ''}
    </Button>
  )
  return (
    <div className="space-y-4">
      {/* 应用 Tab */}
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h2 className="text-xl font-bold">反代应用</h2>
          <p className="text-sm text-muted-foreground">{conf.desc}</p>
        </div>
      </div>
      <div className="space-y-1.5">
        {REGION_ROWS.map((row) => (
          <div key={row.label} className="flex flex-wrap items-center gap-1.5">
            <span className="w-9 text-xs font-medium text-muted-foreground">{row.label}</span>
            {row.apps.map((k) => (
              <Link key={`${row.label}-${k}`} to={`/proxies/${k}`}
                className={`rounded-full border px-4 py-1.5 text-sm font-medium transition-colors ${k === appId ? 'border-primary bg-primary/10 text-primary' : 'border-border text-muted-foreground hover:bg-muted'}`}>
                {APPS[k].label}
              </Link>
            ))}
          </div>
        ))}
      </div>

          <div className="flex flex-wrap gap-2">
            <Button variant="outline" onClick={() => void doExport()}>导出</Button>
            <Button variant="outline" onClick={() => setImportOpen(true)}>导入</Button>
            {appId.startsWith('zcode') && (
              <Button variant="outline" onClick={() => void startLogin()} disabled={login.state === 'waiting'}>
                {login.state === 'waiting' ? '授权中…' : '在线登录'}
              </Button>
            )}
            <Button onClick={() => setBatchOpen(true)}>+ 批量添加</Button>
            {appId === 'zcode-cn' && <Button onClick={() => void claimAll()}>领取闲时额度</Button>}
          </div>

          {login.state !== 'idle' && (
            <Card>
              <CardContent className="flex items-center justify-between gap-3 p-3.5 text-sm">
                <span className={login.state === 'error' ? 'text-destructive' : 'text-muted-foreground'}>
                  {login.message ?? (login.state === 'done' ? '完成' : '')}
                  {login.authorizeUrl && login.state === 'waiting' && (
                    <> · <a className="text-primary hover:underline" href={login.authorizeUrl} target="_blank" rel="noopener">重开授权窗口</a></>
                  )}
                </span>
                {login.state !== 'waiting' && (
                  <Button size="sm" variant="outline" onClick={() => setLogin({ state: 'idle' })}>关闭</Button>
                )}
              </CardContent>
            </Card>
          )}

          {batchOpen && <BatchModal appId={appId} onOpenChange={setBatchOpen} />}
          {detailFor && (
            <DetailModal
              target={detailFor}
              onClose={() => setDetailFor(null)}
              onDone={invalidate}
              onClaim={doClaim}
              claiming={claiming}
            />
          )}
          {editFor && <EditModal target={editFor} onDone={invalidate} onClose={() => setEditFor(null)} />}
          {importOpen && <ImportModal appId={appId} onOpenChange={setImportOpen} onDone={invalidate} />}

          {selected.size > 0 && (
            <Card>
              <CardContent className="flex flex-wrap items-center gap-2 p-3">
                <span className="text-sm text-muted-foreground">已选 {selected.size} 项:</span>
                {bulkBtn('enable', '启用')}
                {bulkBtn('disable', '禁用')}
                {bulkBtn('delete', '删除', 'destructive')}
                <Button size="sm" variant="ghost" onClick={() => setSelected(new Set())}>取消选择</Button>
              </CardContent>
            </Card>
          )}

          <Card>
            <CardContent className="p-0">
              {data.length === 0 ? (
                <div className="py-10 text-center text-sm text-muted-foreground">
                  {list.isLoading ? '加载中...' : `暂无${conf.label}渠道——批量粘贴凭证添加${appId.startsWith('zcode') ? ',或用「在线登录」一键接入' : ''}`}
                </div>
              ) : (
                <Table>
                  <THead>
                    <TR>
                      <TH className="w-8"><Checkbox checked={allChecked} onChange={toggleAll} aria-label="全选" /></TH>
                      <TH>名称</TH><TH>类型</TH><TH>模型</TH><TH>状态</TH><TH>延迟</TH><TH>优先级</TH><TH>权重</TH>
                      <TH>操作</TH>
                    </TR>
                  </THead>
                  <TBody>
                    {data.map((p) => {
                      return (
                        <TR key={p.id} className={selected.has(p.id) ? 'bg-primary/5' : undefined}>
                          <TD><Checkbox checked={selected.has(p.id)} onChange={() => toggleOne(p.id)} aria-label={`选择 ${p.name}`} /></TD>
                          <TD><strong>{p.name}</strong></TD>
                          <TD><Badge>{p.channel_kind || p.provider_type}</Badge></TD>
                          <TD className="text-xs">{(p.models ?? []).slice(0, 2).join(', ')}{(p.models?.length ?? 0) > 2 && <Badge className="ml-1" tone="muted">+{(p.models?.length ?? 0) - 2}</Badge>}</TD>
                          <TD>
                            {!p.is_active
                              ? <Badge tone="destructive" title={p.disabled_reason ?? ''}>已禁用</Badge>
                              : <Badge tone={p.health_status === 'healthy' ? 'success' : p.health_status === 'degraded' ? 'warn' : p.health_status === 'down' ? 'destructive' : 'muted'}>{p.health_status}</Badge>}
                          </TD>
                          <TD>{p.latency_ms > 0 ? `${Math.round(p.latency_ms)}ms` : '-'}</TD>
                          <TD>{p.priority}</TD>
                          <TD>{p.weight}</TD>
                          <TD>
                            <div className="flex flex-wrap gap-1">
                              <Button size="sm" variant="outline" onClick={() => setDetailFor(p)}>
                                详情
                              </Button>
                              <Button size="sm" variant="outline" onClick={() => setEditFor({ id: p.id, name: p.name })}>
                                编辑
                              </Button>
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

          {appId === 'zcode-cn' && (
            <p className="text-xs text-muted-foreground">
              领取闲时额度(仅国内账号有;不自动到账):工作日窗口(晚 23:00–次日 09:00)3 亿 tokens,周末窗口(周五 23:00–周一 09:00,含周末全天)5 亿。
              服务端每日北京时间 07:10 自动领取(工作日早 7 点领当晚额度,周五领周末 5 亿;AIKUN_ZCODE_AUTO_CLAIM / AIKUN_ZCODE_CLAIM_HOUR 可调);
              API Key 形态(zai-plan / bigmodel-plan)无领取序列。
            </p>
          )}
    </div>
  )
}

// ---- 批量添加(按应用给预设;ZCode = 环境 × 凭证形态四预设)----

/** 预设:kind = 落库 channel_kind;zcode-cn 预设同 kind=zcode,仅 base 不同。 */
const PROXY_PRESETS: { id: string; kind: string; name: string; website_url: string; docs_url: string; protocols: string[]; default_protocol: string; openai_base_url: string; anthropic_base_url: string; key_placeholder: string; hint: string; apps: AppId[] }[] = [
  {
    id: 'zcode', kind: 'zcode', name: 'ZCode 订阅·国际', website_url: 'https://zcode.z.ai',
    docs_url: 'https://zcode.z.ai/cn/docs', protocols: ['anthropic'], default_protocol: 'anthropic',
    openai_base_url: 'https://zcode.z.ai', anthropic_base_url: 'https://zcode.z.ai',
    key_placeholder: 'ZCode 客户端 JWT(z.ai 国际账号),每行一个',
    hint: '推荐改用「在线登录」一键授权;手动粘贴 JWT 也可。免费窗口 Flash 零计费。',
    apps: ['zcode'],
  },
  {
    id: 'zcode-cn', kind: 'zcode', name: 'ZCode 订阅·国内', website_url: 'https://bigmodel.cn',
    docs_url: 'https://bigmodel.cn/glm-coding', protocols: ['anthropic'], default_protocol: 'anthropic',
    openai_base_url: 'https://zcode.z.ai', anthropic_base_url: 'https://zcode.z.ai',
    key_placeholder: 'ZCode 客户端 JWT(智谱 BigModel 国内账号),每行一个',
    hint: '国内版客户端(「连接 BigModel」登录)复制 JWT;API 同样走 zcode.z.ai(实测国内域 API 不可用),凭证与国际 z.ai 互不通用。',
    apps: ['zcode-cn'],
  },
  {
    id: 'zai-plan', kind: 'zai-plan', name: 'Coding Plan·国际 API Key', website_url: 'https://z.ai',
    docs_url: 'https://docs.z.ai', protocols: ['anthropic'], default_protocol: 'anthropic',
    openai_base_url: 'https://api.z.ai/api/anthropic', anthropic_base_url: 'https://api.z.ai/api/anthropic',
    key_placeholder: 'z.ai 平台签发的 Coding Plan API Key,每行一个',
    hint: '官方 Anthropic 兼容端点(api.z.ai/api/anthropic);与国际 ZCode 订阅同一套餐额度池。',
    apps: ['zcode'],
  },
  {
    id: 'bigmodel-plan', kind: 'bigmodel-plan', name: 'Coding Plan·国内 API Key', website_url: 'https://bigmodel.cn',
    docs_url: 'https://docs.bigmodel.cn/cn/coding-plan/faq', protocols: ['anthropic'], default_protocol: 'anthropic',
    openai_base_url: 'https://open.bigmodel.cn/api/anthropic', anthropic_base_url: 'https://open.bigmodel.cn/api/anthropic',
    key_placeholder: '智谱开放平台签发的 Coding Plan API Key,每行一个',
    hint: '国内官方 Anthropic 兼容端点(open.bigmodel.cn/api/anthropic);Coding 端点与通用端点额度不互通。',
    apps: ['zcode-cn'],
  },
  {
    id: 'workbuddy', kind: 'workbuddy', name: 'WorkBuddy', website_url: '', docs_url: '',
    protocols: ['openai'], default_protocol: 'openai', openai_base_url: '', anthropic_base_url: '',
    key_placeholder: 'WorkBuddy 凭证,每行一个', hint: '协议尚未接入(实验性),按 OpenAI 兼容转发。', apps: ['workbuddy'],
  },
  {
    id: 'qoder', kind: 'qoder', name: 'Qoder', website_url: '', docs_url: '',
    protocols: ['openai'], default_protocol: 'openai', openai_base_url: '', anthropic_base_url: '',
    key_placeholder: 'Qoder 凭证,每行一个', hint: '协议尚未接入(实验性),按 OpenAI 兼容转发。', apps: ['qoder'],
  },
]

function BatchModal({ appId, onOpenChange }: { appId: AppId; onOpenChange: (v: boolean) => void }) {
  const qc = useQueryClient()
  const presets = PROXY_PRESETS.filter((p) => p.apps.includes(appId))
  // 出网代理池(可选;选中即把 URL 拷贝进渠道 proxy_url)
  const pool = useQuery({
    queryKey: ['proxy-pool'],
    queryFn: async () => (await api<{ items: { id: string; name: string; url: string }[] }>('GET', '/api/proxies')).data,
  })
  const [proxyUrl, setProxyUrl] = useState('')
  const [presetId, setPresetId] = useState(presets[0]?.id ?? '')
  const preset = presets.find((p) => p.id === presetId)
  const [keys, setKeys] = useState('')
  const [models, setModels] = useState('')
  const [modelsStatus, setModelsStatus] = useState('可留空。zcode 建号后会自动获取账号信息并写入模型')
  const [priority, setPriority] = useState('0')
  const [weight, setWeight] = useState('1.0')
  const [busy, setBusy] = useState(false)

  const pullModels = async () => {
    const key = keys.split('\n').map((s) => s.trim()).filter(Boolean)[0]
    if (!preset || !key) return setModelsStatus('请先粘贴凭证')
    setModelsStatus('正在从上游获取模型…')
    const r = await api<{ models?: string[] }>('POST', '/api/providers/fetch-models', {
      base_url: preset.anthropic_base_url || preset.openai_base_url,
      api_key: key,
      protocol: preset.default_protocol,
      channel_kind: preset.kind,
      proxy_url: proxyUrl,
    })
    if (r.ok && (r.data.models ?? []).length > 0) {
      setModels(r.data.models!.join(', '))
      setModelsStatus(`已获取 ${r.data.models!.length} 个模型`)
    } else {
      setModelsStatus(errMsg(r) || '未能获取，可手动填写')
    }
  }

  const submit = async () => {
    const keyList = keys.split('\n').map((s) => s.trim()).filter(Boolean)
    if (!preset) return sonnerToast.error('请选择接入类型')
    if (keyList.length === 0) return sonnerToast.error('请至少粘贴一个凭证(每行一个)')
    setBusy(true)
    try {
      const r = await api<{ created?: number; failed?: unknown[]; providers?: unknown[] }>('POST', '/api/providers/batch', {
        name: preset.name,
        website_url: preset.website_url,
        channel_kind: preset.kind,
        protocols: preset.protocols,
        default_protocol: preset.default_protocol,
        openai_base_url: preset.openai_base_url,
        anthropic_base_url: preset.anthropic_base_url,
        api_keys: keyList,
        note: preset.kind === 'zcode' && preset.id === 'zcode-cn' ? '[国内] 手动添加' : undefined,
        proxy_url: proxyUrl,
        models: models.split(',').map((s) => s.trim()).filter(Boolean),
        priority: parseInt(priority, 10) || 0,
        weight: parseFloat(weight) || 1.0,
      })
      if (r.ok) {
        sonnerToast.success(`批量创建完成:成功 ${r.data.created ?? 0} 个`)
        onOpenChange(false)
        void qc.invalidateQueries({ queryKey: ['proxies'] })
      } else sonnerToast.error(errMsg(r))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open onOpenChange={onOpenChange}>
      <DialogContent title={`批量添加${APPS[appId].label}渠道`}>
        <form className="space-y-3.5" onSubmit={(e) => { e.preventDefault(); void submit() }}>
          <div className="space-y-1.5">
            <label className="text-xs font-medium text-muted-foreground">接入类型</label>
            <div className="flex flex-wrap gap-1.5">
              {presets.map((p) => (
                <button key={p.id} type="button" onClick={() => setPresetId(p.id)}
                  className={`rounded-full border px-3 py-1 text-xs font-medium transition-colors ${presetId === p.id ? 'border-primary bg-primary/10 text-primary' : 'border-border text-muted-foreground hover:bg-muted'}`}>
                  {p.name}
                </button>
              ))}
            </div>
            {preset && (
              <p className="text-xs text-muted-foreground">
                {preset.hint}
                {preset.docs_url && <> · <a className="text-primary hover:underline" href={preset.docs_url} target="_blank" rel="noopener">文档</a></>}
              </p>
            )}
          </div>
          <div className="space-y-1.5">
            <label className="text-xs font-medium text-muted-foreground">{preset?.key_placeholder ?? '凭证(每行一个)'}</label>
            <Textarea rows={4} className="font-mono text-xs" value={keys} onChange={(e) => setKeys(e.target.value)} placeholder={'凭证1\n凭证2\n凭证3'} />
          </div>
          <div className="space-y-1.5">
            <label className="text-xs font-medium text-muted-foreground">出网代理(可选)</label>
            <select
              className="flex h-9 w-full rounded-md border border-input bg-transparent px-3 py-1 text-sm shadow-sm"
              value={proxyUrl} onChange={(e) => setProxyUrl(e.target.value)}>
              <option value="">直连(不走代理)</option>
              {(pool.data?.items ?? []).map((p) => (
                <option key={p.id} value={p.url}>{p.name} · {p.url.replace(/(:\/\/[^:\/]+:)[^@]+@/, '$1***@')}</option>
              ))}
            </select>
            <p className="text-xs text-muted-foreground">socks5/http 代理在「代理」页统一管理</p>
          </div>
          <div className="space-y-1.5">
            <label className="text-xs font-medium text-muted-foreground">模型(逗号分隔)</label>
            <div className="flex gap-2">
              <Input className="flex-1" value={models} onChange={(e) => setModels(e.target.value)} placeholder="留空则建号后自动获取" />
              <Button type="button" variant="outline" size="sm" onClick={() => void pullModels()}>获取模型</Button>
            </div>
            <p className="text-xs text-muted-foreground">{modelsStatus}</p>
          </div>
          <div className="grid grid-cols-2 gap-2">
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">优先级</label>
                <Input type="number" value={priority} onChange={(e) => setPriority(e.target.value)} />
              </div>
              <div className="space-y-1.5">
                <label className="text-xs font-medium text-muted-foreground">权重</label>
                <Input type="number" step="0.1" value={weight} onChange={(e) => setWeight(e.target.value)} />
              </div>
            </div>
          <div className="flex justify-end gap-2 pt-2">
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)}>取消</Button>
            <Button type="submit" disabled={busy}>{busy ? '创建中…' : `批量创建`}</Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  )
}

// ---- 导入(文件 / 粘贴 JSON)----

function ImportModal({ appId, onOpenChange, onDone }: { appId: AppId; onOpenChange: (v: boolean) => void; onDone: () => void }) {
  const [text, setText] = useState('')
  const [busy, setBusy] = useState(false)
  const [result, setResult] = useState<{ imported?: number; skipped?: number; failed?: { index: number; code: string; message?: string }[] } | null>(null)
  const fileRef = useRef<HTMLInputElement>(null)

  const submit = async (content: string) => {
    let body: any
    try {
      body = JSON.parse(content)
    } catch {
      return sonnerToast.error('不是有效的 JSON')
    }
    // 在对应页面导入时强制规范化区域标记，方便网关调度（is_cn_account 依赖 note/website_url）
    const isCn = appId === 'zcode-cn'
    const providers = Array.isArray(body.providers) ? body.providers : (Array.isArray(body) ? body : [])
    for (const p of providers) {
      if (p.channel_kind === 'zcode' || !p.channel_kind) {
        if (isCn) {
          if (!(p.note || '').includes('国内')) {
            p.note = `[国内] ${p.note || '导入账号'}`.trim()
          }
          if (!p.website_url || !p.website_url.includes('bigmodel')) {
            p.website_url = 'https://bigmodel.cn'
          }
        } else if (appId === 'zcode') {
          if ((p.note || '').includes('国内')) {
            p.note = (p.note || '').replace(/\[国内\]\s*/g, '').trim() || '国际账号'
          }
          if (!p.website_url || p.website_url.includes('bigmodel')) {
            p.website_url = 'https://zcode.z.ai'
          }
        }
        p.channel_kind = p.channel_kind || 'zcode'
      }
    }
    if (body.providers) body.providers = providers
    setBusy(true)
    try {
      const r = await api<{ imported?: number; skipped?: number; failed?: { index: number; code: string; message?: string }[] }>('POST', '/api/providers/import', body)
      if (r.ok) {
        setResult(r.data)
        sonnerToast.success(`导入完成:导入 ${r.data.imported ?? 0} / 跳过 ${r.data.skipped ?? 0}`)
        onDone()
      } else sonnerToast.error(errMsg(r))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open onOpenChange={onOpenChange}>
      <DialogContent title="导入反代渠道">
        <p className="mb-2 text-xs text-destructive">⚠ 导入内容含明文凭证;同 api_key 已存在的渠道会被跳过(不覆盖)。</p>
        <div className="mb-2 flex gap-2">
          <Button type="button" variant="outline" size="sm" onClick={() => fileRef.current?.click()}>选择导出文件</Button>
          <input ref={fileRef} type="file" accept=".json,application/json" className="hidden"
            onChange={async (e) => {
              const f = e.target.files?.[0]
              if (!f) return
              const content = await f.text()
              setText(content)
              void submit(content)
            }} />
        </div>
        <Textarea rows={7} className="font-mono text-xs" value={text} onChange={(e) => setText(e.target.value)}
          placeholder='粘贴导出文件内容({"version":1,"providers":[...]})' />
        {result && (
          <div className="mt-2 text-xs">
            导入 {result.imported ?? 0} · 跳过 {result.skipped ?? 0}
            {(result.failed ?? []).length > 0 && (
              <div className="mt-1 max-h-32 overflow-auto text-destructive">
                {(result.failed ?? []).map((f) => <div key={f.index}>#{f.index + 1} {f.code}: {f.message ?? ''}</div>)}
              </div>
            )}
          </div>
        )}
        <div className="mt-3 flex justify-end gap-2">
          <Button variant="outline" onClick={() => onOpenChange(false)}>关闭</Button>
          <Button disabled={busy || !text.trim()} onClick={() => void submit(text)}>{busy ? '导入中…' : '导入'}</Button>
        </div>
      </DialogContent>
    </Dialog>
  )
}


// ---- 账号信息(订阅 / 5h·周额度 / 重置次数) ----

type ZcodeAccountInfo = {
  account_id?: string
  display?: string
  plan_name?: string
  name_updated: boolean
  plan_credentials: boolean
  warnings: string[]
  balance: {
    balances?: Array<{ show_name?: string; used_units?: number; total_units?: number; remaining_units?: number; expires_at?: number }>
  }
  reset_status?: {
    available_five_hour_resets?: Array<{ expire_at: number }>
    available_week_resets?: Array<{ expire_at: number }>
    latest_five_hour_reset_history?: { used_at: number } | null
    latest_week_reset_history?: { used_at: number } | null
  } | null
  subscription?: unknown
  quota?: { level?: string; limits?: Array<{ type?: string; unit?: number; number?: number; usage?: number; remaining?: number; percentage?: number; nextResetTime?: number }> } | null
}

/// 北京时间是否处于闲时免费窗口(与后端 is_off_peak 同口径:
/// 任意日 23:00–次日 09:00,周六/周日全天)。
function beijingOffPeakNow(): boolean {
  const now = new Date(Date.now() + 8 * 3600 * 1000)
  const day = now.getUTCDay() // 0=周日 … 6=周六
  const h = now.getUTCHours()
  return day === 0 || day === 6 || h >= 23 || h < 9
}

/// 时间戳(s)→ 本地短时间;空值 '-'。
function ts(v?: number | null): string {
  return v && v > 0 ? new Date(v * (v > 1e12 ? 1 : 1000)).toLocaleString('zh-CN', { hour12: false }) : '-'
}

/// 千分位紧凑量纲(token 数常为亿级)。
function fmtNum(v?: number | null): string {
  if (v == null || !Number.isFinite(v)) return '-'
  if (v >= 1e8) return `${(v / 1e8).toFixed(2)} 亿`
  if (v >= 1e4) return `${(v / 1e4).toFixed(1)} 万`
  return v.toLocaleString()
}

/// monitor 额度条标签(客户端 i18n 口径:5 小时提示词池/每周配额/月度)。
function quotaLabel(l: { type?: string }): string {
  const t = (l.type ?? '').toLowerCase()
  if (t.includes('five') || t.includes('5h')) return '5 小时额度'
  if (t.includes('week')) return '每周额度'
  if (t.includes('credit') || t.includes('limit')) return '额度上限'
  if (t.includes('time')) return '用量额度'
  return l.type ?? '额度'
}

function bucketLabel(show?: string): string {
  const s = (show ?? '').trim()
  if (!s || /credit|limit|quota/i.test(s)) return '额度'
  return s
}

// ---- 详情(条目信息 + 账号信息缓存 + 模型测试) ----

function DetailModal({ target, onClose, onDone, onClaim, claiming }: {
  target: ProviderDetail
  onClose: () => void
  onDone: () => void
  onClaim: (id: string, name: string) => Promise<void>
  claiming: Set<string>
}) {
  const qc = useQueryClient()
  const isZcode = target.channel_kind === 'zcode'
  const domestic = isZcode && isDomestic(target)
  const [resetBusy, setResetBusy] = useState<string | null>(null)
  const [fetching, setFetching] = useState(false)
  const [pullingModels, setPullingModels] = useState(false)
  const [testingModel, setTestingModel] = useState<string | null>(null)
  const [modelResult, setModelResult] = useState<Record<string, string>>({})

  // 缓存读(GET 零上游调用);「获取/重新获取」走 POST 拉新并刷新缓存
  const cached = useQuery({
    queryKey: ['zcode-account-cached', target.id],
    queryFn: async () => (await api<{ fetched_at?: string | null; info?: ZcodeAccountInfo | null }>(
      'GET', `/api/providers/${target.id}/zcode/account-info`)).data,
    enabled: isZcode, retry: false,
  })

  const refreshInfo = async () => {
    setFetching(true)
    try {
      const r = await api<ZcodeAccountInfo & { name_updated: boolean }>(
        'POST', `/api/providers/${target.id}/zcode/account-info`)
      if (r.ok) {
        sonnerToast.success(`${target.name}:账号信息已获取${r.data.name_updated ? '(名称已回填)' : ''}，模型已同步`)
        void qc.invalidateQueries({ queryKey: ['zcode-account-cached', target.id] })
        void qc.invalidateQueries({ queryKey: ['proxies'] })
        onDone()
      } else {
        sonnerToast.error(`${target.name} 获取失败:${errMsg(r)}`)
      }
    } finally {
      setFetching(false)
    }
  }

  const pullModels = async () => {
    setPullingModels(true)
    try {
      const base = target.anthropic_base_url || target.openai_base_url
      const r = await api<{ models?: string[] }>('POST', '/api/providers/fetch-models', {
        base_url: base,
        protocol: 'anthropic',
        channel_kind: target.channel_kind || 'zcode',
        provider_id: target.id,
      })
      if (!r.ok || !(r.data.models ?? []).length) {
        sonnerToast.error(errMsg(r) || '上游没有返回模型')
        return
      }
      const models = r.data.models!
      const u = await api('PATCH', `/api/providers/${target.id}`, { models })
      if (u.ok) {
        sonnerToast.success(`已同步 ${models.length} 个模型`)
        void qc.invalidateQueries({ queryKey: ['proxies'] })
        onDone()
      } else {
        sonnerToast.error(errMsg(u))
      }
    } finally {
      setPullingModels(false)
    }
  }

  const useReset = async (resetType: 'FIVE_HOUR' | 'WEEK') => {
    const label = resetType === 'FIVE_HOUR' ? '5 小时' : '每周'
    if (!confirm(`确定使用一次「${label}额度重置」吗?\n这是消耗性动作:扣一次重置机会,把${label}额度窗清零重来。`)) return
    setResetBusy(resetType)
    try {
      const r = await api<{ used: boolean }>(
        'POST', `/api/providers/${target.id}/zcode/reset-quota`, { reset_type: resetType })
      if (r.ok && r.data.used) {
        sonnerToast.success(`${target.name}:${label}额度已重置`)
        void refreshInfo()
      } else if (r.ok) {
        sonnerToast.warning(`${target.name}:上游未确认重置(used=false)`)
      } else {
        sonnerToast.error(`${target.name} 重置失败:${errMsg(r)}`)
      }
    } finally {
      setResetBusy(null)
    }
  }

  const testOne = async (laneKey: string, model: string) => {
    setTestingModel(laneKey)
    try {
      const r = await api<{ ok?: boolean; status?: number; error?: string; snippet?: string }>(
        'POST', `/api/providers/${target.id}/model-tests`, { model, message: '回复两个字:成功', stream: false })
      if (r.ok) {
        const d = r.data
        const verdict = d.ok
          ? `✓ ${(d.snippet || '').slice(0, 30)}`
          : (d.error || `上游 ${d.status ?? ''}`)
        setModelResult((m) => ({ ...m, [laneKey]: verdict }))
        if (d.ok) sonnerToast.success(`${target.name} · ${model}:调用成功`)
        else sonnerToast.warning(`${target.name} · ${model}:${verdict}`)
      } else {
        const msg = errMsg(r)
        setModelResult((m) => ({ ...m, [laneKey]: msg }))
        sonnerToast.error(`${target.name} · ${model} 测试失败:${msg}`)
      }
    } finally {
      setTestingModel(null)
    }
  }

  const info = cached.data?.info
  const models = target.models ?? []
  const offPeak = beijingOffPeakNow()
  // 按实际模型列表展示，不再写死 5.3 / Flash 车道。
  const lanes: Array<{ key: string; model: string; badge: string; tone: 'default' | 'success'; hint?: string }> =
    models.map((m) => ({
      key: m,
      model: m,
      badge: m.toLowerCase().includes('flash') && offPeak ? '闲时优先' : '订阅',
      tone: m.toLowerCase().includes('flash') && offPeak ? 'success' as const : 'default' as const,
      hint: m.toLowerCase().includes('flash')
        ? (offPeak ? '闲时窗口内优先走免费桶' : '非闲时走订阅额度')
        : undefined,
    }))

  return (
    <Dialog open onOpenChange={onClose}>
      <DialogContent title={`详情 · ${target.name}`}>
        <div className="max-h-[70vh] space-y-4 overflow-y-auto pr-1 text-sm">
          {/* 条目信息 */}
          <section className="space-y-1.5">
            <h4 className="font-medium">条目信息</h4>
            <div className="grid grid-cols-[5rem_1fr] items-baseline gap-x-2 text-xs">
              <span className="text-muted-foreground">名称(备注)</span><span className="break-all">{target.name}</span>
              <span className="text-muted-foreground">类型</span><span>{target.channel_kind || target.provider_type}</span>
              <span className="text-muted-foreground">优先级 / 权重</span><span>{target.priority} / {target.weight}</span>
              <span className="text-muted-foreground">状态</span>
              <span>{!target.is_active ? '已禁用' : `${target.health_status}${target.latency_ms > 0 ? ` · ${Math.round(target.latency_ms)}ms` : ''}`}</span>
              <span className="text-muted-foreground">端点</span>
              <span className="font-mono break-all text-[11px]">{target.anthropic_base_url || target.openai_base_url}</span>
            </div>
          </section>

          {/* 账号信息(zcode;缓存优先,未获取可拉新) */}
          {isZcode && (
            <section className="space-y-2">
              <div className="flex items-center justify-between">
                <h4 className="font-medium">账号信息</h4>
                <div className="flex gap-2">
                  {domestic && (
                    <Button size="sm" variant="outline" disabled={claiming.has(target.id)}
                      onClick={() => void onClaim(target.id, target.name)}>
                      {claiming.has(target.id) ? '领取中…' : '领取闲时额度'}
                    </Button>
                  )}
                  <Button size="sm" variant="outline" disabled={fetching} onClick={() => void refreshInfo()}>
                    {fetching ? '获取中…' : info ? '重新获取' : '获取账号信息'}
                  </Button>
                </div>
              </div>
              {cached.isLoading ? (
                <p className="py-3 text-center text-xs text-muted-foreground">读取缓存…</p>
              ) : info ? (
                <>
                  {cached.data?.fetched_at && (
                    <p className="text-[11px] text-muted-foreground">
                      拉取于 {new Date(cached.data.fetched_at).toLocaleString('zh-CN', { hour12: false })}
                    </p>
                  )}
                  <AccountInfoBody d={info} resetBusy={resetBusy} useReset={useReset} />
                </>
              ) : (
                <div className="space-y-2 rounded-md border border-dashed p-3">
                  <p className="text-xs text-muted-foreground">
                    尚未获取账号信息——点击「获取账号信息」用渠道凭证查上游(零消耗:免费额度桶、订阅、5 小时/每周额度、重置次数),结果会保存在这里,下次打开直接显示。
                  </p>
                </div>
              )}
            </section>
          )}

          {/* 能力矩阵(zcode:订阅×2 + 闲时免费;其他渠道逐模型) */}
          <section className="space-y-1.5">
            <div className="flex items-center justify-between">
              <h4 className="font-medium">可用模型</h4>
              <Button size="sm" variant="outline" disabled={pullingModels} onClick={() => void pullModels()}>
                {pullingModels ? '获取中…' : '获取模型'}
              </Button>
            </div>
            {lanes.length === 0 ? (
              <p className="rounded-md border border-dashed p-2 text-xs text-muted-foreground">
                暂无模型。点「获取模型」从上游同步。
              </p>
            ) : lanes.map((lane) => {
              const testing = testingModel === lane.key
              const result = modelResult[lane.key]
              return (
                <button
                  key={lane.key}
                  type="button"
                  disabled={testing}
                  className="flex w-full items-center justify-between gap-3 rounded-md border px-2 py-1.5 text-left text-xs hover:bg-accent disabled:opacity-60"
                  onClick={() => void testOne(lane.key, lane.model)}
                >
                  <span className="flex flex-wrap items-center gap-2">
                    <span className="font-mono">{lane.model}</span>
                    {lane.badge && <Badge tone={lane.tone}>{lane.badge}</Badge>}
                    {lane.hint && <span className="text-[10px] text-muted-foreground">{lane.hint}</span>}
                  </span>
                  <span className={result?.startsWith('✓') ? 'text-emerald-600' : result ? 'text-amber-600' : 'text-muted-foreground'}>
                    {testing ? '测试中…' : result ?? '点击测试'}
                  </span>
                </button>
              )
            })}
            {isZcode && (
              <p className="text-[11px] text-muted-foreground">
                闲时窗口(北京时间 23:00–09:00 / 周末全天)内网关优先消耗免费额度,免费桶并发超限自动落回订阅额度。
              </p>
            )}
          </section>
        </div>
      </DialogContent>
    </Dialog>
  )
}

/// 账号信息主体(订阅 / 5h·周额度 / 重置次数)——详情弹窗内嵌,
/// 数据来自缓存或刚刚拉取的 POST 结果。
function AccountInfoBody({ d, resetBusy, useReset }: {
  d: ZcodeAccountInfo
  resetBusy: string | null
  useReset: (t: 'FIVE_HOUR' | 'WEEK') => void
}) {
  // 订阅:响应可能是对象(active 项)或数组;取第一个 active
  const subs: Array<Record<string, unknown>> = Array.isArray(d.subscription)
    ? d.subscription as Array<Record<string, unknown>>
    : d.subscription ? [d.subscription as Record<string, unknown>] : []
  const activeSub = subs.find((s) => String(s.status ?? 'active').toLowerCase() === 'active') ?? subs[0]

  return (
    <div className="space-y-3 text-sm">
      <section className="space-y-1.5">
        <div className="grid grid-cols-[5rem_1fr] items-baseline gap-x-2 text-xs">
          {(d.display || d.account_id) && (
            <>
              <span className="text-muted-foreground">标识</span>
              <span className="font-mono break-all">{d.display || d.account_id}</span>
            </>
          )}
          {d.plan_name && (
            <>
              <span className="text-muted-foreground">套餐</span>
              <span>{d.plan_name}</span>
            </>
          )}
        </div>
        {(d.balance?.balances ?? []).map((b, i) => (
          <div key={i} className="rounded-md border p-2">
            <UsageBar label={bucketLabel(b.show_name)} used={b.used_units} total={b.total_units} />
            {b.expires_at ? <div className="mt-1 text-[11px] text-muted-foreground">到期 {ts(b.expires_at)}</div> : null}
          </div>
        ))}
      </section>

      {/* 订阅(Lite 等付费计划;需在线登录补全的业务 token) */}
      <section className="space-y-1.5">
        {!d.plan_credentials ? (
          <p className="rounded-md border border-dashed p-2 text-xs text-muted-foreground">
            未持有订阅查询凭证——重新「在线登录」一次即可补全订阅与 5 小时/每周额度信息(批量粘贴 JWT 的渠道没有此凭证)
          </p>
        ) : activeSub ? (
          <div className="grid grid-cols-[5rem_1fr] items-baseline gap-x-2 text-xs">
            <span className="text-muted-foreground">计划</span>
            <span>{String(activeSub.productName ?? activeSub.productId ?? '-')}</span>
            {activeSub.billingCycle != null && <><span className="text-muted-foreground">周期</span><span>{String(activeSub.billingCycle)}</span></>}
            {activeSub.expireTime != null && <><span className="text-muted-foreground">到期</span><span>{ts(Number(activeSub.expireTime))}</span></>}
          </div>
        ) : (
          <p className="text-xs text-muted-foreground">无有效订阅记录</p>
        )}
      </section>

      {/* 5 小时 / 每周额度(monitor 域) */}
      {d.plan_credentials && (
        <section className="space-y-1.5">
          {d.quota?.limits?.length ? (
            d.quota.limits.map((l, i) => (
              <div key={i} className="rounded-md border p-2">
                <UsageBar label={quotaLabel(l)} used={l.usage} total={l.number} percentage={l.percentage} />
                <div className="mt-1 text-[11px] text-muted-foreground">重置于 {ts(l.nextResetTime)}</div>
              </div>
            ))
          ) : (
            <p className="text-xs text-muted-foreground">{d.quota ? '无额度记录(可能无付费订阅)' : '额度查询失败(见下方说明)'}</p>
          )}
        </section>
      )}

      {/* 重置次数 + 使用按钮 */}
      {d.reset_status ? (
        <section className="space-y-2">
          <div className="flex flex-wrap gap-2">
            <ResetChip title="5 小时重置" count={d.reset_status.available_five_hour_resets?.length ?? 0}
              lastUsed={d.reset_status.latest_five_hour_reset_history?.used_at}
              expires={d.reset_status.available_five_hour_resets?.[0]?.expire_at} />
            <ResetChip title="每周重置" count={d.reset_status.available_week_resets?.length ?? 0}
              lastUsed={d.reset_status.latest_week_reset_history?.used_at}
              expires={d.reset_status.available_week_resets?.[0]?.expire_at} />
          </div>
          <div className="flex gap-2">
            <Button size="sm" variant="outline" disabled={(d.reset_status.available_five_hour_resets?.length ?? 0) === 0 || resetBusy !== null}
              onClick={() => void useReset('FIVE_HOUR')}>
              {resetBusy === 'FIVE_HOUR' ? '重置中…' : '使用 5 小时重置'}
            </Button>
            <Button size="sm" variant="outline" disabled={(d.reset_status.available_week_resets?.length ?? 0) === 0 || resetBusy !== null}
              onClick={() => void useReset('WEEK')}>
              {resetBusy === 'WEEK' ? '重置中…' : '使用每周重置'}
            </Button>
          </div>
        </section>
      ) : (
        <p className="text-xs text-muted-foreground">重置次数查询失败(见下方说明)</p>
      )}

      {d.warnings?.length > 0 && (
        <p className="text-[11px] text-muted-foreground">⚠ {d.warnings.join(';')}</p>
      )}
    </div>
  )
}

/// 用量进度条(used/total 或直接给 percentage)。
function UsageBar({ label, used, total, percentage }: { label: string; used?: number; total?: number; percentage?: number }) {
  const pct = percentage != null && percentage >= 0 ? percentage
    : total && total > 0 && used != null ? (used / total) * 100 : 0
  const clamped = Math.min(100, Math.max(0, pct))
  return (
    <div>
      <div className="flex items-baseline justify-between text-xs">
        <span>{label}</span>
        <span className="font-mono text-muted-foreground">
          {used != null && total ? `${fmtNum(used)} / ${fmtNum(total)}` : `${Math.round(clamped)}%`}
        </span>
      </div>
      <div className="mt-1 h-1.5 overflow-hidden rounded-full bg-muted">
        <div className={`h-full rounded-full ${clamped > 90 ? 'bg-destructive' : clamped > 70 ? 'bg-amber-500' : 'bg-emerald-500'}`} style={{ width: `${clamped}%` }} />
      </div>
    </div>
  )
}

/// 重置次数徽章(可用 N 次 + 最近使用 + 过期时间)。
function ResetChip({ title, count, lastUsed, expires }: { title: string; count: number; lastUsed?: number | null; expires?: number }) {
  return (
    <div className="rounded-md border p-2 text-xs">
      <div className="flex items-center gap-2">
        <span className="font-medium">{title}</span>
        <Badge tone={count > 0 ? 'success' : 'warn'}>可用 {count} 次</Badge>
      </div>
      <div className="mt-1 text-[11px] text-muted-foreground">
        上次使用 {ts(lastUsed)}{count > 0 && expires ? ` · 机会过期 ${ts(expires)}` : ''}
      </div>
    </div>
  )
}

// ---- 编辑(优先级/权重;账号间分级管理) ----

function EditModal({ target, onDone, onClose }: { target: { id: string; name: string }; onDone: () => void; onClose: () => void }) {
  const [name, setName] = useState(target.name)
  const [busy, setBusy] = useState(false)
  const submit = async () => {
    setBusy(true)
    try {
      const r = await api('PATCH', `/api/providers/${target.id}`, {
        name: name.trim(),
      })
      if (r.ok) {
        sonnerToast.success('已保存')
        onDone()
        onClose()
      } else sonnerToast.error(errMsg(r))
    } finally {
      setBusy(false)
    }
  }
  return (
    <Dialog open onOpenChange={onClose}>
      <DialogContent title={`编辑 · ${target.name}`}>
        <form className="space-y-3.5" onSubmit={(e) => { e.preventDefault(); void submit() }}>
          <div className="space-y-1.5">
            <label className="text-xs font-medium text-muted-foreground">名称(即备注,可自由修改)</label>
            <Input value={name} onChange={(e) => setName(e.target.value)} placeholder="账号标记 / 用途……" />
            <p className="text-xs text-muted-foreground">列表中显示的就是这个名称;自动生成的 zcode- 前缀名会在获取账号信息时回填为账号标识,手动改过后不再覆盖。</p>
          </div>
          <div className="flex justify-end gap-2">
            <Button type="button" variant="outline" onClick={onClose}>取消</Button>
            <Button type="submit" disabled={busy || !name.trim()}>{busy ? '保存中…' : '保存'}</Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  )
}
