// ============ API Keys ============
async function renderApiKeys(container) {
  container.innerHTML = `
    <div class="card-header">
      <div class="page-head" style="margin-bottom:0"><h2>API 密钥</h2><p>创建和管理访问密钥，密钥可直接用于 /v1/* 接口调用</p></div>
      <button class="btn-primary" id="new-apikey-btn">+ 创建密钥</button>
    </div>
    <div id="apikeys-list"><div class="empty"><div class="spinner"></div><p>加载中...</p></div></div>`;

  document.getElementById('new-apikey-btn').onclick = () => showApiKeyModal();

  await loadApiKeys();
}

function toLocalInput(v) {
  if (!v) return '';
  const d = new Date(v);
  if (Number.isNaN(d.getTime())) return '';
  return d.getFullYear() + '-' + String(d.getMonth() + 1).padStart(2, '0') + '-' + String(d.getDate()).padStart(2, '0') +
    'T' + String(d.getHours()).padStart(2, '0') + ':' + String(d.getMinutes()).padStart(2, '0');
}

function showApiKeyModal(id) {
  const key = id ? (state.apiKeys || []).find(k => k.id === id) : null;
  const overlay = document.createElement('div');
  overlay.className = 'modal-overlay';
  overlay.innerHTML = `
    <div class="modal">
      <h2>${key ? '编辑密钥' : '创建密钥'}</h2>
      <div class="form-group"><label>名称</label><input id="kf-name" value="${esc(key?.name || '')}" placeholder="例如: 我的应用"></div>
      <div class="form-group"><label>过期时间（留空永不过期）</label><input id="kf-expires" type="datetime-local" value="${esc(toLocalInput(key?.expires_at))}"></div>
      <div class="form-group"><label>模型限制（逗号分隔，留空不限制）</label><input id="kf-models" value="${esc((key?.models || []).join(', '))}" placeholder="gpt-4, claude-*"></div>
      <div class="form-actions">
        <button class="btn-primary" id="kf-save">${key ? '保存' : '创建'}</button>
        <button class="btn-outline" id="kf-cancel">取消</button>
      </div>
    </div>`;
  document.body.appendChild(overlay);

  document.getElementById('kf-cancel').onclick = () => overlay.remove();
  overlay.onclick = (e) => { if (e.target === overlay) overlay.remove(); };

  document.getElementById('kf-save').onclick = async () => {
    const btn = document.getElementById('kf-save');
    btn.disabled = true;
    try {
    const expInput = document.getElementById('kf-expires').value;
    const expDate = expInput ? new Date(expInput) : null;
    const body = {
      name: document.getElementById('kf-name').value,
      expires_at: expDate && !Number.isNaN(expDate.getTime()) ? expDate.toISOString() : '',
      models: document.getElementById('kf-models').value.split(',').map(s => s.trim()).filter(Boolean),
    };
    if (key) {
      const r = await api('PATCH', `/api/api-keys/${key.id}`, body);
      if (r.ok) { toast('密钥已更新'); overlay.remove(); await loadApiKeys(); }
      else toast(r.data.message || r.data.error || '更新失败', 'error');
    } else {
      if (!body.expires_at) delete body.expires_at;
      const r = await api('POST', '/api/api-keys', body);
      if (r.ok) {
        const newKey = r.data.key;
        overlay.remove();
        toast('密钥已创建');
        showKeyCreatedModal(newKey);
        await loadApiKeys();
      } else toast(r.data.message || r.data.error || '创建失败', 'error');
    }
    } finally {
      btn.disabled = false;
    }
  };
}

function showKeyCreatedModal(key) {
  const overlay = document.createElement('div');
  overlay.className = 'modal-overlay';
  overlay.innerHTML = `
    <div class="modal">
      <h2>密钥已创建</h2>
      <p style="margin-bottom:10px;color:var(--muted);font-size:12.5px">请立即复制，密钥不会再次显示。调用方式:<code>Authorization: Bearer ${esc(String(key || '').slice(0, 10))}...</code></p>
      <div class="code-block" id="new-key-value" style="user-select:all">${esc(key)}</div>
      <div class="form-actions">
        <button class="btn-primary" id="new-key-copy">复制到剪贴板</button>
        <button class="btn-outline" id="new-key-close">关闭</button>
      </div>
    </div>`;
  document.body.appendChild(overlay);
  overlay.onclick = (e) => { if (e.target === overlay) overlay.remove(); };
  // 不用内联 onclick，CSP 将限制 script-src 'self'
  document.getElementById('new-key-copy').addEventListener('click', () =>
    copyText(document.getElementById('new-key-value').textContent));
  document.getElementById('new-key-close').addEventListener('click', () => overlay.remove());
}

async function loadApiKeys() {
  const r = await api('GET', '/api/api-keys');
  const list = document.getElementById('apikeys-list');
  if (!list) return;
  if (!r.ok) { list.innerHTML = '<div class="empty"><p>加载失败</p></div>'; return; }

  const data = r.data;
  state.apiKeys = data;
  if (data.length === 0) {
    list.innerHTML = '<div class="card"><div class="empty"><p>暂无密钥，点击上方创建</p></div></div>';
    return;
  }

  const isExpired = k => k.expires_at && new Date(k.expires_at) < new Date();

  list.innerHTML = `
    <div class="card">
      <div class="table-wrap">
        <table>
          <thead><tr><th>名称</th><th>密钥</th><th>状态</th><th>模型限制</th><th>过期时间</th><th>最后使用</th><th>操作</th></tr></thead>
          <tbody>
            ${data.map(k => `
              <tr>
                <td><strong>${esc(k.name || '-')}</strong></td>
                <td style="font-family:monospace;font-size:12.5px;color:var(--muted)">${esc(k.key)}</td>
                <td><span class="badge ${k.is_active && !isExpired(k) ? 'badge-green' : 'badge-red'}">${!k.is_active ? '已禁用' : isExpired(k) ? '已过期' : '活跃'}</span></td>
                <td style="font-size:12.5px">${(k.models || []).length > 0 ? esc(k.models.join(', ')) : '<span style="color:var(--faint)">全部</span>'}</td>
                <td style="color:var(--muted);font-size:12.5px">${k.expires_at ? esc(k.expires_at) : '<span style="color:var(--faint)">永不</span>'}</td>
                <td style="color:var(--muted);font-size:12.5px">${k.last_used_at ? esc(k.last_used_at) : '<span style="color:var(--faint)">未使用</span>'}</td>
                <td>
                  <button class="btn-outline btn-sm" data-action="edit-api-key" data-id="${esc(k.id)}">编辑</button>
                  <button class="btn-outline btn-sm" data-action="toggle-api-key" data-id="${esc(k.id)}" data-active="${!k.is_active}">${k.is_active ? '禁用' : '启用'}</button>
                  <button class="btn-danger btn-sm" data-action="delete-api-key" data-id="${esc(k.id)}">删除</button>
                </td>
              </tr>
            `).join('')}
          </tbody>
        </table>
      </div>
    </div>`;
}

async function toggleApiKey(id, active) {
  const r = await api('PATCH', `/api/api-keys/${id}`, { is_active: active });
  if (r.ok) { toast(active ? '密钥已启用' : '密钥已禁用'); await loadApiKeys(); }
  else toast('操作失败', 'error');
}

async function deleteApiKey(id) {
  if (!confirm('确定要删除此密钥吗？')) return;
  const r = await api('DELETE', `/api/api-keys/${id}`);
  if (r.ok) { toast('密钥已删除'); await loadApiKeys(); }
  else toast('删除失败', 'error');
}


