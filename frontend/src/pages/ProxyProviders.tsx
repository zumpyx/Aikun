// 反代应用中心:每应用一个子页(/proxies/:app),ZCode 子页含
// 国际/国内 × 订阅JWT/APIKey 四种接入预设与免费额度领取任务面板;
// WorkBuddy/Qoder 为占位协议子页;Z.AI Chat 待接入。
// 批量启停/删除、导入导出随应用(kind 过滤);表单复用批量粘贴。
import { useEffect, useRef, useState } from 'react'
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
    desc: 'GLM Coding Plan·国内(智谱 BigModel):订阅 JWT(API 走 zcode.z.ai)与 API Key(open.bigmodel.cn);凭证与国际 z.ai 互不通用',
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

type ClaimTask = {
  provider_id: string
  last_run_at: string
  source: 'manual' | 'oauth' | 'auto'
  claimed: string[]
  skipped: string[]
  problems: string[]
  ok: boolean
}

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
  // 领取任务面板(仅 zcode 应用拉取)
  const tasks = useQuery({
    queryKey: ['zcode-tasks'],
    queryFn: async () => (await api<{ tasks: ClaimTask[] }>('GET', '/api/providers/zcode/tasks')).data,
    enabled: appId === 'zcode-cn',
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
  const [infoFor, setInfoFor] = useState<{ id: string; name: string } | null>(null)
  const [noteFor, setNoteFor] = useState<{ id: string; name: string; note: string } | null>(null)
  const [testing, setTesting] = useState<string | null>(null)
  const [editFor, setEditFor] = useState<{ id: string; name: string; priority: number; weight: number } | null>(null)
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

  // 行内测试:真实对话(model-tests),模型取渠道第一个
  const testModel = async (id: string, name: string, model: string) => {
    setTesting(id)
    try {
      const r = await api<{ ok?: boolean; status?: number; error?: string; response?: unknown }>(
        'POST', `/api/providers/${id}/model-tests`, { model, message: '回复两个字:成功', stream: false })
      if (r.ok) {
        const d = r.data
        if (d.ok) sonnerToast.success(`${name}:模型调用成功`)
        else sonnerToast.warning(`${name}:${d.error || `上游 ${d.status ?? ''}`}`)
      } else sonnerToast.error(`${name} 测试失败:${errMsg(r)}`)
    } finally {
      setTesting(null)
    }
  }

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
  const taskOf = (id: string) => tasks.data?.tasks.find((t) => t.provider_id === id)

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
          {infoFor && <AccountInfoModal target={infoFor} onClose={() => setInfoFor(null)} onDone={invalidate} />}
          {noteFor && <NoteModal target={noteFor} onDone={invalidate} onClose={() => setNoteFor(null)} />}
          {editFor && <EditModal target={editFor} onDone={invalidate} onClose={() => setEditFor(null)} />}
          {importOpen && <ImportModal onOpenChange={setImportOpen} onDone={invalidate} />}

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
                      {appId === 'zcode-cn' && <TH>上次领取</TH>}
                      <TH>操作</TH>
                    </TR>
                  </THead>
                  <TBody>
                    {data.map((p) => {
                      const t = appId === 'zcode-cn' ? taskOf(p.id) : undefined
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
                          {appId === 'zcode' && (
                            <TD className="text-xs">
                              {t ? (
                                <span title={`领取:${t.claimed.join('、') || '-'}\n已持有:${t.skipped.join('、') || '-'}\n问题:${t.problems.join(';') || '-'}`}>
                                  <Badge tone={t.ok ? 'success' : 'warn'}>{t.source === 'auto' ? '自动' : t.source === 'oauth' ? '登录' : '手动'}</Badge>
                                  {' '}{t.last_run_at.slice(5, 16).replace('T', ' ')}
                                </span>
                              ) : <span className="text-muted-foreground">未签到</span>}
                            </TD>
                          )}
                          <TD>
                            <div className="flex flex-wrap gap-1">
                              {appId === 'zcode-cn' && p.channel_kind === 'zcode' && (
                                <Button size="sm" variant="outline" disabled={claiming.has(p.id)} onClick={() => void doClaim(p.id, p.name)}>
                                  {claiming.has(p.id) ? '领取中…' : '领取'}
                                </Button>
                              )}
                              {p.channel_kind === 'zcode' && (
                                <Button size="sm" variant="outline" onClick={() => setInfoFor({ id: p.id, name: p.name })}>
                                  账号信息
                                </Button>
                              )}
                              <TestMenu
                                providerId={p.id}
                                name={p.name}
                                models={p.models ?? []}
                                kind={p.channel_kind ?? ''}
                                testing={testing === p.id}
                                onTest={(m) => void testModel(p.id, p.name, m)}
                              />
                              <Button size="sm" variant="outline" onClick={() => setEditFor({ id: p.id, name: p.name, priority: p.priority, weight: p.weight })}>
                                编辑
                              </Button>
                              <Button size="sm" variant="ghost" onClick={() => setNoteFor({ id: p.id, name: p.name, note: p.note ?? '' })}>
                                备注
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
  const [models, setModels] = useState('GLM-5.3, GLM-5.3-Flash')
  const [priority, setPriority] = useState('0')
  const [weight, setWeight] = useState('1.0')
  const [busy, setBusy] = useState(false)
  const submit = async () => {
    const keyList = keys.split('\n').map((s) => s.trim()).filter(Boolean)
    if (!preset) return sonnerToast.error('请选择接入类型')
    if (keyList.length === 0) return sonnerToast.error('请至少粘贴一个凭证(每行一个)')
    if (!models.trim()) return sonnerToast.error('请填写模型(逗号分隔)')
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
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">模型(逗号分隔)</label>
              <Input value={models} onChange={(e) => setModels(e.target.value)} />
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

function ImportModal({ onOpenChange, onDone }: { onOpenChange: (v: boolean) => void; onDone: () => void }) {
  const [text, setText] = useState('')
  const [busy, setBusy] = useState(false)
  const [result, setResult] = useState<{ imported?: number; skipped?: number; failed?: { index: number; code: string; message?: string }[] } | null>(null)
  const fileRef = useRef<HTMLInputElement>(null)

  const submit = async (content: string) => {
    let body: unknown
    try {
      body = JSON.parse(content)
    } catch {
      return sonnerToast.error('不是有效的 JSON')
    }
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


// ---- 手动备注(渠道 note 编辑) ----

function NoteModal({ target, onDone, onClose }: { target: { id: string; name: string; note: string }; onDone: () => void; onClose: () => void }) {
  const [note, setNote] = useState(target.note)
  const [busy, setBusy] = useState(false)
  const submit = async () => {
    setBusy(true)
    try {
      const r = await api('PATCH', `/api/providers/${target.id}`, { note })
      if (r.ok) {
        sonnerToast.success('备注已保存')
        onDone()
        onClose()
      } else sonnerToast.error(errMsg(r))
    } finally {
      setBusy(false)
    }
  }
  return (
    <Dialog open onOpenChange={onClose}>
      <DialogContent title={`备注 · ${target.name}`}>
        <form className="space-y-3.5" onSubmit={(e) => { e.preventDefault(); void submit() }}>
          <textarea
            className="flex min-h-24 w-full rounded-md border border-input bg-transparent px-3 py-2 text-sm shadow-sm"
            value={note} onChange={(e) => setNote(e.target.value)}
            placeholder="账号标记 / 用途 / 备注……(「账号信息」一键获取会覆盖为最新摘要)" />
          <div className="flex justify-end gap-2">
            <Button type="button" variant="outline" onClick={onClose}>取消</Button>
            <Button type="submit" disabled={busy}>{busy ? '保存中…' : '保存'}</Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  )
}



/** 行内测试菜单:逐模型发起测试;zcode 渠道按档位标注——名称含
 * flash = 免费档(闲时免费桶,GLM-5.3-Flash 是免费 plan 唯一能力),
 * 其余 = 订阅档(吃付费套餐/正常额度,无订阅时上游 1005/3009)。 */
function TestMenu({
  providerId, name, models, kind, testing, onTest,
}: {
  providerId: string
  name: string
  models: string[]
  kind: string
  testing: boolean
  onTest: (model: string) => void
}) {
  const [open, setOpen] = useState(false)
  const ref = useRef<HTMLDivElement | null>(null)
  const isZcode = kind === 'zcode'

  useEffect(() => {
    if (!open) return
    const close = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener('mousedown', close)
    return () => document.removeEventListener('mousedown', close)
  }, [open])

  const list = models.length > 0 ? models : ['GLM-5.3-Flash']
  return (
    <div ref={ref} className="relative inline-block" data-testid={`test-menu-${providerId}`}>
      <Button size="sm" variant="outline" disabled={testing} onClick={() => setOpen((v) => !v)}>
        {testing ? '测试中…' : '测试 ▾'}
      </Button>
      {open && (
        <div className="absolute right-0 z-30 mt-1 min-w-56 rounded-md border bg-card p-1 shadow-md">
          <div className="px-2 py-1 text-[11px] text-muted-foreground">选模型测试 {name}</div>
          {list.map((m) => {
            const free = m.toLowerCase().includes('flash')
            return (
              <button
                key={m}
                type="button"
                className="flex w-full items-center justify-between gap-3 rounded-sm px-2 py-1.5 text-left text-xs hover:bg-accent"
                onClick={() => { setOpen(false); onTest(m) }}
              >
                <span className="font-mono">{m}</span>
                {isZcode && (free
                  ? <Badge tone="success">免费</Badge>
                  : <Badge tone="default">订阅</Badge>)}
              </button>
            )
          })}
        </div>
      )}
    </div>
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
  if (t.includes('five') || t.includes('5h')) return '5 小时提示词池'
  if (t.includes('week')) return '每周配额'
  if (t.includes('time')) return '用量额度'
  return l.type ?? '额度'
}

function AccountInfoModal({ target, onClose, onDone }: { target: { id: string; name: string }; onClose: () => void; onDone: () => void }) {
  const qc = useQueryClient()
  const [resetBusy, setResetBusy] = useState<string | null>(null)
  const info = useQuery({
    queryKey: ['zcode-account-info', target.id],
    queryFn: async () => (await api<ZcodeAccountInfo>('POST', `/api/providers/${target.id}/zcode/account-info`)).data,
    // 每次打开都拉新(额度/重置次数是实时值)
    staleTime: 0, gcTime: 0, retry: false,
  })

  const useReset = async (resetType: 'FIVE_HOUR' | 'WEEK') => {
    const label = resetType === 'FIVE_HOUR' ? '5 小时' : '每周'
    if (!confirm(`确定使用一次「${label}额度重置」吗?\n这是消耗性动作:扣一次重置机会,把${label}额度窗清零重来。`)) return
    setResetBusy(resetType)
    try {
      const r = await api<{ used: boolean; reset_status: ZcodeAccountInfo['reset_status'] }>(
        'POST', `/api/providers/${target.id}/zcode/reset-quota`, { reset_type: resetType })
      if (r.ok && r.data.used) {
        sonnerToast.success(`${target.name}:${label}额度已重置`)
        void qc.invalidateQueries({ queryKey: ['zcode-account-info', target.id] })
        onDone()
      } else if (r.ok) {
        sonnerToast.warning(`${target.name}:上游未确认重置(used=false)`)
      } else {
        sonnerToast.error(`${target.name} 重置失败:${errMsg(r)}`)
      }
    } finally { setResetBusy(null) }
  }

  const d = info.data
  // 订阅:响应可能是对象(active 项)或数组;取第一个 active
  const subs: Array<Record<string, unknown>> = Array.isArray(d?.subscription)
    ? d!.subscription as Array<Record<string, unknown>>
    : d?.subscription ? [d!.subscription as Record<string, unknown>] : []
  const activeSub = subs.find((s) => String(s.status ?? 'active').toLowerCase() === 'active') ?? subs[0]

  return (
    <Dialog open onOpenChange={onClose}>
      <DialogContent title={`账号信息 · ${target.name}`}>
        {info.isLoading ? (
          <div className="py-10 text-center text-sm text-muted-foreground">查询上游中…</div>
        ) : info.isError || !d ? (
          <div className="space-y-3 py-6 text-center text-sm text-destructive">
            获取失败:{errMsg(info as never)}
            <div><Button size="sm" variant="outline" onClick={() => void info.refetch()}>重试</Button></div>
          </div>
        ) : (
          <div className="max-h-[70vh] space-y-4 overflow-y-auto pr-1 text-sm">
            {/* 账号 + 免费额度桶 */}
            <section className="space-y-1.5">
              <h4 className="font-medium">账号</h4>
              <div className="grid grid-cols-[5rem_1fr] items-baseline gap-x-2 text-xs">
                <span className="text-muted-foreground">标识</span><span className="font-mono break-all">{d.display || d.account_id || '-'}</span>
                {d.account_id && <><span className="text-muted-foreground">ID</span><span className="font-mono break-all">{d.account_id}</span></>}
                <span className="text-muted-foreground">当前套餐</span><span>{d.plan_name || '-'}</span>
              </div>
              {(d.balance?.balances ?? []).map((b, i) => (
                <div key={i} className="rounded-md border p-2">
                  <UsageBar label={b.show_name || '额度'} used={b.used_units} total={b.total_units} />
                  <div className="mt-1 text-[11px] text-muted-foreground">到期 {ts(b.expires_at)}</div>
                </div>
              ))}
              {d.name_updated && <p className="text-[11px] text-muted-foreground">名称已回填为账号标识</p>}
            </section>

            {/* 订阅(Lite 等付费计划;需在线登录补全的业务 token) */}
            <section className="space-y-1.5">
              <h4 className="font-medium">订阅</h4>
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
                <h4 className="font-medium">额度(5 小时 / 每周)</h4>
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
            <section className="space-y-2">
              <h4 className="font-medium">额度重置</h4>
              {d.reset_status ? (
                <>
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
                </>
              ) : (
                <p className="text-xs text-muted-foreground">重置次数查询失败(见下方说明)</p>
              )}
            </section>

            {d.warnings?.length > 0 && (
              <p className="text-[11px] text-muted-foreground">⚠ {d.warnings.join(';')}</p>
            )}
          </div>
        )}
      </DialogContent>
    </Dialog>
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

function EditModal({ target, onDone, onClose }: { target: { id: string; name: string; priority: number; weight: number }; onDone: () => void; onClose: () => void }) {
  const [priority, setPriority] = useState(String(target.priority))
  const [weight, setWeight] = useState(String(target.weight))
  const [busy, setBusy] = useState(false)
  const submit = async () => {
    setBusy(true)
    try {
      const r = await api('PATCH', `/api/providers/${target.id}`, {
        priority: parseInt(priority, 10) || 0,
        weight: parseFloat(weight) || 1.0,
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
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">优先级(大者先选)</label>
              <Input type="number" value={priority} onChange={(e) => setPriority(e.target.value)} />
            </div>
            <div className="space-y-1.5">
              <label className="text-xs font-medium text-muted-foreground">权重(同级分流)</label>
              <Input type="number" step="0.1" value={weight} onChange={(e) => setWeight(e.target.value)} />
            </div>
          </div>
          <p className="text-xs text-muted-foreground">同级优先级内按权重加权分流;数值越大越优先。</p>
          <div className="flex justify-end gap-2">
            <Button type="button" variant="outline" onClick={onClose}>取消</Button>
            <Button type="submit" disabled={busy}>{busy ? '保存中…' : '保存'}</Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  )
}
