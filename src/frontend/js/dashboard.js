// ============ Dashboard ============
// 防止快速切换页面时乱序响应写脏 DOM：只有最后一次 render 可以落地
let dashboardSeq = 0;

async function renderDashboard(container) {
  const seq = ++dashboardSeq;
  container.innerHTML = `
    <div class="page-head"><h2>数据总览</h2><p>系统运行状态与渠道健康一览</p></div>
    <div class="stats-grid" id="stats-grid"><div class="stat-card"><div class="spinner"></div><div class="label" style="margin-left:10px">加载中...</div></div></div>`;

  try {
    const [providers, users, apiKeys, usage] = await Promise.all([
      api('GET', '/api/admin/providers'),
      api('GET', '/api/admin/users'),
      api('GET', '/api/api-keys'),
      api('GET', '/api/admin/usage-stats'),
    ]);
    if (seq !== dashboardSeq) return; // 已有更新的渲染在进行，丢弃旧响应

    const pData = providers.ok ? providers.data : [];
    const uData = users.ok ? users.data : [];
    const kData = apiKeys.ok ? apiKeys.data : [];
    const usageData = usage.ok ? usage.data : null;

    const healthy = pData.filter(p => p.health_status === 'healthy').length;
    const totalModels = new Set(pData.flatMap(p => p.models || [])).size;

    const stat = (icon, color, bg, value, label) =>
      `<div class="stat-card"><div class="stat-icon" style="--si-fg:${color};--si-bg:${bg}">${icon}</div><div><div class="value">${value}</div><div class="label">${label}</div></div></div>`;

    const grid = document.getElementById('stats-grid');
    if (!grid) return;
    grid.innerHTML =
      stat(icons.providers, '#6366f1', 'rgba(99,102,241,.1)', fmtNum(pData.length), '上游渠道') +
      stat(icons.pulse, '#10b981', 'rgba(16,185,129,.1)', fmtNum(healthy), '健康渠道') +
      stat(icons.models, '#8b5cf6', 'rgba(139,92,246,.1)', fmtNum(totalModels), '可用模型') +
      stat(icons.users, '#0ea5e9', 'rgba(14,165,233,.1)', fmtNum(uData.length), '用户') +
      stat(icons.key, '#f59e0b', 'rgba(245,158,11,.1)', fmtNum(kData.length), 'API 密钥');

    // ---- AI 使用情况:日/周/月调用量 + 趋势图 + 模型分布 ----
    if (usageData) {
      const u = usageData;
      grid.innerHTML +=
        stat(icons.chat, '#f43f5e', 'rgba(244,63,94,.1)', fmtNum(u.today.requests), `今日调用 · ${fmtNum(u.today.tokens)} tokens`) +
        stat(icons.chat, '#f97316', 'rgba(249,115,22,.1)', fmtNum(u.week.requests), `近 7 天调用 · ${fmtNum(u.week.tokens)} tokens`) +
        stat(icons.chat, '#14b8a6', 'rgba(20,184,166,.1)', fmtNum(u.month.requests), `近 30 天调用 · ${fmtNum(u.month.tokens)} tokens`);

      container.insertAdjacentHTML('beforeend', `
        <div class="usage-grid">
          <div class="card" style="margin-bottom:0">
            <div class="card-header"><h2>近 30 天调用趋势</h2><span style="font-size:12.5px;color:var(--muted)">每日请求数</span></div>
            ${renderUsageBarChart(u.daily || [])}
          </div>
          <div class="card" style="margin-bottom:0">
            <div class="card-header"><h2>模型调用分布</h2><span style="font-size:12.5px;color:var(--muted)">近 30 天</span></div>
            ${renderModelBars(u.top_models || [])}
          </div>
        </div>`);
    }

    container.insertAdjacentHTML('beforeend', `
      <div class="card">
        <div class="card-header"><h2>渠道状态</h2></div>
        ${pData.length === 0 ? '<div class="empty"><p>暂无渠道，请先在「渠道」页添加</p></div>' : `
        <div class="table-wrap">
          <table>
            <thead><tr><th>名称</th><th>类型</th><th>模型</th><th>状态</th><th>延迟</th><th>优先级</th></tr></thead>
            <tbody>
              ${pData.map(p => `
                <tr>
                  <td><strong>${esc(p.name)}</strong></td>
                  <td><span class="badge badge-blue">${esc(p.provider_type)}</span></td>
                  <td>${esc((p.models || []).slice(0, 3).join(', '))}${p.models?.length > 3 ? '...' : ''}</td>
                  <td><span class="badge ${p.health_status === 'healthy' ? 'badge-green' : p.health_status === 'degraded' ? 'badge-yellow' : p.health_status === 'unhealthy' ? 'badge-red' : 'badge-gray'}">${esc(p.health_status)}</span></td>
                  <td>${p.latency_ms > 0 ? Math.round(p.latency_ms) + 'ms' : '-'}</td>
                  <td>${p.priority}</td>
                </tr>
              `).join('')}
            </tbody>
          </table>
        </div>`}
      </div>`);
  } catch (e) {
    if (seq !== dashboardSeq) return;
    container.insertAdjacentHTML('beforeend', '<div class="card"><div class="empty"><p>加载失败: ' + esc(e.message) + '</p></div></div>');
  }
}

