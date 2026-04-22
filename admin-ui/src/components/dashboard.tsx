import { useState, useEffect, useRef, type ReactNode } from 'react'
import { RefreshCw, LogOut, Moon, Sun, Server, Plus, Upload, FileUp, Trash2, RotateCcw, CheckCircle2, MoreHorizontal, Database, ShieldCheck, AlertTriangle, Ban, Clock, Zap } from 'lucide-react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { storage } from '@/lib/storage'
import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { CredentialCard } from '@/components/credential-card'
import { BalanceDialog } from '@/components/balance-dialog'
import { AddCredentialDialog } from '@/components/add-credential-dialog'
import { BatchImportDialog } from '@/components/batch-import-dialog'
import { KamImportDialog } from '@/components/kam-import-dialog'
import { BatchVerifyDialog, type VerifyResult } from '@/components/batch-verify-dialog'
import { useCredentials, useDeleteCredential, useResetFailure, useLoadBalancingMode, useSetLoadBalancingMode, useCacheScope, useSetCacheScope, useCacheSkipRate, useSetCacheSkipRate } from '@/hooks/use-credentials'
import type { CacheScope } from '@/api/credentials'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu'
import { getCredentialBalance, forceRefreshToken } from '@/api/credentials'
import { cn, extractErrorMessage, formatLastUsed } from '@/lib/utils'
import type { BalanceResponse } from '@/types/api'

interface StatBoxProps {
  icon: ReactNode
  label: string
  value: number
  accent?: string
}

