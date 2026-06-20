import { useState, useEffect } from 'react'
import { toast } from 'sonner'
import {
  RefreshCw,
  RotateCcw,
  Wallet,
  Trash2,
  Loader2,
  Pencil,
  Check,
  MoreHorizontal,
  ChevronUp,
  ChevronDown,
  Network,
  Gauge,
  Activity,
  CircleDollarSign,
  Boxes,
  X,
  Clock,
  KeyRound,
  AlertTriangle,
} from 'lucide-react'
import { cn } from '@/lib/utils'
import { LineChart, Line } from 'recharts'
import { RelativeTime } from '@/components/relative-time'
import { Button } from '@/components/ui/button'
import { Switch } from '@/components/ui/switch'
import { Input } from '@/components/ui/input'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import type { CredentialStatusItem, BalanceResponse, StatGroup } from '@/types/api'
import {
  useSetDisabled,
  useSetPriority,
  useSetRpmLimit,
  useSetConcurrencyLimit,
  useSetOverage,
  useResetFailure,
  useDeleteCredential,
  useForceRefreshToken,
  useProxyGroups,
  useSetCredentialGroup,
  useCredentialModels,
  useIsReadOnly,
} from '@/hooks/use-credentials'

interface CredentialCardProps {
  credential: CredentialStatusItem
  defaultRpmLimit?: number
  defaultConcurrencyLimit?: number
  onViewBalance: (id: number) => void
  selected: boolean
  onToggleSelect: () => void
  balance: BalanceResponse | null
  loadingBalance: boolean
  /** 近 N 天该凭据的用量聚合（来自 stats summary by_credential），无则不显示用量行 */
  usage?: StatGroup
  /** 近 N 天每桶平均首字 TTFT 序列（来自 stats timeseries by_credential），用于 sparkline */
  ttftSeries?: number[]
  /**
   * 「详情」展开的广播信号：父级一键展开/收起全部时下发。
   * `version` 每次点击递增，卡片据此同步到 `expanded`；同步后仍可在卡内单独切换。
   * 不传则卡片完全自管（默认收起）。
   */
  expandSignal?: { expanded: boolean; version: number }
}

// 计算生效的 RPM 上限：凭据级 0 / 正整数覆盖全局；undefined 回退全局；
// 任意一级显式为 0 视为"不限流"。返回 0 表示不限流，正整数为限制值。
function resolveEffectiveRpm(credLimit: number | undefined, defaultLimit: number | undefined): number {
  if (credLimit === 0) return 0
  if (typeof credLimit === 'number' && credLimit > 0) return credLimit
  if (defaultLimit === 0) return 0
  if (typeof defaultLimit === 'number' && defaultLimit > 0) return defaultLimit
  return 0
}

// 计算生效的并发上限：与 resolveEffectiveRpm 同语义。返回 0 表示不限并发。
function resolveEffectiveConcurrency(credLimit: number | undefined, defaultLimit: number | undefined): number {
  if (credLimit === 0) return 0
  if (typeof credLimit === 'number' && credLimit > 0) return credLimit
  if (defaultLimit === 0) return 0
  if (typeof defaultLimit === 'number' && defaultLimit > 0) return defaultLimit
  return 0
}

// 小额 USD 简短格式（卡片用）
// 毫秒简短：>1s 显示秒
function fmtMs(v: number): string {
  return v >= 1000 ? `${(v / 1000).toFixed(1)}s` : `${Math.round(v)}ms`
}
// 认证方式短标签
function authLabel(m?: string | null): string | null {
  if (!m) return null
  const v = m.toLowerCase()
  if (v === 'api_key' || v === 'apikey') return 'API'
  if (v === 'idc' || v === 'builder-id' || v === 'iam') return 'IdC'
  if (v === 'social') return 'Social'
  return m
}
// unix 秒 → ISO（喂给 RelativeTime）；无则 undefined
function unixToIso(sec?: number | null): string | undefined {
  return typeof sec === 'number' ? new Date(sec * 1000).toISOString() : undefined
}

// 把剩余毫秒格式化为简短中文（"45s" / "2m" / "1h12m"）
function formatRemaining(ms: number): string {
  const total = Math.ceil(ms / 1000)
  if (total < 60) return `${total}s`
  const m = Math.floor(total / 60)
  if (m < 60) return `${m}m`
  const h = Math.floor(m / 60)
  const rem = m % 60
  return rem > 0 ? `${h}h${rem}m` : `${h}h`
}

