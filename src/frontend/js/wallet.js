// ============ Wallet ============
// 余额与近 30 天消费分析(数据来自 /api/wallet,任意角色仅本人数据)。
// 防止快速切换页面时乱序响应写脏 DOM:只有最后一次 render 可以落地。
let walletSeq = 0;

// 钱包页展示最多保留两位小数;后台仍按整数微元精确计费。
const fmtWalletCost = n => {
  const v = Number(n ?? 0);
  if (!Number.isFinite(v) || Math.abs(v) < 0.005) return '0';
  return v.toFixed(2).replace(/\.?0+$/, '');
};

async function renderWallet(container) {
  const seq = ++walletSeq;
  container.innerHTML = `
    <div class="page-head"><h2>钱包</h2><p>余额与每日消费分析</p></div>
    <div class="stats-grid" id="wallet-grid"><div class="stat-card"><div class="spinner"></div><div class="label" style="margin-left:10px">加载中...</div></div></div>`;

  try {
    const r = await api('GET', '/api/wallet');
    if (seq !== walletSeq) return; // 已有更新的渲染在进行,丢弃旧响应
    if (!r.ok) {
      container.insertAdjacentHTML('beforeend',
        `<div class="card"><div class="empty"><p>加载失败: ${esc(r.data.message || r.data.error || r.status)}</p></div></div>`);
      return;
    }
    const w = r.data;

    const stat = (icon, color, bg, value, label) =>
      `<div class="stat-card"><div class="stat-icon" style="--si-fg:${color};--si-bg:${bg}">${icon}</div><div><div class="value">${value}</div><div class="label">${label}</div></div></div>`;

    const grid = document.getElementById('wallet-grid');
    if (!grid) return;
    grid.innerHTML =
      stat(icons.wallet, '#6366f1', 'rgba(99,102,241,.1)', `¥${fmtWalletCost(w.balance ?? 0)}`, '当前余额') +
      stat(icons.billing, '#f43f5e', 'rgba(244,63,94,.1)', `¥${fmtWalletCost(w.today.cost)}`, `今日消耗 (UTC) · ${fmtNum(w.today.requests)} 次`) +
      stat(icons.billing, '#f97316', 'rgba(249,115,22,.1)', `¥${fmtWalletCost(w.week.cost)}`, `近 7 天消耗 (UTC) · ${fmtNum(w.week.requests)} 次`) +
      stat(icons.billing, '#14b8a6', 'rgba(20,184,166,.1)', `¥${fmtWalletCost(w.month.cost)}`, `近 30 天消耗 (UTC) · ${fmtNum(w.month.requests)} 次 · ${fmtNum(w.month.tokens)} tokens`);

    // 兑换码充值:成功/失败都 toast 提示,成功后整页刷新钱包数据
    container.insertAdjacentHTML('beforeend', `
      <div class="card redeem-card">
        <div class="redeem-icon">${icons.gift}</div>
        <div class="redeem-copy">
          <h3>兑换码充值</h3>
          <p>输入管理员发放的兑换码,余额立即到账</p>
        </div>
        <div class="redeem-actions">
          <input id="redeem-code-input" placeholder="AK-XXXX-XXXX-XXXX-XXXX" autocomplete="off" spellcheck="false">
          <button class="btn-primary" id="redeem-code-btn">立即兑换</button>
        </div>
      </div>`);
    document.getElementById('redeem-code-btn').onclick = async () => {
      const btn = document.getElementById('redeem-code-btn');
      const code = document.getElementById('redeem-code-input').value.trim();
      if (!code) { toast('请输入兑换码', 'error'); return; }
      btn.disabled = true;
      try {
        const r = await api('POST', '/api/wallet/redeem', { code });
        if (r.ok) {
          toast(`充值成功 +¥${fmtWalletCost(r.data.amount)},当前余额 ¥${fmtWalletCost(r.data.balance)}`);
          renderWallet(container);
        } else {
          toast(r.data.message || r.data.error || '兑换失败', 'error');
        }
      } finally {
        btn.disabled = false;
      }
    };

    container.insertAdjacentHTML('beforeend', `
      <div class="usage-grid">
        <div class="card" style="margin-bottom:0">
          <div class="card-header"><h2>近 30 天消费趋势</h2><span style="font-size:12.5px;color:var(--muted)">每日消耗(元)</span></div>
          ${renderCostBarChart(w.daily || [])}
        </div>
        <div class="card" style="margin-bottom:0">
          <div class="card-header"><h2>模型消费分布</h2><span style="font-size:12.5px;color:var(--muted)">近 30 天</span></div>
          ${renderModelCostBars(w.top_models || [])}
        </div>
      </div>`);
  } catch (e) {
    if (seq !== walletSeq) return;
    container.insertAdjacentHTML('beforeend', '<div class="card"><div class="empty"><p>加载失败: ' + esc(e.message) + '</p></div></div>');
  }
}

