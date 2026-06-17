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
  ChevronDown,
  X,
  Search,
  MoreHorizontal,
  Settings2,
  Network,
  ArrowUpDown,
  Gauge,
  Activity,
  CircleDollarSign,
  Power,
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
import { ProxyGroupsDialog } from '@/components/proxy-groups-dialog'
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
  useUsageMultiplier,
  useSetUsageMultiplier,
  useIsReadOnly,
  useBatchSetPriority,
  useBatchSetRpmLimit,
  useBatchSetConcurrencyLimit,
  useBatchSetDisabled,
  useBatchSetOverage,
  useDefaultRpmLimit,
  useSetDefaultRpmLimit,
  useDefaultConcurrencyLimit,
  useSetDefaultConcurrencyLimit,
  useBillingStats,
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
import type { BalanceResponse, CredentialStatusItem } from '@/types/api'

type FilterKey = 'all' | 'available' | 'faulty' | 'throttled' | 'disabled'

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
  const [batchPriorityDialogOpen, setBatchPriorityDialogOpen] = useState(false)
  const [batchPriorityValue, setBatchPriorityValue] = useState('0')
  const [batchRpmDialogOpen, setBatchRpmDialogOpen] = useState(false)
  const [batchRpmValue, setBatchRpmValue] = useState('')
  const [defaultRpmDialogOpen, setDefaultRpmDialogOpen] = useState(false)
  const [defaultRpmValue, setDefaultRpmValue] = useState('')
  const [batchConcurrencyDialogOpen, setBatchConcurrencyDialogOpen] = useState(false)
  const [batchConcurrencyValue, setBatchConcurrencyValue] = useState('')
  const [defaultConcurrencyDialogOpen, setDefaultConcurrencyDialogOpen] = useState(false)
  const [defaultConcurrencyValue, setDefaultConcurrencyValue] = useState('')
  const [batchDisabledDialogOpen, setBatchDisabledDialogOpen] = useState(false)
  const [batchOverageDialogOpen, setBatchOverageDialogOpen] = useState(false)
  const [proxyGroupsOpen, setProxyGroupsOpen] = useState(false)
  const PAGE_SIZE_OPTIONS = [12, 24, 48, 96] as const
  const [itemsPerPage, setItemsPerPage] = useState<number>(() => {
    if (typeof window === 'undefined') return 12
    const saved = Number(localStorage.getItem('kiro-page-size'))
    return PAGE_SIZE_OPTIONS.includes(saved as (typeof PAGE_SIZE_OPTIONS)[number]) ? saved : 12
  })
  const handleChangePageSize = (size: number) => {
    setItemsPerPage(size)
    setCurrentPage(1)
    localStorage.setItem('kiro-page-size', String(size))
  }

  type SortKey =
    | 'default'
    | 'plan-desc'
    | 'plan-asc'
    | 'group'
    | 'last-used-desc'
    | 'last-used-asc'
    | 'added-desc'
    | 'added-asc'
    | 'usage-desc'
    | 'usage-asc'
  const SORT_OPTIONS: { key: SortKey; label: string }[] = [
    { key: 'default', label: '默认（优先级）' },
    { key: 'added-desc', label: '添加顺序 · 新→旧' },
    { key: 'added-asc', label: '添加顺序 · 旧→新' },
    { key: 'usage-desc', label: '已用量 · 多→少' },
    { key: 'usage-asc', label: '已用量 · 少→多' },
    { key: 'plan-desc', label: '订阅等级 · 高→低' },
    { key: 'plan-asc', label: '订阅等级 · 低→高' },
    { key: 'last-used-desc', label: '最近使用 · 最新→最旧' },
    { key: 'last-used-asc', label: '最近使用 · 最旧→最新' },
    { key: 'group', label: '代理分组（聚合）' },
  ]
  const [sortKey, setSortKey] = useState<SortKey>(() => {
    if (typeof window === 'undefined') return 'default'
    const saved = localStorage.getItem('kiro-sort') as SortKey | null
    return saved && SORT_OPTIONS.some(o => o.key === saved) ? saved : 'default'
  })
  const handleChangeSort = (key: SortKey) => {
    setSortKey(key)
    setCurrentPage(1)
    localStorage.setItem('kiro-sort', key)
  }
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
  const readOnly = useIsReadOnly()
  const { data, isLoading, error, refetch } = useCredentials()
  const { mutate: deleteCredential } = useDeleteCredential()
  const { mutate: resetFailure } = useResetFailure()
  const batchSetPriorityMutation = useBatchSetPriority()
  const batchSetRpmLimitMutation = useBatchSetRpmLimit()
  const batchSetConcurrencyLimitMutation = useBatchSetConcurrencyLimit()
  const batchSetDisabledMutation = useBatchSetDisabled()
  const batchSetOverageMutation = useBatchSetOverage()
  const { data: defaultRpmData } = useDefaultRpmLimit()
  const setDefaultRpmMutation = useSetDefaultRpmLimit()
  const { data: defaultConcurrencyData } = useDefaultConcurrencyLimit()
  const setDefaultConcurrencyMutation = useSetDefaultConcurrencyLimit()
  const { data: billingStats } = useBillingStats()
  const { data: loadBalancingData, isLoading: isLoadingMode } = useLoadBalancingMode()
  const { mutate: setLoadBalancingMode, isPending: isSettingMode } = useSetLoadBalancingMode()
  const { data: cacheScopeData, isLoading: isLoadingCacheScope } = useCacheScope()
  const { mutate: setCacheScopeMutation, isPending: isSettingCacheScope } = useSetCacheScope()
  const { data: cacheSkipRateData, isLoading: isLoadingCacheSkipRate } = useCacheSkipRate()
  const { mutate: setCacheSkipRateMutation, isPending: isSettingCacheSkipRate } = useSetCacheSkipRate()
  const [cacheSkipRateDialogOpen, setCacheSkipRateDialogOpen] = useState(false)
  const [cacheSkipRateInput, setCacheSkipRateInput] = useState('')
  const { data: usageMultiplierData, isLoading: isLoadingUsageMultiplier } = useUsageMultiplier()
  const { mutate: setUsageMultiplierMutation, isPending: isSettingUsageMultiplier } = useSetUsageMultiplier()
  const [usageMultiplierDialogOpen, setUsageMultiplierDialogOpen] = useState(false)
  const [usageMultiplierInput, setUsageMultiplierInput] = useState('')

  const allCreds = data?.credentials || []
  const totalCount = data?.total || 0
  const isThrottledNow = (c: CredentialStatusItem) =>
    !!c.throttledUntil && Date.parse(c.throttledUntil) > Date.now()
  const throttledCredentialCount = allCreds.filter(c => !c.disabled && isThrottledNow(c)).length
  // server-side "available" 仅排除 disabled；冷却中的凭据虽未真正可用，再从前端二次扣减
  const availableCount = Math.max(0, (data?.available || 0) - throttledCredentialCount)
  const disabledCredentialCount = allCreds.filter(c => c.disabled).length
  const faultyCredentialCount = allCreds.filter(c => !c.disabled && !isThrottledNow(c) && (c.failureCount > 0 || c.refreshFailureCount > 0)).length
  const activeCredential = data?.currentId ? allCreds.find(c => c.id === data.currentId) : undefined
  const activeBalance = data?.currentId ? balanceMap.get(data.currentId) : undefined

  const filteredCreds = useMemo(() => {
    const q = search.trim().toLowerCase()
    const filtered = allCreds.filter(c => {
      if (filter === 'available' && (c.disabled || isThrottledNow(c) || c.failureCount > 0 || c.refreshFailureCount > 0)) return false
      if (filter === 'faulty' && (c.disabled || isThrottledNow(c) || !(c.failureCount > 0 || c.refreshFailureCount > 0))) return false
      if (filter === 'throttled' && (c.disabled || !isThrottledNow(c))) return false
      if (filter === 'disabled' && !c.disabled) return false
      if (!q) return true
      return (
        (c.email || '').toLowerCase().includes(q) ||
        String(c.id).includes(q) ||
        (c.proxyUrl || '').toLowerCase().includes(q) ||
        (c.group || '').toLowerCase().includes(q)
      )
    })

    if (sortKey === 'default') return filtered

    if (sortKey === 'group') {
      // 按代理分组聚合：先按 group 名升序（无分组的排到末尾），
      // 组内沿用 priority asc + id asc 的稳定次级序
      return [...filtered].sort((a, b) => {
        const ga = a.group || ''
        const gb = b.group || ''
        if (ga !== gb) {
          if (!ga) return 1
          if (!gb) return -1
          return ga.localeCompare(gb)
        }
        if (a.priority !== b.priority) return a.priority - b.priority
        return a.id - b.id
      })
    }

    if (sortKey === 'added-desc' || sortKey === 'added-asc') {
      // id 在添加凭据时自增分配（max+1），故 id 序即添加顺序：
      // 升序 = 旧→新，降序 = 新→旧。
      const desc = sortKey === 'added-desc'
      return [...filtered].sort((a, b) => (desc ? b.id - a.id : a.id - b.id))
    }

    if (sortKey === 'usage-desc' || sortKey === 'usage-asc') {
      // 按已用 credits 绝对值（currentUsage）排序。用量数据仅对查询过余额的
      // 凭证可用，未查询的（balanceMap 无记录）无论升降序都排到末尾。
      const desc = sortKey === 'usage-desc'
      return [...filtered].sort((a, b) => {
        const ua = balanceMap.get(a.id)?.currentUsage
        const ub = balanceMap.get(b.id)?.currentUsage
        if (ua == null && ub == null) {
          if (a.priority !== b.priority) return a.priority - b.priority
          return a.id - b.id
        }
        if (ua == null) return 1
        if (ub == null) return -1
        if (ua !== ub) return desc ? ub - ua : ua - ub
        if (a.priority !== b.priority) return a.priority - b.priority
        return a.id - b.id
      })
    }

    if (sortKey === 'last-used-desc' || sortKey === 'last-used-asc') {
      // lastUsedAt 是 RFC3339 字符串，字典序与时间序一致。null（从未使用）
      // 无论升降序都排到末尾，避免与"很久没用"的真实活动混淆。
      const desc = sortKey === 'last-used-desc'
      return [...filtered].sort((a, b) => {
        const la = a.lastUsedAt
        const lb = b.lastUsedAt
        if (la == null && lb == null) {
          if (a.priority !== b.priority) return a.priority - b.priority
          return a.id - b.id
        }
        if (la == null) return 1
        if (lb == null) return -1
        const cmp = la < lb ? -1 : la > lb ? 1 : 0
        if (cmp !== 0) return desc ? -cmp : cmp
        if (a.priority !== b.priority) return a.priority - b.priority
        return a.id - b.id
      })
    }

    const tierRank = (title: string | null | undefined): number => {
      if (!title) return 0
      const t = title.toUpperCase()
      if (t.includes('POWER')) return 3
      if (t.includes('PRO')) return 2
      if (t.includes('FREE')) return 1
      return 0
    }
    const direction = sortKey === 'plan-desc' ? -1 : 1
    return [...filtered].sort((a, b) => {
      const rankDelta =
        (tierRank(balanceMap.get(a.id)?.subscriptionTitle) -
          tierRank(balanceMap.get(b.id)?.subscriptionTitle)) * direction
      if (rankDelta !== 0) return rankDelta
      // Stable secondary order: priority asc, then id asc
      if (a.priority !== b.priority) return a.priority - b.priority
      return a.id - b.id
    })
  }, [allCreds, filter, search, sortKey, balanceMap])

  const totalPages = Math.max(1, Math.ceil(filteredCreds.length / itemsPerPage))
  const safePage = Math.min(currentPage, totalPages)
  const startIndex = (safePage - 1) * itemsPerPage
  const currentCredentials = filteredCreds.slice(startIndex, startIndex + itemsPerPage)

  const selectedDisabledCount = Array.from(selectedIds).filter(id => {
    const credential = allCreds.find(c => c.id === id)
    return Boolean(credential?.disabled)
  }).length
  const selectedEnabledCount = selectedIds.size - selectedDisabledCount

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

  const openBatchPriorityDialog = () => {
    if (selectedIds.size === 0) { toast.error('请先选择要调整的凭据'); return }
    setBatchPriorityValue('0')
    setBatchPriorityDialogOpen(true)
  }

  const handleBatchPrioritySubmit = () => {
    const trimmed = batchPriorityValue.trim()
    const parsed = Number(trimmed)
    if (trimmed === '' || !Number.isFinite(parsed) || parsed < 0 || !Number.isInteger(parsed)) {
      toast.error('请输入 ≥ 0 的整数')
      return
    }
    const ids = Array.from(selectedIds)
    batchSetPriorityMutation.mutate(
      { credentialIds: ids, priority: parsed },
      {
        onSuccess: res => {
          const failed = res.failed.length
          if (failed === 0) toast.success(`已将 ${res.succeeded.length} 个凭据优先级设为 ${parsed}`)
          else toast.warning(`成功 ${res.succeeded.length} 个，失败 ${failed} 个`)
          setBatchPriorityDialogOpen(false)
          deselectAll()
        },
        onError: err => toast.error('批量调整失败: ' + (err as Error).message),
      },
    )
  }

  const runBatchSetDisabled = (disabled: boolean, ids: number[]) => {
    if (ids.length === 0) {
      toast.info(disabled ? '选中的凭据均已禁用' : '选中的凭据均已启用')
      return
    }
    batchSetDisabledMutation.mutate(
      { credentialIds: ids, disabled },
      {
        onSuccess: res => {
          const failed = res.failed.length
          const verb = disabled ? '禁用' : '启用'
          if (failed === 0) toast.success(`成功${verb} ${res.succeeded.length} 个凭据`)
          else toast.warning(`${verb}：成功 ${res.succeeded.length} 个，失败 ${failed} 个`)
          setBatchDisabledDialogOpen(false)
          deselectAll()
        },
        onError: err => toast.error('操作失败: ' + (err as Error).message),
      },
    )
  }

  const handleBatchToggleDisabled = () => {
    if (selectedIds.size === 0) { toast.error('请先选择要切换的凭据'); return }
    const selected = allCreds.filter(c => selectedIds.has(c.id))
    const enabledIds = selected.filter(c => !c.disabled).map(c => c.id)
    const disabledIds = selected.filter(c => c.disabled).map(c => c.id)
    // 统一态：直接执行反向操作
    if (enabledIds.length === 0) {
      runBatchSetDisabled(false, disabledIds)
    } else if (disabledIds.length === 0) {
      runBatchSetDisabled(true, enabledIds)
    } else {
      // 混合态：弹窗让用户选方向
      setBatchDisabledDialogOpen(true)
    }
  }

  const openBatchRpmDialog = () => {
    if (selectedIds.size === 0) { toast.error('请先选择要调整的凭据'); return }
    setBatchRpmValue('')
    setBatchRpmDialogOpen(true)
  }

  const handleBatchRpmSubmit = () => {
    const trimmed = batchRpmValue.trim()
    let payload: number | null
    if (trimmed === '') {
      payload = null
    } else {
      const parsed = Number(trimmed)
      if (!Number.isFinite(parsed) || parsed < 0 || !Number.isInteger(parsed)) {
        toast.error('请输入 ≥ 0 的整数（0 表示不限流，留空表示回退全局默认）')
        return
      }
      payload = parsed
    }
    const ids = Array.from(selectedIds)
    batchSetRpmLimitMutation.mutate(
      { credentialIds: ids, rpmLimit: payload },
      {
        onSuccess: res => {
          const failed = res.failed.length
          const label = payload === null
            ? '回退全局默认'
            : payload === 0
              ? '显式不限流'
              : `${payload} 次/分钟`
          if (failed === 0) toast.success(`已将 ${res.succeeded.length} 个凭据 RPM 设为${label}`)
          else toast.warning(`成功 ${res.succeeded.length} 个，失败 ${failed} 个`)
          setBatchRpmDialogOpen(false)
          deselectAll()
        },
        onError: err => toast.error('批量调整失败: ' + (err as Error).message),
      },
    )
  }

  const openBatchConcurrencyDialog = () => {
    if (selectedIds.size === 0) { toast.error('请先选择要调整的凭据'); return }
    setBatchConcurrencyValue('')
    setBatchConcurrencyDialogOpen(true)
  }

  const handleBatchConcurrencySubmit = () => {
    const trimmed = batchConcurrencyValue.trim()
    let payload: number | null
    if (trimmed === '') {
      payload = null
    } else {
      const parsed = Number(trimmed)
      if (!Number.isFinite(parsed) || parsed < 0 || !Number.isInteger(parsed)) {
        toast.error('请输入 ≥ 0 的整数（0 表示不限并发，留空表示回退全局默认）')
        return
      }
      payload = parsed
    }
    const ids = Array.from(selectedIds)
    batchSetConcurrencyLimitMutation.mutate(
      { credentialIds: ids, concurrencyLimit: payload },
      {
        onSuccess: res => {
          const failed = res.failed.length
          const label = payload === null
            ? '回退全局默认'
            : payload === 0
              ? '显式不限并发'
              : `${payload} 个在途`
          if (failed === 0) toast.success(`已将 ${res.succeeded.length} 个凭据并发上限设为${label}`)
          else toast.warning(`成功 ${res.succeeded.length} 个，失败 ${failed} 个`)
          setBatchConcurrencyDialogOpen(false)
          deselectAll()
        },
        onError: err => toast.error('批量调整失败: ' + (err as Error).message),
      },
    )
  }

  const openBatchOverageDialog = () => {
    if (selectedIds.size === 0) { toast.error('请先选择要调整的凭据'); return }
    setBatchOverageDialogOpen(true)
  }

  // 批量切换 overage：后端顺序排队逐个下发，单次请求返回成功/失败汇总。
  const runBatchOverage = (enabled: boolean) => {
    const ids = Array.from(selectedIds)
    batchSetOverageMutation.mutate(
      { credentialIds: ids, enabled },
      {
        onSuccess: res => {
          const label = enabled ? '开启' : '关闭'
          if (res.failed.length === 0) toast.success(`已为 ${res.succeeded.length} 个凭据${label}超额`)
          else toast.warning(`${label}超额：成功 ${res.succeeded.length} 个，失败 ${res.failed.length} 个`)
          setBatchOverageDialogOpen(false)
          deselectAll()
        },
        onError: err => toast.error('批量切换 overage 失败: ' + (err as Error).message),
      },
    )
  }

  const openDefaultRpmDialog = () => {
    setDefaultRpmValue(
      typeof defaultRpmData?.rpmLimit === 'number' ? String(defaultRpmData.rpmLimit) : '',
    )
    setDefaultRpmDialogOpen(true)
  }

  const handleDefaultRpmSubmit = () => {
    const trimmed = defaultRpmValue.trim()
    let payload: number | null
    if (trimmed === '') {
      payload = null
    } else {
      const parsed = Number(trimmed)
      if (!Number.isFinite(parsed) || parsed < 0 || !Number.isInteger(parsed)) {
        toast.error('请输入 ≥ 0 的整数（0 表示不限流，留空表示清除）')
        return
      }
      payload = parsed
    }
    setDefaultRpmMutation.mutate(payload, {
      onSuccess: () => {
        toast.success(
          payload === null
            ? '已清除全局默认 RPM'
            : payload === 0
              ? '全局默认已设为不限流'
              : `全局默认 RPM 已设为 ${payload} 次/分钟`,
        )
        setDefaultRpmDialogOpen(false)
      },
      onError: err => toast.error('保存失败: ' + (err as Error).message),
    })
  }

  const openDefaultConcurrencyDialog = () => {
    setDefaultConcurrencyValue(
      typeof defaultConcurrencyData?.concurrencyLimit === 'number' ? String(defaultConcurrencyData.concurrencyLimit) : '',
    )
    setDefaultConcurrencyDialogOpen(true)
  }

  const handleDefaultConcurrencySubmit = () => {
    const trimmed = defaultConcurrencyValue.trim()
    let payload: number | null
    if (trimmed === '') {
      payload = null
    } else {
      const parsed = Number(trimmed)
      if (!Number.isFinite(parsed) || parsed < 0 || !Number.isInteger(parsed)) {
        toast.error('请输入 ≥ 0 的整数（0 表示不限并发，留空表示清除）')
        return
      }
      payload = parsed
    }
    setDefaultConcurrencyMutation.mutate(payload, {
      onSuccess: () => {
        toast.success(
          payload === null
            ? '已清除全局默认并发上限'
            : payload === 0
              ? '全局默认已设为不限并发'
              : `全局默认并发上限已设为 ${payload} 个在途`,
        )
        setDefaultConcurrencyDialogOpen(false)
      },
      onError: err => toast.error('保存失败: ' + (err as Error).message),
    })
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

  const handleOpenUsageMultiplierDialog = () => {
    const current = usageMultiplierData?.multiplier
    setUsageMultiplierInput(current == null || current === 1 ? '' : String(current))
    setUsageMultiplierDialogOpen(true)
  }

  const handleSaveUsageMultiplier = () => {
    if (isSettingUsageMultiplier) return
    const trimmed = usageMultiplierInput.trim()
    let multiplier: number | null
    if (trimmed === '') multiplier = null
    else {
      const parsed = Number(trimmed)
      if (!Number.isFinite(parsed) || parsed <= 0) {
        toast.error('请输入大于 0 的倍率（留空表示 1.0 不放大）')
        return
      }
      multiplier = parsed
    }
    setUsageMultiplierMutation(multiplier, {
      onSuccess: () => {
        toast.success(multiplier == null || multiplier === 1 ? '已恢复倍率 1.0' : `已设置倍率为 ${multiplier}×`)
        setUsageMultiplierDialogOpen(false)
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
          className="mx-auto flex h-12 max-w-[1440px] items-center justify-between px-4 sm:h-14 sm:px-8 lg:px-12"
          style={{
            paddingLeft: 'max(1rem, env(safe-area-inset-left))',
            paddingRight: 'max(1rem, env(safe-area-inset-right))',
          }}
        >
          <div className="flex items-center gap-2.5">
            <div className="flex h-7 w-7 items-center justify-center rounded-md bg-foreground text-background">
              <span className="font-mono text-xs font-bold">K</span>
            </div>
            <div className="flex items-baseline gap-1.5">
              <span className="text-sm font-semibold tracking-tight">Kiro</span>
              <span className="hidden text-xs text-muted-foreground sm:inline">Admin</span>
              {readOnly && (
                <span
                  title="当前以游客身份登录，仅可只读浏览"
                  className="ml-1 inline-flex items-center rounded-full border border-warn/40 bg-warn-soft px-1.5 py-0.5 font-mono text-2xs font-semibold uppercase tracking-wider text-warn"
                >
                  Guest
                </span>
              )}
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
          className="mx-auto max-w-[1440px] px-4 pt-4 sm:px-8 sm:pt-8 lg:px-12 lg:pt-10"
          style={{
            paddingLeft: 'max(1rem, env(safe-area-inset-left))',
            paddingRight: 'max(1rem, env(safe-area-inset-right))',
          }}
        >
          {/* ━━━━━━━━━━━━ HERO — single inline row, flex-wrap ━━━━━━━━━━━━ */}
          <section className="mb-5 sm:mb-6">
            <div className="flex flex-wrap items-baseline gap-x-5 gap-y-2">
              {/* Title + ratio */}
              <h1 className="flex items-baseline gap-2 text-balance tracking-tight">
                <span className="text-2xl font-semibold sm:text-3xl">凭据控制台</span>
                <span className="tnum text-base font-medium sm:text-lg">
                  <span className={cn(availableCount > 0 ? 'text-foreground' : 'text-muted-foreground')}>{availableCount}</span>
                  <span className="text-muted-foreground/50">/</span>
                  <span className="text-muted-foreground">{totalCount}</span>
                </span>
              </h1>

              {/* Active credential */}
              {data?.currentId && (
                <p className="flex min-w-0 items-center gap-1.5 font-mono text-xs text-muted-foreground">
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

              {/* Policies — pushed right on wide screens */}
              <button
                onClick={() => !readOnly && setPoliciesOpen(true)}
                disabled={readOnly}
                className="flex shrink-0 items-center gap-1.5 font-mono text-xs text-muted-foreground transition-colors hover:text-foreground disabled:cursor-default disabled:hover:text-muted-foreground sm:ml-auto cursor-pointer disabled:cursor-default"
                title={readOnly ? '游客身份仅可查看策略' : '运行时策略 · 点击调整'}
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
            </div>
          </section>

          {/* ━━━━━━━━━━━━ BILLING SUMMARY ━━━━━━━━━━━━ */}
          {billingStats && (
            <section className="mb-5 sm:mb-6">
              <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
                <BillingStatCard
                  label="累计请求"
                  value={billingStats.requests.toLocaleString('en-US')}
                />
                <BillingStatCard
                  label="实际成本"
                  value={formatUsd(billingStats.actual_cost_usd)}
                  hint="上游折扣后真实成本"
                />
                <BillingStatCard
                  label="官方折算"
                  value={formatUsd(billingStats.official_price_usd)}
                  hint="Anthropic 零售价"
                />
                <BillingStatCard
                  label="累计毛利"
                  value={formatUsd(billingStats.margin_usd)}
                  hint={
                    billingStats.official_price_usd > 0
                      ? `毛利率 ${((billingStats.margin_usd / billingStats.official_price_usd) * 100).toFixed(1)}%`
                      : undefined
                  }
                  emphasis={billingStats.margin_usd >= 0 ? 'good' : 'bad'}
                />
              </div>
            </section>
          )}

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
                    placeholder="搜索邮箱 / ID / 代理 / 分组"
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
                {!readOnly && (
                  <button
                    onClick={() => setKamImportDialogOpen(true)}
                    title="KAM 导入"
                    aria-label="KAM 导入"
                    className="inline-flex h-9 shrink-0 cursor-pointer items-center justify-center gap-1.5 rounded-lg border border-border px-2.5 text-xs font-medium text-foreground transition-colors hover:bg-muted sm:px-3"
                  >
                    <FileUp className="h-3.5 w-3.5" />
                    <span className="hidden sm:inline">KAM</span>
                  </button>
                )}

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
                    {!readOnly && (
                      <>
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
                        <DropdownMenuItem onClick={() => setProxyGroupsOpen(true)}>
                          <Network /> 代理分组
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
                      </>
                    )}
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
                  active={filter === 'throttled'}
                  onClick={() => setFilter('throttled')}
                  count={throttledCredentialCount}
                  tone={throttledCredentialCount > 0 ? 'warn' : 'default'}
                >
                  限流冷却
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
                <div className="flex items-center gap-3 font-mono text-2xs text-muted-foreground">
                  {/* Sort */}
                  <DropdownMenu>
                    <DropdownMenuTrigger asChild>
                      <button
                        className="inline-flex cursor-pointer items-center gap-1 rounded-md px-1.5 py-1 transition-colors hover:text-foreground"
                        title="排序方式"
                      >
                        排序{' '}
                        <span className="text-foreground">
                          {sortKey === 'default'
                            ? '默认'
                            : sortKey === 'plan-desc'
                              ? '等级 ↓'
                              : sortKey === 'plan-asc'
                                ? '等级 ↑'
                                : sortKey === 'last-used-desc'
                                  ? '最近 ↓'
                                  : sortKey === 'last-used-asc'
                                    ? '最近 ↑'
                                    : '分组'}
                        </span>
                        <ChevronDown className="h-3 w-3" />
                      </button>
                    </DropdownMenuTrigger>
                    <DropdownMenuContent align="end" className="w-44">
                      {SORT_OPTIONS.map(opt => (
                        <DropdownMenuItem
                          key={opt.key}
                          onClick={() => handleChangeSort(opt.key)}
                          className={cn(opt.key === sortKey && 'bg-muted font-semibold')}
                        >
                          {opt.label}
                        </DropdownMenuItem>
                      ))}
                    </DropdownMenuContent>
                  </DropdownMenu>

                  {/* Page size */}
                  <DropdownMenu>
                    <DropdownMenuTrigger asChild>
                      <button
                        className="inline-flex cursor-pointer items-center gap-1 rounded-md px-1.5 py-1 transition-colors hover:text-foreground"
                        title="每页显示数量"
                      >
                        每页 <span className="tnum text-foreground">{itemsPerPage}</span>
                        <ChevronDown className="h-3 w-3" />
                      </button>
                    </DropdownMenuTrigger>
                    <DropdownMenuContent align="end" className="w-28">
                      {PAGE_SIZE_OPTIONS.map(n => (
                        <DropdownMenuItem
                          key={n}
                          onClick={() => handleChangePageSize(n)}
                          className={cn(
                            'tnum font-mono',
                            n === itemsPerPage && 'bg-muted font-semibold',
                          )}
                        >
                          {n} 条/页
                        </DropdownMenuItem>
                      ))}
                    </DropdownMenuContent>
                  </DropdownMenu>

                  <span>
                    页 <span className="tnum text-foreground">{safePage}/{totalPages}</span>
                  </span>
                </div>
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
                <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3 2xl:grid-cols-4">
                  {currentCredentials.map(credential => (
                    <CredentialCard
                      key={credential.id}
                      credential={credential}
                      defaultRpmLimit={data?.defaultRpmLimit}
                      defaultConcurrencyLimit={data?.defaultConcurrencyLimit}
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
          <div className="mx-auto flex max-w-[1440px] items-center gap-2 px-4 pt-2 sm:px-8 lg:px-12">
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
              {!readOnly && (
                <>
                  <BarAction
                    onClick={handleBatchForceRefresh}
                    disabled={batchRefreshing}
                    icon={<RefreshCw className={cn('h-3.5 w-3.5', batchRefreshing && 'animate-spin')} />}
                  >
                    {batchRefreshing ? `${batchRefreshProgress.current}/${batchRefreshProgress.total}` : '刷 Token'}
                  </BarAction>
                  <BarAction onClick={handleBatchResetFailure} icon={<RotateCcw className="h-3.5 w-3.5" />}>恢复</BarAction>
                  <BarAction
                    onClick={handleBatchToggleDisabled}
                    disabled={batchSetDisabledMutation.isPending}
                    icon={<Power className="h-3.5 w-3.5" />}
                    title={
                      selectedEnabledCount > 0 && selectedDisabledCount > 0
                        ? `${selectedEnabledCount} 个启用 / ${selectedDisabledCount} 个禁用`
                        : undefined
                    }
                  >
                    {selectedEnabledCount === 0
                      ? '启用'
                      : selectedDisabledCount === 0
                        ? '禁用'
                        : '启用/禁用'}
                  </BarAction>
                  <BarAction
                    onClick={openBatchPriorityDialog}
                    disabled={batchSetPriorityMutation.isPending}
                    icon={<ArrowUpDown className="h-3.5 w-3.5" />}
                  >
                    优先级
                  </BarAction>
                  <BarAction
                    onClick={openBatchRpmDialog}
                    disabled={batchSetRpmLimitMutation.isPending}
                    icon={<Gauge className="h-3.5 w-3.5" />}
                  >
                    RPM
                  </BarAction>
                  <BarAction
                    onClick={openBatchConcurrencyDialog}
                    disabled={batchSetConcurrencyLimitMutation.isPending}
                    icon={<Activity className="h-3.5 w-3.5" />}
                  >
                    并发
                  </BarAction>
                  <BarAction
                    onClick={openBatchOverageDialog}
                    disabled={batchSetOverageMutation.isPending}
                    icon={<CircleDollarSign className="h-3.5 w-3.5" />}
                  >
                    超额
                  </BarAction>
                  <BarAction
                    onClick={handleBatchDelete}
                    disabled={selectedDisabledCount === 0}
                    tone="bad"
                    icon={<Trash2 className="h-3.5 w-3.5" />}
                    title={selectedDisabledCount === 0 ? '只能删除已禁用凭据' : undefined}
                  >
                    删除
                  </BarAction>
                </>
              )}
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
      <ProxyGroupsDialog open={proxyGroupsOpen} onOpenChange={setProxyGroupsOpen} />

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
              label="计费倍率"
              sub={usageMultiplierData?.multiplier == null || usageMultiplierData.multiplier === 1 ? '按上游真实 token 上报' : '放大上报 token 抬高费用'}
              value={
                isLoadingUsageMultiplier ? '—'
                : usageMultiplierData?.multiplier == null || usageMultiplierData.multiplier === 1 ? '1.0×'
                : `${usageMultiplierData.multiplier}×`
              }
              loading={isLoadingUsageMultiplier}
              disabled={isLoadingUsageMultiplier || isSettingUsageMultiplier}
              onClick={() => { setPoliciesOpen(false); handleOpenUsageMultiplierDialog() }}
            />
            <PolicyRow
              label="负载均衡"
              sub={loadBalancingData?.mode === 'balanced' ? 'LRU · 最久未使用优先' : '固定最高优先级'}
              value={loadBalancingData?.mode === 'balanced' ? 'LRU 均衡' : '优先级'}
              loading={isLoadingMode}
              disabled={isLoadingMode || isSettingMode}
              onClick={handleToggleLoadBalancing}
            />
            <PolicyRow
              label="全局 RPM 默认"
              sub={
                defaultRpmData?.rpmLimit == null
                  ? '未配置（不限流）'
                  : defaultRpmData.rpmLimit === 0
                    ? '显式不限流'
                    : '凭据未单独配置时回退此值'
              }
              value={
                defaultRpmData?.rpmLimit == null
                  ? '关闭'
                  : defaultRpmData.rpmLimit === 0
                    ? '不限'
                    : `${defaultRpmData.rpmLimit}/min`
              }
              disabled={setDefaultRpmMutation.isPending}
              onClick={() => { setPoliciesOpen(false); openDefaultRpmDialog() }}
            />
            <PolicyRow
              label="全局并发默认"
              sub={
                defaultConcurrencyData?.concurrencyLimit == null
                  ? '未配置（不限并发）'
                  : defaultConcurrencyData.concurrencyLimit === 0
                    ? '显式不限并发'
                    : '凭据未单独配置时回退此值'
              }
              value={
                defaultConcurrencyData?.concurrencyLimit == null
                  ? '关闭'
                  : defaultConcurrencyData.concurrencyLimit === 0
                    ? '不限'
                    : `${defaultConcurrencyData.concurrencyLimit} 在途`
              }
              disabled={setDefaultConcurrencyMutation.isPending}
              onClick={() => { setPoliciesOpen(false); openDefaultConcurrencyDialog() }}
            />
          </div>
        </DialogContent>
      </Dialog>

      <Dialog open={batchPriorityDialogOpen} onOpenChange={setBatchPriorityDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>批量调整优先级</DialogTitle>
            <DialogDescription>
              将选中的 <span className="font-mono tnum font-semibold">{selectedIds.size}</span> 个凭据统一设置为以下优先级（数字越小优先级越高）。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2 py-2">
            <Input
              type="number"
              min={0}
              step={1}
              placeholder="例如 0"
              value={batchPriorityValue}
              onChange={e => setBatchPriorityValue(e.target.value)}
              onKeyDown={e => { if (e.key === 'Enter') handleBatchPrioritySubmit() }}
              autoFocus
              className="tnum font-mono"
            />
            <p className="text-2xs text-muted-foreground">
              整数 ≥ 0。该值生效后会按新优先级重新选择当前活跃凭据。
            </p>
          </div>
          <DialogFooter className="gap-2 sm:gap-2">
            <Button
              variant="outline"
              onClick={() => setBatchPriorityDialogOpen(false)}
              disabled={batchSetPriorityMutation.isPending}
            >
              取消
            </Button>
            <Button onClick={handleBatchPrioritySubmit} disabled={batchSetPriorityMutation.isPending}>
              {batchSetPriorityMutation.isPending ? '保存中…' : '保存'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog open={batchOverageDialogOpen} onOpenChange={setBatchOverageDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>批量切换超额计费</DialogTitle>
            <DialogDescription>
              将对选中的 <span className="font-mono tnum font-semibold text-foreground">{selectedIds.size}</span> 个凭据下发 overage 开关。
              逐个排队向上游提交，请选择方向；处理期间请勿关闭页面。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter className="gap-2 sm:gap-2">
            <Button
              variant="outline"
              onClick={() => setBatchOverageDialogOpen(false)}
              disabled={batchSetOverageMutation.isPending}
            >
              取消
            </Button>
            <Button
              variant="outline"
              onClick={() => runBatchOverage(false)}
              disabled={batchSetOverageMutation.isPending}
            >
              {batchSetOverageMutation.isPending ? '处理中…' : '全部关闭'}
            </Button>
            <Button
              onClick={() => runBatchOverage(true)}
              disabled={batchSetOverageMutation.isPending}
            >
              {batchSetOverageMutation.isPending ? '处理中…' : '全部开启'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog open={batchDisabledDialogOpen} onOpenChange={setBatchDisabledDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>批量启用/禁用</DialogTitle>
            <DialogDescription>
              选中的凭据状态不一致：
              <span className="font-mono tnum font-semibold text-foreground"> {selectedEnabledCount}</span> 个已启用、
              <span className="font-mono tnum font-semibold text-foreground"> {selectedDisabledCount}</span> 个已禁用。请选择批量操作方向。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter className="gap-2 sm:gap-2">
            <Button
              variant="outline"
              onClick={() => setBatchDisabledDialogOpen(false)}
              disabled={batchSetDisabledMutation.isPending}
            >
              取消
            </Button>
            <Button
              variant="outline"
              onClick={() => {
                const ids = allCreds
                  .filter(c => selectedIds.has(c.id) && c.disabled)
                  .map(c => c.id)
                runBatchSetDisabled(false, ids)
              }}
              disabled={batchSetDisabledMutation.isPending || selectedDisabledCount === 0}
            >
              全部启用（{selectedDisabledCount}）
            </Button>
            <Button
              onClick={() => {
                const ids = allCreds
                  .filter(c => selectedIds.has(c.id) && !c.disabled)
                  .map(c => c.id)
                runBatchSetDisabled(true, ids)
              }}
              disabled={batchSetDisabledMutation.isPending || selectedEnabledCount === 0}
            >
              全部禁用（{selectedEnabledCount}）
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog open={batchRpmDialogOpen} onOpenChange={setBatchRpmDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>批量调整 RPM 上限</DialogTitle>
            <DialogDescription>
              将选中的 <span className="font-mono tnum font-semibold">{selectedIds.size}</span> 个凭据统一设置每分钟请求上限。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2 py-2">
            <div className="flex items-center gap-2">
              <Input
                type="number"
                min={0}
                step={1}
                placeholder={
                  typeof defaultRpmData?.rpmLimit === 'number'
                    ? `全局默认 ${defaultRpmData.rpmLimit}`
                    : '留空使用全局默认'
                }
                value={batchRpmValue}
                onChange={e => setBatchRpmValue(e.target.value)}
                onKeyDown={e => { if (e.key === 'Enter') handleBatchRpmSubmit() }}
                autoFocus
                className="tnum font-mono"
              />
              <span className="shrink-0 text-xs text-muted-foreground">次/分钟</span>
            </div>
            <p className="text-2xs text-muted-foreground">
              · 留空：清除凭据级覆盖，回退到全局默认
              <br />
              · 0：显式不限流（即使全局有默认）
              <br />
              · 正整数：限制为 N 次/分钟
            </p>
          </div>
          <DialogFooter className="gap-2 sm:gap-2">
            <Button
              variant="outline"
              onClick={() => setBatchRpmDialogOpen(false)}
              disabled={batchSetRpmLimitMutation.isPending}
            >
              取消
            </Button>
            <Button onClick={handleBatchRpmSubmit} disabled={batchSetRpmLimitMutation.isPending}>
              {batchSetRpmLimitMutation.isPending ? '保存中…' : '保存'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog open={defaultRpmDialogOpen} onOpenChange={setDefaultRpmDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>全局默认 RPM 上限</DialogTitle>
            <DialogDescription>
              凭据未单独设置 RPM 时回退到此值，立即生效并持久化到 config.json。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2 py-2">
            <div className="flex items-center gap-2">
              <Input
                type="number"
                min={0}
                step={1}
                placeholder="留空表示关闭（不限流）"
                value={defaultRpmValue}
                onChange={e => setDefaultRpmValue(e.target.value)}
                onKeyDown={e => { if (e.key === 'Enter') handleDefaultRpmSubmit() }}
                autoFocus
                className="tnum font-mono"
              />
              <span className="shrink-0 text-xs text-muted-foreground">次/分钟</span>
            </div>
            <p className="text-2xs text-muted-foreground">
              · 留空：关闭全局默认（凭据未单独配置时不限流）
              <br />
              · 0：显式关闭，效果同留空
              <br />
              · 正整数：所有未单独配置的凭据均按此值限流
            </p>
          </div>
          <DialogFooter className="gap-2 sm:gap-2">
            <Button
              variant="outline"
              onClick={() => setDefaultRpmDialogOpen(false)}
              disabled={setDefaultRpmMutation.isPending}
            >
              取消
            </Button>
            <Button onClick={handleDefaultRpmSubmit} disabled={setDefaultRpmMutation.isPending}>
              {setDefaultRpmMutation.isPending ? '保存中…' : '保存'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog open={batchConcurrencyDialogOpen} onOpenChange={setBatchConcurrencyDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>批量调整并发上限</DialogTitle>
            <DialogDescription>
              将选中的 <span className="font-mono tnum font-semibold">{selectedIds.size}</span> 个凭据统一设置最大同时在途请求数。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2 py-2">
            <div className="flex items-center gap-2">
              <Input
                type="number"
                min={0}
                step={1}
                placeholder={
                  typeof defaultConcurrencyData?.concurrencyLimit === 'number'
                    ? `全局默认 ${defaultConcurrencyData.concurrencyLimit}`
                    : '留空使用全局默认'
                }
                value={batchConcurrencyValue}
                onChange={e => setBatchConcurrencyValue(e.target.value)}
                onKeyDown={e => { if (e.key === 'Enter') handleBatchConcurrencySubmit() }}
                autoFocus
                className="tnum font-mono"
              />
              <span className="shrink-0 text-xs text-muted-foreground">个在途</span>
            </div>
            <p className="text-2xs text-muted-foreground">
              · 留空：清除凭据级覆盖，回退到全局默认
              <br />
              · 0：显式不限并发（即使全局有默认）
              <br />
              · 正整数：最多 N 个同时在途
            </p>
          </div>
          <DialogFooter className="gap-2 sm:gap-2">
            <Button
              variant="outline"
              onClick={() => setBatchConcurrencyDialogOpen(false)}
              disabled={batchSetConcurrencyLimitMutation.isPending}
            >
              取消
            </Button>
            <Button onClick={handleBatchConcurrencySubmit} disabled={batchSetConcurrencyLimitMutation.isPending}>
              {batchSetConcurrencyLimitMutation.isPending ? '保存中…' : '保存'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog open={defaultConcurrencyDialogOpen} onOpenChange={setDefaultConcurrencyDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>全局默认并发上限</DialogTitle>
            <DialogDescription>
              凭据未单独设置并发上限时回退到此值，立即生效并持久化到 config.json。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2 py-2">
            <div className="flex items-center gap-2">
              <Input
                type="number"
                min={0}
                step={1}
                placeholder="留空表示关闭（不限并发）"
                value={defaultConcurrencyValue}
                onChange={e => setDefaultConcurrencyValue(e.target.value)}
                onKeyDown={e => { if (e.key === 'Enter') handleDefaultConcurrencySubmit() }}
                autoFocus
                className="tnum font-mono"
              />
              <span className="shrink-0 text-xs text-muted-foreground">个在途</span>
            </div>
            <p className="text-2xs text-muted-foreground">
              · 留空：关闭全局默认（凭据未单独配置时不限并发）
              <br />
              · 0：显式关闭，效果同留空
              <br />
              · 正整数：所有未单独配置的凭据均按此值限制并发
            </p>
          </div>
          <DialogFooter className="gap-2 sm:gap-2">
            <Button
              variant="outline"
              onClick={() => setDefaultConcurrencyDialogOpen(false)}
              disabled={setDefaultConcurrencyMutation.isPending}
            >
              取消
            </Button>
            <Button onClick={handleDefaultConcurrencySubmit} disabled={setDefaultConcurrencyMutation.isPending}>
              {setDefaultConcurrencyMutation.isPending ? '保存中…' : '保存'}
            </Button>
          </DialogFooter>
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

      <Dialog open={usageMultiplierDialogOpen} onOpenChange={setUsageMultiplierDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>计费倍率</DialogTitle>
            <DialogDescription>
              放大上报给客户端的 token 计数（input/output/cache），用于按倍率抬高下游按 token 计费的费用。不影响内部记录的真实上游成本。留空表示 1.0（不放大）。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2 py-2">
            <Input
              type="number"
              min={0}
              step={0.1}
              placeholder="例如 1.5；留空表示 1.0"
              value={usageMultiplierInput}
              onChange={e => setUsageMultiplierInput(e.target.value)}
              onKeyDown={e => { if (e.key === 'Enter') handleSaveUsageMultiplier() }}
              autoFocus
            />
            <p className="font-mono text-2xs uppercase tracking-wider text-muted-foreground">
              当前：{usageMultiplierData?.multiplier == null ? '1.0×' : `${usageMultiplierData.multiplier}×`}
            </p>
          </div>
          <DialogFooter className="gap-2 sm:gap-2">
            <Button variant="outline" onClick={() => setUsageMultiplierInput('')} disabled={isSettingUsageMultiplier}>
              清空
            </Button>
            <Button onClick={handleSaveUsageMultiplier} disabled={isSettingUsageMultiplier}>
              {isSettingUsageMultiplier ? '保存中…' : '保存'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  )
}

// ─── Primitives ───

// 格式化 USD 金额：自适应小数位（小额保留更多位以免显示为 $0.00）
function formatUsd(usd: number): string {
  const abs = Math.abs(usd)
  const digits = abs >= 100 ? 2 : abs >= 1 ? 3 : 4
  const sign = usd < 0 ? '-' : ''
  return `${sign}$${abs.toLocaleString('en-US', {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  })}`
}

// 计费汇总卡片
function BillingStatCard({
  label, value, hint, emphasis = 'default',
}: {
  label: string
  value: string
  hint?: string
  emphasis?: 'default' | 'good' | 'bad'
}) {
  const valueColor =
    emphasis === 'good' ? 'text-ok' : emphasis === 'bad' ? 'text-bad' : 'text-foreground'
  // 焦点 KPI（毛利等）以极淡 tint 自然吸睛，与普通指标拉开层级；普通卡片保持纯描边。
  const cardTone =
    emphasis === 'good'
      ? 'border-ok/30 bg-ok-soft/40'
      : emphasis === 'bad'
        ? 'border-bad/30 bg-bad-soft/40'
        : 'border-border bg-card'
  return (
    <div className={cn('rounded-xl border px-3.5 py-3 sm:px-4 sm:py-3.5', cardTone)}>
      <div className="text-2xs font-medium uppercase tracking-wider text-muted-foreground">
        {label}
      </div>
      <div className={cn('tnum mt-1.5 text-lg font-semibold leading-none tracking-tight sm:text-xl', valueColor)}>
        {value}
      </div>
      {hint && (
        <div className="mt-1 truncate font-mono text-2xs text-muted-foreground">{hint}</div>
      )}
    </div>
  )
}

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
