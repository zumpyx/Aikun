// ============ Redemption Codes (admin) ============
// 兑换码批量生成与管理。明文码只在生成响应中返回一次,库中只存哈希,
// 列表展示掩码(AK-****-****-****-XXXX)。生成结果提供一键复制,关闭后无法找回。
let redemptionCodes = [];
// 列表分页状态:筛选变更时归零。
let redemptionPage = 0;
const REDEMPTION_PAGE_SIZE = 50;

async function renderRedemption(container) {
  container.innerHTML = `
    <div class="card-header">
      <div class="page-head" style="margin-bottom:0"><h2>兑换码</h2><p>批量生成兑换码,用户在钱包页兑换充值</p></div>
      <button class="btn-primary" id="new-codes-btn">+ 批量生成</button>
    </div>
    <div class="card" style="padding:14px 18px">
      <div style="display:flex;gap:14px;flex-wrap:wrap;align-items:flex-end">
        <div class="form-group" style="margin-bottom:0;max-width:200px">
          <label>批次</label><input id="rc-filter-batch" placeholder="全部批次">
        </div>
        <div class="form-group" style="margin-bottom:0;max-width:160px">
          <label>状态</label>
          <select id="rc-filter-status" class="inline-select">
            <option value="">全部</option>
            <option value="unused">未使用</option>
            <option value="used">已使用</option>
            <option value="disabled">已禁用</option>
            <option value="expired">已过期</option>
          </select>
        </div>
        <button class="btn-outline" id="rc-filter-btn">筛选</button>
      </div>
    </div>
    <div id="codes-list"><div class="empty"><div class="spinner"></div><p>加载中...</p></div></div>`;

  document.getElementById('new-codes-btn').onclick = () => showGenerateCodesModal();
  document.getElementById('rc-filter-btn').onclick = () => { redemptionPage = 0; loadRedemptionCodes(); };
  await loadRedemptionCodes();
}

async function loadRedemptionCodes() {
  const batch = (document.getElementById('rc-filter-batch')?.value || '').trim();
  const status = document.getElementById('rc-filter-status')?.value || '';
  const qs = new URLSearchParams();
  if (batch) qs.set('batch', batch);
  if (status) qs.set('status', status);
  qs.set('limit', REDEMPTION_PAGE_SIZE);
  qs.set('offset', redemptionPage * REDEMPTION_PAGE_SIZE);
  const r = await api('GET', `/api/admin/redemption-codes?${qs}`);
  const list = document.getElementById('codes-list');
  if (!list) return;
  if (!r.ok) { list.innerHTML = '<div class="empty"><p>加载失败</p></div>'; return; }
  const data = r.data.items || [];
  const total = r.data.total || 0;
  redemptionCodes = data;
  if (data.length === 0 && redemptionPage === 0) {
    list.innerHTML = '<div class="card"><div class="empty"><p>暂无兑换码,点击右上角批量生成</p></div></div>';
    return;
  }
  const totalPages = Math.max(1, Math.ceil(total / REDEMPTION_PAGE_SIZE));
  const statusText = { unused: '<span style="color:#14b8a6">未使用</span>', used: '<span style="color:var(--muted)">已使用</span>', disabled: '<span style="color:var(--danger,#f43f5e)">已禁用</span>' };
  // 过期未使用的码显示为「已过期」(status 仍为 unused)
  const displayStatus = (c) => (c.status === 'unused' && c.expired)
    ? '<span style="color:#f59e0b">已过期</span>'
    : (statusText[c.status] || esc(c.status));
  list.innerHTML = `
    <div class="card">
      <div class="table-wrap">
        <table>
          <thead><tr><th>兑换码</th><th>面值(元)</th><th>批次</th><th>状态</th><th>使用者</th><th>创建时间</th><th>过期时间 (UTC)</th><th>使用时间</th><th>备注</th><th>操作</th></tr></thead>
          <tbody>
            ${data.map(c => `
              <tr>
                <td><code>${esc(c.code_masked)}</code></td>
                <td>${fmtCost(c.amount)}</td>
                <td>${esc(c.batch)}</td>
                <td>${displayStatus(c)}</td>
                <td>${c.used_by ? esc(c.used_by) : '<span style="color:var(--faint)">—</span>'}</td>
                <td style="color:var(--muted);font-size:12.5px">${esc(fmtTime(c.created_at))}</td>
                <td style="color:var(--muted);font-size:12.5px">${c.expires_at ? esc(fmtTime(c.expires_at)) : '永不'}</td>
                <td style="color:var(--muted);font-size:12.5px">${c.used_at ? esc(fmtTime(c.used_at)) : '—'}</td>
                <td style="color:var(--muted);font-size:12.5px">${esc(c.note || '')}</td>
                <td>
                  ${c.status === 'unused' ? `<button class="btn-danger btn-sm" data-action="disable-code" data-id="${esc(c.id)}">禁用</button>` : ''}
                </td>
              </tr>
            `).join('')}
          </tbody>
        </table>
      </div>
      <div style="display:flex;justify-content:space-between;align-items:center;margin-top:12px">
        <span style="color:var(--muted);font-size:13px">共 ${total} 条 · 第 ${redemptionPage + 1}/${totalPages} 页</span>
        <div style="display:flex;gap:8px">
          <button class="btn-outline btn-sm" id="rc-prev" ${redemptionPage === 0 ? 'disabled' : ''}>上一页</button>
          <button class="btn-outline btn-sm" id="rc-next" ${redemptionPage + 1 >= totalPages ? 'disabled' : ''}>下一页</button>
        </div>
      </div>
    </div>`;
  document.getElementById('rc-prev').onclick = () => {
    if (redemptionPage > 0) { redemptionPage--; loadRedemptionCodes(); }
  };
  document.getElementById('rc-next').onclick = () => {
    if (redemptionPage + 1 < totalPages) { redemptionPage++; loadRedemptionCodes(); }
  };
}

