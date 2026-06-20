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
  ChevronsUpDown,
  ChevronsDownUp,
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
  BarChart3,
  Users,
  PanelLeftClose,
  PanelLeftOpen,
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
import { StatsView } from '@/components/stats-view'
import {
  useCredentials,
  useDeleteCredential,
  useBatchDeleteCredentials,
  useResetFailure,
  useLoadBalancingMode,
  useSetLoadBalancingMode,
  useCacheScope,
  useSetCacheScope,
  useCacheSkipRate,
  useSetCacheSkipRate,
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
  useStatsSummary,
  useStatsTimeseries,
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
import { ToggleGroup, ToggleGroupItem } from '@/components/ui/toggle-group'
import { getCredentialBalance, forceRefreshToken } from '@/api/credentials'
import { cn, extractErrorMessage } from '@/lib/utils'
import { RelativeTime } from '@/components/relative-time'
import type { CredentialStatusItem, StatGroup } from '@/types/api'

// 业界标准分页：返回页码序列，过多时用省略号收拢（首页/末页恒显，当前页两侧各留 1）
function getPageList(current: number, total: number, sibling = 1): (number | 'dots')[] {
  const totalSlots = sibling * 2 + 5 // 首 + 末 + 当前 + 2*sibling + 2 省略
  if (total <= totalSlots) return Array.from({ length: total }, (_, i) => i + 1)
  const left = Math.max(current - sibling, 1)
  const right = Math.min(current + sibling, total)
  const out: (number | 'dots')[] = [1]
  if (left > 2) out.push('dots')
  for (let i = left; i <= right; i++) if (i !== 1 && i !== total) out.push(i)
  if (right < total - 1) out.push('dots')
  out.push(total)
  return out
}

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
  // 「详情」一键展开/收起全部：广播给所有卡片（version 每次点击递增触发同步）
  const [detailsAllExpanded, setDetailsAllExpanded] = useState(false)
  const [expandVersion, setExpandVersion] = useState(0)
  const toggleAllDetails = () => {
    setDetailsAllExpanded(v => !v)
    setExpandVersion(v => v + 1)
  }
  const [verifyDialogOpen, setVerifyDialogOpen] = useState(false)
  const [verifying, setVerifying] = useState(false)
  const [verifyProgress, setVerifyProgress] = useState({ current: 0, total: 0 })
  const [verifyResults, setVerifyResults] = useState<Map<number, VerifyResult>>(new Map())
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

  // 侧栏折叠（桌面端）：默认展开，收起为图标轨；状态持久化
  const [sidebarCollapsed, setSidebarCollapsed] = useState(() => {
    if (typeof window === 'undefined') return false
    return localStorage.getItem('kiro-sidebar-collapsed') === '1'
  })
  const toggleSidebar = () => {
    setSidebarCollapsed(v => {
      const next = !v
      localStorage.setItem('kiro-sidebar-collapsed', next ? '1' : '0')
      return next
    })
  }

  const queryClient = useQueryClient()
  const readOnly = useIsReadOnly()
  // 顶部视图切换：凭据控制台 / 统计分析（持久化，刷新后停留在当前标签）
  const [view, setView] = useState<'credentials' | 'stats'>(() =>
    localStorage.getItem('kiro-view') === 'stats' ? 'stats' : 'credentials',
  )
  useEffect(() => {
    localStorage.setItem('kiro-view', view)
  }, [view])
  const { data, isLoading, error, refetch } = useCredentials()
  const { mutate: deleteCredential } = useDeleteCredential()
  const { mutateAsync: batchDeleteCredentials } = useBatchDeleteCredentials()
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
  // 近 7 天每凭据用量（请求/成本/TTFT），按 id 映射给卡片
  const { data: statsSummary } = useStatsSummary({ hours: 168 })
  const usageMap = useMemo(() => {
    const m = new Map<number, StatGroup>()
    statsSummary?.by_credential?.forEach(g => {
      const id = Number(g.key)
      if (!Number.isNaN(id)) m.set(id, g)
    })
    return m
  }, [statsSummary])
  // 近 7 天每凭据 TTFT 时序（按天分桶，轻量），透视成 id → 有序数组供卡片画 sparkline
  const { data: ttftTs } = useStatsTimeseries({ hours: 168, bucket: 'day', groupBy: 'credential' })
  const ttftSeriesMap = useMemo(() => {
    const byId = new Map<number, { bucket: number; v: number }[]>()
    ttftTs?.forEach(r => {
      const id = Number(r.group)
      if (Number.isNaN(id)) return
      if (!byId.has(id)) byId.set(id, [])
      byId.get(id)!.push({ bucket: r.bucket, v: r.avg_ttft_ms })
    })
    const m = new Map<number, number[]>()
    byId.forEach((arr, id) => {
      arr.sort((a, b) => a.bucket - b.bucket)
      m.set(id, arr.map(x => x.v))
    })
    return m
  }, [ttftTs])
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
  const isThrottledNow = (c: CredentialStatusItem) =>
    !!c.throttledUntil && Date.parse(c.throttledUntil) > Date.now()
  const throttledCredentialCount = allCreds.filter(c => !c.disabled && isThrottledNow(c)).length
  // server-side "available" 仅排除 disabled；冷却中的凭据虽未真正可用，再从前端二次扣减
  const availableCount = Math.max(0, (data?.available || 0) - throttledCredentialCount)
  const disabledCredentialCount = allCreds.filter(c => c.disabled).length
  const faultyCredentialCount = allCreds.filter(c => !c.disabled && !isThrottledNow(c) && (c.failureCount > 0 || c.refreshFailureCount > 0)).length
  const activeCredential = data?.currentId ? allCreds.find(c => c.id === data.currentId) : undefined
  // 余额来自服务端列表（credential.balance）—— kiro.db 为单一真相源
  const balanceOf = (c: CredentialStatusItem) => c.balance
  const activeBalance = activeCredential?.balance

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
      // 凭证可用，未查询过余额的（credential.balance 为空）无论升降序都排到末尾。
      const desc = sortKey === 'usage-desc'
      return [...filtered].sort((a, b) => {
        const ua = balanceOf(a)?.currentUsage
        const ub = balanceOf(b)?.currentUsage
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
        (tierRank(balanceOf(a)?.subscriptionTitle) -
          tierRank(balanceOf(b)?.subscriptionTitle)) * direction
      if (rankDelta !== 0) return rankDelta
      // Stable secondary order: priority asc, then id asc
      if (a.priority !== b.priority) return a.priority - b.priority
      return a.id - b.id
    })
  }, [allCreds, filter, search, sortKey])

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
  useEffect(() => {
    if (!data?.credentials) {
      setLoadingBalanceIds(new Set()); return
    }
    const validIds = new Set(data.credentials.map(c => c.id))
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

  // 弹窗查到最新余额：失效凭据列表，使卡片的值/时间戳从服务端同步刷新
  const handleBalanceLoaded = () => {
    queryClient.invalidateQueries({ queryKey: ['credentials'] })
  }

  const handleRefresh = () => { refetch(); toast.success('已刷新凭据列表') }

  const handleLogout = () => {
    storage.removeApiKey(); queryClient.clear(); onLogout()
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
    const tail = skippedCount > 0 ? `，已跳过 ${skippedCount} 个未禁用凭据` : ''
    try {
      const res = await batchDeleteCredentials(disabledIds)
      if (res.failed.length === 0) toast.success(`成功删除 ${res.succeeded.length} 个已禁用凭据${tail}`)
      else toast.warning(`删除已禁用凭据：成功 ${res.succeeded.length} 个，失败 ${res.failed.length} 个${tail}`)
    } catch {
      toast.error('批量删除失败')
    }
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
        await getCredentialBalance(id)
        successCount++
      } catch { failCount++ }
      finally {
        setLoadingBalanceIds(prev => { const next = new Set(prev); next.delete(id); return next })
      }
      setQueryInfoProgress({ current: i + 1, total: ids.length })
    }
    setQueryingInfo(false)
    // 余额已更新到服务端缓存，失效列表让卡片从服务端同步刷新
    queryClient.invalidateQueries({ queryKey: ['credentials'] })
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
            <Button onClick={() => refetch()} className="flex-1">
              <RefreshCw className="h-4 w-4" /> Retry
            </Button>
            <Button variant="outline" onClick={handleLogout}>
              <LogOut className="h-4 w-4" /> Sign out
            </Button>
          </div>
        </div>
      </div>
    )
  }

  return (
    <div className="relative min-h-screen bg-background text-foreground">
      {/* Desktop icon rail — Stripe-style collapsible sidebar (hover to expand) */}
      <Sidebar
        view={view}
        onChange={setView}
        readOnly={readOnly}
        darkMode={darkMode}
        collapsed={sidebarCollapsed}
        onToggleTheme={toggleDarkMode}
        onRefresh={handleRefresh}
        onLogout={handleLogout}
        onToggleCollapse={toggleSidebar}
      />

      {/* Top header — mobile only (desktop nav lives in the icon rail) */}
      <header
        className="sticky top-0 z-30 border-b border-border bg-background/80 backdrop-blur-xl lg:hidden"
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
            <div className="flex h-7 w-7 items-center justify-center rounded-lg bg-primary text-primary-foreground">
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
          {/* 视图切换：凭据 / 统计 */}
          <ViewSwitcher view={view} onChange={setView} />
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
        className={cn(
          'relative z-10 min-h-screen pb-20 transition-[padding] duration-200 lg:pb-10',
          sidebarCollapsed ? 'lg:pl-14' : 'lg:pl-60',
        )}
        style={{ paddingBottom: 'max(5rem, env(safe-area-inset-bottom))' }}
      >
        <div
          className="mx-auto max-w-[1440px] px-4 pt-4 sm:px-8 sm:pt-8 lg:px-12 lg:pt-10"
          style={{
            paddingLeft: 'max(1rem, env(safe-area-inset-left))',
            paddingRight: 'max(1rem, env(safe-area-inset-right))',
          }}
        >
          {view === 'stats' ? (
            <StatsView />
          ) : (
          <>
          {/* ━━━━━━━━━━━━ PAGE HEADER — title · active credential · policies ━━━━━━━━━━━━ */}
          <section className="mb-5 sm:mb-6">
            <div className="flex flex-wrap items-baseline gap-x-5 gap-y-2">
              <h1 className="flex shrink-0 items-baseline gap-2 tracking-tight">
                <span className="text-2xl font-semibold sm:text-3xl">凭据控制台</span>
                <span className="tnum text-base font-semibold sm:text-lg">
                  <span className={cn(availableCount > 0 ? 'text-foreground' : 'text-muted-foreground')}>{availableCount}</span>
                  <span className="font-normal text-muted-foreground/40">/</span>
                  <span className="font-medium text-muted-foreground">{totalCount}</span>
                </span>
              </h1>

              {/* Active credential */}
              {data?.currentId && (
                <p className="flex min-w-0 items-center gap-1.5 font-mono text-xs text-muted-foreground">
                  <span className="relative flex h-1.5 w-1.5 shrink-0">
                    <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-ok/70" />
                    <span className="relative inline-flex h-1.5 w-1.5 rounded-full bg-ok" />
                  </span>
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

          {/* ━━━━━━━━━━━━ CONTENT ━━━━━━━━━━━━ */}
          <section>

            {/* Sticky toolbar — single row */}
            <div className="sticky top-12 z-20 -mx-4 mb-4 border-b border-border bg-background/92 px-4 py-2.5 backdrop-blur-md sm:-mx-8 sm:top-14 sm:px-8 lg:-mx-12 lg:top-0 lg:px-12">
              <div className="flex flex-wrap items-center gap-x-2 gap-y-2">
                {/* Search — fills the row on mobile, capped & left-aligned on desktop */}
                <div className="relative order-1 min-w-0 flex-1 lg:max-w-xs">
                  <Search className="pointer-events-none absolute left-3 top-1/2 z-10 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
                  <Input
                    type="search"
                    inputMode="search"
                    placeholder="搜索邮箱 / ID / 代理 / 分组"
                    value={search}
                    onChange={e => setSearch(e.target.value)}
                    className="h-9 pl-9 pr-9"
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

                {/* Primary inline actions — icon-only on mobile, text on sm+; far right on desktop */}
                <div className="order-2 flex shrink-0 items-center gap-1.5 lg:order-3">
                <Button
                  variant="outline"
                  size="sm"
                  onClick={handleQueryCurrentPageInfo}
                  disabled={queryingInfo}
                  title={queryingInfo ? `查询中 ${queryInfoProgress.current}/${queryInfoProgress.total}` : '查询当前页信息'}
                  aria-label="查询当前页信息"
                  className="shrink-0 gap-1.5 px-2.5 text-xs [&_svg]:size-3.5 sm:px-3"
                >
                  <RefreshCw className={cn(queryingInfo && 'animate-spin')} />
                  <span className="hidden sm:inline">
                    {queryingInfo ? `${queryInfoProgress.current}/${queryInfoProgress.total}` : '查询'}
                  </span>
                </Button>
                {!readOnly && (
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => setKamImportDialogOpen(true)}
                    title="KAM 导入"
                    aria-label="KAM 导入"
                    className="shrink-0 gap-1.5 px-2.5 text-xs [&_svg]:size-3.5 sm:px-3"
                  >
                    <FileUp />
                    <span className="hidden sm:inline">KAM</span>
                  </Button>
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

                {/* Filter chips — full second row on mobile, inline between search & actions on desktop */}
                <div className="order-3 flex w-full items-center gap-1 overflow-x-auto no-scrollbar lg:order-2 lg:w-auto lg:flex-1">
                <ToggleGroup
                  type="single"
                  value={filter}
                  onValueChange={v => { if (v) setFilter(v as FilterKey) }}
                  className="shrink-0"
                >
                  <FilterToggle value="all" active={filter === 'all'} count={allCreds.length}>全部</FilterToggle>
                  <FilterToggle
                    value="available"
                    active={filter === 'available'}
                    count={availableCount}
                    tone={availableCount > 0 ? 'ok' : 'default'}
                  >
                    可用
                  </FilterToggle>
                  <FilterToggle
                    value="faulty"
                    active={filter === 'faulty'}
                    count={faultyCredentialCount}
                    tone={faultyCredentialCount > 0 ? 'warn' : 'default'}
                  >
                    异常
                  </FilterToggle>
                  <FilterToggle
                    value="throttled"
                    active={filter === 'throttled'}
                    count={throttledCredentialCount}
                    tone={throttledCredentialCount > 0 ? 'warn' : 'default'}
                  >
                    限流冷却
                  </FilterToggle>
                  <FilterToggle
                    value="disabled"
                    active={filter === 'disabled'}
                    count={disabledCredentialCount}
                    tone={disabledCredentialCount > 0 ? 'bad' : 'default'}
                  >
                    禁用
                  </FilterToggle>
                </ToggleGroup>

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
            </div>

            {/* Select-all strip (very subtle) */}
            {filteredCreds.length > 0 && (
              <div className="mb-3 flex flex-wrap items-center justify-between gap-y-2 px-1">
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
                <div className="flex flex-wrap items-center justify-end gap-x-3 gap-y-1 font-mono text-2xs text-muted-foreground">
                  {/* 一键展开/收起全部卡片详情 */}
                  <button
                    onClick={toggleAllDetails}
                    aria-expanded={detailsAllExpanded}
                    className="inline-flex cursor-pointer items-center gap-1 rounded-md px-1.5 py-1 transition-colors hover:text-foreground"
                    title={detailsAllExpanded ? '收起全部详情' : '展开全部详情'}
                  >
                    {detailsAllExpanded ? (
                      <ChevronsDownUp className="h-3 w-3" />
                    ) : (
                      <ChevronsUpDown className="h-3 w-3" />
                    )}
                    {detailsAllExpanded ? '收起详情' : '展开详情'}
                  </button>

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

                  {/* 结果计数（业界标准：共 N 条 · 当前区间），替代难看的「页 1/1」 */}
                  <span className="tnum">
                    共 <span className="text-foreground">{filteredCreds.length}</span> 条
                    {totalPages > 1 && (
                      <span className="text-muted-foreground/60">
                        {' · '}
                        {startIndex + 1}–{Math.min(startIndex + itemsPerPage, filteredCreds.length)}
                      </span>
                    )}
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
                      balance={credential.balance ?? null}
                      usage={usageMap.get(credential.id)}
                      ttftSeries={ttftSeriesMap.get(credential.id)}
                      loadingBalance={loadingBalanceIds.has(credential.id)}
                      expandSignal={{ expanded: detailsAllExpanded, version: expandVersion }}
                    />
                  ))}
                </div>

                {totalPages > 1 && (
                  <nav
                    className="mt-8 flex flex-wrap items-center justify-center gap-1.5"
                    aria-label="分页"
                  >
                    {/* 上一页 */}
                    <button
                      onClick={() => setCurrentPage(p => Math.max(1, p - 1))}
                      disabled={safePage === 1}
                      aria-label="上一页"
                      className="inline-flex h-9 min-w-9 cursor-pointer items-center justify-center rounded-lg border border-border px-2 text-sm text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:cursor-not-allowed disabled:opacity-40"
                    >
                      <ChevronLeft className="h-4 w-4" />
                    </button>

                    {/* 页码 */}
                    {getPageList(safePage, totalPages).map((p, i) =>
                      p === 'dots' ? (
                        <span
                          key={`dots-${i}`}
                          className="inline-flex h-9 min-w-9 items-center justify-center px-1 text-sm text-muted-foreground/50"
                        >
                          …
                        </span>
                      ) : (
                        <button
                          key={p}
                          onClick={() => setCurrentPage(p)}
                          aria-current={p === safePage ? 'page' : undefined}
                          className={cn(
                            'tnum inline-flex h-9 min-w-9 cursor-pointer items-center justify-center rounded-lg border px-2 text-sm font-medium transition-colors',
                            p === safePage
                              ? 'border-primary bg-primary text-primary-foreground'
                              : 'border-border text-muted-foreground hover:bg-muted hover:text-foreground',
                          )}
                        >
                          {p}
                        </button>
                      ),
                    )}

                    {/* 下一页 */}
                    <button
                      onClick={() => setCurrentPage(p => Math.min(totalPages, p + 1))}
                      disabled={safePage === totalPages}
                      aria-label="下一页"
                      className="inline-flex h-9 min-w-9 cursor-pointer items-center justify-center rounded-lg border border-border px-2 text-sm text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:cursor-not-allowed disabled:opacity-40"
                    >
                      <ChevronRight className="h-4 w-4" />
                    </button>
                  </nav>
                )}
              </>
            )}
          </section>
          </>
          )}
        </div>
      </main>

      {/* Mobile selection bar */}
      {selectedIds.size > 0 && (
        <div
          className={cn(
            'fixed inset-x-0 bottom-0 z-40 border-t border-border bg-background/95 shadow-pop backdrop-blur-xl animate-in fade-in slide-in-from-bottom-2 duration-300',
            sidebarCollapsed ? 'lg:left-14' : 'lg:left-60',
          )}
          style={{ paddingBottom: 'max(0.5rem, env(safe-area-inset-bottom))' }}
        >
          <div className="mx-auto flex max-w-[1440px] items-center gap-2 px-4 pt-2 sm:px-8 lg:px-12">
            <span className="inline-flex shrink-0 items-center gap-1.5 text-sm font-medium">
              <span className="tnum flex h-6 w-6 items-center justify-center rounded-full bg-primary text-2xs font-bold text-primary-foreground">
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
      <BalanceDialog credentialId={selectedCredentialId} open={balanceDialogOpen} onOpenChange={setBalanceDialogOpen} onBalanceLoaded={handleBalanceLoaded} />
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

    </div>
  )
}

// ─── Primitives ───

// Stripe 式常驻展开侧边栏：桌面端（lg+）固定 240px，图标 + 文字始终可见。
// 顶部账户头（logo + 名称 + Guest 徽章）→ 分组导航（主导航 / 系统）→ 底部账户操作。
function Sidebar({
  view, onChange, readOnly, darkMode, collapsed,
  onToggleTheme, onRefresh, onLogout, onToggleCollapse,
}: {
  view: 'credentials' | 'stats'
  onChange: (v: 'credentials' | 'stats') => void
  readOnly: boolean
  darkMode: boolean
  collapsed: boolean
  onToggleTheme: () => void
  onRefresh: () => void
  onLogout: () => void
  onToggleCollapse: () => void
}) {
  return (
    <aside
      className={cn(
        'fixed inset-y-0 left-0 z-40 hidden flex-col border-r border-border bg-surface transition-[width] duration-200 ease-out lg:flex',
        collapsed ? 'w-14' : 'w-60',
      )}
      style={{ paddingTop: 'env(safe-area-inset-top)', paddingBottom: 'env(safe-area-inset-bottom)' }}
    >
      {/* Account header */}
      <div className={cn('flex h-14 shrink-0 items-center gap-2.5', collapsed ? 'justify-center px-0' : 'px-4')}>
        <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded-lg bg-primary text-primary-foreground">
          <span className="font-mono text-xs font-bold">K</span>
        </div>
        {!collapsed && (
          <>
            <div className="flex min-w-0 items-baseline gap-1.5">
              <span className="text-sm font-semibold tracking-tight">Kiro</span>
              <span className="text-xs text-muted-foreground">Admin</span>
            </div>
            {readOnly && (
              <span
                title="当前以游客身份登录，仅可只读浏览"
                className="ml-auto inline-flex items-center rounded-full border border-warn/40 bg-warn-soft px-1.5 py-0.5 font-mono text-2xs font-semibold uppercase tracking-wider text-warn"
              >
                Guest
              </span>
            )}
          </>
        )}
      </div>

      {/* Primary nav — 主组不带标题（Stripe 语汇），分组靠留白 + 次级小标题 */}
      <nav className={cn('flex-1 overflow-y-auto py-3', collapsed ? 'px-2' : 'px-3')}>
        <NavSection collapsed={collapsed}>
          <NavItem
            collapsed={collapsed}
            icon={<Users />}
            label="凭据"
            active={view === 'credentials'}
            onClick={() => onChange('credentials')}
          />
          <NavItem
            collapsed={collapsed}
            icon={<BarChart3 />}
            label="统计"
            active={view === 'stats'}
            onClick={() => onChange('stats')}
          />
        </NavSection>

        <NavSection collapsed={collapsed} label="系统">
          <NavItem
            collapsed={collapsed}
            icon={darkMode ? <Sun /> : <Moon />}
            label={darkMode ? '浅色模式' : '深色模式'}
            onClick={onToggleTheme}
          />
          <NavItem collapsed={collapsed} icon={<RefreshCw />} label="刷新数据" onClick={onRefresh} />
        </NavSection>
      </nav>

      {/* Footer: 折叠开关 + 退出（靠留白区隔，不画硬分割线） */}
      <div className={cn('flex flex-col gap-0.5 pb-3 pt-1', collapsed ? 'px-2' : 'px-3')}>
        <NavItem
          collapsed={collapsed}
          icon={collapsed ? <PanelLeftOpen /> : <PanelLeftClose />}
          label={collapsed ? '展开侧栏' : '收起侧栏'}
          onClick={onToggleCollapse}
        />
        <NavItem collapsed={collapsed} icon={<LogOut />} label="退出登录" onClick={onLogout} tone="bad" />
      </div>
    </aside>
  )
}

// 侧栏分组：可选小号 uppercase 段标题 + 一组导航项。
// 主组不传 label（Stripe 首组无标题）；次级组（如「系统」）才配标题；折叠时隐藏标题。
function NavSection({ label, collapsed, children }: { label?: string; collapsed?: boolean; children: ReactNode }) {
  return (
    <div className="mb-4 last:mb-0">
      {label && !collapsed && <div className="label-eyebrow px-2.5 pb-1.5">{label}</div>}
      <div className="flex flex-col gap-0.5">{children}</div>
    </div>
  )
}

// 侧栏导航项：active 用 blurple 浅底高亮 + 品牌色文字图标（Stripe 圆角整行高亮）；
// tone='bad' 给退出登录的危险 hover；collapsed 时仅图标居中，文字转为 tooltip。
function NavItem({ icon, label, active, onClick, tone = 'default', collapsed }: {
  icon: ReactNode
  label: string
  active?: boolean
  onClick?: () => void
  tone?: 'default' | 'bad'
  collapsed?: boolean
}) {
  return (
    <button
      onClick={onClick}
      title={label}
      aria-label={label}
      aria-current={active ? 'page' : undefined}
      className={cn(
        'flex h-9 w-full cursor-pointer items-center rounded-md text-sm transition-colors [&_svg]:h-[18px] [&_svg]:w-[18px]',
        collapsed ? 'justify-center px-0' : 'gap-3 px-2.5',
        active
          ? 'bg-muted font-semibold text-foreground'
          : tone === 'bad'
            ? 'font-medium text-muted-foreground hover:bg-bad-soft hover:text-bad'
            : 'font-medium text-muted-foreground hover:bg-muted hover:text-foreground',
      )}
    >
      {/* Stripe：导航激活态淡灰底 + 深色文字，仅图标点缀品牌色（色彩留给交互/数据） */}
      <span className={cn('flex h-[18px] w-[18px] shrink-0 items-center justify-center', active && 'text-primary')}>
        {icon}
      </span>
      {!collapsed && <span className="truncate">{label}</span>}
    </button>
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

// 顶部视图切换：凭据 / 统计。等宽双标签 + 滑动指示器（pill 随选中项平移），
// 移动端同样显示文字，点按区域加大；指示器用 iOS spring 缓动平移。
function ViewSwitcher({
  view, onChange,
}: {
  view: 'credentials' | 'stats'
  onChange: (v: 'credentials' | 'stats') => void
}) {
  const tabs = [
    { key: 'credentials' as const, label: '凭据', Icon: Users },
    { key: 'stats' as const, label: '统计', Icon: BarChart3 },
  ]
  return (
    <div
      role="tablist"
      aria-label="视图切换"
      className="relative inline-flex items-center rounded-xl border border-border bg-muted/40 p-0.5"
    >
      {/* 滑动指示器：宽度恰为半幅，随选中项左/右平移 */}
      <span
        aria-hidden
        className="absolute inset-y-0.5 left-0.5 rounded-lg bg-background shadow-sm ring-1 ring-border/40 transition-transform duration-300 ease-[cubic-bezier(0.32,0.72,0,1)]"
        style={{
          width: 'calc(50% - 2px)',
          transform: view === 'stats' ? 'translateX(100%)' : 'translateX(0)',
        }}
      />
      {tabs.map(({ key, label, Icon }) => {
        const active = view === key
        return (
          <button
            key={key}
            type="button"
            role="tab"
            aria-selected={active}
            onClick={() => onChange(key)}
            className={cn(
              'relative z-10 inline-flex flex-1 cursor-pointer items-center justify-center gap-1.5 rounded-lg px-3 py-1.5 text-xs font-medium transition-colors',
              active ? 'text-foreground' : 'text-muted-foreground hover:text-foreground',
            )}
          >
            <Icon className="h-3.5 w-3.5 shrink-0" />
            <span>{label}</span>
          </button>
        )
      })}
    </div>
  )
}

// 基于 Radix ToggleGroupItem 的筛选药丸：药丸样式由 toggle-group 提供，
// 这里只负责计数徽标 —— 选中态用半透明前景底，未选中态按 tone 着色。
function FilterToggle({ children, value, active, count, tone = 'default' }: {
  children: ReactNode
  value: string
  active: boolean
  count: number
  tone?: 'default' | 'ok' | 'warn' | 'bad'
}) {
  const countClass = active
    ? 'rounded-full bg-primary-foreground/20 px-1.5 py-px text-primary-foreground'
    : tone === 'ok'
      ? 'text-ok'
      : tone === 'warn'
        ? 'text-warn'
        : tone === 'bad'
          ? 'text-bad'
          : 'text-muted-foreground/60'
  return (
    <ToggleGroupItem value={value} aria-label={typeof children === 'string' ? children : value}>
      {children}
      <span className={cn('tnum font-mono text-2xs', countClass)}>{count}</span>
    </ToggleGroupItem>
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
