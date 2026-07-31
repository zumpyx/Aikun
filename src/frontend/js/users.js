// ============ Users ============
async function renderUsers(container) {
  if (!container) return;
  container.innerHTML = `
    <div class="card-header">
      <div class="page-head" style="margin-bottom:0"><h2>用户管理</h2><p>管理系统用户与权限</p></div>
      <div>
        <button class="btn-outline" id="batch-user-btn" style="margin-right:8px">批量创建</button>
        <button class="btn-primary" id="add-user-btn">+ 添加用户</button>
      </div>
    </div>
    <div id="users-list"><div class="empty"><div class="spinner"></div><p>加载中...</p></div></div>`;

  document.getElementById('add-user-btn').onclick = () => showUserModal();
  document.getElementById('batch-user-btn').onclick = () => showBatchUserModal();

  const r = await api('GET', '/api/admin/users');
  const list = document.getElementById('users-list');
  if (!list) return;
  if (!r.ok) { list.innerHTML = '<div class="empty"><p>加载失败</p></div>'; return; }

  const data = r.data;
  state.users = data;
  if (data.length === 0) {
    list.innerHTML = '<div class="card"><div class="empty"><p>暂无用户</p></div></div>';
    return;
  }

  list.innerHTML = `
    <div class="card">
      <div class="table-wrap">
        <table>
          <thead><tr><th>用户名</th><th>显示名</th><th>角色</th><th>状态</th><th>余额(元)</th><th>创建时间</th><th>操作</th></tr></thead>
          <tbody>
            ${data.map(u => `
              <tr>
                <td><strong>${esc(u.username)}</strong></td>
                <td>${esc(u.display_name)}</td>
                <td><span class="badge ${u.role === 'admin' ? 'badge-blue' : 'badge-gray'}">${u.role === 'admin' ? 'Admin' : 'User'}</span></td>
                <td><span class="badge ${u.is_active ? 'badge-green' : 'badge-red'}">${u.is_active ? '活跃' : '禁用'}</span></td>
                <td style="font-size:12.5px;color:${(u.balance ?? 0) < 0 ? 'var(--danger,#f43f5e)' : 'inherit'}">${fmtCost(u.balance ?? 0)}</td>
                <td style="color:var(--muted);font-size:12.5px">${esc(u.created_at)}</td>
                <td>
                  <button class="btn-outline btn-sm" data-action="edit-user" data-id="${esc(u.id)}">编辑</button>
                  <button class="btn-outline btn-sm" data-action="adjust-balance" data-id="${esc(u.id)}">调账</button>
                  <button class="btn-danger btn-sm" data-action="toggle-user" data-id="${esc(u.id)}" data-active="${!u.is_active}">${u.is_active ? '禁用' : '启用'}</button>
                </td>
              </tr>
            `).join('')}
          </tbody>
        </table>
      </div>
    </div>`;
}

async function showUserModal(id) {
  let user = null;
  if (id) {
    const r = await api('GET', `/api/admin/users/${id}`);
    // 详情加载失败时直接报错退出，避免误当新建用户覆盖
    if (!r.ok) return toast(r.data.message || r.data.error || '加载用户详情失败', 'error');
    user = r.data;
  }

  const overlay = document.createElement('div');
  overlay.className = 'modal-overlay';
  overlay.innerHTML = `
    <div class="modal">
      <h2>${user ? '编辑用户' : '添加用户'}</h2>
      <div class="form-group"><label>用户名</label><input id="uf-user" value="${esc(user?.username || '')}" ${user ? 'disabled' : ''}></div>
      <div class="form-group"><label>显示名称</label><input id="uf-name" value="${esc(user?.display_name || '')}"></div>
      <div class="form-group"><label>密码${user ? '（留空不修改）' : ''}</label><input id="uf-pass" type="password" ${user ? '' : 'required'}></div>
      ${!user ? `<div class="form-group"><label>角色</label><select id="uf-role"><option value="user">User</option><option value="admin">Admin</option></select></div>` : ''}
      <div class="form-actions">
        <button class="btn-primary" id="uf-save">${user ? '保存' : '创建'}</button>
        <button class="btn-outline" id="uf-cancel">取消</button>
      </div>
    </div>`;

  document.body.appendChild(overlay);

  document.getElementById('uf-cancel').onclick = () => overlay.remove();
  overlay.onclick = (e) => { if (e.target === overlay) overlay.remove(); };

  document.getElementById('uf-save').onclick = async () => {
    const btn = document.getElementById('uf-save');
    btn.disabled = true;
    try {
    if (user) {
      const upd = {};
      const name = document.getElementById('uf-name').value;
      const pass = document.getElementById('uf-pass').value;
      if (name && name !== user.display_name) upd.display_name = name;
      if (pass) upd.password = pass;
      if (Object.keys(upd).length === 0) return toast('没有修改', 'info');
      const r = await api('PATCH', `/api/admin/users/${user.id}`, upd);
      if (!r.ok) return toast(r.data.message || r.data.error || '操作失败', 'error');
      toast('用户已更新');
    } else {
      const u = document.getElementById('uf-user').value;
      const p = document.getElementById('uf-pass').value;
      const n = document.getElementById('uf-name').value;
      const r = document.getElementById('uf-role').value;
      if (!u || !p) return toast('请填写用户名和密码', 'error');
      const res = await api('POST', '/api/admin/users', { username: u, password: p, display_name: n || u, role: r });
      if (!res.ok) return toast(res.data.message || res.data.error || '操作失败', 'error');
      toast('用户已创建');
    }
    overlay.remove();
    // await 期间用户可能已切走页面,用户列表不在 DOM 中就放弃重渲染
    if (document.getElementById('users-list')) await renderUsers(document.getElementById('main-content'));
    } finally {
      btn.disabled = false;
    }
  };
}

