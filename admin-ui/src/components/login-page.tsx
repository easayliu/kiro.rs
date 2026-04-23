import { useState, useEffect } from 'react'
import { ArrowRight, Lock } from 'lucide-react'
import { storage } from '@/lib/storage'

interface LoginPageProps {
  onLogin: (apiKey: string) => void
}

export function LoginPage({ onLogin }: LoginPageProps) {
  const [apiKey, setApiKey] = useState('')
  const [show, setShow] = useState(false)

  useEffect(() => {
    const saved = storage.getApiKey()
    if (saved) setApiKey(saved)
  }, [])

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()
    if (apiKey.trim()) {
      storage.setApiKey(apiKey.trim())
      onLogin(apiKey.trim())
    }
  }

  return (
    <div
      className="flex min-h-screen items-center justify-center bg-background px-5 text-foreground"
      style={{
        paddingTop: 'env(safe-area-inset-top)',
        paddingBottom: 'env(safe-area-inset-bottom)',
      }}
    >
      <div className="w-full max-w-sm">
        {/* Brand */}
        <div className="mb-10 flex items-center gap-2.5">
          <div className="flex h-9 w-9 items-center justify-center rounded-md bg-foreground text-background">
            <span className="font-mono text-sm font-bold">K</span>
          </div>
          <div>
            <div className="text-sm font-semibold leading-none tracking-tight">Kiro</div>
            <div className="label-eyebrow mt-1">Admin Console</div>
          </div>
        </div>

        {/* Title */}
        <h1 className="text-2xl font-semibold tracking-tight sm:text-3xl">登录</h1>
        <p className="mt-1.5 text-sm text-muted-foreground">
          输入管理员 API Key 以进入控制台
        </p>

        {/* Form */}
        <form onSubmit={handleSubmit} className="mt-8 space-y-3">
          <div>
            <label htmlFor="apikey" className="mb-2 flex items-center justify-between text-sm font-medium">
              <span>API Key</span>
              <button
                type="button"
                onClick={() => setShow(s => !s)}
                className="cursor-pointer text-xs font-normal text-muted-foreground hover:text-foreground"
              >
                {show ? '隐藏' : '显示'}
              </button>
            </label>
            <div className="group relative">
              <Lock className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
              <input
                id="apikey"
                type={show ? 'text' : 'password'}
                placeholder="sk-admin-•••••••••••••••"
                value={apiKey}
                onChange={e => setApiKey(e.target.value)}
                className="h-11 w-full rounded-lg border border-input bg-background px-4 py-2 pl-9 font-mono text-sm tracking-wide transition-colors placeholder:text-muted-foreground/50 focus:border-primary focus:outline-none focus:ring-2 focus:ring-primary/15"
                autoFocus
              />
            </div>
          </div>

          <button
            type="submit"
            disabled={!apiKey.trim()}
            className="group inline-flex h-11 w-full cursor-pointer items-center justify-center gap-1.5 rounded-lg bg-foreground text-sm font-medium text-background transition-opacity hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-40"
          >
            <span>进入控制台</span>
            <ArrowRight className="h-4 w-4 transition-transform duration-200 ease-out group-hover:translate-x-0.5" />
          </button>
        </form>

        {/* Footer */}
        <div className="mt-10 border-t border-border pt-4 text-center">
          <p className="font-mono text-2xs uppercase tracking-wider text-muted-foreground">
            Kiro · v2026.3
          </p>
        </div>
      </div>
    </div>
  )
}