function showGenerateCodesModal() {
  const overlay = document.createElement('div');
  overlay.className = 'modal-overlay';
  overlay.innerHTML = `
    <div class="modal">
      <h2>批量生成兑换码</h2>
      <div class="form-group"><label>数量(1-500)</label><input id="gc-count" type="number" min="1" max="500" step="1" value="10"></div>
      <div class="form-group"><label>单张面值(元)</label><input id="gc-amount" type="number" min="0" step="any" placeholder="如 10"></div>
      <div class="form-group"><label>批次名</label><input id="gc-batch" placeholder="如 2026-08 活动"></div>
      <div class="form-group"><label>过期时间(UTC,留空永不过期)</label><input id="gc-expires" type="date"></div>
      <div class="form-group"><label>备注(可选)</label><input id="gc-note" placeholder="仅管理员可见"></div>
      <div class="form-actions">
        <button class="btn-primary" id="gc-save">生成</button>
        <button class="btn-outline" id="gc-cancel">取消</button>
      </div>
    </div>`;
  document.body.appendChild(overlay);
  document.getElementById('gc-cancel').onclick = () => overlay.remove();
  overlay.onclick = (e) => { if (e.target === overlay) overlay.remove(); };

  document.getElementById('gc-save').onclick = async () => {
    const btn = document.getElementById('gc-save');
    btn.disabled = true;
    try {
      const expires = document.getElementById('gc-expires').value.trim();
      const body = {
        count: parseInt(document.getElementById('gc-count').value, 10) || 0,
        amount: parseFloat(document.getElementById('gc-amount').value) || 0,
        batch: document.getElementById('gc-batch').value.trim(),
        note: document.getElementById('gc-note').value.trim(),
      };
      if (expires) body.expires_at = expires;
      const r = await api('POST', '/api/admin/redemption-codes', body);
      if (r.ok) {
        overlay.remove();
        showGeneratedCodesResult(r.data);
        loadRedemptionCodes();
      } else {
        toast(r.data.message || r.data.error || '生成失败', 'error');
      }
    } finally {
      btn.disabled = false;
    }
  };
}

// 生成结果一次性展示:明文只存在这里,关闭后无法再次查看。
function showGeneratedCodesResult(data) {
  const overlay = document.createElement('div');
  overlay.className = 'modal-overlay';
  const text = (data.codes || []).join('\n');
  overlay.innerHTML = `
    <div class="modal" style="max-width:560px">
      <h2>已生成 ${data.count} 张兑换码(面值 ¥${fmtCost(data.amount)})</h2>
      <p style="color:var(--danger,#f43f5e);font-size:12.5px;margin:0 0 10px">明文仅本次可见,请立即复制保存;关闭后无法找回。</p>
      <textarea readonly style="width:100%;height:220px;font-family:monospace;font-size:12.5px;resize:vertical">${esc(text)}</textarea>
      <div class="form-actions">
        <button class="btn-primary" id="gr-copy">复制全部</button>
        <button class="btn-outline" id="gr-close">关闭</button>
      </div>
    </div>`;
  document.body.appendChild(overlay);
  document.getElementById('gr-copy').onclick = () => copyText(text);
  document.getElementById('gr-close').onclick = () => overlay.remove();
}

async function disableRedemptionCode(id) {
  const r = await api('POST', `/api/admin/redemption-codes/${id}/disable`);
  if (r.ok) {
    toast('已禁用');
    loadRedemptionCodes();
  } else {
    toast(r.data.message || r.data.error || '禁用失败', 'error');
  }
}
