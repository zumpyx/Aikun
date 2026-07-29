// ============ Chat / Test ============
// Admin view: a per-channel model test list — one row per channel, each model
// a fixed-size box, gray by default, click to run a live test, green/red by
// result. Below it, the raw chat playground that hits the gateway directly.
async function renderChat(container) {
  if (!container) return;
  const isAdmin = state.user && state.user.role === 'admin';
  const savedMsg = localStorage.getItem('test-message') || 'Say OK';
  container.innerHTML = `
    <div class="page-head"><h2>模型测试</h2><p>验证各渠道模型的连通性，或直接向网关发送测试请求</p></div>
    ${isAdmin ? `
    <div class="card" style="margin-bottom:16px">
      <div style="display:flex;gap:10px;align-items:flex-end;flex-wrap:wrap">
        <div class="form-group" style="flex:1;min-width:260px;margin-bottom:0">
          <label>测试消息（对所有渠道模型发送同一条）</label>
          <input id="mt-message" value="${esc(savedMsg)}" placeholder="例如: Say OK">
        </div>
        <button class="btn-primary" id="mt-test-all">一键测试全部</button>
      </div>
      <div id="mt-matrix" style="margin-top:14px"><div class="empty"><div class="spinner"></div><p>加载中...</p></div></div>
    </div>` : ''}
    <div class="card">
      <div class="card-header" style="margin-bottom:12px"><h3>请求测试${isAdmin ? '' : '（OpenAI 协议）'}</h3></div>
      ${isAdmin ? `
      <div style="display:flex;gap:10px;flex-wrap:wrap">
        <div class="form-group" style="flex:2;min-width:180px">
          <label>渠道</label>
          <select id="chat-provider" class="inline-select"><option value="">-- 加载中 --</option></select>
        </div>
        <div class="form-group" style="flex:2;min-width:180px">
          <label>模型</label>
          <select id="chat-model" class="inline-select"><option value="">-- 加载中 --</option></select>
        </div>
        <div class="form-group" style="flex:1;min-width:130px">
          <label>协议</label>
          <select id="chat-protocol" class="inline-select"><option value="">-- 加载中 --</option></select>
        </div>
        <div class="form-group" style="flex:1;min-width:110px">
          <label>方式</label>
          <select id="chat-stream" class="inline-select">
            <option value="">非流式</option>
            <option value="1">流式</option>
          </select>
        </div>
      </div>` : `
      <div class="form-group">
        <label>模型</label>
        <select id="chat-model" class="inline-select">
          <option value="">-- 加载中 --</option>
        </select>
      </div>`}
      <div class="form-group">
        <label>消息 (JSON 数组)</label>
        <textarea id="chat-messages">[{"role":"user","content":"你好，请用中文回复"}]</textarea>
      </div>
      <button class="btn-primary" id="chat-send">发送请求</button>
    </div>
    <div class="card" id="chat-result" style="display:none">
      <div class="card-header"><h3>响应</h3></div>
      <div class="code-block" id="chat-response"></div>
    </div>`;

  if (isAdmin) {
    loadTestMatrix();
    initTestCascade();
    return;
  }

  api('GET', '/v1/models').then(r => {
    const sel = document.getElementById('chat-model');
    if (!sel) return;
    if (r.ok && r.data.data && r.data.data.length > 0) {
      sel.innerHTML = '<option value="">-- 选择模型 --</option>' +
        r.data.data.map(m => '<option value="' + esc(m.id) + '">' + esc(m.id) + '</option>').join('');
    } else {
      sel.innerHTML = '<option value="">-- 暂无可用模型, 请先配置渠道 --</option>';
    }
  });

  document.getElementById('chat-send').onclick = async () => {
    const model = document.getElementById('chat-model').value;
    if (!model) return toast('请选择模型', 'error');
    let messages;
    try { messages = JSON.parse(document.getElementById('chat-messages').value); }
    catch { return toast('消息格式错误，请输入有效的 JSON', 'error'); }
    if (!Array.isArray(messages) || messages.length === 0) {
      return toast('消息必须是非空 JSON 数组', 'error');
    }

    const btn = document.getElementById('chat-send');
    btn.disabled = true; btn.textContent = '发送中…';
    const result = document.getElementById('chat-result');
    result.style.display = 'none';

    try {
      const r = await api('POST', '/v1/chat/completions', { model, messages });
      const resp = document.getElementById('chat-response');
      if (!resp) return;
      result.style.display = 'block';
      resp.textContent = JSON.stringify(r.data, null, 2);
      if (r.ok) toast('请求成功');
      else {
        const errMsg = r.data?.error?.message || r.data?.error || '请求失败';
        toast(errMsg, 'error');
      }
    } finally {
      btn.disabled = false; btn.textContent = '发送请求';
    }
  };
}