async function toggleUser(id, active) {
  const r = await api('PATCH', `/api/admin/users/${id}`, { is_active: active });
  if (r.ok) { toast(active ? '用户已启用' : '用户已禁用'); if (document.getElementById('users-list')) await renderUsers(document.getElementById('main-content')); }
  else toast('操作失败', 'error');
}

function showBatchUserModal() {
  const overlay = document.createElement('div');
  overlay.className = 'modal-overlay';
  overlay.innerHTML = `
    <div class="modal" style="max-width:560px">
      <h2>批量创建用户</h2>
      <p style="font-size:12.5px;color:var(--muted);margin-bottom:10px">
        每行一个用户，格式：<code>用户名,密码,显示名</code>。密码可留空（自动生成并只显示一次），显示名可省略。<br>
        示例：<code>alice,,爱丽丝</code> 或 <code>bob,pass123</code>
      </p>
      <div class="form-group"><textarea id="batch-input" rows="8" style="width:100%;font-family:monospace" placeholder="alice,,爱丽丝&#10;bob,pass123&#10;carol"></textarea></div>
      <div class="form-group"><label>角色（应用于全部）</label><select id="batch-role"><option value="user">User</option><option value="admin">Admin</option></select></div>
      <div id="batch-result"></div>
      <div class="form-actions">
        <button class="btn-primary" id="batch-save">创建</button>
        <button class="btn-outline" id="batch-cancel">关闭</button>
      </div>
    </div>`;

  document.body.appendChild(overlay);
  document.getElementById('batch-cancel').onclick = () => overlay.remove();
  overlay.onclick = (e) => { if (e.target === overlay) overlay.remove(); };

  document.getElementById('batch-save').onclick = async () => {
    const btn = document.getElementById('batch-save');
    const role = document.getElementById('batch-role').value;
    const lines = document.getElementById('batch-input').value.split('\n')
      .map(l => l.trim()).filter(l => l && !l.startsWith('#'));
    if (lines.length === 0) return toast('请输入至少一行用户', 'error');

    const users = [];
    for (const line of lines) {
      const parts = line.split(',').map(s => s.trim());
      const [username, password, display_name] = parts;
      if (!username) return toast(`格式错误:${line}`, 'error');
      const u = { username, role };
      if (password) u.password = password;
      if (display_name) u.display_name = display_name;
      users.push(u);
    }

    btn.disabled = true; btn.textContent = '创建中…';
    try {
      const r = await api('POST', '/api/admin/users/batch', { users });
      if (!r.ok) { toast(r.data.message || r.data.error || '操作失败', 'error'); return; }
      const { created, failed, results } = r.data;
      toast(`创建完成:成功 ${created} 个,失败 ${failed} 个`);
      document.getElementById('batch-result').innerHTML = `
        <div style="max-height:240px;overflow:auto;margin-top:10px;border:1px solid var(--border,#e5e7eb);border-radius:8px;padding:10px;font-size:12.5px">
          ${results.map(x => x.ok
            ? `<div>✅ <strong>${esc(x.username)}</strong>${x.password ? ` — 密码:<code>${esc(x.password)}</code>` : ''}</div>`
            : `<div>❌ <strong>${esc(x.username)}</strong> — ${esc(x.error)}</div>`).join('')}
        </div>
        ${results.some(x => x.ok && x.password) ? '<p style="font-size:12.5px;color:var(--danger);margin-top:6px">自动生成的密码仅在此显示一次,请立即保存。</p>' : ''}`;
      document.getElementById('batch-input').value = '';
      if (document.getElementById('users-list')) await renderUsers(document.getElementById('main-content'));
    } finally {
      btn.disabled = false; btn.textContent = '创建';
    }
  };
}


