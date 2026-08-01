// ============ Billing ============
// 价格表默认只显示渠道里已添加模型会命中的条目(内置默认价格有 200+ 条,
// 全列出来没法看);勾选框可切回全部。
let priceShowAll = false;

async function renderBilling(container) {
  container.innerHTML = `
    <div class="card-header">
      <div class="page-head" style="margin-bottom:0"><h2>计费</h2><p>模型价格与调账记录,价格单位为每 1M tokens(元)</p></div>
      <div style="display:flex;align-items:center;gap:14px">
        <label style="display:flex;align-items:center;gap:6px;font-size:12.5px;color:var(--muted);cursor:pointer;user-select:none">
          <input type="checkbox" id="price-show-all" ${priceShowAll ? 'checked' : ''}> 显示全部价格
        </label>
        <button class="btn-primary" id="new-price-btn">+ 添加价格</button>
      </div>
    </div>
    <div id="prices-list"><div class="empty"><div class="spinner"></div><p>加载中...</p></div></div>
    <div class="page-head" style="margin-top:24px"><h2>调账记录</h2></div>
    <div class="card" style="padding:14px 18px">
      <div class="form-group" style="margin-bottom:0;max-width:220px">
        <label>用户</label>
        <select id="tx-filter-user" class="inline-select">
          <option value="">全部用户</option>
        </select>
      </div>
    </div>
    <div id="transactions-list"><div class="empty"><div class="spinner"></div><p>加载中...</p></div></div>`;

  document.getElementById('new-price-btn').onclick = () => showPriceModal();
  document.getElementById('price-show-all').onchange = (e) => {
    priceShowAll = e.target.checked;
    loadPrices();
  };
  document.getElementById('tx-filter-user').onchange = () => loadTransactions();

  // 筛选下拉复用用户页缓存的 state.users,未访问过用户页时先拉一次
  if (!state.users.length) {
    api('GET', '/api/admin/users').then(r => {
      if (r.ok) state.users = r.data;
      fillTxUserFilter();
    });
  } else fillTxUserFilter();

  await Promise.all([loadPrices(), loadTransactions()]);
}

function fillTxUserFilter() {
  const sel = document.getElementById('tx-filter-user');
  if (!sel) return;
  (state.users || []).forEach(u => {
    const o = document.createElement('option');
    o.value = u.id; o.textContent = u.username;
    sel.appendChild(o);
  });
}

async function loadPrices() {
  const r = await api('GET', '/api/admin/prices' + (priceShowAll ? '' : '?in_use=1'));
  const list = document.getElementById('prices-list');
  if (!list) return;
  if (!r.ok) { list.innerHTML = '<div class="empty"><p>加载失败</p></div>'; return; }
  const data = r.data;
  state.prices = data;
  if (data.length === 0) {
    list.innerHTML = `<div class="card"><div class="empty"><p>${priceShowAll ? '暂无价格,未计费的请求按 0 元记账' : '渠道模型暂无价格条目,可切换"显示全部价格"或添加'}</p></div></div>`;
    return;
  }
  list.innerHTML = `
    <div class="card">
      <div class="table-wrap">
        <table>
          <thead><tr><th>模型</th><th>输入价(元/1M)</th><th>输出价(元/1M)</th><th>缓存价(元/1M)</th><th>更新时间</th><th>操作</th></tr></thead>
          <tbody>
            ${data.map(p => `
              <tr>
                <td><strong>${esc(p.model)}</strong></td>
                <td>${fmtCost(p.prompt_price)}</td>
                <td>${fmtCost(p.completion_price)}</td>
                <td>${p.cached_price == null ? '<span style="color:var(--faint)">同输入价</span>' : fmtCost(p.cached_price)}</td>
                <td style="color:var(--muted);font-size:12.5px">${esc(fmtTime(p.updated_at))}</td>
                <td>
                  <button class="btn-outline btn-sm" data-action="edit-price" data-id="${esc(p.id)}">编辑</button>
                  <button class="btn-danger btn-sm" data-action="delete-price" data-id="${esc(p.id)}">删除</button>
                </td>
              </tr>
            `).join('')}
          </tbody>
        </table>
      </div>
    </div>`;
}