// Admin request tester: channel → model → protocol cascading selects. The
// first channel and its first model are pre-selected, and the protocol
// defaults to the channel's default_protocol, so a quick test is one click.
async function initTestCascade() {
  const provSel = document.getElementById('chat-provider');
  const modelSel = document.getElementById('chat-model');
  const protoSel = document.getElementById('chat-protocol');
  if (!provSel || !modelSel || !protoSel) return;

  const r = await api('GET', '/api/admin/providers');
  // await 期间页面可能已切换，选择器已不在文档中就直接放弃
  if (!provSel.isConnected) return;
  const provs = (r.ok ? (r.data || []) : []).filter(p => (p.models || []).length > 0);
  const protosOf = p => (p.protocols && p.protocols.length) ? p.protocols : [p.provider_type];

  provSel.innerHTML = provs.length
    ? provs.map(p => `<option value="${esc(p.id)}">${esc(p.name)}</option>`).join('')
    : '<option value="">暂无可用渠道</option>';

  function refreshModels() {
    const p = provs.find(x => x.id === provSel.value);
    const models = p ? (p.models || []) : [];
    modelSel.innerHTML = models.length
      ? models.map(m => `<option value="${esc(m)}">${esc(m)}</option>`).join('')
      : '<option value="">该渠道未配置模型</option>';
    refreshProtocols();
  }
  function refreshProtocols() {
    const p = provs.find(x => x.id === provSel.value);
    const protos = p ? protosOf(p) : [];
    protoSel.innerHTML = protos.length
      ? protos.map(t => `<option value="${esc(t)}">${esc(t)}${p.default_protocol === t ? '（默认）' : ''}</option>`).join('')
      : '<option value="">无可用协议</option>';
    if (p && protos.includes(p.default_protocol)) protoSel.value = p.default_protocol;
  }
  provSel.onchange = refreshModels;
  refreshModels();

  document.getElementById('chat-send').onclick = async () => {
    const pid = provSel.value;
    const model = modelSel.value;
    if (!pid || !model) return toast('请选择协议、渠道和模型', 'error');
    let messages;
    try { messages = JSON.parse(document.getElementById('chat-messages').value); }
    catch { return toast('消息格式错误，请输入有效的 JSON', 'error'); }
    if (!Array.isArray(messages) || messages.length === 0) {
      return toast('消息必须是非空 JSON 数组', 'error');
    }

    const btn = document.getElementById('chat-send');
    btn.disabled = true; btn.textContent = '发送中…';
    const result = document.getElementById('chat-result');
    result.style.display = 'none';

    try {
      const r = await api('POST', `/api/admin/providers/${pid}/test-model`, {
        model, messages, protocol: protoSel.value,
        stream: document.getElementById('chat-stream')?.value === '1',
      });
      const resp = document.getElementById('chat-response');
      if (!resp) return;
      result.style.display = 'block';
      resp.textContent = JSON.stringify(r.data.response || r.data, null, 2);
      if (r.ok && r.data.ok) toast(`请求成功（${Math.round(r.data.latency_ms)}ms）`);
      else toast(r.data.error || r.data.message || '请求失败', 'error');
    } finally {
      btn.disabled = false; btn.textContent = '发送请求';
    }
  };
}

function provHealthBadge(p) {
  if (!p.is_active) {
    return `<span class="badge badge-red" title="${esc(p.disabled_reason || '已手动禁用')}">已禁用</span>`;
  }
  const cls = p.health_status === 'healthy' ? 'badge-green'
    : p.health_status === 'degraded' ? 'badge-yellow'
    : p.health_status === 'unhealthy' ? 'badge-red' : 'badge-gray';
  return `<span class="badge ${cls}">${esc(p.health_status)}</span>`;
}

