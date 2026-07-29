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
                  ${p.website_url
                    ? (/^https?:\/\//.test(p.website_url)
                      ? `<a href="${esc(p.website_url)}" target="_blank" rel="noopener" style="color:var(--primary);text-decoration:none" title="官网地址">${esc(p.website_url)}</a>`
                      : `<span title="官网地址">${esc(p.website_url)}</span>`)
                    : '-'}
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
                  <button class="${p.is_active ? 'btn-outline' : 'btn-primary'} btn-sm" data-action="toggle-provider" data-id="${esc(p.id)}" data-active="${p.is_active}">${p.is_active ? '禁用' : '启用'}</button>
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
        <div class="form-group"><label>备注</label><input id="pf-note" value="${esc(provider?.note || '')}"></div>
      </div>
      <div class="form-group"><label>官网地址</label><input id="pf-website" value="${esc(provider?.website_url || '')}" placeholder="https://..."></div>
      <div class="form-group">
        <div style="display:grid;grid-template-columns:auto auto 1fr;gap:10px 14px;align-items:center">
          <span style="font-size:12.5px;font-weight:600;color:var(--text-2);text-align:center">默认</span>
          <span style="font-size:12.5px;font-weight:600;color:var(--text-2)">API 协议</span>
          <span style="font-size:12.5px;font-weight:600;color:var(--text-2)">Base URL</span>
          <input type="radio" name="pf-default" id="pf-default-openai" value="openai" style="width:auto;margin:0;justify-self:center">
          <label style="display:flex;gap:6px;align-items:center;font-weight:400;margin-bottom:0"><input type="checkbox" id="pf-proto-openai" value="openai" style="width:auto"> OpenAI</label>
          <input id="pf-url-openai" value="${esc(provider?.openai_base_url || '')}" placeholder="https://api.openai.com">
          <input type="radio" name="pf-default" id="pf-default-anthropic" value="anthropic" style="width:auto;margin:0;justify-self:center">
          <label style="display:flex;gap:6px;align-items:center;font-weight:400;margin-bottom:0"><input type="checkbox" id="pf-proto-anthropic" value="anthropic" style="width:auto"> Anthropic</label>
          <input id="pf-url-anthropic" value="${esc(provider?.anthropic_base_url || '')}" placeholder="https://api.anthropic.com">
        </div>
      </div>
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

  // 复选=启用协议并放开对应 URL 输入(保存时必填);单选=默认协议。
  // 只有已勾选的协议能作为默认:取消勾选当前默认协议时,
  // 默认自动落到仍勾选的协议上。
  const protoBoxes = ['openai', 'anthropic'].map(t => document.getElementById('pf-proto-' + t));
  const defaultRadios = ['openai', 'anthropic'].map(t => document.getElementById('pf-default-' + t));
  const urlInputs = {
    openai: document.getElementById('pf-url-openai'),
    anthropic: document.getElementById('pf-url-anthropic'),
  };
  const savedProtocols = (provider?.protocols && provider.protocols.length) ? provider.protocols : ['openai'];
  protoBoxes.forEach(b => { b.checked = savedProtocols.includes(b.value); });
  const savedDefault = provider?.default_protocol || savedProtocols[0];
  defaultRadios.forEach(r => { r.checked = r.value === savedDefault; });
  function syncProtoRows() {
    for (const b of protoBoxes) {
      urlInputs[b.value].disabled = !b.checked;
      defaultRadios.find(r => r.value === b.value).disabled = !b.checked;
    }
    const enabled = defaultRadios.filter(r => !r.disabled);
    if (!enabled.some(r => r.checked) && enabled.length > 0) enabled[0].checked = true;
  }
  protoBoxes.forEach(b => { b.onchange = syncProtoRows; });
  syncProtoRows();

  // 按协议取对应的上游地址;留空的一路回退到另一个,与后端 base_url_for 一致。
  const urlFor = (proto) => {
    const o = document.getElementById('pf-url-openai').value.trim();
    const a = document.getElementById('pf-url-anthropic').value.trim();
    return proto === 'anthropic' ? (a || o) : (o || a);
  };

  document.getElementById('pf-cancel').onclick = () => overlay.remove();
  overlay.onclick = (e) => { if (e.target === overlay) overlay.remove(); };

  // Pull the available model list from the upstream with the form's current
  // connection settings; in edit mode the stored key is used when the key
  // field is left blank.
  document.getElementById('pf-fetch-models').onclick = async () => {
    const btn = document.getElementById('pf-fetch-models');
    btn.disabled = true; btn.textContent = '获取中…';
    try {
      const proto = (defaultRadios.find(r => r.checked && !r.disabled) || {}).value || 'openai';
      const r = await api('POST', '/api/admin/providers/fetch-models', {
        base_url: urlFor(proto),
        api_key: document.getElementById('pf-key').value,
        protocol: proto,
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
      note: document.getElementById('pf-note').value.trim(),
      website_url: document.getElementById('pf-website').value.trim(),
      protocols,
      default_protocol: (defaultRadios.find(r => r.checked && !r.disabled) || {}).value || protocols[0],
      openai_base_url: document.getElementById('pf-url-openai').value.trim(),
      anthropic_base_url: document.getElementById('pf-url-anthropic').value.trim(),
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
    if (!body.name || (!provider && !body.api_key)) return toast('请填写名称和 API Key', 'error');
    if (protocols.includes('openai') && !body.openai_base_url) return toast('已勾选 OpenAI 协议，请填写 OPENAI_BASE_URL', 'error');
    if (protocols.includes('anthropic') && !body.anthropic_base_url) return toast('已勾选 Anthropic 协议，请填写 ANTHROPIC_BASE_URL', 'error');

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

async function toggleProvider(id, active) {
  const r = await api('PATCH', `/api/admin/providers/${id}`, { is_active: active });
  if (r.ok) { toast(active ? '渠道已启用' : '渠道已禁用'); await loadProviders(); }
  else toast(r.data.message || r.data.error || '操作失败', 'error');
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


