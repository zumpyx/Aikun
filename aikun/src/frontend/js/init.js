// ============ Init ============
(async function init() {
  try {
    if (state.token) {
      // 0.5:GET /api/me → GET /api/users/me(资源化路径,响应形状不变)
      const r = await api('GET', '/api/users/me');
      if (r.ok) {
        state.user = r.data;
        await loadVersion();
      }
      else if (r.status === 401) { state.token = null; localStorage.removeItem('token'); }
      else toast('网络异常，无法获取用户信息，请稍后刷新重试', 'error');
    }
  } finally {
    render();
  }
})();