function showPriceModal(id) {
  const price = id ? (state.prices || []).find(p => p.id === id) : null;
  const overlay = document.createElement('div');
  overlay.className = 'modal-overlay';
  overlay.innerHTML = `
    <div class="modal">
      <h2>${price ? '编辑价格' : '添加价格'}</h2>
      <div class="form-group"><label>模型</label><input id="pf-model" value="${esc(price?.model || '')}" placeholder="gpt-4 或 gpt-*"></div>
      <div class="form-group"><label>输入价(元/1M tokens)</label><input id="pf-prompt" type="number" min="0" step="any" value="${price ? esc(String(price.prompt_price)) : ''}"></div>
      <div class="form-group"><label>输出价(元/1M tokens)</label><input id="pf-completion" type="number" min="0" step="any" value="${price ? esc(String(price.completion_price)) : ''}"></div>
      <div class="form-group"><label>缓存价(元/1M tokens,留空按输入价)</label><input id="pf-cached" type="number" min="0" step="any" value="${price?.cached_price != null ? esc(String(price.cached_price)) : ''}"></div>
      <div class="form-actions">
        <button class="btn-primary" id="pf-save">${price ? '保存' : '创建'}</button>
        <button class="btn-outline" id="pf-cancel">取消</button>
      </div>
    </div>`;
  document.body.appendChild(overlay);
  document.getElementById('pf-cancel').onclick = () => overlay.remove();
  overlay.onclick = (e) => { if (e.target === overlay) overlay.remove(); };

  document.getElementById('pf-save').onclick = async () => {
    const btn = document.getElementById('pf-save');
    btn.disabled = true;
    try {
      const cachedRaw = document.getElementById('pf-cached').value.trim();
      const body = {
        model: document.getElementById('pf-model').value.trim(),
        prompt_price: parseFloat(document.getElementById('pf-prompt').value) || 0,
        completion_price: parseFloat(document.getElementById('pf-completion').value) || 0,
        // 缓存价留空传 null:缓存 token 按输入价计费
        cached_price: cachedRaw === '' ? null : (parseFloat(cachedRaw) || 0),
      };
      const r = price
        ? await api('PATCH', `/api/admin/prices/${price.id}`, body)
        : await api('POST', '/api/admin/prices', body);
      if (r.ok) { toast(price ? '价格已更新' : '价格已创建'); overlay.remove(); await loadPrices(); }
      else toast(r.data.message || r.data.error || '操作失败', 'error');
    } finally {
      btn.disabled = false;
    }
  };
}

async function deletePrice(id) {
  if (!confirm('确定要删除此价格吗?')) return;
  const r = await api('DELETE', `/api/admin/prices/${id}`);
  if (r.ok) { toast('价格已删除'); await loadPrices(); }
  else toast(r.data.message || r.data.error || '删除失败', 'error');
}

const TX_PAGE_SIZE = 200;
let txOffset = 0;

function txQuerySuffix() {
  const uid = document.getElementById('tx-filter-user')?.value;
  return uid ? '&user_id=' + encodeURIComponent(uid) : '';
}

function txRow(t) {
  return `
    <tr>
      <td style="color:var(--muted);font-size:12.5px">${esc(fmtTime(t.created_at))}</td>
      <td><strong>${esc(t.username || t.user_id)}</strong></td>
      <td style="color:${t.amount >= 0 ? 'var(--ok,#10b981)' : 'var(--danger,#f43f5e)'}">${t.amount >= 0 ? '+' : ''}${fmtCost(t.amount)}</td>
      <td>${fmtCost(t.balance_after)}</td>
      <td><span class="badge ${t.kind === 'recharge' ? 'badge-green' : 'badge-gray'}">${t.kind === 'recharge' ? '充值' : '调整'}</span></td>
      <td style="font-size:12.5px">${esc(t.note) || '<span style="color:var(--faint)">-</span>'}</td>
    </tr>`;
}

