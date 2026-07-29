// ============ Providers ============
async function renderProviders(container) {
  container.innerHTML = `
    <div class="card-header">
      <div class="page-head" style="margin-bottom:0"><h2>渠道管理</h2><p>配置上游渠道、代理与路由策略</p></div>
      <button class="btn-primary" id="add-provider-btn">+ 添加渠道</button>
    </div>
    <div id="providers-list"><div class="empty"><div class="spinner"></div><p>加载中...</p></div></div>`;

  document.getElementById('add-provider-btn').onclick = () => showProviderModal();

  await loadProviders();
}

async function loadProviders() {
  const r = await api('GET', '/api/admin/providers');
  const list = document.getElementById('providers-list');
  if (!list) return;
  if (!r.ok) { list.innerHTML = '<div class="empty"><p>加载失败</p></div>'; return; }

  const data = r.data;
  if (data.length === 0) {
    list.innerHTML = '<div class="card"><div class="empty"><p>暂无渠道，点击上方按钮添加</p></div></div>';
    return;
  }

  list.innerHTML = `
    <div class="card">
      <div class="table-wrap">
        <table>
          <thead><tr><th>名称</th><th>协议</th><th>地址</th><th>模型</th><th>状态</th><th>延迟</th><th>优先级</th><th>权重</th><th>操作</th></tr></thead>
          <tbody>
            ${data.map(p => `
              <tr>
                <td><strong>${esc(p.name)}</strong></td>
                <td>${((p.protocols && p.protocols.length) ? p.protocols : [p.provider_type]).map(t =>
                  `<span class="badge ${t === p.default_protocol ? 'badge-blue' : 'badge-gray'}" title="${t === p.default_protocol ? '默认协议' : '支持协议'}">${esc(t)}</span>`
                ).join(' ')}</td>
                <td style="max-width:220px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">
                  ${esc(p.base_url)}
                  ${p.proxy_url ? `<span class="badge badge-violet" title="${esc(p.proxy_url)}">代理</span>` : ''}
                </td>
                <td>${esc((p.models || []).slice(0, 2).join(', '))}${p.models?.length > 2 ? ` <span class="badge badge-gray">+${p.models.length - 2}</span>` : ''}</td>
                <td>
                  ${!p.is_active
                    ? `<span class="badge badge-red" title="${esc(p.disabled_reason || '已手动禁用')}">已禁用</span>`
                    : `<span class="badge ${p.health_status === 'healthy' ? 'badge-green' : p.health_status === 'degraded' ? 'badge-yellow' : p.health_status === 'unhealthy' ? 'badge-red' : 'badge-gray'}" title="${esc(p.disabled_reason || '')}">${esc(p.health_status)}</span>`}
                </td>
                <td>${p.latency_ms > 0 ? Math.round(p.latency_ms) + 'ms' : '-'}</td>
                <td>${p.priority}</td>
                <td>${p.weight}</td>
                <td>
                  <button class="btn-outline btn-sm" data-action="edit-provider" data-id="${esc(p.id)}">编辑</button>
                  <button class="btn-outline btn-sm" data-action="duplicate-provider" data-id="${esc(p.id)}">复制</button>
                  <button class="btn-outline btn-sm" data-action="test-provider" data-id="${esc(p.id)}">测试</button>
                  <button class="btn-danger btn-sm" data-action="delete-provider" data-id="${esc(p.id)}">删除</button>
                </td>
              </tr>
            `).join('')}
          </tbody>
        </table>
      </div>
    </div>`;
}