function StatBox({ icon, label, value, accent }: StatBoxProps) {
  return (
    <div className="flex items-center gap-3 rounded-md border bg-muted/30 px-3 py-2.5">
      <div className={cn('flex h-8 w-8 shrink-0 items-center justify-center rounded-md bg-background text-muted-foreground', accent)}>
        {icon}
      </div>
      <div className="min-w-0">
        <div className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">{label}</div>
        <div className={cn('text-lg font-semibold leading-tight tabular-nums', accent)}>{value}</div>
      </div>
    </div>
  )
}

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
  const itemsPerPage = 12
  const [darkMode, setDarkMode] = useState(() => {
    if (typeof window !== 'undefined') {
      return document.documentElement.classList.contains('dark')
    }
    return false
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

  // 计算分页
  const totalPages = Math.ceil((data?.credentials.length || 0) / itemsPerPage)
  const startIndex = (currentPage - 1) * itemsPerPage
  const endIndex = startIndex + itemsPerPage
  const currentCredentials = data?.credentials.slice(startIndex, endIndex) || []
  const disabledCredentialCount = data?.credentials.filter(credential => credential.disabled).length || 0
  const faultyCredentialCount = data?.credentials.filter(
    c => !c.disabled && (c.failureCount > 0 || c.refreshFailureCount > 0),
  ).length || 0
  const totalCount = data?.total || 0
  const availableCount = data?.available || 0
  const activeCredential = data?.currentId
    ? data.credentials.find(c => c.id === data.currentId)
    : undefined
  const activeBalance = data?.currentId ? balanceMap.get(data.currentId) : undefined
  const selectedDisabledCount = Array.from(selectedIds).filter(id => {
    const credential = data?.credentials.find(c => c.id === id)
    return Boolean(credential?.disabled)
  }).length

  // 当凭据列表变化时重置到第一页
  useEffect(() => {
    setCurrentPage(1)
  }, [data?.credentials.length])

  // balance 缓存变化时同步到 localStorage（刷新页面后仍可见）
  useEffect(() => {
    storage.saveBalanceCache(balanceMap)
  }, [balanceMap])

  // 只保留当前仍存在的凭据缓存，避免删除后残留旧数据
  useEffect(() => {
    if (!data?.credentials) {
      setBalanceMap(new Map())
      setLoadingBalanceIds(new Set())
      return
    }

    const validIds = new Set(data.credentials.map(credential => credential.id))

    setBalanceMap(prev => {
      const next = new Map<number, BalanceResponse>()
      prev.forEach((value, id) => {
        if (validIds.has(id)) {
          next.set(id, value)
        }
      })
      return next.size === prev.size ? prev : next
    })

    setLoadingBalanceIds(prev => {
      if (prev.size === 0) {
        return prev
      }
      const next = new Set<number>()
      prev.forEach(id => {
        if (validIds.has(id)) {
          next.add(id)
        }
      })
      return next.size === prev.size ? prev : next
    })
  }, [data?.credentials])

  const toggleDarkMode = () => {
    setDarkMode(!darkMode)
    document.documentElement.classList.toggle('dark')
  }

  const handleViewBalance = (id: number) => {
    setSelectedCredentialId(id)
    setBalanceDialogOpen(true)
  }

  const handleRefresh = () => {
    refetch()
    toast.success('已刷新凭据列表')
  }

  const handleLogout = () => {
    storage.removeApiKey()
    storage.clearBalanceCache()
    queryClient.clear()
    onLogout()
  }

  // 选择管理
  const toggleSelect = (id: number) => {
    const newSelected = new Set(selectedIds)
    if (newSelected.has(id)) {
      newSelected.delete(id)
    } else {
      newSelected.add(id)
    }
    setSelectedIds(newSelected)
  }

  const deselectAll = () => {
    setSelectedIds(new Set())
  }

  // 批量删除（仅删除已禁用项）
  const handleBatchDelete = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要删除的凭据')
      return
    }

    const disabledIds = Array.from(selectedIds).filter(id => {
      const credential = data?.credentials.find(c => c.id === id)
      return Boolean(credential?.disabled)
    })

    if (disabledIds.length === 0) {
      toast.error('选中的凭据中没有已禁用项')
      return
    }

    const skippedCount = selectedIds.size - disabledIds.length
    const skippedText = skippedCount > 0 ? `（将跳过 ${skippedCount} 个未禁用凭据）` : ''

    if (!confirm(`确定要删除 ${disabledIds.length} 个已禁用凭据吗？此操作无法撤销。${skippedText}`)) {
      return
    }

    let successCount = 0
    let failCount = 0

    for (const id of disabledIds) {
      try {
        await new Promise<void>((resolve, reject) => {
          deleteCredential(id, {
            onSuccess: () => {
              successCount++
              resolve()
            },
            onError: (err) => {
              failCount++
              reject(err)
            }
          })
        })
      } catch (error) {
        // 错误已在 onError 中处理
      }
    }

    const skippedResultText = skippedCount > 0 ? `，已跳过 ${skippedCount} 个未禁用凭据` : ''

    if (failCount === 0) {
      toast.success(`成功删除 ${successCount} 个已禁用凭据${skippedResultText}`)
    } else {
      toast.warning(`删除已禁用凭据：成功 ${successCount} 个，失败 ${failCount} 个${skippedResultText}`)
    }

    deselectAll()
  }

  // 批量恢复异常
  const handleBatchResetFailure = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要恢复的凭据')
      return
    }

    const failedIds = Array.from(selectedIds).filter(id => {
      const cred = data?.credentials.find(c => c.id === id)
      return cred && cred.failureCount > 0
    })

    if (failedIds.length === 0) {
      toast.error('选中的凭据中没有失败的凭据')
      return
    }

    let successCount = 0
    let failCount = 0

    for (const id of failedIds) {
      try {
        await new Promise<void>((resolve, reject) => {
          resetFailure(id, {
            onSuccess: () => {
              successCount++
              resolve()
            },
            onError: (err) => {
              failCount++
              reject(err)
            }
          })
        })
      } catch (error) {
        // 错误已在 onError 中处理
      }
    }

    if (failCount === 0) {
      toast.success(`成功恢复 ${successCount} 个凭据`)
    } else {
      toast.warning(`成功 ${successCount} 个，失败 ${failCount} 个`)
    }

    deselectAll()
  }

  // 批量刷新 Token
  const handleBatchForceRefresh = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要刷新的凭据')
      return
    }

    const enabledIds = Array.from(selectedIds).filter(id => {
      const cred = data?.credentials.find(c => c.id === id)
      return cred && !cred.disabled
    })

    if (enabledIds.length === 0) {
      toast.error('选中的凭据中没有启用的凭据')
      return
    }

    setBatchRefreshing(true)
    setBatchRefreshProgress({ current: 0, total: enabledIds.length })

    let successCount = 0
    let failCount = 0

    for (let i = 0; i < enabledIds.length; i++) {
      try {
        await forceRefreshToken(enabledIds[i])
        successCount++
      } catch {
        failCount++
      }
      setBatchRefreshProgress({ current: i + 1, total: enabledIds.length })
    }

    setBatchRefreshing(false)
    queryClient.invalidateQueries({ queryKey: ['credentials'] })

    if (failCount === 0) {
      toast.success(`成功刷新 ${successCount} 个凭据的 Token`)
    } else {
      toast.warning(`刷新 Token：成功 ${successCount} 个，失败 ${failCount} 个`)
    }

    deselectAll()
  }

  // 一键清除所有已禁用凭据
  const handleClearAll = async () => {
    if (!data?.credentials || data.credentials.length === 0) {
      toast.error('没有可清除的凭据')
      return
    }

    const disabledCredentials = data.credentials.filter(credential => credential.disabled)

    if (disabledCredentials.length === 0) {
      toast.error('没有可清除的已禁用凭据')
      return
    }

    if (!confirm(`确定要清除所有 ${disabledCredentials.length} 个已禁用凭据吗？此操作无法撤销。`)) {
      return
    }

    let successCount = 0
    let failCount = 0

    for (const credential of disabledCredentials) {
      try {
        await new Promise<void>((resolve, reject) => {
          deleteCredential(credential.id, {
            onSuccess: () => {
              successCount++
              resolve()
            },
            onError: (err) => {
              failCount++
              reject(err)
            }
          })
        })
      } catch (error) {
        // 错误已在 onError 中处理
      }
    }

    if (failCount === 0) {
      toast.success(`成功清除所有 ${successCount} 个已禁用凭据`)
    } else {
      toast.warning(`清除已禁用凭据：成功 ${successCount} 个，失败 ${failCount} 个`)
    }

    deselectAll()
  }

  // 查询当前页凭据信息（逐个查询，避免瞬时并发）
  const handleQueryCurrentPageInfo = async () => {
    if (currentCredentials.length === 0) {
      toast.error('当前页没有可查询的凭据')
      return
    }

    const ids = currentCredentials
      .filter(credential => !credential.disabled)
      .map(credential => credential.id)

    if (ids.length === 0) {
      toast.error('当前页没有可查询的启用凭据')
      return
    }

    setQueryingInfo(true)
    setQueryInfoProgress({ current: 0, total: ids.length })

    let successCount = 0
    let failCount = 0

    for (let i = 0; i < ids.length; i++) {
      const id = ids[i]

      setLoadingBalanceIds(prev => {
        const next = new Set(prev)
        next.add(id)
        return next
      })

      try {
        const balance = await getCredentialBalance(id)
        successCount++

        setBalanceMap(prev => {
          const next = new Map(prev)
          next.set(id, balance)
          return next
        })
      } catch (error) {
        failCount++
      } finally {
        setLoadingBalanceIds(prev => {
          const next = new Set(prev)
          next.delete(id)
          return next
        })
      }

      setQueryInfoProgress({ current: i + 1, total: ids.length })
    }

    setQueryingInfo(false)

    if (failCount === 0) {
      toast.success(`查询完成：成功 ${successCount}/${ids.length}`)
    } else {
      toast.warning(`查询完成：成功 ${successCount} 个，失败 ${failCount} 个`)
    }
  }

  // 批量验活
  const handleBatchVerify = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要验活的凭据')
      return
    }

    // 初始化状态
    setVerifying(true)
    cancelVerifyRef.current = false
    const ids = Array.from(selectedIds)
    setVerifyProgress({ current: 0, total: ids.length })

    let successCount = 0

    // 初始化结果，所有凭据状态为 pending
    const initialResults = new Map<number, VerifyResult>()
    ids.forEach(id => {
      initialResults.set(id, { id, status: 'pending' })
    })
    setVerifyResults(initialResults)
    setVerifyDialogOpen(true)

    // 开始验活
    for (let i = 0; i < ids.length; i++) {
      // 检查是否取消
      if (cancelVerifyRef.current) {
        toast.info('已取消验活')
        break
      }

      const id = ids[i]

      // 更新当前凭据状态为 verifying
      setVerifyResults(prev => {
        const newResults = new Map(prev)
        newResults.set(id, { id, status: 'verifying' })
        return newResults
      })

      try {
        const balance = await getCredentialBalance(id)
        successCount++

        // 更新为成功状态
        setVerifyResults(prev => {
          const newResults = new Map(prev)
          newResults.set(id, {
            id,
            status: 'success',
            usage: `${balance.currentUsage}/${balance.usageLimit}`
          })
          return newResults
        })
      } catch (error) {
        // 更新为失败状态
        setVerifyResults(prev => {
          const newResults = new Map(prev)
          newResults.set(id, {
            id,
            status: 'failed',
            error: extractErrorMessage(error)
          })
          return newResults
        })
      }

      // 更新进度
      setVerifyProgress({ current: i + 1, total: ids.length })

      // 添加延迟防止封号（最后一个不需要延迟）
      if (i < ids.length - 1 && !cancelVerifyRef.current) {
        await new Promise(resolve => setTimeout(resolve, 2000))
      }
    }

    setVerifying(false)

    if (!cancelVerifyRef.current) {
      toast.success(`验活完成：成功 ${successCount}/${ids.length}`)
    }
  }

  // 取消验活
  const handleCancelVerify = () => {
    cancelVerifyRef.current = true
    setVerifying(false)
  }

  // 打开跳过率设置对话框
  const handleOpenCacheSkipRateDialog = () => {
    const current = cacheSkipRateData?.rate
    setCacheSkipRateInput(current == null ? '' : String(current))
    setCacheSkipRateDialogOpen(true)
  }

  // 提交缓存查找跳过率
  const handleSaveCacheSkipRate = () => {
    if (isSettingCacheSkipRate) return
    const trimmed = cacheSkipRateInput.trim()
    let rate: number | null
    if (trimmed === '') {
      rate = null
    } else {
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
      onError: (error) => {
        toast.error(`设置失败: ${extractErrorMessage(error)}`)
      }
    })
  }

  // 缓存分桶策略：两态切换（两者都按用户身份 metadata.user_id 基础分桶）
  const cacheScopeLabel = (scope?: CacheScope) =>
    scope === 'per_credential' ? '凭据隔离' : '全局共享'
  const cacheScopeTitle = (scope?: CacheScope) =>
    scope === 'per_credential'
      ? '当前：按用户身份 + credential 双层分桶（同一用户跨凭据不共享） · 点击切换到全局共享'
      : '当前：按用户身份分桶（同一用户跨凭据共享，不同用户天然隔离） · 点击切换到凭据隔离'
  const handleCycleCacheScope = () => {
    const current = cacheScopeData?.scope ?? 'global'
    const next: CacheScope = current === 'global' ? 'per_credential' : 'global'
    setCacheScopeMutation(next, {
      onSuccess: () => {
        toast.success(`缓存模式已切换到 ${cacheScopeLabel(next)}`)
      },
      onError: (error) => {
        toast.error(`切换失败: ${extractErrorMessage(error)}`)
      },
    })
  }

  // 切换负载均衡模式
  const handleToggleLoadBalancing = () => {
    const currentMode = loadBalancingData?.mode || 'priority'
    const newMode = currentMode === 'priority' ? 'balanced' : 'priority'

    setLoadBalancingMode(newMode, {
      onSuccess: () => {
        const modeName = newMode === 'priority' ? '优先级模式' : '均衡负载模式'
        toast.success(`已切换到${modeName}`)
      },
      onError: (error) => {
        toast.error(`切换失败: ${extractErrorMessage(error)}`)
      }
    })
  }

  if (isLoading) {
    return (
      <div className="min-h-screen flex items-center justify-center bg-background">
        <div className="text-center">
          <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-primary mx-auto mb-4"></div>
          <p className="text-muted-foreground">加载中...</p>
        </div>
      </div>
    )
  }

  if (error) {
    return (
      <div className="min-h-screen flex items-center justify-center bg-background p-4">
        <Card className="w-full max-w-md">
          <CardContent className="pt-6 text-center">
            <div className="text-red-500 mb-4">加载失败</div>
            <p className="text-muted-foreground mb-4">{(error as Error).message}</p>
            <div className="space-x-2">
              <Button onClick={() => refetch()}>重试</Button>
              <Button variant="outline" onClick={handleLogout}>重新登录</Button>
            </div>
          </CardContent>
        </Card>
      </div>
    )
  }

  return (
    <div className="min-h-screen bg-background">
      {/* 顶部导航 */}
      <header
        className="sticky top-0 z-50 w-full border-b bg-background/95 backdrop-blur supports-[backdrop-filter]:bg-background/60"
        style={{ paddingTop: 'env(safe-area-inset-top)' }}
      >
        <div
          className="container mx-auto flex h-14 items-center justify-between px-4 md:px-8"
          style={{ paddingLeft: 'max(1rem, env(safe-area-inset-left))', paddingRight: 'max(1rem, env(safe-area-inset-right))' }}
        >
          <div className="flex items-center gap-2">
            <div className="flex h-8 w-8 items-center justify-center rounded-md bg-primary text-primary-foreground">
              <Server className="h-4 w-4" />
            </div>
            <div className="flex flex-col leading-tight">
              <span className="text-sm font-semibold">Kiro Admin</span>
              <span className="hidden text-[11px] text-muted-foreground sm:inline">凭据与缓存治理</span>
            </div>
          </div>
          <div className="flex items-center gap-1">
            <Button variant="ghost" size="icon" onClick={toggleDarkMode} title={darkMode ? '切换到浅色' : '切换到深色'}>
              {darkMode ? <Sun className="h-5 w-5" /> : <Moon className="h-5 w-5" />}
            </Button>
            <Button variant="ghost" size="icon" onClick={handleRefresh} title="刷新凭据列表">
              <RefreshCw className="h-5 w-5" />
            </Button>
            <Button variant="ghost" size="icon" onClick={handleLogout} title="退出登录">
              <LogOut className="h-5 w-5" />
            </Button>
          </div>
        </div>
      </header>

      {/* 主内容 */}
      <main
        className="container mx-auto px-4 md:px-8 py-6 space-y-6"
        style={{
          paddingLeft: 'max(1rem, env(safe-area-inset-left))',
          paddingRight: 'max(1rem, env(safe-area-inset-right))',
          paddingBottom: 'max(1.5rem, env(safe-area-inset-bottom))',
        }}
      >
        {/* 系统策略面板 */}
        <Card>
          <CardContent className="grid gap-4 p-4 md:grid-cols-3">
            <div className="flex items-center justify-between gap-3 md:border-r md:pr-4">
              <div className="min-w-0">
                <div className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">缓存分桶</div>
                <div className="mt-0.5 truncate text-xs text-muted-foreground/80">
                  {cacheScopeData?.scope === 'per_credential' ? '按用户 + 凭据双层' : '按用户身份共享'}
                </div>
              </div>
              <Button
                variant="outline"
                size="sm"
                className="h-8 shrink-0"
                onClick={handleCycleCacheScope}
                disabled={isLoadingCacheScope || isSettingCacheScope}
                title={cacheScopeTitle(cacheScopeData?.scope)}
              >
                {isLoadingCacheScope ? '加载中...' : cacheScopeLabel(cacheScopeData?.scope)}
              </Button>
            </div>
            <div className="flex items-center justify-between gap-3 md:border-r md:pr-4">
              <div className="min-w-0">
                <div className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">缓存跳过率</div>
                <div className="mt-0.5 truncate text-xs text-muted-foreground/80">
                  {cacheSkipRateData?.rate == null ? '按自然 breakpoint 计算' : '按概率跳过 cache 查找'}
                </div>
              </div>
              <Button
                variant="outline"
                size="sm"
                className="h-8 shrink-0 tabular-nums"
                onClick={handleOpenCacheSkipRateDialog}
                disabled={isLoadingCacheSkipRate || isSettingCacheSkipRate}
                title={
                  cacheSkipRateData?.rate == null
                    ? '未启用缓存跳过 · 点击设置跳过概率（0.0-1.0）'
                    : `当前跳过率：${(cacheSkipRateData.rate * 100).toFixed(0)}% · 点击修改或关闭`
                }
              >
                {isLoadingCacheSkipRate
                  ? '加载中...'
                  : cacheSkipRateData?.rate == null
                    ? '关闭'
                    : `${(cacheSkipRateData.rate * 100).toFixed(0)}%`}
              </Button>
            </div>
            <div className="flex items-center justify-between gap-3">
              <div className="min-w-0">
                <div className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">负载均衡</div>
                <div className="mt-0.5 truncate text-xs text-muted-foreground/80">
                  {loadBalancingData?.mode === 'balanced' ? 'LRU，最久未使用优先' : '固定最高优先级'}
                </div>
              </div>
              <Button
                variant="outline"
                size="sm"
                className="h-8 shrink-0"
                onClick={handleToggleLoadBalancing}
                disabled={isLoadingMode || isSettingMode}
                title={
                  loadBalancingData?.mode === 'balanced'
                    ? '当前：均衡负载（LRU）· 点击切换到优先级模式'
                    : '当前：优先级模式 · 点击切换到均衡负载'
                }
              >
                {isLoadingMode ? '加载中...' : (loadBalancingData?.mode === 'priority' ? '优先级模式' : '均衡负载')}
              </Button>
            </div>
          </CardContent>
        </Card>

        {/* 状态栏 */}
        <Card>
          <CardContent className="flex flex-col gap-4 p-4">
            {/* 4 指标 */}
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
              <StatBox
                icon={<Database className="h-4 w-4" />}
                label="凭据总数"
                value={totalCount}
              />
              <StatBox
                icon={<ShieldCheck className="h-4 w-4" />}
                label="可用"
                value={availableCount}
                accent={availableCount > 0 ? 'text-emerald-600 dark:text-emerald-500' : ''}
              />
              <StatBox
                icon={<AlertTriangle className="h-4 w-4" />}
                label="异常"
                value={faultyCredentialCount}
                accent={faultyCredentialCount > 0 ? 'text-amber-600 dark:text-amber-500' : 'text-muted-foreground/70'}
              />
              <StatBox
                icon={<Ban className="h-4 w-4" />}
                label="已禁用"
                value={disabledCredentialCount}
                accent={disabledCredentialCount > 0 ? 'text-red-500' : 'text-muted-foreground/70'}
              />
            </div>

            {/* 当前活跃 */}
            <div className="flex flex-wrap items-center gap-x-4 gap-y-1.5 border-t pt-3">
              <div className="flex items-center gap-2">
                <Zap className="h-3.5 w-3.5 text-emerald-500" aria-hidden />
                <span className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">当前活跃</span>
              </div>
              {data?.currentId ? (
                <>
                  <Badge variant="success" className="h-5 px-1.5 font-mono text-[10px] tabular-nums">
                    #{data.currentId}
                  </Badge>
                  <span
                    className="min-w-0 flex-1 truncate text-sm font-medium"
                    title={activeCredential?.email || undefined}
                  >
                    {activeCredential?.email || `凭据 #${data.currentId}`}
                  </span>
                  <div className="ml-auto flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
                    {activeBalance?.subscriptionTitle && (
                      <span className="inline-flex items-center gap-1">
                        <span className="h-1.5 w-1.5 rounded-full bg-primary/60" aria-hidden />
                        {activeBalance.subscriptionTitle}
                      </span>
                    )}
                    <span className="inline-flex items-center gap-1 tabular-nums">
                      <Clock className="h-3 w-3" />
                      {formatLastUsed(activeCredential?.lastUsedAt)}
                    </span>
                  </div>
                </>
              ) : (
                <span className="text-sm text-muted-foreground">暂无活跃凭据</span>
              )}
            </div>
          </CardContent>
        </Card>

        {/* 凭据列表 */}
        <section className="space-y-4">
          {/* 标题行：标题 + 次要工具 + 主要入口 */}
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="flex items-center gap-3">
              <h2 className="text-xl font-semibold tracking-tight">凭据管理</h2>
              <Badge variant="secondary" className="font-normal">
                共 {data?.credentials.length || 0} 个
              </Badge>
              {verifying && !verifyDialogOpen && (
                <Button onClick={() => setVerifyDialogOpen(true)} size="sm" variant="secondary">
                  <CheckCircle2 className="h-4 w-4 mr-2 animate-spin" />
                  验活中 {verifyProgress.current}/{verifyProgress.total}
                </Button>
              )}
            </div>
            <div className="flex flex-wrap items-center gap-2">
              {/* md+ 平铺次要动作 */}
              <div className="hidden items-center gap-2 md:flex">
                {data?.credentials && data.credentials.length > 0 && (
                  <>
                    <Button
                      onClick={handleQueryCurrentPageInfo}
                      size="sm"
                      variant="ghost"
                      disabled={queryingInfo}
                    >
                      <RefreshCw className={`h-4 w-4 mr-2 ${queryingInfo ? 'animate-spin' : ''}`} />
                      {queryingInfo ? `查询中 ${queryInfoProgress.current}/${queryInfoProgress.total}` : '查询信息'}
                    </Button>
                    <Button
                      onClick={handleClearAll}
                      size="sm"
                      variant="ghost"
                      className="text-destructive hover:text-destructive"
                      disabled={disabledCredentialCount === 0}
                      title={disabledCredentialCount === 0 ? '没有可清除的已禁用凭据' : undefined}
                    >
                      <Trash2 className="h-4 w-4 mr-2" />
                      清除已禁用
                    </Button>
                    <div className="mx-1 h-6 w-px bg-border" aria-hidden />
                  </>
                )}
                <Button onClick={() => setKamImportDialogOpen(true)} size="sm" variant="outline">
                  <FileUp className="h-4 w-4 mr-2" />
                  KAM 导入
                </Button>
                <Button onClick={() => setBatchImportDialogOpen(true)} size="sm" variant="outline">
                  <Upload className="h-4 w-4 mr-2" />
                  批量导入
                </Button>
              </div>

              {/* md 以下折叠到 dropdown */}
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button size="sm" variant="outline" className="md:hidden" title="更多操作">
                    <MoreHorizontal className="h-4 w-4" />
                    <span className="sr-only">更多操作</span>
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end" className="w-52">
                  {data?.credentials && data.credentials.length > 0 && (
                    <>
                      <DropdownMenuItem
                        onClick={handleQueryCurrentPageInfo}
                        disabled={queryingInfo}
                      >
                        <RefreshCw className={queryingInfo ? 'animate-spin' : ''} />
                        {queryingInfo ? `查询中 ${queryInfoProgress.current}/${queryInfoProgress.total}` : '查询信息'}
                      </DropdownMenuItem>
                      <DropdownMenuItem
                        onClick={handleClearAll}
                        disabled={disabledCredentialCount === 0}
                        className="text-destructive focus:text-destructive"
                      >
                        <Trash2 />
                        清除已禁用
                      </DropdownMenuItem>
                      <DropdownMenuSeparator />
                    </>
                  )}
                  <DropdownMenuItem onClick={() => setKamImportDialogOpen(true)}>
                    <FileUp />
                    KAM 导入
                  </DropdownMenuItem>
                  <DropdownMenuItem onClick={() => setBatchImportDialogOpen(true)}>
                    <Upload />
                    批量导入
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>

              <Button onClick={() => setAddDialogOpen(true)} size="sm">
                <Plus className="h-4 w-4 mr-2" />
                添加凭据
              </Button>
            </div>
          </div>

          {/* 批量操作条（选中时显示） */}
          {selectedIds.size > 0 && (
            <div className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-primary/20 bg-accent/40 px-4 py-2.5">
              <div className="flex items-center gap-3">
                <Badge variant="secondary">已选 {selectedIds.size} 个</Badge>
                <Button onClick={deselectAll} size="sm" variant="ghost" className="h-7 px-2 text-muted-foreground">
                  取消选择
                </Button>
              </div>
              <div className="flex flex-wrap items-center gap-2">
                <Button onClick={handleBatchVerify} size="sm" variant="outline">
                  <CheckCircle2 className="h-4 w-4 mr-2" />
                  批量验活
                </Button>
                <Button
                  onClick={handleBatchForceRefresh}
                  size="sm"
                  variant="outline"
                  disabled={batchRefreshing}
                >
                  <RefreshCw className={`h-4 w-4 mr-2 ${batchRefreshing ? 'animate-spin' : ''}`} />
                  {batchRefreshing ? `刷新中 ${batchRefreshProgress.current}/${batchRefreshProgress.total}` : '批量刷新 Token'}
                </Button>
                <Button onClick={handleBatchResetFailure} size="sm" variant="outline">
                  <RotateCcw className="h-4 w-4 mr-2" />
                  恢复异常
                </Button>
                <Button
                  onClick={handleBatchDelete}
                  size="sm"
                  variant="destructive"
                  disabled={selectedDisabledCount === 0}
                  title={selectedDisabledCount === 0 ? '只能删除已禁用凭据' : undefined}
                >
                  <Trash2 className="h-4 w-4 mr-2" />
                  批量删除
                </Button>
              </div>
            </div>
          )}

          {data?.credentials.length === 0 ? (
            <Card>
              <CardContent className="flex flex-col items-center justify-center gap-3 py-16 text-center">
                <div className="flex h-12 w-12 items-center justify-center rounded-full bg-muted">
                  <Server className="h-5 w-5 text-muted-foreground" />
                </div>
                <div>
                  <div className="text-sm font-medium">暂无凭据</div>
                  <p className="mt-1 text-sm text-muted-foreground">
                    使用右上角的「添加凭据」或「批量导入」开始。
                  </p>
                </div>
              </CardContent>
            </Card>
          ) : (
            <>
              <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
                {currentCredentials.map((credential) => (
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

              {/* 分页控件 */}
              {totalPages > 1 && (
                <div className="flex flex-wrap items-center justify-center gap-3 pt-2">
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => setCurrentPage(p => Math.max(1, p - 1))}
                    disabled={currentPage === 1}
                  >
                    上一页
                  </Button>
                  <span className="text-sm text-muted-foreground">
                    第 {currentPage} / {totalPages} 页 · 共 {data?.credentials.length} 个
                  </span>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => setCurrentPage(p => Math.min(totalPages, p + 1))}
                    disabled={currentPage === totalPages}
                  >
                    下一页
                  </Button>
                </div>
              )}
            </>
          )}
        </section>
      </main>

      {/* 余额对话框 */}
      <BalanceDialog
        credentialId={selectedCredentialId}
        open={balanceDialogOpen}
        onOpenChange={setBalanceDialogOpen}
      />

      {/* 添加凭据对话框 */}
      <AddCredentialDialog
        open={addDialogOpen}
        onOpenChange={setAddDialogOpen}
      />

      {/* 批量导入对话框 */}
      <BatchImportDialog
        open={batchImportDialogOpen}
        onOpenChange={setBatchImportDialogOpen}
      />

      {/* KAM 账号导入对话框 */}
      <KamImportDialog
        open={kamImportDialogOpen}
        onOpenChange={setKamImportDialogOpen}
      />

      {/* 批量验活对话框 */}
      <BatchVerifyDialog
        open={verifyDialogOpen}
        onOpenChange={setVerifyDialogOpen}
        verifying={verifying}
        progress={verifyProgress}
        results={verifyResults}
        onCancel={handleCancelVerify}
      />

      {/* 缓存跳过率对话框 */}
      <Dialog open={cacheSkipRateDialogOpen} onOpenChange={setCacheSkipRateDialogOpen}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>缓存查找跳过率</DialogTitle>
            <DialogDescription>
              输入 0.0 – 1.0 之间的跳过概率。每个请求按此概率跳过 cache 查找（当作首次请求，
              <code className="mx-1 rounded bg-muted px-1">cache_read = 0</code>
              ），但仍正常写入 checkpoint；用于在自然命中率偏高时整体降低观察到的缓存率。留空则关闭。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2 py-2">
            <Input
              type="number"
              min={0}
              max={1}
              step={0.05}
              placeholder="例如 0.3 表示 30% 请求跳过查找；留空关闭"
              value={cacheSkipRateInput}
              onChange={(e) => setCacheSkipRateInput(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') handleSaveCacheSkipRate()
              }}
              autoFocus
            />
            <p className="text-xs text-muted-foreground">
              当前：{cacheSkipRateData?.rate == null ? '关闭（按自然 breakpoint 计算）' : `${(cacheSkipRateData.rate * 100).toFixed(0)}%`}
            </p>
          </div>
          <DialogFooter className="gap-2 sm:gap-2">
            <Button
              variant="outline"
              onClick={() => {
                setCacheSkipRateInput('')
              }}
              disabled={isSettingCacheSkipRate}
            >
              清空（关闭）
            </Button>
            <Button onClick={handleSaveCacheSkipRate} disabled={isSettingCacheSkipRate}>
              {isSettingCacheSkipRate ? '保存中...' : '保存'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  )
}
