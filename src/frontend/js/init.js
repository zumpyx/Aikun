// ============ Init ============
(async function init() {
  try {
    if (state.token) {
      const r = await api('GET', '/api/me');
      if (r.ok) state.user = r.data;
      else if (r.status === 401) { state.token = null; localStorage.removeItem('token'); }
      else toast('网络异常，无法获取用户信息，请稍后刷新重试', 'error');
    }
  } finally {
    render();
  }
})();