export function CredentialCard({
  credential,
  defaultRpmLimit,
  defaultConcurrencyLimit,
  onViewBalance,
  selected,
  onToggleSelect,
  balance,
  loadingBalance,
  usage,
  ttftSeries,
  expandSignal,
}: CredentialCardProps) {
  const [editingPriority, setEditingPriority] = useState(false)
  const [priorityValue, setPriorityValue] = useState(String(credential.priority))
  const [showDeleteDialog, setShowDeleteDialog] = useState(false)
  const [showRpmDialog, setShowRpmDialog] = useState(false)
  const [rpmInputValue, setRpmInputValue] = useState('')
  const [showConcurrencyDialog, setShowConcurrencyDialog] = useState(false)
  const [concurrencyInputValue, setConcurrencyInputValue] = useState('')
  // 懒加载可用模型：仅在"可用模型"子菜单首次展开后才发起请求
  const [modelsRequested, setModelsRequested] = useState(false)
  // 「详情」展开：默认收起，点击在卡内展开次要数据（近7天用量、添加时间、限流器、余额时效等）
  const [showDetails, setShowDetails] = useState(false)
  // 父级一键展开/收起全部时同步本卡（按 version 触发，同步后仍可单独切换）
  useEffect(() => {
    if (expandSignal) setShowDetails(expandSignal.expanded)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [expandSignal?.version])

  const readOnly = useIsReadOnly()
  const setDisabled = useSetDisabled()
  const setPriority = useSetPriority()
  const setRpmLimit = useSetRpmLimit()
  const setConcurrencyLimit = useSetConcurrencyLimit()
  const setOverage = useSetOverage()
  const resetFailure = useResetFailure()
  const deleteCredential = useDeleteCredential()
  const forceRefresh = useForceRefreshToken()
  const setCredentialGroup = useSetCredentialGroup()
  const { data: proxyGroupsData } = useProxyGroups()
  const {
    data: modelsData,
    isLoading: loadingModels,
    isError: modelsError,
  } = useCredentialModels(credential.id, modelsRequested)

  const handleToggleDisabled = () => {
    setDisabled.mutate(
      { id: credential.id, disabled: !credential.disabled },
      {
        onSuccess: res => toast.success(res.message),
        onError: err => toast.error('操作失败: ' + (err as Error).message),
      },
    )
  }

  const handlePriorityChange = () => {
    const newPriority = parseInt(priorityValue, 10)
    if (isNaN(newPriority) || newPriority < 0) {
      toast.error('优先级必须是非负整数')
      return
    }
    setPriority.mutate(
      { id: credential.id, priority: newPriority },
      {
        onSuccess: res => { toast.success(res.message); setEditingPriority(false) },
        onError: err => toast.error('操作失败: ' + (err as Error).message),
      },
    )
  }

  const handlePriorityBump = (delta: number) => {
    const newPriority = Math.max(0, credential.priority + delta)
    setPriority.mutate(
      { id: credential.id, priority: newPriority },
      {
        onSuccess: res => toast.success(res.message),
        onError: err => toast.error('操作失败: ' + (err as Error).message),
      },
    )
  }

  const handleReset = () => {
    resetFailure.mutate(credential.id, {
      onSuccess: res => toast.success(res.message),
      onError: err => toast.error('操作失败: ' + (err as Error).message),
    })
  }

  const handleForceRefresh = () => {
    forceRefresh.mutate(credential.id, {
      onSuccess: res => toast.success(res.message),
      onError: err => toast.error('刷新失败: ' + (err as Error).message),
    })
  }

  const handleSetGroup = (group: string | null) => {
    if ((credential.group || null) === group) return
    setCredentialGroup.mutate(
      { id: credential.id, group },
      {
        onSuccess: res => toast.success(res.message),
        onError: err => toast.error('操作失败: ' + (err as Error).message),
      },
    )
  }

  const handleSetOverage = (enabled: boolean) => {
    if (credential.overage === enabled) return
    setOverage.mutate(
      { id: credential.id, enabled },
      {
        onSuccess: res => toast.success(res.message),
        onError: err => toast.error('切换 overage 失败: ' + (err as Error).message),
      },
    )
  }

  const handleDelete = () => {
    if (!credential.disabled) {
      toast.error('请先禁用凭据再删除')
      setShowDeleteDialog(false)
      return
    }
    deleteCredential.mutate(credential.id, {
      onSuccess: res => { toast.success(res.message); setShowDeleteDialog(false) },
      onError: err => toast.error('删除失败: ' + (err as Error).message),
    })
  }

  const openRpmDialog = () => {
    setRpmInputValue(typeof credential.rpmLimit === 'number' ? String(credential.rpmLimit) : '')
    setShowRpmDialog(true)
  }
  const handleRpmSave = (rpmLimit: number | null) => {
    setRpmLimit.mutate({ id: credential.id, rpmLimit }, {
      onSuccess: res => { toast.success(res.message); setShowRpmDialog(false) },
      onError: err => toast.error('保存失败: ' + (err as Error).message),
    })
  }
  const handleRpmSubmit = () => {
    const trimmed = rpmInputValue.trim()
    if (trimmed === '') {
      handleRpmSave(null)
      return
    }
    const parsed = Number(trimmed)
    if (!Number.isFinite(parsed) || parsed < 0 || !Number.isInteger(parsed)) {
      toast.error('请输入 ≥ 0 的整数（0 表示不限流，留空表示回退全局默认）')
      return
    }
    handleRpmSave(parsed)
  }

  const openConcurrencyDialog = () => {
    setConcurrencyInputValue(typeof credential.concurrencyLimit === 'number' ? String(credential.concurrencyLimit) : '')
    setShowConcurrencyDialog(true)
  }
  const handleConcurrencySave = (concurrencyLimit: number | null) => {
    setConcurrencyLimit.mutate({ id: credential.id, concurrencyLimit }, {
      onSuccess: res => { toast.success(res.message); setShowConcurrencyDialog(false) },
      onError: err => toast.error('保存失败: ' + (err as Error).message),
    })
  }
  const handleConcurrencySubmit = () => {
    const trimmed = concurrencyInputValue.trim()
    if (trimmed === '') {
      handleConcurrencySave(null)
      return
    }
    const parsed = Number(trimmed)
    if (!Number.isFinite(parsed) || parsed < 0 || !Number.isInteger(parsed)) {
      toast.error('请输入 ≥ 0 的整数（0 表示不限并发，留空表示回退全局默认）')
      return
    }
    handleConcurrencySave(parsed)
  }

  const hasFailures = credential.failureCount > 0 || credential.refreshFailureCount > 0
  const usedPercent = balance ? Math.max(0, Math.min(100, balance.usagePercentage)) : 0
  const isOverLimit = !!balance && balance.usagePercentage >= 100
  // 后端把 usagePercentage 封顶在 100；overage 开启后用量会越过额度继续计费，
  // 用 currentUsage/usageLimit 还原真实（可 >100%）的百分比。
  const rawUsagePercent =
    balance && balance.usageLimit > 0
      ? (balance.currentUsage / balance.usageLimit) * 100
      : balance?.usagePercentage ?? 0
  // 超额计费状态优先取上游真实下发的 overageStatus，回退到本地 overage 开关。
  const overageFromUpstream = balance?.overageStatus != null
  const overageEnabled = overageFromUpstream
    ? balance!.overageStatus!.toUpperCase() === 'ENABLED'
    : credential.overage === true
  // 三态：上游已下发 overageStatus，或本地开关曾下发过，才算"已知"。
  const overageKnown = overageFromUpstream || credential.overage !== undefined
  // 已实际产生超额用量，或（开了超额且越过额度）= 正在超额计费（amber 警示，而非"耗尽"红色）。
  const hasActualOverage = !!balance && balance.currentOverages > 0
  const isOverageBilling = hasActualOverage || (overageEnabled && isOverLimit)
  // 超额本身也已触顶（currentOverages 越过 overageCap）→ 从 amber 升级为 red 警示
  const overageCapExceeded =
    !!balance && balance.overageCap > 0 && balance.currentOverages >= balance.overageCap
  // 超额段进度：currentOverages 占 overageCap 的比例（0–100）；cap 未知时填满表示"正在超额"
  const overageFillPercent =
    balance && balance.overageCap > 0
      ? Math.max(0, Math.min(100, (balance.currentOverages / balance.overageCap) * 100))
      : isOverageBilling
        ? 100
        : 0

  const tier = resolveTier(balance?.subscriptionTitle)
  const displayName = credential.email || `凭据 #${credential.id}`
  const initial = (credential.email?.[0] || '#').toUpperCase()
  // 额度重置：剩余天数（粗粒度，未来时间）
  const resetInDays =
    balance?.nextResetAt != null
      ? Math.max(0, Math.ceil((balance.nextResetAt * 1000 - Date.now()) / 86_400_000))
      : null
  // Token 健康：剩余有效期（ms）。仅在临期/过期/刷新失败时高亮提示，正常态不占行。
  const expiryMs = credential.expiresAt
    ? new Date(credential.expiresAt).getTime() - Date.now()
    : null
  const tokenIssue =
    (credential.refreshFailureCount ?? 0) > 0 ||
    (expiryMs != null && expiryMs < 30 * 60 * 1000)

  const throttledRemainingMs = credential.throttledUntil
    ? Math.max(0, Date.parse(credential.throttledUntil) - Date.now())
    : 0
  const isThrottled = throttledRemainingMs > 0

  const effectiveRpm = resolveEffectiveRpm(credential.rpmLimit, defaultRpmLimit)
  const rpmCurrent = credential.rpmCurrent ?? 0
  const rpmActive = effectiveRpm > 0
  const rpmUsageRatio = rpmActive ? Math.min(1, rpmCurrent / effectiveRpm) : 0
  const rpmIsExhausted = rpmActive && rpmCurrent >= effectiveRpm
  const rpmColorClass = !rpmActive
    ? 'text-muted-foreground'
    : rpmIsExhausted
      ? 'text-bad'
      : rpmUsageRatio >= 0.7
        ? 'text-warn'
        : 'text-foreground'

  const effectiveConcurrency = resolveEffectiveConcurrency(credential.concurrencyLimit, defaultConcurrencyLimit)
  const concurrencyCurrent = credential.concurrencyCurrent ?? 0
  const concurrencyActive = effectiveConcurrency > 0
  const concurrencyUsageRatio = concurrencyActive ? Math.min(1, concurrencyCurrent / effectiveConcurrency) : 0
  const concurrencyIsExhausted = concurrencyActive && concurrencyCurrent >= effectiveConcurrency
  const concurrencyColorClass = !concurrencyActive
    ? 'text-muted-foreground'
    : concurrencyIsExhausted
      ? 'text-bad'
      : concurrencyUsageRatio >= 0.7
        ? 'text-warn'
        : 'text-foreground'

  // 近7天错误率（来自 stats by_credential）：失败 / (成功 + 失败)。
  // 有失败时卡面红字常驻（健康告警），无失败时收进详情。
  const usageTotal = usage ? usage.requests + usage.failures : 0
  const errRate = usageTotal > 0 ? (usage!.failures / usageTotal) * 100 : 0
  const hasErrors = !!usage && usage.failures > 0
  // 代理出口（凭据级 proxyUrl，脱去 user:pass 仅留 scheme+host，避免泄露口令）
  const proxyDisplay = credential.proxyUrl
    ? credential.proxyUrl.replace(/\/\/[^@/]*@/, '//')
    : null

  // 限流是否处于告警态（≥70% 或耗尽）。告警态常驻卡面；非告警态（正常占用）收进详情。
  const limiterActive = rpmActive || concurrencyActive
  const limiterWarn =
    (rpmActive && rpmUsageRatio >= 0.7) || (concurrencyActive && concurrencyUsageRatio >= 0.7)
  // 限流器展示行：RPM · 并发（卡面告警态与详情非告警态复用同一份标记，避免重复）。
  const limiterRow = (
    <div className="flex items-center gap-x-2">
      {rpmActive && (
        <span
          className={cn('inline-flex items-center gap-1 tnum font-mono', rpmColorClass)}
          title={`RPM ${rpmCurrent}/${effectiveRpm} · 60s 窗口${typeof credential.rpmLimit !== 'number' ? '（默认）' : ''}`}
        >
          <Gauge className="h-3 w-3" />
          {rpmCurrent}/{effectiveRpm}
        </span>
      )}
      {rpmActive && concurrencyActive && <span className="text-muted-foreground/40">·</span>}
      {concurrencyActive && (
        <span
          className={cn('inline-flex items-center gap-1 tnum font-mono', concurrencyColorClass)}
          title={`并发 ${concurrencyCurrent}/${effectiveConcurrency} 在途${typeof credential.concurrencyLimit !== 'number' ? '（默认）' : ''}`}
        >
          <Activity className="h-3 w-3" />
          {concurrencyCurrent}/{effectiveConcurrency}
        </span>
      )}
    </div>
  )

  const barColor =
    !balance
      ? 'bg-muted'
      : isOverageBilling
        ? overageCapExceeded ? 'bg-bad' : 'bg-warn'
        : balance.usagePercentage >= 90
          ? 'bg-bad'
          : balance.usagePercentage >= 70
            ? 'bg-warn'
            : 'bg-ok'

  // 禁用原因 → 中文展示
  const disabledReasonLabels: Record<string, string> = {
    Manual: '手动禁用',
    TooManyFailures: '连续失败',
    TooManyRefreshFailures: '刷新失败',
    QuotaExceeded: '额度用尽',
    InvalidRefreshToken: 'Token 失效',
    InvalidConfig: '配置无效',
    FreeSubscription: 'Free 订阅（自动禁用）',
  }

  // 副标题状态：只在异常态显示（禁用/限流/失败）。「活跃」是常态、且已由卡片高亮边框
  // 与开关表达，不再占字。
  const statusLabel = credential.disabled
    ? (credential.disabledReason
        ? disabledReasonLabels[credential.disabledReason] || credential.disabledReason
        : '已禁用')
    : isThrottled
      ? `限流冷却 剩${formatRemaining(throttledRemainingMs)}`
      : hasFailures
        ? '异常'
        : null
  const statusClass = credential.disabled
    ? 'text-bad'
    : isThrottled
      ? 'text-warn'
      : 'text-warn'

  return (
    <>
      <div
        className={cn(
          'group relative flex flex-col overflow-hidden rounded-lg border bg-surface shadow-sm transition-shadow duration-200 hover:shadow-md',
          selected
            ? 'border-primary ring-2 ring-primary/20'
            : overageCapExceeded
              ? 'border-bad/50 ring-1 ring-bad/20'
              : isOverageBilling
                ? 'border-warn/50 ring-1 ring-warn/20'
                : credential.isCurrent
                  ? 'border-primary/40'
                  : 'border-border',
          credential.disabled && 'opacity-75',
        )}
      >
        {/* ─── HEADER: avatar + name/meta + switch ─── */}
        <div className={cn('flex items-start gap-3 p-4', tier.cardBg)}>
          {/* Avatar — click to select */}
          <button
            onClick={onToggleSelect}
            aria-label={selected ? '取消选择' : '选择'}
            className={cn(
              'relative flex h-10 w-10 shrink-0 cursor-pointer items-center justify-center rounded-full text-sm font-semibold transition-all',
              selected
                ? 'bg-primary text-primary-foreground ring-2 ring-primary/30 ring-offset-2 ring-offset-background'
                : tier.avatarBg,
              tier.avatarText,
            )}
          >
            {selected ? <Check className="h-4 w-4" /> : initial}
          </button>

          <div className="min-w-0 flex-1 pt-0.5">
            <h3
              className="truncate text-sm font-semibold leading-tight"
              title={credential.email || undefined}
            >
              {displayName}
            </h3>
            <div className="mt-1 flex min-w-0 items-center gap-x-2 overflow-hidden whitespace-nowrap font-mono text-2xs text-muted-foreground">
              <span className="tnum shrink-0">#{String(credential.id).padStart(3, '0')}</span>
              {authLabel(credential.authMethod) && (
                <span
                  className="inline-flex shrink-0 items-center gap-0.5"
                  title={`认证方式: ${credential.authMethod}`}
                >
                  <KeyRound className="h-2.5 w-2.5 shrink-0" />
                  {authLabel(credential.authMethod)}
                </span>
              )}
              {balance?.subscriptionTitle && (
                <span
                  className={cn('shrink-0 font-medium', tier.labelText)}
                  title={balance.subscriptionTitle}
                >
                  {balance.subscriptionTitle.replace(/^KIRO\s+/i, '')}
                </span>
              )}
              <span
                className="inline-flex shrink-0 items-center"
                title={
                  !overageKnown
                    ? '超额计费：未知（尚未下发过）'
                    : overageEnabled
                      ? `超额计费：已开启${overageFromUpstream ? '（上游确认）' : ''}`
                      : `超额计费：已关闭${overageFromUpstream ? '（上游确认）' : ''}`
                }
              >
                <CircleDollarSign
                  className={cn(
                    'h-3 w-3 shrink-0',
                    !overageKnown
                      ? 'text-muted-foreground/40'
                      : overageEnabled
                        ? 'text-warn'
                        : 'text-muted-foreground',
                  )}
                />
              </span>
              {statusLabel && (
                <span className={cn('shrink-0 font-medium', statusClass)}>{statusLabel}</span>
              )}
              {credential.group && (
                <span
                  className="inline-flex min-w-0 items-center gap-0.5 text-foreground"
                  title={`代理分组: ${credential.group}`}
                >
                  <Network className="h-2.5 w-2.5 shrink-0" />
                  <span className="truncate">{credential.group}</span>
                </span>
              )}
            </div>
          </div>

          <Switch
            checked={!credential.disabled}
            onCheckedChange={handleToggleDisabled}
            disabled={setDisabled.isPending || readOnly}
            title={readOnly ? '游客身份不可修改启用状态' : credential.disabled ? '启用凭据' : '禁用凭据'}
            className="shrink-0"
          />
        </div>

        {/* ─── BODY: usage + meta ─── */}
        <div className="space-y-2 px-4 pb-4">
          {/* Usage line: percent + overage tag (left) · used/limit (right) */}
          <div>
            <div className="mb-1.5 flex items-baseline justify-between gap-2">
              <div className="flex min-w-0 items-baseline gap-1.5">
                {loadingBalance ? (
                  <span className="flex items-center gap-1 text-xs text-muted-foreground">
                    <Loader2 className="h-3 w-3 animate-spin" /> 加载中
                  </span>
                ) : balance ? (
                  <>
                    <span
                      className={cn(
                        'tnum text-sm font-bold leading-none',
                        isOverageBilling
                          ? overageCapExceeded ? 'text-bad' : 'text-warn'
                          : isOverLimit ? 'text-bad' : 'text-foreground',
                      )}
                    >
                      {(isOverageBilling ? rawUsagePercent : balance.usagePercentage).toFixed(1)}%
                    </span>
                    {isOverageBilling && (
                      <span
                        className={cn(
                          'shrink-0 rounded-full px-1.5 py-0.5 text-2xs font-medium leading-none',
                          overageCapExceeded ? 'bg-bad-soft text-bad' : 'bg-warn-soft text-warn',
                        )}
                        title={
                          overageCapExceeded
                            ? '超额用量已达上限（overageCap），请尽快处理'
                            : '用量已越过额度，正在按超额计费'
                        }
                      >
                        {overageCapExceeded ? '超额已触顶' : '超额计费中'}
                      </span>
                    )}
                  </>
                ) : (
                  <span className="text-xs text-muted-foreground">—</span>
                )}
              </div>
              {balance && (
                <span className="tnum shrink-0 font-mono text-2xs text-muted-foreground">
                  {balance.currentUsage.toFixed(2)}/{balance.usageLimit.toFixed(2)}
                </span>
              )}
            </div>
            <div
              className="relative h-1 w-full overflow-hidden rounded-full bg-muted"
              title={
                isOverageBilling && balance.overageCap > 0
                  ? `超额已用 ${overageFillPercent.toFixed(0)}% 上限（${balance.currentOverages.toFixed(0)}/${balance.overageCap.toFixed(0)}）`
                  : undefined
              }
            >
              <div
                className={cn('absolute inset-y-0 left-0 rounded-full transition-[width] duration-500 ease-out', barColor)}
                style={{ width: balance ? `${usedPercent}%` : '0%' }}
              />
              {/* 超额段：在填满的基础额度条上叠加斜纹，宽度 = 超额已用占 overageCap 的比例 */}
              {isOverageBilling && (
                <div
                  className="overage-stripe absolute inset-y-0 left-0 transition-[width] duration-500 ease-out"
                  style={{ width: `${overageFillPercent}%` }}
                />
              )}
            </div>
            {/* 超额行固定占位：无超额时也保留高度，保证各卡片元信息行垂直对齐 */}
            <div className="mt-1 flex h-4 items-center gap-1.5 text-[11px] text-warn">
              {balance && hasActualOverage && (
                <span
                  className="flex items-center gap-1.5"
                  title={balance.overageRate > 0 ? `超额计费 @${balance.overageRate}/次` : undefined}
                >
                  <span className="tnum font-mono font-medium">
                    超额 +{balance.overageCharges.toFixed(2)} {balance.currency ?? ''}
                  </span>
                  <span className="text-muted-foreground/40">·</span>
                  <span
                    className={cn(
                      'tnum font-mono',
                      overageCapExceeded ? 'font-semibold text-bad' : 'text-muted-foreground',
                    )}
                    title={overageCapExceeded ? '超额用量已达/超过上限' : undefined}
                  >
                    {balance.currentOverages.toFixed(2)}
                    {balance.overageCap > 0 && `/${balance.overageCap.toFixed(0)}`}
                  </span>
                </span>
              )}
            </div>
          </div>

          {/* Meta — 常驻只留「活动」一行 + 异常告警；其余次要数据收进「详情」展开 */}
          <div className="space-y-1 text-xs">
            {/* 活动（常驻）：优先级 · 成功/失败 · 最近使用 */}
            <div className="flex items-center gap-x-2">
            {/* 优先级（可编辑） */}
            {readOnly ? (
              <span className="inline-flex items-center gap-0.5 tnum font-mono" title="优先级">
                <span className="text-muted-foreground/60">P</span>
                <span className="font-semibold text-foreground">{credential.priority}</span>
              </span>
            ) : editingPriority ? (
              <div className="flex items-center gap-0.5">
                <Input
                  type="number"
                  value={priorityValue}
                  onChange={e => setPriorityValue(e.target.value)}
                  onKeyDown={e => {
                    if (e.key === 'Enter') handlePriorityChange()
                    if (e.key === 'Escape') {
                      setEditingPriority(false)
                      setPriorityValue(String(credential.priority))
                    }
                  }}
                  className="h-6 w-10 rounded-md border-primary px-1 text-center font-mono text-xs"
                  min="0"
                  autoFocus
                />
                <button
                  onClick={handlePriorityChange}
                  disabled={setPriority.isPending}
                  className="flex h-6 w-5 cursor-pointer items-center justify-center rounded text-ok hover:bg-ok-soft"
                  aria-label="确认"
                >
                  <Check className="h-3 w-3" />
                </button>
              </div>
            ) : (
              <button
                onClick={() => setEditingPriority(true)}
                className="group/p inline-flex items-center gap-0.5 tnum font-mono hover:text-primary"
                title="点击编辑优先级"
              >
                <span className="text-muted-foreground/60">P</span>
                <span className="font-semibold text-foreground">{credential.priority}</span>
                <Pencil className="h-2.5 w-2.5 opacity-0 transition-opacity group-hover/p:opacity-60" />
              </button>
            )}

            <span className="text-muted-foreground/40">·</span>

            {/* 成功/失败 */}
            <span className="inline-flex items-center gap-1 tnum font-mono" title="成功 / 失败">
              <Check className="h-3 w-3 text-ok" />
              <span className="text-foreground">{credential.successCount}</span>
              <X className={cn('h-3 w-3', hasFailures ? 'text-bad' : 'text-muted-foreground/50')} />
              <span className={cn(hasFailures ? 'font-medium text-bad' : 'text-muted-foreground')}>
                {credential.failureCount}
              </span>
            </span>

            <span className="text-muted-foreground/40">·</span>

            {/* 最近使用 */}
            <span className="tnum font-mono text-muted-foreground" title="最近使用">
              <RelativeTime value={credential.lastUsedAt} />
            </span>
            </div>

            {/* 近7天错误率：有失败才红字常驻（健康告警）；无失败收进详情 */}
            {hasErrors && (
              <div
                className="flex items-center gap-1 font-mono text-bad"
                title={`近7天 ${usageTotal} 次请求中 ${usage!.failures} 次失败（上游 API 错误）`}
              >
                <AlertTriangle className="h-3 w-3" />
                错误率 {errRate.toFixed(1)}% · {usage!.failures} 次失败
              </div>
            )}

            {/* 限流告警态（常驻，仅 ≥70%/耗尽时；正常占用收进详情） */}
            {limiterWarn && limiterRow}

            {/* Token 健康：仅在临期/过期/刷新失败时常驻显示（正常态收进详情） */}
            {tokenIssue && (
              <div className="flex items-center gap-x-2 font-mono">
                {expiryMs != null && (
                  <span
                    className={cn(
                      'inline-flex items-center gap-1',
                      expiryMs <= 0 ? 'text-bad' : 'text-warn',
                    )}
                    title="Token 过期时间"
                  >
                    <Clock className="h-3 w-3" />
                    {expiryMs <= 0 ? 'token 已过期' : `token ${formatRemaining(expiryMs)}后过期`}
                  </span>
                )}
                {(credential.refreshFailureCount ?? 0) > 0 && (
                  <>
                    <span className="text-muted-foreground/40">·</span>
                    <span className="text-bad" title="Token 刷新连续失败次数">
                      刷新失败 {credential.refreshFailureCount}
                    </span>
                  </>
                )}
              </div>
            )}

            {/* 详情切换 */}
            <button
              onClick={() => setShowDetails(v => !v)}
              className="inline-flex cursor-pointer items-center gap-0.5 font-mono text-muted-foreground transition-colors hover:text-foreground"
              aria-expanded={showDetails}
            >
              {showDetails ? <ChevronUp className="h-3 w-3" /> : <ChevronDown className="h-3 w-3" />}
              详情
            </button>

            {/* 详情展开区：键值对齐的 properties list（标签左 / 值右） */}
            {showDetails && (
              <dl className="grid grid-cols-[auto_1fr] items-center gap-x-4 gap-y-1.5 rounded-md bg-muted/30 px-2.5 py-2 font-mono text-2xs">
                {usage && usage.requests > 0 && (
                  <>
                    <dt className="text-muted-foreground">近7天请求</dt>
                    <dd className="tnum justify-self-end text-foreground">{usage.requests}</dd>
                    <dt className="text-muted-foreground">首字延迟</dt>
                    <dd
                      className="flex items-center justify-end gap-1 justify-self-end text-foreground"
                      title="近7天首字 TTFT 趋势 / 平均"
                    >
                      {ttftSeries && ttftSeries.length >= 2 && (
                        <LineChart
                          width={56}
                          height={16}
                          data={ttftSeries.map((v, i) => ({ i, v }))}
                          margin={{ top: 3, right: 1, bottom: 3, left: 1 }}
                        >
                          <Line
                            type="monotone"
                            dataKey="v"
                            stroke="#f59e0b"
                            strokeWidth={1.5}
                            dot={false}
                            isAnimationActive={false}
                          />
                        </LineChart>
                      )}
                      <span className="tnum">{fmtMs(usage.avg_ttft_ms)}</span>
                    </dd>
                    <dt className="text-muted-foreground">平均耗时</dt>
                    <dd className="tnum justify-self-end text-foreground" title="近7天平均总耗时">
                      {fmtMs(usage.avg_elapsed_ms)}
                    </dd>
                    {!hasErrors && (
                      <>
                        <dt className="text-muted-foreground">错误率</dt>
                        <dd className="tnum justify-self-end text-foreground">{errRate.toFixed(1)}%</dd>
                      </>
                    )}
                  </>
                )}

                {/* 限流器（非告警态；告警态已在卡面常驻） */}
                {limiterActive && !limiterWarn && rpmActive && (
                  <>
                    <dt className="text-muted-foreground">RPM</dt>
                    <dd
                      className={cn('tnum justify-self-end', rpmColorClass)}
                      title={`RPM ${rpmCurrent}/${effectiveRpm} · 60s 窗口${typeof credential.rpmLimit !== 'number' ? '（默认）' : ''}`}
                    >
                      {rpmCurrent}/{effectiveRpm}
                    </dd>
                  </>
                )}
                {limiterActive && !limiterWarn && concurrencyActive && (
                  <>
                    <dt className="text-muted-foreground">并发在途</dt>
                    <dd
                      className={cn('tnum justify-self-end', concurrencyColorClass)}
                      title={`并发 ${concurrencyCurrent}/${effectiveConcurrency} 在途${typeof credential.concurrencyLimit !== 'number' ? '（默认）' : ''}`}
                    >
                      {concurrencyCurrent}/{effectiveConcurrency}
                    </dd>
                  </>
                )}

                {/* Token 到期（正常态；异常态已在卡面常驻） */}
                {!tokenIssue && expiryMs != null && (
                  <>
                    <dt className="text-muted-foreground">Token</dt>
                    <dd className="justify-self-end text-foreground">{formatRemaining(expiryMs)}后过期</dd>
                  </>
                )}

                {balance && credential.balanceCachedAt && (
                  <>
                    <dt className="text-muted-foreground">余额更新</dt>
                    <dd
                      className="justify-self-end text-foreground"
                      title="余额为缓存值，非实时；点「余额」可拉取最新"
                    >
                      <RelativeTime value={unixToIso(credential.balanceCachedAt)} />
                    </dd>
                  </>
                )}
                {resetInDays != null && (
                  <>
                    <dt className="text-muted-foreground">额度重置</dt>
                    <dd className="justify-self-end text-foreground">{resetInDays}天后</dd>
                  </>
                )}
                {balance && (
                  <>
                    <dt className="text-muted-foreground">剩余额度</dt>
                    <dd className="tnum justify-self-end text-foreground">{balance.remaining.toFixed(2)}</dd>
                  </>
                )}

                {credential.createdAt != null && (
                  <>
                    <dt className="text-muted-foreground">添加于</dt>
                    <dd className="justify-self-end text-foreground">
                      <RelativeTime value={unixToIso(credential.createdAt)} />
                    </dd>
                  </>
                )}

                {proxyDisplay && (
                  <>
                    <dt className="text-muted-foreground">代理出口</dt>
                    <dd
                      className="max-w-[12rem] justify-self-end truncate text-foreground"
                      title={proxyDisplay}
                    >
                      {proxyDisplay}
                    </dd>
                  </>
                )}

                {credential.hasProfileArn && (
                  <>
                    <dt className="text-muted-foreground">Profile ARN</dt>
                    <dd className="justify-self-end text-foreground">已配置</dd>
                  </>
                )}
              </dl>
            )}
          </div>
        </div>

        {/* ─── FOOTER: divided action cells ─── */}
        <div className={cn('mt-auto grid divide-x divide-border border-t border-border', readOnly ? 'grid-cols-1' : 'grid-cols-3')}>
          <FooterAction
            onClick={() => onViewBalance(credential.id)}
            icon={<Wallet className="h-4 w-4" />}
            label="余额"
          />
          {!readOnly && (
            <FooterAction
              onClick={handleForceRefresh}
              disabled={forceRefresh.isPending || credential.disabled}
              icon={<RefreshCw className={cn('h-4 w-4', forceRefresh.isPending && 'animate-spin')} />}
              label="Token"
              title={credential.disabled ? '已禁用的凭据无法刷新 Token' : '强制刷新 Token'}
            />
          )}
          {!readOnly && <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <button
                className="group/more flex cursor-pointer items-center justify-center gap-1.5 py-2.5 text-xs font-medium text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
                aria-label="更多操作"
              >
                <MoreHorizontal className="h-4 w-4" />
                更多
              </button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="w-44">
              <DropdownMenuItem
                onClick={handleReset}
                disabled={resetFailure.isPending || !hasFailures}
              >
                <RotateCcw /> 重置失败计数
              </DropdownMenuItem>
              <DropdownMenuItem
                onClick={() => handlePriorityBump(-1)}
                disabled={setPriority.isPending || credential.priority === 0}
              >
                <ChevronUp /> 提高优先级
              </DropdownMenuItem>
              <DropdownMenuItem
                onClick={() => handlePriorityBump(1)}
                disabled={setPriority.isPending}
              >
                <ChevronDown /> 降低优先级
              </DropdownMenuItem>
              <DropdownMenuItem
                onClick={openRpmDialog}
                disabled={setRpmLimit.isPending}
              >
                <Gauge /> RPM 上限
              </DropdownMenuItem>
              <DropdownMenuItem
                onClick={openConcurrencyDialog}
                disabled={setConcurrencyLimit.isPending}
              >
                <Activity /> 并发上限
              </DropdownMenuItem>
              <DropdownMenuSub>
                <DropdownMenuSubTrigger>
                  <CircleDollarSign /> 超额计费
                  <span className="ml-auto text-2xs text-muted-foreground">
                    {!overageKnown ? '未知' : overageEnabled ? '开' : '关'}
                  </span>
                </DropdownMenuSubTrigger>
                <DropdownMenuSubContent className="w-40">
                  <DropdownMenuItem
                    onClick={() => handleSetOverage(true)}
                    disabled={setOverage.isPending}
                  >
                    {overageKnown && overageEnabled && <Check />}
                    开启超额
                  </DropdownMenuItem>
                  <DropdownMenuItem
                    onClick={() => handleSetOverage(false)}
                    disabled={setOverage.isPending}
                  >
                    {overageKnown && !overageEnabled && <Check />}
                    关闭超额
                  </DropdownMenuItem>
                </DropdownMenuSubContent>
              </DropdownMenuSub>
              <DropdownMenuSub>
                <DropdownMenuSubTrigger>
                  <Network /> 代理分组
                </DropdownMenuSubTrigger>
                <DropdownMenuSubContent className="w-48">
                  <DropdownMenuItem
                    onClick={() => handleSetGroup(null)}
                    disabled={setCredentialGroup.isPending || !credential.group}
                  >
                    {!credential.group && <Check />}
                    无分组
                  </DropdownMenuItem>
                  {(proxyGroupsData?.groups || []).length > 0 && <DropdownMenuSeparator />}
                  {(proxyGroupsData?.groups || []).map(g => (
                    <DropdownMenuItem
                      key={g.name}
                      onClick={() => handleSetGroup(g.name)}
                      disabled={setCredentialGroup.isPending}
                    >
                      {credential.group === g.name && <Check />}
                      <span className="truncate">{g.name}</span>
                    </DropdownMenuItem>
                  ))}
                  {(proxyGroupsData?.groups || []).length === 0 && (
                    <div className="px-2 py-1.5 text-2xs text-muted-foreground">
                      暂无分组，请先在"代理分组"中创建
                    </div>
                  )}
                </DropdownMenuSubContent>
              </DropdownMenuSub>
              <DropdownMenuSub onOpenChange={open => { if (open) setModelsRequested(true) }}>
                <DropdownMenuSubTrigger>
                  <Boxes /> 可用模型
                </DropdownMenuSubTrigger>
                <DropdownMenuSubContent className="max-h-72 w-52 overflow-auto">
                  {loadingModels ? (
                    <div className="flex items-center gap-1.5 px-2 py-1.5 text-xs text-muted-foreground">
                      <Loader2 className="h-3 w-3 animate-spin" /> 加载中
                    </div>
                  ) : modelsError ? (
                    <div className="px-2 py-1.5 text-2xs text-bad">查询失败，请稍后重试</div>
                  ) : (modelsData?.models || []).length > 0 ? (
                    modelsData!.models.map(m => (
                      <div
                        key={m}
                        className="truncate px-2 py-1.5 font-mono text-2xs text-foreground"
                        title={m}
                      >
                        {m}
                      </div>
                    ))
                  ) : (
                    <div className="px-2 py-1.5 text-2xs text-muted-foreground">无可用模型</div>
                  )}
                </DropdownMenuSubContent>
              </DropdownMenuSub>
              <DropdownMenuSeparator />
              <DropdownMenuItem
                onClick={() => setShowDeleteDialog(true)}
                disabled={!credential.disabled}
                className="text-bad focus:text-bad"
              >
                <Trash2 /> 删除凭据
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>}
        </div>
      </div>

      <Dialog open={showDeleteDialog} onOpenChange={setShowDeleteDialog}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>确认删除凭据</DialogTitle>
            <DialogDescription>
              您确定要删除凭据{' '}
              <span className="font-mono tnum">#{String(credential.id).padStart(3, '0')}</span> 吗？此操作无法撤销。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setShowDeleteDialog(false)} disabled={deleteCredential.isPending}>
              取消
            </Button>
            <Button variant="destructive" onClick={handleDelete} disabled={deleteCredential.isPending || !credential.disabled}>
              确认删除
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog open={showRpmDialog} onOpenChange={setShowRpmDialog}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>设置 RPM 上限</DialogTitle>
            <DialogDescription>
              凭据 <span className="font-mono tnum">#{String(credential.id).padStart(3, '0')}</span> 每分钟最多发送的请求数。
              超限后该凭据会被本地冷却到当前 60s 滑动窗口结束，期间自动切换到其他凭据。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2 py-2">
            <div className="flex items-center gap-2">
              <Input
                type="number"
                inputMode="numeric"
                min="0"
                value={rpmInputValue}
                placeholder={typeof defaultRpmLimit === 'number' ? `全局默认 ${defaultRpmLimit}` : '留空使用全局默认'}
                onChange={e => setRpmInputValue(e.target.value)}
                onKeyDown={e => { if (e.key === 'Enter') handleRpmSubmit() }}
                className="tnum font-mono"
              />
              <span className="shrink-0 text-xs text-muted-foreground">次/分钟</span>
            </div>
            <p className="text-2xs text-muted-foreground">
              · 留空：清除凭据级覆盖，回退到全局默认
              {typeof defaultRpmLimit === 'number'
                ? defaultRpmLimit === 0 ? '（当前全局不限流）' : `（当前全局 ${defaultRpmLimit}）`
                : '（当前全局未配置）'}
              <br />
              · 0：显式不限流（即使全局有默认）
              <br />
              · 正整数：限制为 N 次/分钟
            </p>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setShowRpmDialog(false)} disabled={setRpmLimit.isPending}>
              取消
            </Button>
            <Button onClick={handleRpmSubmit} disabled={setRpmLimit.isPending}>
              保存
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog open={showConcurrencyDialog} onOpenChange={setShowConcurrencyDialog}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>设置并发上限</DialogTitle>
            <DialogDescription>
              凭据 <span className="font-mono tnum">#{String(credential.id).padStart(3, '0')}</span> 允许的最大同时在途请求数。
              在途数达到上限后该凭据会被跳过、自动切换到其他凭据；所有凭据都满时回退到负载最轻者（不拒绝请求）。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2 py-2">
            <div className="flex items-center gap-2">
              <Input
                type="number"
                inputMode="numeric"
                min="0"
                value={concurrencyInputValue}
                placeholder={typeof defaultConcurrencyLimit === 'number' ? `全局默认 ${defaultConcurrencyLimit}` : '留空使用全局默认'}
                onChange={e => setConcurrencyInputValue(e.target.value)}
                onKeyDown={e => { if (e.key === 'Enter') handleConcurrencySubmit() }}
                className="tnum font-mono"
              />
              <span className="shrink-0 text-xs text-muted-foreground">个在途</span>
            </div>
            <p className="text-2xs text-muted-foreground">
              · 留空：清除凭据级覆盖，回退到全局默认
              {typeof defaultConcurrencyLimit === 'number'
                ? defaultConcurrencyLimit === 0 ? '（当前全局不限并发）' : `（当前全局 ${defaultConcurrencyLimit}）`
                : '（当前全局未配置）'}
              <br />
              · 0：显式不限并发（即使全局有默认）
              <br />
              · 正整数：最多 N 个同时在途
            </p>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setShowConcurrencyDialog(false)} disabled={setConcurrencyLimit.isPending}>
              取消
            </Button>
            <Button onClick={handleConcurrencySubmit} disabled={setConcurrencyLimit.isPending}>
              保存
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}

