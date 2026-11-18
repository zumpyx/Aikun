import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import path from 'node:path'

// 构建产物提交进仓库(frontend/dist),由 aikun-admin 的 build.rs 在
// cargo 构建时扫描生成 include_bytes! 资产表(与 aikun-openapi 生成物
// 同一纪律:构建零外部工具)。改前端 → npm run build → 提交 dist。
export default defineConfig({
  plugins: [react()],
  // 资产由后端 embed 后挂在 /static/* 服务,产物引用要用绝对前缀
  base: '/static/',
  resolve: {
    alias: { '@': path.resolve(__dirname, 'src') },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // 单 bundle + 单 css:embed 表最小,静态资产服务无需目录遍历
    rollupOptions: {
      output: {
        entryFileNames: 'assets/app.js',
        chunkFileNames: 'assets/[name].js',
        assetFileNames: 'assets/[name][extname]',
        manualChunks: undefined,
      },
    },
  },
  server: {
    proxy: {
      // 开发流程:vite dev + 后端二进制;API/静态回源到本地网关
      '/api': { target: 'http://127.0.0.1:3000', changeOrigin: false },
    },
  },
})
