import React from 'react'
import ReactDOM from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { HashRouter } from 'react-router-dom'
import { Toaster } from 'sonner'
import App from './App'
import { AuthProvider } from './lib/auth'
import './index.css'

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: 1, refetchOnWindowFocus: false } },
})

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <HashRouter>
        {/* AuthProvider 必须包住 App:App/LoginGate 首屏即 useAuth(),
            缺失时渲染即抛错卸载整树(白屏,曾在线上漏过——e2e 只验
            HTTP 层,渲染冒烟由浏览器验证) */}
        <AuthProvider>
          <App />
        </AuthProvider>
        <Toaster position="top-center" richColors closeButton />
      </HashRouter>
    </QueryClientProvider>
  </React.StrictMode>,
)
