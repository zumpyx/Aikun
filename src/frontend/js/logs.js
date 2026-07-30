// ============ Logs ============
async function renderLogs(container) {
  const isAdmin = state.user && state.user.role === 'admin';

  container.innerHTML = `
    <div class="card-header">
      <div class="page-head" style="margin-bottom:0"><h2>日志审计</h2><p>请求调用记录与统计</p></div>
      <button class="btn-outline btn-sm" id="log-refresh">刷新</button>
    </div>
    <div class="card" style="padding:14px 18px">
      <div style="display:flex;gap:12px;flex-wrap:wrap;align-items:end">
        <div class="form-group" style="margin-bottom:0;min-width:150px;flex:1">
          <label>模型</label>
          <select id="log-filter-model" class="inline-select">
            <option value="">全部模型</option>
          </select>
        </div>
        ${isAdmin ? `
        <div class="form-group" style="margin-bottom:0;min-width:150px;flex:1">
          <label>用户</label>
          <select id="log-filter-user" class="inline-select">
            <option value="">全部用户</option>
          </select>
        </div>` : ''}
        <div class="form-group" style="margin-bottom:0;min-width:130px">
          <label>状态</label>
          <select id="log-filter-status" class="inline-select">
            <option value="">全部状态</option>
            <option value="1">成功</option>
            <option value="0">失败</option>
          </select>
        </div>
        <div class="form-group" style="margin-bottom:0;min-width:140px">
          <label>时间范围</label>
          <select id="log-filter-time" class="inline-select">
            <option value="">全部时间</option>
            <option value="1h">最近 1 小时</option>
            <option value="24h">最近 24 小时</option>
            <option value="7d">最近 7 天</option>
            <option value="30d">最近 30 天</option>
          </select>
        </div>
        <button class="btn-primary btn-sm" id="log-filter-btn" style="height:34px;padding:0 18px">筛选</button>
      </div>
    </div>
    <div id="logs-stats" style="margin-bottom:14px"></div>
    <div id="logs-list"><div class="empty"><div class="spinner"></div><p>加载中...</p></div></div>`;

  document.getElementById('log-refresh').onclick = () => { loadLogStats(isAdmin); loadLogs(isAdmin); };
  document.getElementById('log-filter-btn').onclick = () => { loadLogStats(isAdmin); loadLogs(isAdmin); };

  // 下拉选项与日志列表/统计并行加载,/v1/models 慢不阻塞首屏
  api('GET', '/v1/models').then(modelsR => {
    if (!modelsR.ok || !modelsR.data.data) return;
    const sel = document.getElementById('log-filter-model');
    if (!sel) return;
    modelsR.data.data.forEach(m => {
      const o = document.createElement('option');
      o.value = m.id; o.textContent = m.id;
      sel.appendChild(o);
    });
  });

  if (isAdmin) {
    api('GET', '/api/admin/users').then(usersR => {
      if (!usersR.ok || !Array.isArray(usersR.data)) return;
      const sel = document.getElementById('log-filter-user');
      if (!sel) return;
      usersR.data.forEach(u => {
        const o = document.createElement('option');
        o.value = u.id; o.textContent = u.username;
        sel.appendChild(o);
      });
    });
  }

  loadLogStats(isAdmin);
  loadLogs(isAdmin);
}

// 当前筛选条件拼成查询串(不带前导 ?/&),日志列表与统计共用,
// 保证统计数字始终跟随筛选变化。
function logFilterQuery(isAdmin) {
  const model = document.getElementById('log-filter-model')?.value;
  const status = document.getElementById('log-filter-status')?.value;
  const user = document.getElementById('log-filter-user')?.value;
  const time = document.getElementById('log-filter-time')?.value;

  let q = '';
  if (model) q += '&model=' + encodeURIComponent(model);
  if (status) q += '&success=' + status;
  if (isAdmin && user) q += '&user_id=' + encodeURIComponent(user);
  if (time) {
    let ms = 0;
    if (time === '1h') ms = 3600000;
    else if (time === '24h') ms = 86400000;
    else if (time === '7d') ms = 604800000;
    else if (time === '30d') ms = 2592000000;
    if (ms > 0) q += '&since=' + encodeURIComponent(new Date(Date.now() - ms).toISOString());
  }
  return q;
}

