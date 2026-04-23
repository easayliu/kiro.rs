import { useState, useEffect, useRef, useMemo, type ReactNode } from 'react'
import {
  RefreshCw,
  LogOut,
  Moon,
  Sun,
  Plus,
  Upload,
  FileUp,
  Trash2,
  RotateCcw,
  CheckCircle2,
  Database,
  AlertTriangle,
  ChevronLeft,
  ChevronRight,
  X,
  Search,
  MoreHorizontal,
  Settings2,
} from 'lucide-react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { storage } from '@/lib/storage'
import { CredentialCard } from '@/components/credential-card'
import { BalanceDialog } from '@/components/balance-dialog'
import { AddCredentialDialog } from '@/components/add-credential-dialog'
import { BatchImportDialog } from '@/components/batch-import-dialog'
import { KamImportDialog } from '@/components/kam-import-dialog'
import { BatchVerifyDialog, type VerifyResult } from '@/components/batch-verify-dialog'
import {
  useCredentials,
  useDeleteCredential,
  useResetFailure,
  useLoadBalancingMode,
  useSetLoadBalancingMode,
  useCacheScope,
  useSetCacheScope,
  useCacheSkipRate,
  useSetCacheSkipRate,
} from '@/hooks/use-credentials'
import type { CacheScope } from '@/api/credentials'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu'
import { Input } from '@/components/ui/input'
import { Button } from '@/components/ui/button'
import { getCredentialBalance, forceRefreshToken } from '@/api/credentials'
import { cn, extractErrorMessage } from '@/lib/utils'
import { RelativeTime } from '@/components/relative-time'
import type { BalanceResponse } from '@/types/api'

type FilterKey = 'all' | 'available' | 'faulty' | 'disabled'

interface DashboardProps {
  onLogout: () => void
}

