// ============ Models ============
// Admin cells are colored by the measured per-(channel, model) test result
// (model_health table, refreshed by manual tests and the 30-minute auto
// loop); channels with no test record yet fall back to their ping health.
function healthCellClass(p) {
  if (!p.is_active) return 'mcell-gray';
  return p.health_status === 'healthy' ? 'mcell-green'
    : p.health_status === 'degraded' ? 'mcell-yellow'
    : p.health_status === 'unhealthy' ? 'mcell-red'
    : 'mcell-gray';
}

function modelCell(p, m, healthMap) {
  const h = healthMap[p.id + '|' + m];
  if (!p.is_active) {
    return `<span class="mcell mcell-gray" title="${esc(p.name)} — 已禁用"></span>`;
  }
  if (h) {
    const cls = h.status === 'healthy' ? 'mcell-green' : h.status === 'unhealthy' ? 'mcell-red' : 'mcell-gray';
    const tip = `${p.name} — 实测${h.status === 'healthy' ? '通过' : h.status === 'unhealthy' ? '失败' : '未知'}` +
      `${h.checked_at ? ' · ' + h.checked_at : ''}${h.latency_ms > 0 ? ' · ' + Math.round(h.latency_ms) + 'ms' : ''}${h.error ? ' · ' + h.error : ''}`;
    return `<span class="mcell ${cls}" title="${esc(tip)}"></span>`;
  }
  const tip = `${p.name} — ${p.health_status}（未实测）${p.latency_ms > 0 ? ' · ' + Math.round(p.latency_ms) + 'ms' : ''}`;
  return `<span class="mcell ${healthCellClass(p)}" title="${esc(tip)}"></span>`;
}

// 防止快速切换页面时乱序响应写脏 DOM：只有最后一次 render 可以落地
let modelsSeq = 0;

async function renderModels(container) {
  if (!container) return;
  const seq = ++modelsSeq;
  container.innerHTML = `
    <div class="page-head"><h2>模型列表</h2><p>网关上所有可用模型</p></div>
    <div id="models-list"><div class="empty"><div class="spinner"></div><p>加载中...</p></div></div>`;

  const isAdmin = state.user && state.user.role === 'admin';
  const r = await api('GET', '/v1/models');
  if (seq !== modelsSeq) return; // 已有更新的渲染在进行，丢弃旧响应
  const list = document.getElementById('models-list');
  if (!list) return;
  if (!r.ok) { list.innerHTML = '<div class="empty"><p>加载失败</p></div>'; return; }

  const models = r.data.data || [];
  if (models.length === 0) {
    list.innerHTML = '<div class="card"><div class="empty"><p>暂无可用模型，请先配置上游渠道</p></div></div>';
    return;
  }

  // Admin view: every model gets one cell per supporting channel, colored by
  // that (channel, model)'s measured test health — coverage and live status
  // are visible at a glance.
  let provMap = null;
  let healthMap = {};
  if (isAdmin) {
    const [pr, hr] = await Promise.all([
      api('GET', '/api/admin/providers'),
      api('GET', '/api/admin/model-health'),
    ]);
    if (pr.ok && Array.isArray(pr.data)) {
      provMap = {};
      for (const p of pr.data) {
        for (const m of (p.models || [])) {
          (provMap[m] = provMap[m] || []).push(p);
        }
      }
    }
    if (hr.ok && Array.isArray(hr.data)) {
      for (const h of hr.data) healthMap[h.provider_id + '|' + h.model] = h;
    }
  }
  if (seq !== modelsSeq || !list.isConnected) return;

  if (!provMap) {
    list.innerHTML = `
      <div class="card">
        <div class="table-wrap">
          <table>
            <thead><tr><th>模型 ID</th><th>来源</th></tr></thead>
            <tbody>
              ${models.map(m => `
                <tr><td><code>${esc(m.id)}</code></td><td><span class="badge badge-blue">${esc(m.owned_by)}</span></td></tr>
              `).join('')}
            </tbody>
          </table>
        </div>
      </div>`;
    return;
  }

  list.innerHTML = `
    <div class="legend">
      <span><span class="mcell mcell-green"></span>实测通过</span>
      <span><span class="mcell mcell-yellow"></span>降级</span>
      <span><span class="mcell mcell-red"></span>实测失败</span>
      <span><span class="mcell mcell-gray"></span>未测试/已禁用</span>
      <span style="margin-left:auto">共 ${models.length} 个模型 · 每 30 分钟自动实测更新</span>
    </div>
    <div class="card">
      <div class="table-wrap">
        <table>
          <thead><tr><th style="width:42%">模型 ID</th><th>渠道健康（每个方块代表一个支持渠道）</th></tr></thead>
          <tbody>
            ${models.map(m => {
              const provs = provMap[m.id] || [];
              return `<tr>
                <td><code>${esc(m.id)}</code></td>
                <td>${provs.length === 0
                  ? '<span style="color:var(--faint)">无渠道</span>'
                  : provs.map(p => modelCell(p, m.id, healthMap)).join('')}
                </td>
              </tr>`;
            }).join('')}
          </tbody>
        </table>
      </div>
    </div>`;
}


