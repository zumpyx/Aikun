// ============ State ============
const state = {
  token: localStorage.getItem('token') || null,
  user: null,
  view: null,
  apiKeys: [],
  users: [],
  prices: [],
  version: null,
};

// ============ Utils ============
const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
// 千分位数字:1234567 → 1,234,567
const fmtNum = n => Number(n ?? 0).toLocaleString('en-US');
// 金额:保留 6 位小数并去掉末尾的 0(每请求费用通常极小)
const fmtCost = n => {
  const v = Number(n ?? 0);
  if (!v) return '0';
  return v.toFixed(6).replace(/\.?0+$/, '');
};
// 时间显示:兼容 RFC3339 与 'YYYY-MM-DD HH:MM:SS'(后者 Safari 不认,
// 需把空格换成 'T' 再 parse),解析失败回退原字符串;输出本地化格式。
const fmtTime = v => {
  if (!v) return '';
  const s = String(v);
  const d = new Date(s.includes('T') ? s : s.replace(' ', 'T'));
  return Number.isNaN(d.getTime()) ? s : d.toLocaleString();
};
// Truncate a string to at most `max` UTF-8 bytes without cutting a
// multi-byte character in half (sidebar nickname is capped at 8 bytes).
const truncBytes = (s, max) => {
  let bytes = 0, out = '';
  for (const ch of String(s ?? '')) {
    const b = new TextEncoder().encode(ch).length;
    if (bytes + b > max) break;
    bytes += b; out += ch;
  }
  return out || '?';
};

function copyText(text) {
  const fallback = () => {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    try {
      if (document.execCommand('copy')) toast('已复制');
      else toast('复制失败，请手动复制', 'error');
    } catch {
      toast('复制失败，请手动复制', 'error');
    }
    ta.remove();
  };
  if (navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(text).then(() => toast('已复制'), fallback);
  } else fallback();
}

// ============ API ============
async function api(method, path, body) {
  const h = { 'Content-Type': 'application/json' };
  if (state.token) h['Authorization'] = `Bearer ${state.token}`;
  try {
    const res = await fetch(path, { method, headers: h, body: body ? JSON.stringify(body) : undefined, signal: AbortSignal.timeout(30000) });
    let data;
    try { data = await res.json(); }
    catch { data = {}; }
    if (data === null || typeof data !== 'object') data = {};
    if (!res.ok && !data.error && !data.message) data = { error: 'unknown_error' };
    if (res.status === 401 && path !== '/api/login') {
      state.token = null;
      state.user = null;
      localStorage.removeItem('token');
      document.querySelectorAll('.modal-overlay').forEach(m => m.remove());
      renderLogin(document.getElementById('app'));
    }
    return { ok: res.ok, status: res.status, data };
  } catch {
    return { ok: false, status: 0, data: { error: 'network_error' } };
  }
}

function toast(msg, type = 'success') {
  const c = document.querySelector('.toast-container') || (() => { const d = document.createElement('div'); d.className = 'toast-container'; document.body.appendChild(d); return d; })();
  const t = document.createElement('div'); t.className = `toast toast-${type}`; t.textContent = msg;
  c.appendChild(t);
  setTimeout(() => t.remove(), 3000);
}

// 版本号接口需要授权,登录态就绪后调用;结果存入 state.version 供侧栏展示
async function loadVersion() {
  const r = await api('GET', '/api/version');
  if (r.ok && r.data.version) state.version = r.data.version;
}

