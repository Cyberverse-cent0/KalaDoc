import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { BrowserRouter } from 'react-router-dom'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import App from './App'
import { applyTheme, loadPrefs } from './lib/prefs'
import './index.css'

applyTheme(loadPrefs().theme)
if (loadPrefs().theme === 'system') {
  window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () =>
    applyTheme('system'),
  )
}

const queryClient = new QueryClient({
  defaultOptions: {
    queries: { retry: 1, refetchOnWindowFocus: false },
  },
})

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <BrowserRouter>
        <App />
      </BrowserRouter>
    </QueryClientProvider>
  </StrictMode>,
)