// 纯 SVG 柱状图(与总览页同款手法,无第三方库)。悬停 <title> 显示
// 当日的消耗、Token 与请求数。
function renderCostBarChart(daily) {
  if (daily.length === 0) return '<div class="empty"><p>暂无消费数据</p></div>';
  const W = 920, H = 220, PT = 16, PB = 26, PL = 48, PR = 8;
  const iw = W - PL - PR, ih = H - PT - PB;
  const max = Math.max(0.000001, ...daily.map(d => d.cost));
  const bw = iw / daily.length;

  const grid = [0, 0.5, 1].map(f => {
    const y = (PT + ih * (1 - f)).toFixed(1);
    return `<line x1="${PL}" y1="${y}" x2="${W - PR}" y2="${y}" stroke="var(--border)" stroke-width="1"${f === 0 ? '' : ' stroke-dasharray="3 4"'}/>`
      + `<text x="${PL - 6}" y="${(+y + 3.5).toFixed(1)}" text-anchor="end" font-size="10" fill="var(--muted)">${fmtWalletCost(max * f)}</text>`;
  }).join('');

  const bars = daily.map((d, i) => {
    const h = (d.cost / max) * ih;
    const x = PL + i * bw;
    const y = PT + (ih - h);
    const tip = `${d.date}\n消耗 ¥${fmtWalletCost(d.cost)} · Token ${fmtNum(d.tokens)}\n请求 ${fmtNum(d.requests)} 次`;
    return `<rect x="${(x + 1).toFixed(1)}" y="${y.toFixed(1)}" width="${Math.max(1, bw - 3).toFixed(1)}" height="${Math.max(0, h).toFixed(1)}" rx="2.5" fill="url(#cost-bar-grad)"><title>${esc(tip)}</title></rect>`;
  }).join('');

  const xlabels = [0, Math.floor(daily.length / 2), daily.length - 1].map(i => {
    const d = daily[i];
    if (!d) return '';
    const x = (PL + i * bw + bw / 2).toFixed(1);
    return `<text x="${x}" y="${H - 8}" text-anchor="middle" font-size="10" fill="var(--muted)">${esc(d.date.slice(5))}</text>`;
  }).join('');

  return `<svg viewBox="0 0 ${W} ${H}" style="width:100%;display:block" role="img" aria-label="近 30 天消费趋势">
    <defs><linearGradient id="cost-bar-grad" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#f43f5e"/><stop offset="1" stop-color="#f97316"/>
    </linearGradient></defs>
    ${grid}${bars}${xlabels}
  </svg>`;
}

// 模型消费分布:横向条形,按费用占比
function renderModelCostBars(models) {
  if (models.length === 0) return '<div class="empty"><p>暂无消费数据</p></div>';
  const max = Math.max(0.000001, ...models.map(m => m.cost));
  return models.map(m => `
    <div style="margin-bottom:12px">
      <div style="display:flex;justify-content:space-between;gap:10px;font-size:12.5px;margin-bottom:4px">
        <code style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap">${esc(m.model)}</code>
        <span style="color:var(--muted);white-space:nowrap">¥${fmtWalletCost(m.cost)} · ${fmtNum(m.requests)} 次</span>
      </div>
      <div style="height:8px;background:var(--border);border-radius:99px;overflow:hidden">
        <div style="height:100%;width:${((m.cost / max) * 100).toFixed(1)}%;background:var(--grad);border-radius:99px"></div>
      </div>
    </div>`).join('');
}