// ============ Icons ============
const I = (p, filled) => `<svg width="18" height="18" viewBox="0 0 24 24" fill="${filled ? 'currentColor' : 'none'}" stroke="${filled ? 'none' : 'currentColor'}" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">${p}</svg>`;
const icons = {
  bolt: I('<path d="M13 2 4.5 13.5H11L10 22l8.5-11.5H12L13 2z"/>', true),
  dashboard: I('<rect x="3" y="3" width="7.5" height="7.5" rx="1.8"/><rect x="13.5" y="3" width="7.5" height="7.5" rx="1.8"/><rect x="3" y="13.5" width="7.5" height="7.5" rx="1.8"/><rect x="13.5" y="13.5" width="7.5" height="7.5" rx="1.8"/>'),
  providers: I('<circle cx="5.5" cy="12" r="2.5"/><circle cx="18.5" cy="5.5" r="2.5"/><circle cx="18.5" cy="18.5" r="2.5"/><path d="M7.8 10.8l8.4-4.2M7.8 13.2l8.4 4.2"/>'),
  users: I('<circle cx="9" cy="8" r="3.5"/><path d="M2.5 20c.6-3.4 3.3-5.5 6.5-5.5s5.9 2.1 6.5 5.5"/><circle cx="17.5" cy="9" r="2.5"/><path d="M16 14.7c2.8.5 4.9 2.3 5.4 5.3"/>'),
  key: I('<circle cx="8" cy="15.5" r="4.5"/><path d="M11.3 12.3 20 3.6M16.5 7l3 3M13.5 10l2 2"/>'),
  models: I('<rect x="5" y="5" width="14" height="14" rx="2.5"/><rect x="9.5" y="9.5" width="5" height="5" rx="1"/><path d="M9 2.5V5M15 2.5V5M9 19v2.5M15 19v2.5M2.5 9H5M2.5 15H5M19 9h2.5M19 15h2.5"/>'),
  chat: I('<path d="M21 11.5a8.5 8.5 0 0 1-12.4 7.5L3 21l2-5.6A8.5 8.5 0 1 1 21 11.5z"/>'),
  logs: I('<path d="M9 6h12M9 12h12M9 18h12"/><path d="M4 6h.01M4 12h.01M4 18h.01" stroke-width="2.6"/>'),
  docs: I('<path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20V4a2 2 0 0 0-2-2H6.5A2.5 2.5 0 0 0 4 4.5v15z"/><path d="M4 19.5A2.5 2.5 0 0 0 6.5 22H20v-5"/>'),
  pulse: I('<path d="M2.5 12h4l2.5-6.5 4.5 13L16 12h5.5"/>'),
  billing: I('<circle cx="12" cy="12" r="9"/><path d="M12 7v10M15.5 9.5c-.7-1-2-1.5-3.5-1.5-1.9 0-3.5 1-3.5 2.5s1.3 2.2 3.5 2.5c2.2.3 3.5 1 3.5 2.5s-1.6 2.5-3.5 2.5c-1.5 0-2.8-.5-3.5-1.5"/>'),
};

// ============ Router ============
function navigate(view) {
  state.view = view;
  render();
}

// ============ Render ============
function render() {
  const app = document.getElementById('app');
  if (!state.token) { renderLogin(app); return; }
  renderApp(app);
}

function renderLogin(container) {
  container.innerHTML = `
    <div class="login-page">
      <div class="login-card">
        <div class="login-logo"><span class="logo-chip">${icons.bolt}</span></div>
        <h1>Aikun</h1>
        <p>统一管理你的 AI 服务渠道</p>
        <div class="form-group">
          <label>用户名</label>
          <input type="text" id="login-user" placeholder="用户名">
        </div>
        <div class="form-group">
          <label>密码</label>
          <input type="password" id="login-pass" placeholder="密码">
        </div>
        <button class="btn-primary" id="login-btn" style="width:100%">登 录</button>
        <p class="toggle-row" style="color:var(--muted);font-size:12.5px">账号由管理员创建，请联系管理员获取</p>
      </div>
    </div>`;

  document.getElementById('login-btn').onclick = async () => {
    const btn = document.getElementById('login-btn');
    const user = document.getElementById('login-user').value;
    const pass = document.getElementById('login-pass').value;
    btn.disabled = true; btn.textContent = '登录中…';
    try {
      const r = await api('POST', '/api/login', { username: user, password: pass });
      if (r.ok) {
        state.token = r.data.token;
        state.user = r.data.user;
        localStorage.setItem('token', state.token);
        await loadVersion();
        toast('登录成功');
        render();
      } else {
        toast(r.data.message || r.data.error || '登录失败', 'error');
      }
    } finally {
      btn.disabled = false; btn.textContent = '登 录';
    }
  };

  const bindEnter = (inputId, btnId) => {
    document.getElementById(inputId).onkeydown = (e) => { if (e.key === 'Enter') document.getElementById(btnId).click(); };
  };
  bindEnter('login-user', 'login-btn');
  bindEnter('login-pass', 'login-btn');
}