async function loadTransactions() {
  const r = await api('GET', '/api/admin/billing/transactions?offset=0' + txQuerySuffix());
  const list = document.getElementById('transactions-list');
  if (!list) return;
  if (!r.ok) { list.innerHTML = '<div class="empty"><p>加载失败</p></div>'; return; }
  const data = r.data;
  txOffset = 0;
  if (data.length === 0) {
    list.innerHTML = '<div class="card"><div class="empty"><p>暂无调账记录</p></div></div>';
    return;
  }
  list.innerHTML = `
    <div class="card">
      <div class="table-wrap">
        <table>
          <thead><tr><th>时间</th><th>用户</th><th>金额(元)</th><th>调后余额(元)</th><th>类型</th><th>备注</th></tr></thead>
          <tbody id="tx-tbody">
            ${data.map(txRow).join('')}
          </tbody>
        </table>
      </div>
      <div id="tx-more-wrap" style="text-align:center;margin-top:12px;${data.length < TX_PAGE_SIZE ? 'display:none' : ''}">
        <button class="btn-outline btn-sm" id="tx-more">加载更多</button>
      </div>
    </div>`;
  const moreBtn = document.getElementById('tx-more');
  if (moreBtn) moreBtn.onclick = () => loadMoreTransactions();
}

// 追加下一页:offset 累加,返回不足一页时隐藏按钮
async function loadMoreTransactions() {
  const btn = document.getElementById('tx-more');
  const tbody = document.getElementById('tx-tbody');
  if (!btn || !tbody) return;
  btn.disabled = true;
  try {
    const next = txOffset + TX_PAGE_SIZE;
    const r = await api('GET', `/api/admin/billing/transactions?offset=${next}` + txQuerySuffix());
    if (!r.ok) { toast(r.data.message || r.data.error || '加载失败', 'error'); return; }
    if (!document.getElementById('tx-tbody')) return; // 等待期间已切页/重筛
    txOffset = next;
    tbody.insertAdjacentHTML('beforeend', r.data.map(txRow).join(''));
    if (r.data.length < TX_PAGE_SIZE) document.getElementById('tx-more-wrap')?.remove();
  } finally {
    btn.disabled = false;
  }
}

function showAdjustBalanceModal(id) {
  const user = (state.users || []).find(u => u.id === id);
  const overlay = document.createElement('div');
  overlay.className = 'modal-overlay';
  overlay.innerHTML = `
    <div class="modal">
      <h2>调账</h2>
      <div class="form-group"><label>用户</label><input value="${esc(user?.username || id)}" disabled></div>
      <div class="form-group"><label>当前余额(元)</label><input value="${esc(fmtCost(user?.balance ?? 0))}" disabled></div>
      <div class="form-group"><label>金额(正为充值,负为扣减)</label><input id="bf-amount" type="number" step="any" placeholder="100 或 -50"></div>
      <div class="form-group"><label>备注</label><input id="bf-note" placeholder="选填"></div>
      <div class="form-actions">
        <button class="btn-primary" id="bf-save">确认</button>
        <button class="btn-outline" id="bf-cancel">取消</button>
      </div>
    </div>`;
  document.body.appendChild(overlay);
  document.getElementById('bf-cancel').onclick = () => overlay.remove();
  overlay.onclick = (e) => { if (e.target === overlay) overlay.remove(); };

  document.getElementById('bf-save').onclick = async () => {
    const btn = document.getElementById('bf-save');
    const amount = parseFloat(document.getElementById('bf-amount').value);
    if (!Number.isFinite(amount) || amount === 0) return toast('金额必须是非零数字', 'error');
    btn.disabled = true;
    try {
      const r = await api('POST', `/api/admin/users/${id}/balance`, {
        amount,
        note: document.getElementById('bf-note').value,
      });
      if (r.ok) {
        toast(`调账成功,当前余额 ${fmtCost(r.data.balance)} 元`);
        overlay.remove();
        if (document.getElementById('users-list')) await renderUsers(document.getElementById('main-content'));
      } else toast(r.data.message || r.data.error || '调账失败', 'error');
    } finally {
      btn.disabled = false;
    }
  };
}