async function showProviderModal(id) {
  let provider = null;
  if (id) {
    const r = await api('GET', `/api/admin/providers/${id}`);
    // 详情加载失败时直接报错退出，避免误当新建渠道覆盖
    if (!r.ok) return toast(r.data.message || r.data.error || '加载渠道详情失败', 'error');
    provider = r.data;
  }

  const overlay = document.createElement('div');
  overlay.className = 'modal-overlay';
  overlay.innerHTML = `
    <div class="modal">
      <h2>${provider ? '编辑渠道' : '添加渠道'}</h2>
      <div class="form-row">
        <div class="form-group"><label>名称</label><input id="pf-name" value="${esc(provider?.name || '')}" placeholder="例如: OpenAI"></div>
        <div class="form-group"><label>支持协议（右侧为默认）</label>
          <div style="display:flex;gap:14px;align-items:center;height:38px">
            <label style="display:flex;gap:6px;align-items:center;font-weight:400;margin-bottom:0"><input type="checkbox" id="pf-proto-openai" value="openai" style="width:auto"> OpenAI</label>
            <label style="display:flex;gap:6px;align-items:center;font-weight:400;margin-bottom:0"><input type="checkbox" id="pf-proto-anthropic" value="anthropic" style="width:auto"> Anthropic</label>
            <select id="pf-default-protocol" style="width:auto;margin-left:auto"></select>
          </div>
        </div>
      </div>
      <div class="form-group"><label>Base URL</label><input id="pf-url" value="${esc(provider?.base_url || '')}" placeholder="https://api.openai.com"></div>
      <div class="form-group"><label>API Key${provider ? '（留空不修改）' : ''}</label><input id="pf-key" value="${esc(provider?.api_key || '')}" placeholder="sk-..." type="password" ${provider ? '' : 'required'}></div>
      <div class="form-group"><label>模型（逗号分隔）</label>
        <div style="display:flex;gap:8px">
          <input id="pf-models" style="flex:1" value="${esc((provider?.models || []).join(', '))}" placeholder="gpt-4, gpt-3.5-turbo">
          <button class="btn-outline" id="pf-fetch-models" type="button" style="white-space:nowrap">获取模型列表</button>
        </div>
      </div>
      <div class="form-row">
        <div class="form-group"><label>优先级</label><input id="pf-priority" type="number" value="${provider?.priority ?? 0}"></div>
        <div class="form-group"><label>权重</label><input id="pf-weight" type="number" step="0.1" value="${provider?.weight ?? 1.0}"></div>
      </div>
      <div class="form-row">
        <div class="form-group"><label>最大重试</label><input id="pf-retries" type="number" value="${provider?.max_retries ?? 3}"></div>
        <div class="form-group"><label>超时(秒)</label><input id="pf-timeout" type="number" value="${provider?.timeout_secs ?? 120}"></div>
      </div>
      <div class="form-group"><label>代理（可选）</label><input id="pf-proxy" value="${esc(provider?.proxy_url || '')}" placeholder="socks5://127.0.0.1:1080 或 http://127.0.0.1:8080"></div>
      <div class="form-group"><label>模型重定向（可选，JSON 对象）</label><textarea id="pf-mapping" style="min-height:56px" placeholder='{"gpt-4": "gpt-4-turbo"}'>${esc(provider?.model_mapping && Object.keys(provider.model_mapping).length > 0 ? JSON.stringify(provider.model_mapping) : '')}</textarea></div>
      <div class="form-actions">
        <button class="btn-primary" id="pf-save">${provider ? '保存' : '创建'}</button>
        <button class="btn-outline" id="pf-cancel">取消</button>
      </div>
    </div>`;

  document.body.appendChild(overlay);

  // Protocol checkboxes drive the default-protocol dropdown: only checked
  // protocols can be the default.
  const protoBoxes = ['openai', 'anthropic'].map(t => document.getElementById('pf-proto-' + t));
  const savedProtocols = (provider?.protocols && provider.protocols.length) ? provider.protocols : ['openai'];
  protoBoxes.forEach(b => { b.checked = savedProtocols.includes(b.value); });
  const defaultProtoSel = document.getElementById('pf-default-protocol');
  function syncDefaultProtocol() {
    const cur = defaultProtoSel.value || provider?.default_protocol || '';
    const checked = protoBoxes.filter(b => b.checked).map(b => b.value);
    defaultProtoSel.innerHTML = checked.map(t => `<option value="${t}">${t}</option>`).join('');
    if (checked.includes(cur)) defaultProtoSel.value = cur;
  }
  protoBoxes.forEach(b => { b.onchange = syncDefaultProtocol; });
  syncDefaultProtocol();

  document.getElementById('pf-cancel').onclick = () => overlay.remove();
  overlay.onclick = (e) => { if (e.target === overlay) overlay.remove(); };

  // Pull the available model list from the upstream with the form's current
  // connection settings; in edit mode the stored key is used when the key
  // field is left blank.
  document.getElementById('pf-fetch-models').onclick = async () => {
    const btn = document.getElementById('pf-fetch-models');
    btn.disabled = true; btn.textContent = '获取中…';
    try {
      const r = await api('POST', '/api/admin/providers/fetch-models', {
        base_url: document.getElementById('pf-url').value.trim(),
        api_key: document.getElementById('pf-key').value,
        protocol: document.getElementById('pf-default-protocol').value || 'openai',
        proxy_url: document.getElementById('pf-proxy').value.trim(),
        provider_id: provider?.id || null,
      });
      if (r.ok && (r.data.models || []).length > 0) {
        document.getElementById('pf-models').value = r.data.models.join(', ');
        toast(`获取到 ${r.data.models.length} 个模型`);
      } else {
        toast(r.data.message || r.data.error || '未获取到模型列表', 'error');
      }
    } finally {
      btn.disabled = false; btn.textContent = '获取模型列表';
    }
  };

  document.getElementById('pf-save').onclick = async () => {
    const btn = document.getElementById('pf-save');
    btn.disabled = true;
    try {
    const toNum = (v, d) => { const n = parseFloat(v); return Number.isNaN(n) ? d : n; };
    const toInt = (v, d) => { const n = parseInt(v); return Number.isNaN(n) ? d : n; };
    const protocols = protoBoxes.filter(b => b.checked).map(b => b.value);
    if (protocols.length === 0) return toast('请至少勾选一个支持协议', 'error');
    const body = {
      name: document.getElementById('pf-name').value,
      protocols,
      default_protocol: defaultProtoSel.value || protocols[0],
      base_url: document.getElementById('pf-url').value,
      api_key: document.getElementById('pf-key').value,
      models: document.getElementById('pf-models').value.split(',').map(s => s.trim()).filter(Boolean),
      priority: toInt(document.getElementById('pf-priority').value, 0),
      weight: toNum(document.getElementById('pf-weight').value, 1.0),
      max_retries: toInt(document.getElementById('pf-retries').value, 3),
      timeout_secs: toInt(document.getElementById('pf-timeout').value, 120),
      proxy_url: document.getElementById('pf-proxy').value.trim(),
    };
    const mappingText = document.getElementById('pf-mapping').value.trim();
    if (mappingText) {
      try { body.model_mapping = JSON.parse(mappingText); }
      catch { return toast('模型重定向不是有效的 JSON', 'error'); }
    } else {
      body.model_mapping = {};
    }
    if (!body.name || !body.base_url || (!provider && !body.api_key)) return toast('请填写名称、地址和 API Key', 'error');

    if (provider) {
      // Only send changed fields
      const upd = {};
      for (const k of Object.keys(body)) {
        const nv = typeof body[k] === 'object' && body[k] !== null ? JSON.stringify(body[k]) : String(body[k]);
        let old = provider[k];
        // Normalize missing old values so empty input doesn't count as a change
        if (old === null || old === undefined || old === '') {
          old = typeof body[k] === 'object' && body[k] !== null ? (Array.isArray(body[k]) ? [] : {}) : '';
        }
        const ov = typeof old === 'object' && old !== null ? JSON.stringify(old) : String(old);
        if (nv !== ov) upd[k] = body[k];
      }
      if (Object.keys(upd).length === 0) return toast('没有修改', 'info');
      const r = await api('PATCH', `/api/admin/providers/${provider.id}`, upd);
      if (!r.ok) return toast(r.data.message || r.data.error || '操作失败', 'error');
      toast('渠道已更新');
    } else {
      const r = await api('POST', '/api/admin/providers', body);
      if (!r.ok) return toast(r.data.message || r.data.error || '操作失败', 'error');
      toast('渠道已创建');
    }
    overlay.remove();
    await loadProviders();
    } finally {
      btn.disabled = false;
    }
  };
}

async function duplicateProvider(id) {
  const r = await api('POST', `/api/admin/providers/${id}/duplicate`);
  if (r.ok) { toast(`已创建副本: ${r.data.name || '新渠道'}`); await loadProviders(); }
  else toast('复制失败: ' + (r.data.message || r.data.error || '未知错误'), 'error');
}

async function testProvider(id, btn) {
  btn.disabled = true; btn.textContent = '…';
  try {
    const r = await api('POST', `/api/admin/providers/${id}/test`);
    if (r.ok) {
      toast(r.data.message || '测试完成', r.data.status === 'healthy' ? 'success' : 'info');
    } else {
      toast('测试失败: ' + (r.data.message || r.data.error || '未知错误'), 'error');
    }
    await loadProviders();
  } finally {
    btn.disabled = false; btn.textContent = '测试';
  }
}

async function deleteProvider(id) {
  if (!confirm('确定要删除此渠道吗？')) return;
  const r = await api('DELETE', `/api/admin/providers/${id}`);
  if (r.ok) { toast('渠道已删除'); await loadProviders(); }
  else toast('删除失败', 'error');
}