function renderApp(container) {
  const isAdmin = state.user && state.user.role === 'admin';
  const menu = [
    { id: 'dashboard', label: '总览', icon: icons.dashboard, admin: true },
    { id: 'providers', label: '渠道', icon: icons.providers, admin: true },
    { id: 'users', label: '用户', icon: icons.users, admin: true },
    { id: 'billing', label: '计费', icon: icons.billing, admin: true },
    { id: 'apikeys', label: '密钥', icon: icons.key, admin: false },
    { id: 'models', label: '模型', icon: icons.models, admin: false },
    { id: 'chat', label: '测试', icon: icons.chat, admin: false },
    { id: 'logs', label: '日志', icon: icons.logs, admin: false },
    { id: 'apidocs', label: '文档', icon: icons.docs, admin: false },
  ];

  const name = state.user?.display_name || state.user?.username || '?';
  const visibleMenu = menu.filter(m => !m.admin || isAdmin);
  if (!visibleMenu.some(m => m.id === state.view)) state.view = visibleMenu[0].id;
  container.innerHTML = `
    <div class="app">
      <div class="sidebar">
        <div class="logo"><span class="logo-chip">${icons.bolt}</span><span class="logo-text">Aikun</span></div>
        <nav>
          ${visibleMenu.map(m =>
            `<a href="#" data-view="${m.id}" class="${state.view === m.id ? 'active' : ''}">${m.icon}<span>${m.label}</span></a>`
          ).join('')}
        </nav>
        <div class="user-info">
          <div class="user-meta">
            <strong title="${esc(name)}">${esc(truncBytes(name, 8))}</strong>
          </div>
          <div class="user-balance" id="user-balance" style="font-size:12px;color:var(--muted)" title="账户余额">余额 ¥${fmtCost(state.user?.balance ?? 0)}</div>
          <a href="#" id="logout-link" title="退出登录">退出</a>
        </div>
        ${state.version ? `<div class="sidebar-version">v${esc(state.version)}</div>` : ''}
      </div>
      <div class="main" id="main-content"></div>
    </div>`;

  document.querySelectorAll('.sidebar nav a').forEach(a => {
    a.onclick = (e) => { e.preventDefault(); navigate(a.dataset.view); };
  });

  document.getElementById('logout-link').onclick = (e) => {
    e.preventDefault();
    state.token = null; state.user = null;
    localStorage.removeItem('token');
    render();
  };

  // 余额随每次页面切换后台刷新(扣费是实时的,渲染用的 state.user
  // 是登录/初始化时的快照)。
  refreshBalance();

  renderView(document.getElementById('main-content'));
}

async function refreshBalance() {
  const el = document.getElementById('user-balance');
  if (!el) return;
  const r = await api('GET', '/api/me');
  if (r.ok) {
    state.user = { ...state.user, ...r.data };
    el.textContent = `余额 ¥${fmtCost(r.data.balance ?? 0)}`;
  }
}

function renderView(container) {
  const isAdmin = state.user && state.user.role === 'admin';
  const fallback = isAdmin ? 'dashboard' : 'apikeys';
  const view = state.view || fallback;
  if (view === 'dashboard') renderDashboard(container);
  else if (view === 'providers') renderProviders(container);
  else if (view === 'users') renderUsers(container);
  else if (view === 'billing') renderBilling(container);
  else if (view === 'apikeys') renderApiKeys(container);
  else if (view === 'models') renderModels(container);
  else if (view === 'chat') renderChat(container);
  else if (view === 'logs') renderLogs(container);
  else if (view === 'apidocs') renderApiDocs(container);
  else if (isAdmin) renderDashboard(container);
  else renderApiKeys(container);
}


// ============ Action delegation ============
// Dynamically rendered list buttons carry data-action/data-id attributes
// instead of inline onclick — one delegated listener covers every re-render.
const listActions = {
  'edit-provider':      (id) => showProviderModal(id),
  'duplicate-provider': (id, btn) => duplicateProvider(id, btn),
  'test-provider':      (id, btn) => testProvider(id, btn),
  'toggle-provider':    (id, btn) => toggleProvider(id, btn.dataset.active !== 'true'),
  'delete-provider':    (id) => deleteProvider(id),
  'edit-user':          (id) => showUserModal(id),
  'toggle-user':        (id, btn) => toggleUser(id, btn.dataset.active === 'true'),
  'adjust-balance':     (id) => showAdjustBalanceModal(id),
  'user-keys':          (id) => showUserKeysModal(id),
  'toggle-user-key':    (id, btn) => toggleUserKey(id, btn.dataset.active === 'true', btn.dataset.user),
  'delete-user-key':    (id, btn) => deleteUserKey(id, btn.dataset.user),
  'edit-price':         (id) => showPriceModal(id),
  'delete-price':       (id) => deletePrice(id),
  'edit-api-key':       (id) => showApiKeyModal(id),
  'toggle-api-key':     (id, btn) => toggleApiKey(id, btn.dataset.active === 'true'),
  'delete-api-key':     (id) => deleteApiKey(id),
};
document.addEventListener('click', (e) => {
  const btn = e.target.closest('[data-action]');
  if (!btn) return;
  const fn = listActions[btn.dataset.action];
  if (fn) fn(btn.dataset.id, btn);
});


