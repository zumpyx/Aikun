// ============ Dashboard ============
// 防止快速切换页面时乱序响应写脏 DOM：只有最后一次 render 可以落地
let dashboardSeq = 0;

async function renderDashboard(container) {
  const seq = ++dashboardSeq;
  container.innerHTML = `
    <div class="page-head"><h2>数据总览</h2><p>系统运行状态与渠道健康一览</p></div>
    <div class="stats-grid" id="stats-grid"><div class="stat-card"><div class="spinner"></div><div class="label" style="margin-left:10px">加载中...</div></div></div>`;

  try {
    const [providers, users, apiKeys] = await Promise.all([
      api('GET', '/api/admin/providers'),
      api('GET', '/api/admin/users'),
      api('GET', '/api/api-keys'),
    ]);
    if (seq !== dashboardSeq) return; // 已有更新的渲染在进行，丢弃旧响应

    const pData = providers.ok ? providers.data : [];
    const uData = users.ok ? users.data : [];
    const kData = apiKeys.ok ? apiKeys.data : [];

    const healthy = pData.filter(p => p.health_status === 'healthy').length;
    const totalModels = new Set(pData.flatMap(p => p.models || [])).size;

    const stat = (icon, color, bg, value, label) =>
      `<div class="stat-card"><div class="stat-icon" style="--si-fg:${color};--si-bg:${bg}">${icon}</div><div><div class="value">${value}</div><div class="label">${label}</div></div></div>`;

    const grid = document.getElementById('stats-grid');
    if (!grid) return;
    grid.innerHTML =
      stat(icons.providers, '#6366f1', 'rgba(99,102,241,.1)', pData.length, '上游渠道') +
      stat(icons.pulse, '#10b981', 'rgba(16,185,129,.1)', healthy, '健康渠道') +
      stat(icons.models, '#8b5cf6', 'rgba(139,92,246,.1)', totalModels, '可用模型') +
      stat(icons.users, '#0ea5e9', 'rgba(14,165,233,.1)', uData.length, '用户') +
      stat(icons.key, '#f59e0b', 'rgba(245,158,11,.1)', kData.length, 'API 密钥');

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