// Build the per-channel model test list: one row per channel, each model a
// fixed-size box showing up to 12 chars (centered). Colors come from the last
// measured model_health result (gray when never tested); clicking re-tests
// live and re-persists that result server-side. Hover shows the full model
// name, status, latency and error.
async function loadTestMatrix() {
  const host = document.getElementById('mt-matrix');
  if (!host) return;
  const [r, hr] = await Promise.all([
    api('GET', '/api/admin/providers'),
    api('GET', '/api/admin/model-health'),
  ]);
  if (!r.ok) { host.innerHTML = '<div class="empty"><p>加载失败</p></div>'; return; }

  const healthMap = {};
  if (hr.ok && Array.isArray(hr.data)) {
    for (const h of hr.data) healthMap[h.provider_id + '|' + h.model] = h;
  }

  const provs = (r.data || []).filter(p => (p.models || []).length > 0);
  if (provs.length === 0) {
    host.innerHTML = '<div class="empty"><p>暂无配置了模型的渠道，请先在渠道页添加</p></div>';
    return;
  }

  // 方框统一大小,最多显示 12 个字符,超长截断补 ..
  const truncModel = m => m.length > 12 ? m.slice(0, 10) + '..' : m;

  // Initial box from the last recorded probe for this (channel, model).
  const boxOf = (p, m) => {
    const h = healthMap[p.id + '|' + m];
    let cls = 'mbox-gray';
    let tip = `${m} — 未测试，点击测试`;
    if (h) {
      cls = h.status === 'healthy' ? 'mbox-green' : h.status === 'unhealthy' ? 'mbox-red' : 'mbox-gray';
      tip = `${m} — ${h.status === 'healthy' ? '✓' : h.status === 'unhealthy' ? '✗' : '?'} ` +
        `${h.checked_at || ''}${h.latency_ms > 0 ? ' · ' + Math.round(h.latency_ms) + 'ms' : ''}${h.error ? ' · ' + h.error : ''}`.trim();
    }
    return `<span class="mbox ${cls} mt-cell" data-pid="${esc(p.id)}" data-model="${esc(m)}" title="${esc(tip)}">${esc(truncModel(m))}</span>`;
  };

  host.innerHTML = `
    <div class="mt-rows">
      ${provs.map(p => `
        <div class="mt-row">
          <div class="mt-row-name" title="${esc(p.name)}"><strong class="mt-chan">${esc(p.name)}</strong>${provHealthBadge(p)}</div>
          <div class="mt-row-models">${p.models.map(m => boxOf(p, m)).join('')}</div>
        </div>`).join('')}
    </div>
    <div class="legend" style="margin:10px 0 0">
      <span><span class="mcell mcell-gray"></span>未测试</span>
      <span><span class="mcell mcell-green"></span>通过</span>
      <span><span class="mcell mcell-red"></span>失败</span>
      <span style="margin-left:auto">每 30 分钟自动测试更新，点击方框单独测试</span>
    </div>`;

  host.querySelector('.mt-rows').onclick = (e) => {
    const cell = e.target.closest('.mt-cell');
    if (cell && !cell.classList.contains('mbox-testing')) runCellTest(cell);
  };

  // 加载完成前用户可能已切走页面,按钮已不在 DOM 中
  const testAllBtn = document.getElementById('mt-test-all');
  if (testAllBtn) testAllBtn.onclick = async (e) => {
    const allBtn = e.target;
    localStorage.setItem('test-message', document.getElementById('mt-message')?.value.trim() || 'Say OK');
    const cells = [...host.querySelectorAll('.mt-cell')];
    if (cells.length === 0) return;
    allBtn.disabled = true; allBtn.textContent = '测试中…';
    try {
      // 4 concurrent workers keep upstream pressure bounded.
      const queue = [...cells];
      const worker = async () => { while (queue.length > 0) await runCellTest(queue.shift()); };
      await Promise.all([worker(), worker(), worker(), worker()]);
      // 图例方块不含 mt-cell，只统计列表里的测试格
      const failed = host.querySelectorAll('.mt-cell.mbox-red').length;
      toast(failed === 0 ? '全部测试通过' : `测试完成，${failed} 个失败`, failed === 0 ? 'success' : 'error');
    } finally {
      allBtn.disabled = false; allBtn.textContent = '一键测试全部';
    }
  };
}

// Run one box's live test and repaint it by result; details go to the tooltip.
async function runCellTest(cell) {
  cell.className = 'mbox mbox-testing mt-cell';
  const model = cell.dataset.model;
  const label = cell.textContent;
  try {
    const message = document.getElementById('mt-message')?.value.trim() || 'Say OK';
    const r = await api('POST', `/api/admin/providers/${cell.dataset.pid}/test-model`, { model, message });
    if (r.ok && r.data.ok) {
      cell.className = 'mbox mbox-green mt-cell';
      cell.title = `${model} ✓ ${Math.round(r.data.latency_ms)}ms${r.data.snippet ? ' · ' + r.data.snippet : ''}`;
    } else {
      const err = String(r.data.error || r.data.message || '请求失败').slice(0, 120);
      cell.className = 'mbox mbox-red mt-cell';
      cell.title = `${model} ✗${r.data.status ? ' [' + r.data.status + ']' : ''} ${err}`;
    }
  } catch {
    cell.className = 'mbox mbox-red mt-cell';
    cell.title = `${model} ✗ 网络错误`;
  }
  cell.textContent = label;
}


