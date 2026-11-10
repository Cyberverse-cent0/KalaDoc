import { useNavigate } from 'react-router-dom'
import ConnectForm from '../components/ConnectForm'

export default function ConnectPage() {
  const navigate = useNavigate()
  return (
    <div className="flex min-h-screen items-center justify-center bg-slate-950 px-4">
      <div className="w-full max-w-md rounded-xl border border-slate-800 bg-slate-900 p-6 shadow-xl">
        <h1 className="text-xl font-semibold">Connect to KalaDoc</h1>
        <div className="mt-5">
          <ConnectForm onSuccess={() => navigate('/documents')} />
        </div>
        <p className="mt-4 text-center text-xs text-slate-500">
          Have an account?{' '}
          <a href="/login" className="text-sky-400 hover:underline">
            Sign in or create one
          </a>
        </p>
      </div>
    </div>
  )
}