// 纯 SVG 柱状图(无第三方库,CSP 只允许自身来源)。悬停 <title> 显示
// 当日的请求数、Token、成功率与平均延迟。
function renderUsageBarChart(daily) {
  if (daily.length === 0) return '<div class="empty"><p>暂无调用数据</p></div>';
  const W = 920, H = 220, PT = 16, PB = 26, PL = 48, PR = 8;
  const iw = W - PL - PR, ih = H - PT - PB;
  const max = Math.max(1, ...daily.map(d => d.requests));
  const bw = iw / daily.length;

  const grid = [0, 0.5, 1].map(f => {
    const y = (PT + ih * (1 - f)).toFixed(1);
    return `<line x1="${PL}" y1="${y}" x2="${W - PR}" y2="${y}" stroke="var(--border)" stroke-width="1"${f === 0 ? '' : ' stroke-dasharray="3 4"'}/>`
      + `<text x="${PL - 6}" y="${(+y + 3.5).toFixed(1)}" text-anchor="end" font-size="10" fill="var(--muted)">${fmtNum(Math.round(max * f))}</text>`;
  }).join('');

  const bars = daily.map((d, i) => {
    const h = (d.requests / max) * ih;
    const x = PL + i * bw;
    const y = PT + (ih - h);
    const tip = `${d.date}\n请求 ${fmtNum(d.requests)} · Token ${fmtNum(d.tokens)}\n成功率 ${(d.success_rate ?? 0).toFixed(1)}% · 平均延迟 ${fmtNum(d.avg_latency_ms)}ms`;
    return `<rect x="${(x + 1).toFixed(1)}" y="${y.toFixed(1)}" width="${Math.max(1, bw - 3).toFixed(1)}" height="${Math.max(0, h).toFixed(1)}" rx="2.5" fill="url(#usage-bar-grad)"><title>${esc(tip)}</title></rect>`;
  }).join('');

  const xlabels = [0, Math.floor(daily.length / 2), daily.length - 1].map(i => {
    const d = daily[i];
    if (!d) return '';
    const x = (PL + i * bw + bw / 2).toFixed(1);
    return `<text x="${x}" y="${H - 8}" text-anchor="middle" font-size="10" fill="var(--muted)">${esc(d.date.slice(5))}</text>`;
  }).join('');

  return `<svg viewBox="0 0 ${W} ${H}" style="width:100%;display:block" role="img" aria-label="近 30 天调用趋势">
    <defs><linearGradient id="usage-bar-grad" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#6366f1"/><stop offset="1" stop-color="#8b5cf6"/>
    </linearGradient></defs>
    ${grid}${bars}${xlabels}
  </svg>`;
}

// 模型调用分布:横向条形,按请求数占比
function renderModelBars(models) {
  if (models.length === 0) return '<div class="empty"><p>暂无调用数据</p></div>';
  const max = Math.max(1, ...models.map(m => m.requests));
  return models.map(m => `
    <div style="margin-bottom:12px">
      <div style="display:flex;justify-content:space-between;gap:10px;font-size:12.5px;margin-bottom:4px">
        <code style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap">${esc(m.model)}</code>
        <span style="color:var(--muted);white-space:nowrap">${fmtNum(m.requests)} 次 · ${fmtNum(m.tokens)} tokens</span>
      </div>
      <div style="height:8px;background:var(--border);border-radius:99px;overflow:hidden">
        <div style="height:100%;width:${((m.requests / max) * 100).toFixed(1)}%;background:var(--grad);border-radius:99px"></div>
      </div>
    </div>`).join('');
}
