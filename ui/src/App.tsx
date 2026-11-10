import { Navigate, Route, Routes } from 'react-router-dom'
import Layout from './components/Layout'
import ConnectPage from './pages/Connect'
import SettingsPage from './pages/Settings'
import DocumentsPage from './pages/Documents'
import DocumentDetailPage from './pages/DocumentDetail'
import JobsPage from './pages/Jobs'
import EnginesPage from './pages/Engines'
import PipelinesPage from './pages/Pipelines'
import { isConfigured } from './lib/settings'

function RequireAuth({ children }: { children: React.ReactNode }) {
  return isConfigured() ? <>{children}</> : <Navigate to="/connect" replace />
}

export default function App() {
  return (
    <Routes>
      <Route path="/connect" element={<ConnectPage />} />
      <Route
        element={
          <RequireAuth>
            <Layout />
          </RequireAuth>
        }
      >
        <Route path="/" element={<Navigate to="/documents" replace />} />
        <Route path="/documents" element={<DocumentsPage />} />
        <Route path="/documents/:id" element={<DocumentDetailPage />} />
        <Route path="/jobs" element={<JobsPage />} />
        <Route path="/engines" element={<EnginesPage />} />
        <Route path="/pipelines" element={<PipelinesPage />} />
        <Route path="/settings" element={<SettingsPage />} />
      </Route>
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  )
}