// ─── Primitives ───

function FooterAction({
  onClick, disabled, icon, label, title,
}: {
  onClick?: () => void
  disabled?: boolean
  icon: React.ReactNode
  label: string
  title?: string
}) {
  return (
    <button
      onClick={onClick}
      disabled={disabled}
      title={title}
      className="flex cursor-pointer items-center justify-center gap-1.5 py-2.5 text-xs font-medium text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:cursor-not-allowed disabled:opacity-40"
    >
      {icon}
      {label}
    </button>
  )
}

// ─── Tier resolution ───

interface Tier {
  key: 'free' | 'pro' | 'power' | 'unknown'
  cardBg: string
  avatarBg: string
  avatarText: string
  labelText: string
}

function resolveTier(title: string | null | undefined): Tier {
  if (!title) {
    return {
      key: 'unknown',
      cardBg: '',
      avatarBg: 'bg-muted',
      avatarText: 'text-muted-foreground',
      labelText: 'text-muted-foreground',
    }
  }
  const t = title.toUpperCase()
  if (t.includes('POWER')) {
    return {
      key: 'power',
      cardBg: 'bg-gradient-to-br from-amber-500/[0.06] to-transparent dark:from-amber-400/[0.06]',
      avatarBg: 'bg-amber-100 dark:bg-amber-950',
      avatarText: 'text-amber-700 dark:text-amber-400',
      labelText: 'text-amber-700 dark:text-amber-400',
    }
  }
  if (t.includes('PRO')) {
    return {
      key: 'pro',
      cardBg: 'bg-gradient-to-br from-sky-500/[0.05] to-transparent dark:from-sky-400/[0.05]',
      avatarBg: 'bg-sky-100 dark:bg-sky-950',
      avatarText: 'text-sky-700 dark:text-sky-400',
      labelText: 'text-sky-700 dark:text-sky-400',
    }
  }
  if (t.includes('FREE')) {
    return {
      key: 'free',
      cardBg: '',
      avatarBg: 'bg-muted',
      avatarText: 'text-muted-foreground',
      labelText: 'text-muted-foreground',
    }
  }
  return {
    key: 'unknown',
    cardBg: '',
    avatarBg: 'bg-muted',
    avatarText: 'text-muted-foreground',
    labelText: 'text-muted-foreground',
  }
}