export function Dashboard({ onLogout }: DashboardProps) {
  const [selectedCredentialId, setSelectedCredentialId] = useState<number | null>(null)
  const [balanceDialogOpen, setBalanceDialogOpen] = useState(false)
  const [addDialogOpen, setAddDialogOpen] = useState(false)
  const [batchImportDialogOpen, setBatchImportDialogOpen] = useState(false)
  const [kamImportDialogOpen, setKamImportDialogOpen] = useState(false)
  const [selectedIds, setSelectedIds] = useState<Set<number>>(new Set())
  const [verifyDialogOpen, setVerifyDialogOpen] = useState(false)
  const [verifying, setVerifying] = useState(false)
  const [verifyProgress, setVerifyProgress] = useState({ current: 0, total: 0 })
  const [verifyResults, setVerifyResults] = useState<Map<number, VerifyResult>>(new Map())
  const [balanceMap, setBalanceMap] = useState<Map<number, BalanceResponse>>(() => storage.loadBalanceCache())
  const [loadingBalanceIds, setLoadingBalanceIds] = useState<Set<number>>(new Set())
  const [queryingInfo, setQueryingInfo] = useState(false)
  const [queryInfoProgress, setQueryInfoProgress] = useState({ current: 0, total: 0 })
  const [batchRefreshing, setBatchRefreshing] = useState(false)
  const [batchRefreshProgress, setBatchRefreshProgress] = useState({ current: 0, total: 0 })
  const cancelVerifyRef = useRef(false)
  const [currentPage, setCurrentPage] = useState(1)
  const [search, setSearch] = useState('')
  const [filter, setFilter] = useState<FilterKey>('all')
  const [policiesOpen, setPoliciesOpen] = useState(false)
  const itemsPerPage = 12
  const [darkMode, setDarkMode] = useState(() => {
    if (typeof window === 'undefined') return true
    if (document.documentElement.classList.contains('dark')) return true
    const saved = localStorage.getItem('kiro-theme')
    if (saved === 'dark') { document.documentElement.classList.add('dark'); return true }
    if (saved === 'light') return false
    const prefers = window.matchMedia('(prefers-color-scheme: dark)').matches
    if (prefers) document.documentElement.classList.add('dark')
    return prefers
  })

  const queryClient = useQueryClient()
  const { data, isLoading, error, refetch } = useCredentials()
  const { mutate: deleteCredential } = useDeleteCredential()
  const { mutate: resetFailure } = useResetFailure()
  const { data: loadBalancingData, isLoading: isLoadingMode } = useLoadBalancingMode()
  const { mutate: setLoadBalancingMode, isPending: isSettingMode } = useSetLoadBalancingMode()
  const { data: cacheScopeData, isLoading: isLoadingCacheScope } = useCacheScope()
  const { mutate: setCacheScopeMutation, isPending: isSettingCacheScope } = useSetCacheScope()
  const { data: cacheSkipRateData, isLoading: isLoadingCacheSkipRate } = useCacheSkipRate()
  const { mutate: setCacheSkipRateMutation, isPending: isSettingCacheSkipRate } = useSetCacheSkipRate()
  const [cacheSkipRateDialogOpen, setCacheSkipRateDialogOpen] = useState(false)
  const [cacheSkipRateInput, setCacheSkipRateInput] = useState('')

  const allCreds = data?.credentials || []
  const totalCount = data?.total || 0
  const availableCount = data?.available || 0
  const disabledCredentialCount = allCreds.filter(c => c.disabled).length
  const faultyCredentialCount = allCreds.filter(c => !c.disabled && (c.failureCount > 0 || c.refreshFailureCount > 0)).length
  const activeCredential = data?.currentId ? allCreds.find(c => c.id === data.currentId) : undefined
  const activeBalance = data?.currentId ? balanceMap.get(data.currentId) : undefined

  const filteredCreds = useMemo(() => {
    const q = search.trim().toLowerCase()
    return allCreds.filter(c => {
      if (filter === 'available' && (c.disabled || c.failureCount > 0 || c.refreshFailureCount > 0)) return false
      if (filter === 'faulty' && !(c.failureCount > 0 || c.refreshFailureCount > 0)) return false
      if (filter === 'disabled' && !c.disabled) return false
      if (!q) return true
      return (
        (c.email || '').toLowerCase().includes(q) ||
        String(c.id).includes(q) ||
        (c.proxyUrl || '').toLowerCase().includes(q)
      )
    })
  }, [allCreds, filter, search])

  const totalPages = Math.max(1, Math.ceil(filteredCreds.length / itemsPerPage))
  const safePage = Math.min(currentPage, totalPages)
  const startIndex = (safePage - 1) * itemsPerPage
  const currentCredentials = filteredCreds.slice(startIndex, startIndex + itemsPerPage)

  const selectedDisabledCount = Array.from(selectedIds).filter(id => {
    const credential = allCreds.find(c => c.id === id)
    return Boolean(credential?.disabled)
  }).length

  useEffect(() => { setCurrentPage(1) }, [filter, search, allCreds.length])
  useEffect(() => { storage.saveBalanceCache(balanceMap) }, [balanceMap])
  useEffect(() => {
    if (!data?.credentials) {
      setBalanceMap(new Map()); setLoadingBalanceIds(new Set()); return
    }
    const validIds = new Set(data.credentials.map(c => c.id))
    setBalanceMap(prev => {
      const next = new Map<number, BalanceResponse>()
      prev.forEach((v, id) => validIds.has(id) && next.set(id, v))
      return next.size === prev.size ? prev : next
    })
    setLoadingBalanceIds(prev => {
      if (prev.size === 0) return prev
      const next = new Set<number>()
      prev.forEach(id => validIds.has(id) && next.add(id))
      return next.size === prev.size ? prev : next
    })
  }, [data?.credentials])

  const toggleDarkMode = () => {
    const next = !darkMode
    setDarkMode(next)
    document.documentElement.classList.toggle('dark', next)
    localStorage.setItem('kiro-theme', next ? 'dark' : 'light')
  }

  const handleViewBalance = (id: number) => {
    setSelectedCredentialId(id); setBalanceDialogOpen(true)
  }

  const handleRefresh = () => { refetch(); toast.success('已刷新凭据列表') }

  const handleLogout = () => {
    storage.removeApiKey(); storage.clearBalanceCache(); queryClient.clear(); onLogout()
  }

  const toggleSelect = (id: number) => {
    setSelectedIds(prev => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id); else next.add(id)
      return next
    })
  }

  const deselectAll = () => setSelectedIds(new Set())

  const toggleSelectAllOnPage = () => {
    const pageIds = currentCredentials.map(c => c.id)
    const allSelected = pageIds.every(id => selectedIds.has(id))
    setSelectedIds(prev => {
      const next = new Set(prev)
      if (allSelected) pageIds.forEach(id => next.delete(id))
      else pageIds.forEach(id => next.add(id))
      return next
    })
  }

  const handleBatchDelete = async () => {
    if (selectedIds.size === 0) { toast.error('请先选择要删除的凭据'); return }
    const disabledIds = Array.from(selectedIds).filter(id => {
      const c = allCreds.find(x => x.id === id)
      return Boolean(c?.disabled)
    })
    if (disabledIds.length === 0) { toast.error('选中的凭据中没有已禁用项'); return }
    const skippedCount = selectedIds.size - disabledIds.length
    const skippedText = skippedCount > 0 ? `（将跳过 ${skippedCount} 个未禁用凭据）` : ''
    if (!confirm(`确定要删除 ${disabledIds.length} 个已禁用凭据吗？此操作无法撤销。${skippedText}`)) return
    let successCount = 0, failCount = 0
    for (const id of disabledIds) {
      try {
        await new Promise<void>((resolve, reject) => {
          deleteCredential(id, {
            onSuccess: () => { successCount++; resolve() },
            onError: err => { failCount++; reject(err) },
          })
        })
      } catch {}
    }
    const tail = skippedCount > 0 ? `，已跳过 ${skippedCount} 个未禁用凭据` : ''
    if (failCount === 0) toast.success(`成功删除 ${successCount} 个已禁用凭据${tail}`)
    else toast.warning(`删除已禁用凭据：成功 ${successCount} 个，失败 ${failCount} 个${tail}`)
    deselectAll()
  }

  const handleBatchResetFailure = async () => {
    if (selectedIds.size === 0) { toast.error('请先选择要恢复的凭据'); return }
    const failedIds = Array.from(selectedIds).filter(id => {
      const c = allCreds.find(x => x.id === id)
      return c && c.failureCount > 0
    })
    if (failedIds.length === 0) { toast.error('选中的凭据中没有失败的凭据'); return }
    let successCount = 0, failCount = 0
    for (const id of failedIds) {
      try {
        await new Promise<void>((resolve, reject) => {
          resetFailure(id, {
            onSuccess: () => { successCount++; resolve() },
            onError: err => { failCount++; reject(err) },
          })
        })
      } catch {}
    }
    if (failCount === 0) toast.success(`成功恢复 ${successCount} 个凭据`)
    else toast.warning(`成功 ${successCount} 个，失败 ${failCount} 个`)
    deselectAll()
  }

  const handleBatchForceRefresh = async () => {
    if (selectedIds.size === 0) { toast.error('请先选择要刷新的凭据'); return }
    const enabledIds = Array.from(selectedIds).filter(id => {
      const c = allCreds.find(x => x.id === id)
      return c && !c.disabled
    })
    if (enabledIds.length === 0) { toast.error('选中的凭据中没有启用的凭据'); return }
    setBatchRefreshing(true)
    setBatchRefreshProgress({ current: 0, total: enabledIds.length })
    let successCount = 0, failCount = 0
    for (let i = 0; i < enabledIds.length; i++) {
      try { await forceRefreshToken(enabledIds[i]); successCount++ } catch { failCount++ }
      setBatchRefreshProgress({ current: i + 1, total: enabledIds.length })
    }
    setBatchRefreshing(false)
    queryClient.invalidateQueries({ queryKey: ['credentials'] })
    if (failCount === 0) toast.success(`成功刷新 ${successCount} 个凭据的 Token`)
    else toast.warning(`刷新 Token：成功 ${successCount} 个，失败 ${failCount} 个`)
    deselectAll()
  }

  const handleClearAll = async () => {
    if (!allCreds.length) { toast.error('没有可清除的凭据'); return }
    const disabledCredentials = allCreds.filter(c => c.disabled)
    if (disabledCredentials.length === 0) { toast.error('没有可清除的已禁用凭据'); return }
    if (!confirm(`确定要清除所有 ${disabledCredentials.length} 个已禁用凭据吗？此操作无法撤销。`)) return
    let successCount = 0, failCount = 0
    for (const credential of disabledCredentials) {
      try {
        await new Promise<void>((resolve, reject) => {
          deleteCredential(credential.id, {
            onSuccess: () => { successCount++; resolve() },
            onError: err => { failCount++; reject(err) },
          })
        })
      } catch {}
    }
    if (failCount === 0) toast.success(`成功清除所有 ${successCount} 个已禁用凭据`)
    else toast.warning(`清除已禁用凭据：成功 ${successCount} 个，失败 ${failCount} 个`)
    deselectAll()
  }

  const handleQueryCurrentPageInfo = async () => {
    if (currentCredentials.length === 0) { toast.error('当前页没有可查询的凭据'); return }
    const ids = currentCredentials.filter(c => !c.disabled).map(c => c.id)
    if (ids.length === 0) { toast.error('当前页没有可查询的启用凭据'); return }
    setQueryingInfo(true)
    setQueryInfoProgress({ current: 0, total: ids.length })
    let successCount = 0, failCount = 0
    for (let i = 0; i < ids.length; i++) {
      const id = ids[i]
      setLoadingBalanceIds(prev => { const next = new Set(prev); next.add(id); return next })
      try {
        const balance = await getCredentialBalance(id)
        successCount++
        setBalanceMap(prev => { const next = new Map(prev); next.set(id, balance); return next })
      } catch { failCount++ }
      finally {
        setLoadingBalanceIds(prev => { const next = new Set(prev); next.delete(id); return next })
      }
      setQueryInfoProgress({ current: i + 1, total: ids.length })
    }
    setQueryingInfo(false)
    if (failCount === 0) toast.success(`查询完成：成功 ${successCount}/${ids.length}`)
    else toast.warning(`查询完成：成功 ${successCount} 个，失败 ${failCount} 个`)
  }

  const handleBatchVerify = async () => {
    if (selectedIds.size === 0) { toast.error('请先选择要验活的凭据'); return }
    setVerifying(true)
    cancelVerifyRef.current = false
    const ids = Array.from(selectedIds)
    setVerifyProgress({ current: 0, total: ids.length })
    let successCount = 0
    const initialResults = new Map<number, VerifyResult>()
    ids.forEach(id => initialResults.set(id, { id, status: 'pending' }))
    setVerifyResults(initialResults)
    setVerifyDialogOpen(true)
    for (let i = 0; i < ids.length; i++) {
      if (cancelVerifyRef.current) { toast.info('已取消验活'); break }
      const id = ids[i]
      setVerifyResults(prev => { const next = new Map(prev); next.set(id, { id, status: 'verifying' }); return next })
      try {
        const balance = await getCredentialBalance(id)
        successCount++
        setVerifyResults(prev => {
          const next = new Map(prev)
          next.set(id, { id, status: 'success', usage: `${balance.currentUsage}/${balance.usageLimit}` })
          return next
        })
      } catch (err) {
        setVerifyResults(prev => {
          const next = new Map(prev)
          next.set(id, { id, status: 'failed', error: extractErrorMessage(err) })
          return next
        })
      }
      setVerifyProgress({ current: i + 1, total: ids.length })
      if (i < ids.length - 1 && !cancelVerifyRef.current) {
        await new Promise(r => setTimeout(r, 2000))
      }
    }
    setVerifying(false)
    if (!cancelVerifyRef.current) toast.success(`验活完成：成功 ${successCount}/${ids.length}`)
  }

  const handleCancelVerify = () => { cancelVerifyRef.current = true; setVerifying(false) }

  const handleOpenCacheSkipRateDialog = () => {
    const current = cacheSkipRateData?.rate
    setCacheSkipRateInput(current == null ? '' : String(current))
    setCacheSkipRateDialogOpen(true)
  }

  const handleSaveCacheSkipRate = () => {
    if (isSettingCacheSkipRate) return
    const trimmed = cacheSkipRateInput.trim()
    let rate: number | null
    if (trimmed === '') rate = null
    else {
      const parsed = Number(trimmed)
      if (!Number.isFinite(parsed) || parsed < 0 || parsed > 1) {
        toast.error('请输入 0.0 - 1.0 之间的数字（留空表示关闭）')
        return
      }
      rate = parsed
    }
    setCacheSkipRateMutation(rate, {
      onSuccess: () => {
        toast.success(rate == null ? '已关闭缓存跳过率' : `已设置跳过率为 ${(rate * 100).toFixed(0)}%`)
        setCacheSkipRateDialogOpen(false)
      },
      onError: err => toast.error(`设置失败: ${extractErrorMessage(err)}`),
    })
  }

  const handleCycleCacheScope = () => {
    const current = cacheScopeData?.scope ?? 'global'
    const next: CacheScope = current === 'global' ? 'per_credential' : 'global'
    setCacheScopeMutation(next, {
      onSuccess: () => toast.success(`缓存模式已切换到 ${next === 'per_credential' ? '凭据隔离' : '全局共享'}`),
      onError: err => toast.error(`切换失败: ${extractErrorMessage(err)}`),
    })
  }

  const handleToggleLoadBalancing = () => {
    const currentMode = loadBalancingData?.mode || 'priority'
    const newMode = currentMode === 'priority' ? 'balanced' : 'priority'
    setLoadBalancingMode(newMode, {
      onSuccess: () => toast.success(`已切换到${newMode === 'priority' ? '优先级模式' : '均衡负载模式'}`),
      onError: err => toast.error(`切换失败: ${extractErrorMessage(err)}`),
    })
  }

  if (isLoading) {
    return (
      <div className="flex min-h-screen items-center justify-center bg-background">
        <div className="text-center">
          <div className="label-eyebrow mb-3">loading</div>
          <div className="text-lg font-semibold tracking-tight">Kiro Admin</div>
        </div>
      </div>
    )
  }

  if (error) {
    return (
      <div className="flex min-h-screen items-center justify-center bg-background p-4">
        <div className="w-full max-w-md">
          <div className="mb-3 inline-flex items-center gap-2 font-mono text-2xs text-bad">
            <AlertTriangle className="h-3 w-3" /> connection failed
          </div>
          <div className="text-2xl font-semibold tracking-tight">连接失败</div>
          <p className="mt-2 font-mono text-xs text-muted-foreground">{(error as Error).message}</p>
          <div className="mt-5 flex gap-2">
            <button onClick={() => refetch()} className="inline-flex h-10 flex-1 cursor-pointer items-center justify-center gap-1.5 rounded-lg bg-primary px-3 text-sm font-medium text-primary-foreground transition-colors hover:bg-primary/90">
              <RefreshCw className="h-4 w-4" /> Retry
            </button>
            <button onClick={handleLogout} className="inline-flex h-10 cursor-pointer items-center justify-center gap-1.5 rounded-lg border border-border px-3 text-sm font-medium transition-colors hover:bg-muted">
              <LogOut className="h-4 w-4" /> Sign out
            </button>
          </div>
        </div>
      </div>
    )
  }

  return (
    <div className="relative min-h-screen bg-background text-foreground">
      {/* Top header — shared across mobile & desktop */}
      <header
        className="sticky top-0 z-30 border-b border-border bg-background/90 backdrop-blur-md"
        style={{ paddingTop: 'env(safe-area-inset-top)' }}
      >
        <div
          className="mx-auto flex h-12 max-w-[1280px] items-center justify-between px-4 sm:h-14 sm:px-8 lg:px-12"
          style={{
            paddingLeft: 'max(1rem, env(safe-area-inset-left))',
            paddingRight: 'max(1rem, env(safe-area-inset-right))',
          }}
        >
          <div className="flex items-center gap-2.5">
            <div className="flex h-8 w-8 items-center justify-center rounded-md bg-foreground text-background">
              <span className="font-mono text-sm font-bold">K</span>
            </div>
            <div className="leading-tight">
              <div className="text-sm font-semibold tracking-tight">Kiro</div>
              <div className="label-eyebrow hidden sm:block">Admin Console</div>
            </div>
          </div>
          <div className="flex items-center gap-0.5">
            <MobileIconBtn onClick={toggleDarkMode} label={darkMode ? '浅色模式' : '深色模式'}>
              {darkMode ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
            </MobileIconBtn>
            <MobileIconBtn onClick={handleRefresh} label="刷新"><RefreshCw className="h-4 w-4" /></MobileIconBtn>
            <MobileIconBtn onClick={handleLogout} label="退出登录"><LogOut className="h-4 w-4" /></MobileIconBtn>
          </div>
        </div>
      </header>

      {/* Main */}
      <main
        className="relative z-10 min-h-screen pb-20 lg:pb-10"
        style={{ paddingBottom: 'max(5rem, env(safe-area-inset-bottom))' }}
      >
        <div
          className="mx-auto max-w-[1280px] px-4 pt-4 sm:px-8 sm:pt-8 lg:px-12 lg:pt-10"
          style={{
            paddingLeft: 'max(1rem, env(safe-area-inset-left))',
            paddingRight: 'max(1rem, env(safe-area-inset-right))',
          }}
        >
          {/* ━━━━━━━━━━━━ HERO — compact ━━━━━━━━━━━━ */}
          <section className="mb-5 sm:mb-6">
            <h1 className="flex flex-wrap items-baseline gap-x-2.5 gap-y-1 text-balance tracking-tight">
              <span className="text-2xl font-semibold sm:text-3xl">凭据控制台</span>
              <span className="tnum text-base font-medium text-muted-foreground sm:text-lg">
                {availableCount}<span className="text-muted-foreground/60">/</span>{totalCount}
              </span>
            </h1>

            {/* Active credential — one quiet meta line */}
            {data?.currentId && (
              <p className="mt-1.5 flex min-w-0 items-center gap-1.5 truncate font-mono text-xs text-muted-foreground">
                <span className="shrink-0">当前</span>
                <span className="shrink-0 text-border">·</span>
                <span className="min-w-0 truncate text-foreground">
                  {activeCredential?.email || `#${String(data.currentId).padStart(3, '0')}`}
                </span>
                {activeBalance?.subscriptionTitle && (
                  <>
                    <span className="shrink-0 text-border">·</span>
                    <span className="shrink-0">{activeBalance.subscriptionTitle}</span>
                  </>
                )}
                <span className="shrink-0 text-border">·</span>
                <span className="shrink-0"><RelativeTime value={activeCredential?.lastUsedAt} /></span>
              </p>
            )}

            {/* Policies — compact inline status, click to adjust */}
            <button
              onClick={() => setPoliciesOpen(true)}
              className="mt-1.5 flex w-full cursor-pointer items-center gap-1.5 overflow-x-auto no-scrollbar text-left font-mono text-xs text-muted-foreground transition-colors hover:text-foreground"
              title="运行时策略 · 点击调整"
            >
              <span className="shrink-0">策略</span>
              <span className="shrink-0 text-border">·</span>
              <span className="shrink-0 text-foreground">
                {isLoadingCacheScope ? '—' : cacheScopeData?.scope === 'per_credential' ? '凭据隔离' : '全局共享'}
              </span>
              <span className="shrink-0 text-border">·</span>
              <span className="shrink-0 text-foreground">
                跳过{' '}
                {isLoadingCacheSkipRate ? '—' : cacheSkipRateData?.rate == null ? '关' : `${(cacheSkipRateData.rate * 100).toFixed(0)}%`}
              </span>
              <span className="shrink-0 text-border">·</span>
              <span className="shrink-0 text-foreground">
                {isLoadingMode ? '—' : loadBalancingData?.mode === 'balanced' ? 'LRU' : '优先级'}
              </span>
            </button>
          </section>

          {/* ━━━━━━━━━━━━ CONTENT ━━━━━━━━━━━━ */}
          <section>

            {/* Sticky toolbar — single row */}
            <div className="sticky top-12 z-20 -mx-4 mb-4 border-b border-border bg-background/92 px-4 py-2.5 backdrop-blur-md sm:-mx-8 sm:top-14 sm:px-8 lg:-mx-12 lg:px-12">
              <div className="flex items-center gap-1.5">
                <div className="relative min-w-0 flex-1">
                  <Search className="pointer-events-none absolute left-3 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
                  <input
                    type="search"
                    inputMode="search"
                    placeholder="搜索邮箱 / ID / 代理"
                    value={search}
                    onChange={e => setSearch(e.target.value)}
                    className="h-9 w-full rounded-lg border border-input bg-background pl-9 pr-9 text-sm transition-colors placeholder:text-muted-foreground/70 focus:border-primary focus:outline-none focus:ring-2 focus:ring-primary/20"
                  />
                  {search && (
                    <button
                      onClick={() => setSearch('')}
                      className="absolute right-1.5 top-1/2 flex h-6 w-6 -translate-y-1/2 cursor-pointer items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
                      aria-label="clear"
                    >
                      <X className="h-3.5 w-3.5" />
                    </button>
                  )}
                </div>

                {/* Primary inline actions — icon-only on mobile, text on sm+ */}
                <button
                  onClick={handleQueryCurrentPageInfo}
                  disabled={queryingInfo}
                  title={queryingInfo ? `查询中 ${queryInfoProgress.current}/${queryInfoProgress.total}` : '查询当前页信息'}
                  aria-label="查询当前页信息"
                  className="inline-flex h-9 shrink-0 cursor-pointer items-center justify-center gap-1.5 rounded-lg border border-border px-2.5 text-xs font-medium text-foreground transition-colors hover:bg-muted disabled:cursor-not-allowed disabled:opacity-40 sm:px-3"
                >
                  <RefreshCw className={cn('h-3.5 w-3.5', queryingInfo && 'animate-spin')} />
                  <span className="hidden sm:inline">
                    {queryingInfo ? `${queryInfoProgress.current}/${queryInfoProgress.total}` : '查询'}
                  </span>
                </button>
                <button
                  onClick={() => setKamImportDialogOpen(true)}
                  title="KAM 导入"
                  aria-label="KAM 导入"
                  className="inline-flex h-9 shrink-0 cursor-pointer items-center justify-center gap-1.5 rounded-lg border border-border px-2.5 text-xs font-medium text-foreground transition-colors hover:bg-muted sm:px-3"
                >
                  <FileUp className="h-3.5 w-3.5" />
                  <span className="hidden sm:inline">KAM</span>
                </button>

                <DropdownMenu>
                  <DropdownMenuTrigger asChild>
                    <button
                      aria-label="更多操作"
                      className="flex h-9 w-9 shrink-0 cursor-pointer items-center justify-center rounded-lg border border-border text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
                    >
                      <MoreHorizontal className="h-4 w-4" />
                    </button>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent align="end" className="w-52">
                    <DropdownMenuItem onClick={() => setAddDialogOpen(true)}>
                      <Plus /> 添加凭证
                    </DropdownMenuItem>
                    <DropdownMenuItem onClick={() => setBatchImportDialogOpen(true)}>
                      <Upload /> 批量导入
                    </DropdownMenuItem>
                    <DropdownMenuSeparator />
                    <DropdownMenuItem onClick={() => setPoliciesOpen(true)}>
                      <Settings2 /> 运行时策略
                    </DropdownMenuItem>
                    <DropdownMenuSeparator />
                    <DropdownMenuItem
                      onClick={handleClearAll}
                      disabled={disabledCredentialCount === 0}
                      className="text-bad focus:text-bad"
                    >
                      <Trash2 />
                      清除已禁用 ({disabledCredentialCount})
                    </DropdownMenuItem>
                    <DropdownMenuItem
                      onClick={handleLogout}
                      className="text-bad focus:text-bad lg:hidden"
                    >
                      <LogOut /> 退出登录
                    </DropdownMenuItem>
                  </DropdownMenuContent>
                </DropdownMenu>
              </div>

              {/* Chips row — filters + stat indicators */}
              <div className="mt-2 flex items-center gap-1 overflow-x-auto no-scrollbar">
                <Chip active={filter === 'all'} onClick={() => setFilter('all')} count={allCreds.length}>全部</Chip>
                <Chip
                  active={filter === 'available'}
                  onClick={() => setFilter('available')}
                  count={availableCount}
                  tone={availableCount > 0 ? 'ok' : 'default'}
                >
                  可用
                </Chip>
                <Chip
                  active={filter === 'faulty'}
                  onClick={() => setFilter('faulty')}
                  count={faultyCredentialCount}
                  tone={faultyCredentialCount > 0 ? 'warn' : 'default'}
                >
                  异常
                </Chip>
                <Chip
                  active={filter === 'disabled'}
                  onClick={() => setFilter('disabled')}
                  count={disabledCredentialCount}
                  tone={disabledCredentialCount > 0 ? 'bad' : 'default'}
                >
                  禁用
                </Chip>

                {verifying && !verifyDialogOpen && (
                  <button
                    onClick={() => setVerifyDialogOpen(true)}
                    className="ml-auto inline-flex shrink-0 cursor-pointer items-center gap-1 font-mono text-2xs text-primary hover:opacity-80"
                  >
                    <CheckCircle2 className="h-3 w-3 animate-spin" />
                    验活 {verifyProgress.current}/{verifyProgress.total}
                  </button>
                )}
              </div>
            </div>

            {/* Select-all strip (very subtle) */}
            {filteredCreds.length > 0 && (
              <div className="mb-3 flex items-center justify-between px-1">
                <button
                  onClick={toggleSelectAllOnPage}
                  className="inline-flex min-h-[28px] cursor-pointer items-center gap-1.5 rounded-md px-1.5 py-1 font-mono text-2xs text-muted-foreground transition-colors hover:text-foreground"
                >
                  <span
                    className={cn(
                      'flex h-4 w-4 shrink-0 items-center justify-center rounded border transition-colors',
                      currentCredentials.length > 0 && currentCredentials.every(c => selectedIds.has(c.id))
                        ? 'border-primary bg-primary text-primary-foreground'
                        : 'border-border bg-background',
                    )}
                  >
                    {currentCredentials.length > 0 && currentCredentials.every(c => selectedIds.has(c.id)) && (
                      <CheckCircle2 className="h-3 w-3" />
                    )}
                  </span>
                  {currentCredentials.length > 0 && currentCredentials.every(c => selectedIds.has(c.id)) ? '取消全选' : '全选本页'}
                </button>
                <span className="font-mono text-2xs text-muted-foreground">
                  页 <span className="tnum text-foreground">{safePage}/{totalPages}</span>
                </span>
              </div>
            )}

            {/* Content */}
            {filteredCreds.length === 0 ? (
              <div className="py-16 text-center">
                <div className="mx-auto flex h-11 w-11 items-center justify-center rounded-xl bg-muted text-muted-foreground">
                  <Database className="h-5 w-5" />
                </div>
                <div className="mt-4 text-sm font-semibold">
                  {search || filter !== 'all' ? '未找到匹配凭据' : '暂无凭据'}
                </div>
                <p className="mx-auto mt-1.5 max-w-xs text-xs text-muted-foreground">
                  {search || filter !== 'all' ? '试试调整搜索或筛选条件' : '从右上角菜单的 "添加凭证" 或 "批量导入" 开始'}
                </p>
                {(search || filter !== 'all') && (
                  <button
                    onClick={() => { setSearch(''); setFilter('all') }}
                    className="mt-4 inline-flex min-h-[36px] cursor-pointer items-center gap-1 rounded-lg border border-border px-3 py-1.5 text-xs font-medium transition-colors hover:bg-muted"
                  >
                    清除筛选
                  </button>
                )}
              </div>
            ) : (
              <>
                <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
                  {currentCredentials.map(credential => (
                    <CredentialCard
                      key={credential.id}
                      credential={credential}
                      onViewBalance={handleViewBalance}
                      selected={selectedIds.has(credential.id)}
                      onToggleSelect={() => toggleSelect(credential.id)}
                      balance={balanceMap.get(credential.id) || null}
                      loadingBalance={loadingBalanceIds.has(credential.id)}
                    />
                  ))}
                </div>

                {totalPages > 1 && (
                  <div className="mt-8 flex flex-wrap items-center justify-center gap-3">
                    <button
                      onClick={() => setCurrentPage(p => Math.max(1, p - 1))}
                      disabled={safePage === 1}
                      className="inline-flex h-9 min-w-[80px] cursor-pointer items-center justify-center gap-1 rounded-lg text-sm font-medium text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:cursor-not-allowed disabled:opacity-40"
                    >
                      <ChevronLeft className="h-4 w-4" /> 上一页
                    </button>
                    <span className="tnum font-mono text-xs text-muted-foreground">
                      <span className="font-medium text-foreground">{safePage}</span> / {totalPages}
                    </span>
                    <button
                      onClick={() => setCurrentPage(p => Math.min(totalPages, p + 1))}
                      disabled={safePage === totalPages}
                      className="inline-flex h-9 min-w-[80px] cursor-pointer items-center justify-center gap-1 rounded-lg text-sm font-medium text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:cursor-not-allowed disabled:opacity-40"
                    >
                      下一页 <ChevronRight className="h-4 w-4" />
                    </button>
                  </div>
                )}
              </>
            )}
          </section>
        </div>
      </main>

      {/* Mobile selection bar */}
      {selectedIds.size > 0 && (
        <div
          className="fixed inset-x-0 bottom-0 z-40 border-t border-border bg-background/95 shadow-pop backdrop-blur-xl animate-fade-up"
          style={{ paddingBottom: 'max(0.5rem, env(safe-area-inset-bottom))' }}
        >
          <div className="mx-auto flex max-w-[1280px] items-center gap-2 px-4 pt-2 sm:px-8 lg:px-12">
            <span className="inline-flex shrink-0 items-center gap-1.5 text-sm font-medium">
              <span className="tnum flex h-6 w-6 items-center justify-center rounded-full bg-foreground text-2xs font-bold text-background">
                {selectedIds.size}
              </span>
              已选
            </span>
            <button
              onClick={deselectAll}
              className="shrink-0 cursor-pointer text-2xs font-medium text-muted-foreground hover:text-foreground"
            >
              清除
            </button>
            <div className="ml-1 flex flex-1 items-center gap-1.5 overflow-x-auto no-scrollbar">
              <BarAction onClick={handleBatchVerify} icon={<CheckCircle2 className="h-3.5 w-3.5" />}>验活</BarAction>
              <BarAction
                onClick={handleBatchForceRefresh}
                disabled={batchRefreshing}
                icon={<RefreshCw className={cn('h-3.5 w-3.5', batchRefreshing && 'animate-spin')} />}
              >
                {batchRefreshing ? `${batchRefreshProgress.current}/${batchRefreshProgress.total}` : '刷 Token'}
              </BarAction>
              <BarAction onClick={handleBatchResetFailure} icon={<RotateCcw className="h-3.5 w-3.5" />}>恢复</BarAction>
              <BarAction
                onClick={handleBatchDelete}
                disabled={selectedDisabledCount === 0}
                tone="bad"
                icon={<Trash2 className="h-3.5 w-3.5" />}
                title={selectedDisabledCount === 0 ? '只能删除已禁用凭据' : undefined}
              >
                删除
              </BarAction>
            </div>
          </div>
        </div>
      )}

      {/* Dialogs */}
      <BalanceDialog credentialId={selectedCredentialId} open={balanceDialogOpen} onOpenChange={setBalanceDialogOpen} />
      <AddCredentialDialog open={addDialogOpen} onOpenChange={setAddDialogOpen} />
      <BatchImportDialog open={batchImportDialogOpen} onOpenChange={setBatchImportDialogOpen} />
      <KamImportDialog open={kamImportDialogOpen} onOpenChange={setKamImportDialogOpen} />
      <BatchVerifyDialog
        open={verifyDialogOpen}
        onOpenChange={setVerifyDialogOpen}
        verifying={verifying}
        progress={verifyProgress}
        results={verifyResults}
        onCancel={handleCancelVerify}
      />

      <Dialog open={policiesOpen} onOpenChange={setPoliciesOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>运行时策略</DialogTitle>
            <DialogDescription>调整缓存与负载均衡行为</DialogDescription>
          </DialogHeader>
          <div className="divide-y divide-border">
            <PolicyRow
              label="缓存分桶"
              sub={cacheScopeData?.scope === 'per_credential' ? '按用户 + 凭据双层' : '按用户身份共享'}
              value={cacheScopeData?.scope === 'per_credential' ? '凭据隔离' : '全局共享'}
              loading={isLoadingCacheScope}
              disabled={isLoadingCacheScope || isSettingCacheScope}
              onClick={handleCycleCacheScope}
            />
            <PolicyRow
              label="缓存跳过率"
              sub={cacheSkipRateData?.rate == null ? '按自然 breakpoint 计算' : '按概率跳过 cache 查找'}
              value={
                isLoadingCacheSkipRate ? '—'
                : cacheSkipRateData?.rate == null ? '关闭'
                : `${(cacheSkipRateData.rate * 100).toFixed(0)}%`
              }
              loading={isLoadingCacheSkipRate}
              disabled={isLoadingCacheSkipRate || isSettingCacheSkipRate}
              onClick={() => { setPoliciesOpen(false); handleOpenCacheSkipRateDialog() }}
            />
            <PolicyRow
              label="负载均衡"
              sub={loadBalancingData?.mode === 'balanced' ? 'LRU · 最久未使用优先' : '固定最高优先级'}
              value={loadBalancingData?.mode === 'balanced' ? 'LRU 均衡' : '优先级'}
              loading={isLoadingMode}
              disabled={isLoadingMode || isSettingMode}
              onClick={handleToggleLoadBalancing}
            />
          </div>
        </DialogContent>
      </Dialog>

      <Dialog open={cacheSkipRateDialogOpen} onOpenChange={setCacheSkipRateDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>缓存查找跳过率</DialogTitle>
            <DialogDescription>
              输入 0.0 – 1.0 之间的跳过概率。留空表示关闭。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2 py-2">
            <Input
              type="number"
              min={0}
              max={1}
              step={0.05}
              placeholder="例如 0.3；留空关闭"
              value={cacheSkipRateInput}
              onChange={e => setCacheSkipRateInput(e.target.value)}
              onKeyDown={e => { if (e.key === 'Enter') handleSaveCacheSkipRate() }}
              autoFocus
            />
            <p className="font-mono text-2xs uppercase tracking-wider text-muted-foreground">
              当前：{cacheSkipRateData?.rate == null ? '关闭' : `${(cacheSkipRateData.rate * 100).toFixed(0)}%`}
            </p>
          </div>
          <DialogFooter className="gap-2 sm:gap-2">
            <Button variant="outline" onClick={() => setCacheSkipRateInput('')} disabled={isSettingCacheSkipRate}>
              清空
            </Button>
            <Button onClick={handleSaveCacheSkipRate} disabled={isSettingCacheSkipRate}>
              {isSettingCacheSkipRate ? '保存中…' : '保存'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  )
}

// ─── Primitives ───

function MobileIconBtn({
  children, onClick, label, title,
}: { children: ReactNode; onClick?: () => void; label: string; title?: string }) {
  return (
    <button
      onClick={onClick}
      aria-label={label}
      title={title || label}
      className="flex h-10 w-10 cursor-pointer items-center justify-center rounded-lg text-muted-foreground transition-colors hover:bg-muted hover:text-foreground active:bg-muted/80"
    >
      {children}
    </button>
  )
}

function Chip({ children, active, onClick, count, tone = 'default' }: {
  children: ReactNode
  active: boolean
  onClick: () => void
  count: number
  tone?: 'default' | 'ok' | 'warn' | 'bad'
}) {
  const countDotColor =
    !active && tone === 'ok'
      ? 'text-ok'
      : !active && tone === 'warn'
        ? 'text-warn'
        : !active && tone === 'bad'
          ? 'text-bad'
          : ''
  return (
    <button
      onClick={onClick}
      className={cn(
        'inline-flex min-h-[30px] shrink-0 cursor-pointer items-center gap-1.5 rounded-full px-3 text-xs font-medium transition-colors',
        active
          ? 'bg-foreground text-background'
          : 'text-muted-foreground hover:bg-muted hover:text-foreground',
      )}
    >
      {children}
      <span
        className={cn(
          'tnum font-mono text-2xs',
          active ? 'opacity-70' : countDotColor || 'text-muted-foreground/60',
        )}
      >
        {count}
      </span>
    </button>
  )
}

function BarAction({ children, onClick, disabled, tone = 'default', icon, title }: {
  children: ReactNode
  onClick?: () => void
  disabled?: boolean
  tone?: 'default' | 'bad'
  icon?: ReactNode
  title?: string
}) {
  return (
    <button
      onClick={onClick}
      disabled={disabled}
      title={title}
      className={cn(
        'inline-flex h-9 shrink-0 cursor-pointer items-center gap-1 whitespace-nowrap rounded-lg px-3 text-xs font-semibold transition-all disabled:cursor-not-allowed disabled:opacity-40',
        tone === 'bad'
          ? 'bg-bad text-bad-foreground hover:brightness-110'
          : 'border border-border bg-background text-foreground hover:bg-muted',
      )}
    >
      {icon}
      {children}
    </button>
  )
}

function PolicyRow({ label, sub, value, loading, disabled, onClick }: {
  label: string
  sub: string
  value: string
  loading?: boolean
  disabled?: boolean
  onClick: () => void
}) {
  return (
    <button
      onClick={onClick}
      disabled={disabled}
      className="group flex w-full cursor-pointer items-center justify-between gap-3 py-3 text-left transition-colors disabled:cursor-not-allowed disabled:opacity-60"
    >
      <div className="min-w-0">
        <div className="text-sm font-medium">{label}</div>
        <div className="mt-0.5 truncate text-xs text-muted-foreground">{sub}</div>
      </div>
      <span className="shrink-0 inline-flex items-center gap-1.5 rounded-lg bg-muted px-2.5 py-1 text-xs font-semibold transition-colors group-hover:bg-foreground group-hover:text-background">
        {loading ? '…' : value}
      </span>
    </button>
  )
}