// 统计 chips 渲染,初始加载、筛选与手动刷新共用
let loadLogStatsSeq = 0;
async function loadLogStats(isAdmin) {
  const seq = ++loadLogStatsSeq;
  const statsR = await api('GET', '/api/logs/stats?' + logFilterQuery(isAdmin));
  if (seq !== loadLogStatsSeq) return; // 有更新的请求在途,丢弃旧响应
  if (!statsR.ok) return;
  const s = statsR.data;
  const chip = (label, value) =>
    `<span style="display:inline-flex;align-items:center;gap:6px;background:var(--card);border:1px solid var(--border);border-radius:9px;padding:7px 13px;font-size:12.5px;color:var(--muted);box-shadow:var(--shadow-sm)">${label} <strong style="color:var(--text);font-size:14px">${value}</strong></span>`;
  const statsEl = document.getElementById('logs-stats');
  if (!statsEl) return;
  statsEl.innerHTML = `
    <div style="display:flex;gap:10px;flex-wrap:wrap">
      ${chip('请求数量', fmtNum(s.total_requests ?? 0))}
      ${chip('消耗 Token', fmtNum(s.total_tokens ?? 0))}
      ${chip('平均延迟', Math.round(s.avg_latency_ms ?? 0) + 'ms')}
      ${chip('成功率', (s.success_rate ?? 0).toFixed(1) + '%')}
    </div>`;
}

// Guards against out-of-order responses when filters change quickly:
// only the latest loadLogs call may write to the DOM.
let loadLogsSeq = 0;

async function loadLogs(isAdmin) {
  const list = document.getElementById('logs-list');
  if (!list) return;
  const seq = ++loadLogsSeq;

  const url = '/api/logs?limit=200' + logFilterQuery(isAdmin);

  const r = await api('GET', url);
  if (seq !== loadLogsSeq) return; // a newer request is in flight — drop stale response
  if (!document.getElementById('logs-list')) return;
  if (!r.ok) { list.innerHTML = '<div class="empty"><p>加载失败</p></div>'; return; }

  const data = r.data;
  if (data.length === 0) {
    list.innerHTML = '<div class="card"><div class="empty"><p>暂无匹配的调用记录</p></div></div>';
    return;
  }

  list.innerHTML = `
    <div class="card">
      <div class="table-wrap">
        <table>
          <thead><tr>
            <th>时间</th>
            ${isAdmin ? '<th>用户</th>' : ''}
            <th>模型</th>
            <th>Token</th>
            <th>延迟</th>
            <th>状态</th>
            <th>错误</th>
          </tr></thead>
          <tbody>
            ${data.map(function(l) {
              return `
              <tr>
                <td style="font-size:12.5px;white-space:nowrap;color:var(--muted)">${esc(l.created_at)}</td>
                ${isAdmin ? '<td style="font-size:12.5px">' + (l.user_id ? esc(l.user_id.substring(0, 8)) + '...' : '-') + '</td>' : ''}
                <td><code style="font-size:12.5px">${esc(l.model)}</code></td>
                <td>${fmtNum(l.total_tokens)}</td>
                <td>${l.latency_ms}ms</td>
                <td><span class="badge ${l.success ? 'badge-green' : 'badge-red'}">${l.success ? '成功' : '失败'}</span></td>
                <td style="max-width:160px;overflow:hidden;text-overflow:ellipsis;font-size:12.5px;color:var(--danger)">${esc(l.error_message || '-')}</td>
              </tr>`;
            }).join('')}
          </tbody>
        </table>
      </div>
      <p style="font-size:12.5px;color:var(--muted);margin-top:10px">显示最近 200 条</p>
    </div>`;
}


