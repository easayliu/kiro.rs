import { useState } from 'react'
import { toast } from 'sonner'
import {
  RefreshCw,
  RotateCcw,
  ChevronUp,
  ChevronDown,
  Wallet,
  Trash2,
  Loader2,
  Clock,
  Globe,
  Pencil,
  Check,
  X,
} from 'lucide-react'
import { cn } from '@/lib/utils'
import { RelativeTime } from '@/components/relative-time'
import { Button } from '@/components/ui/button'
import { Switch } from '@/components/ui/switch'
import { Input } from '@/components/ui/input'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import type { CredentialStatusItem, BalanceResponse } from '@/types/api'
import {
  useSetDisabled,
  useSetPriority,
  useResetFailure,
  useDeleteCredential,
  useForceRefreshToken,
} from '@/hooks/use-credentials'

interface CredentialCardProps {
  credential: CredentialStatusItem
  onViewBalance: (id: number) => void
  selected: boolean
  onToggleSelect: () => void
  balance: BalanceResponse | null
  loadingBalance: boolean
}

export function CredentialCard({
  credential,
  onViewBalance,
  selected,
  onToggleSelect,
  balance,
  loadingBalance,
}: CredentialCardProps) {
  const [editingPriority, setEditingPriority] = useState(false)
  const [priorityValue, setPriorityValue] = useState(String(credential.priority))
  const [showDeleteDialog, setShowDeleteDialog] = useState(false)

  const setDisabled = useSetDisabled()
  const setPriority = useSetPriority()
  const resetFailure = useResetFailure()
  const deleteCredential = useDeleteCredential()
  const forceRefresh = useForceRefreshToken()

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

  const hasFailures = credential.failureCount > 0 || credential.refreshFailureCount > 0
  const usedPercent = balance ? Math.max(0, Math.min(100, balance.usagePercentage)) : 0
  const isOverLimit = !!balance && balance.usagePercentage >= 100

  const tier = resolveTier(balance?.subscriptionTitle)

  const barColor =
    !balance
      ? ''
      : balance.usagePercentage >= 90
        ? 'bg-bad'
        : balance.usagePercentage >= 70
          ? 'bg-warn'
          : 'bg-ok'

  return (
    <>
      <div
        className={cn(
          'group relative flex flex-col overflow-hidden rounded-xl border transition-all duration-200 ease-out',
          'hover:border-foreground/30 hover:shadow-card',
          tier.cardBg,
          selected
            ? 'border-primary ring-2 ring-primary/20'
            : credential.isCurrent
              ? 'border-primary/40'
              : 'border-border',
          credential.disabled && 'opacity-75',
        )}
      >
        {/* ─── PRIMARY: email + switch ─── */}
        <div className="flex items-center gap-2.5 px-4 pt-3.5 pb-3">
          <button
            onClick={onToggleSelect}
            className="group/cb relative -m-1.5 flex h-8 w-8 shrink-0 cursor-pointer items-center justify-center rounded-md transition-colors hover:bg-muted"
            aria-label={selected ? 'unselect' : 'select'}
          >
            <span
              className={cn(
                'flex h-4 w-4 items-center justify-center rounded border transition-colors',
                selected
                  ? 'border-primary bg-primary text-primary-foreground'
                  : 'border-border bg-background group-hover/cb:border-foreground/40',
              )}
            >
              {selected && <Check className="h-3 w-3" />}
            </span>
          </button>

          <div className="min-w-0 flex-1">
            <h3
              className="truncate text-[15px] font-semibold leading-tight"
              title={credential.email || undefined}
            >
              {credential.email || `凭据 #${credential.id}`}
            </h3>
            <div className="mt-0.5 flex items-center gap-1.5 font-mono text-2xs text-muted-foreground">
              <span className="tnum">#{String(credential.id).padStart(3, '0')}</span>
              {credential.isCurrent && <span className="text-foreground">· 活跃</span>}
              {credential.hasProfileArn && <span>· ARN</span>}
              {hasFailures && !credential.disabled && <span className="text-warn">· 异常</span>}
              {credential.disabled && (
                <span className="text-bad" title={credential.disabledReason || undefined}>
                  · {credential.disabledReason ? (credential.disabledReason.length > 10 ? credential.disabledReason.slice(0, 10) + '…' : credential.disabledReason) : '已禁用'}
                </span>
              )}
            </div>
          </div>

          <Switch
            checked={!credential.disabled}
            onCheckedChange={handleToggleDisabled}
            disabled={setDisabled.isPending}
            title={credential.disabled ? '启用凭据' : '禁用凭据'}
            className="shrink-0"
          />
        </div>

        {/* ─── SECONDARY: usage bar + numbers ─── */}
        <div className="px-4 pb-3">
          <div className="mb-1.5 flex items-center justify-between gap-2 text-2xs">
            <span className={cn('font-mono font-medium', tier.labelText)}>
              {balance?.subscriptionTitle || 'Plan —'}
            </span>
            {loadingBalance ? (
              <span className="flex items-center gap-1 font-mono text-muted-foreground">
                <Loader2 className="h-2.5 w-2.5 animate-spin" /> loading
              </span>
            ) : balance ? (
              <span className="tnum min-w-0 truncate text-right font-mono">
                <span className={cn('font-semibold text-foreground', isOverLimit && 'text-bad')}>
                  {balance.currentUsage.toFixed(2)}
                </span>
                <span className="text-muted-foreground">/{balance.usageLimit.toFixed(2)}</span>
                <span className={cn('ml-1.5 font-semibold', isOverLimit ? 'text-bad' : 'text-muted-foreground')}>
                  {balance.usagePercentage.toFixed(1)}%
                </span>
              </span>
            ) : (
              <span className="font-mono text-muted-foreground">—</span>
            )}
          </div>
          <div className="h-1 w-full overflow-hidden rounded-full bg-muted">
            {balance && (
              <div
                className={cn('h-full rounded-full transition-[width] duration-500 ease-out', barColor)}
                style={{ width: `${usedPercent}%` }}
              />
            )}
          </div>
        </div>

        {/* ─── TERTIARY: meta row + priority ─── */}
        <div className="flex items-center justify-between gap-2 border-t border-border/50 px-4 py-2 font-mono text-2xs text-muted-foreground">
          <div className="flex min-w-0 flex-wrap items-center gap-x-2.5 gap-y-0.5">
            <span className="inline-flex items-center gap-1">
              <Clock className="h-2.5 w-2.5" />
              <RelativeTime value={credential.lastUsedAt} />
            </span>
            <span>·</span>
            <span>
              P <span className="text-foreground tnum">{credential.priority}</span>
            </span>
            <span>·</span>
            <span>
              ✓ <span className="tnum text-foreground">{credential.successCount}</span>
              {hasFailures && (
                <>
                  {' '}
                  <span className="text-bad">✕ <span className="tnum">{credential.failureCount}/{credential.refreshFailureCount}</span></span>
                </>
              )}
            </span>
            {credential.hasProxy && (
              <>
                <span>·</span>
                <span className="inline-flex min-w-0 items-center gap-1" title={credential.proxyUrl}>
                  <Globe className="h-2.5 w-2.5 shrink-0" />
                  <span className="truncate">{credential.proxyUrl}</span>
                </span>
              </>
            )}
          </div>
        </div>

        {/* ─── ACTIONS: subtle, always visible ─── */}
        <div className="flex items-center gap-1 border-t border-border/50 px-2 py-1.5">
          <SubtleAction onClick={() => onViewBalance(credential.id)} icon={<Wallet className="h-3.5 w-3.5" />} label="余额" />
          <SubtleAction
            onClick={handleForceRefresh}
            disabled={forceRefresh.isPending || credential.disabled}
            icon={<RefreshCw className={cn('h-3.5 w-3.5', forceRefresh.isPending && 'animate-spin')} />}
            label="Token"
            title={credential.disabled ? '已禁用的凭据无法刷新 Token' : '强制刷新 Token'}
          />
          <SubtleAction
            onClick={handleReset}
            disabled={resetFailure.isPending || !hasFailures}
            icon={<RotateCcw className="h-3.5 w-3.5" />}
            label="重置"
            title={!hasFailures ? '无失败计数可重置' : '重置失败计数'}
          />

          <div className="ml-auto flex items-center gap-0.5">
            {/* Priority edit */}
            {editingPriority ? (
              <div className="flex items-center gap-0.5 rounded-md bg-muted px-1">
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
                  className="h-6 w-10 rounded border-0 bg-transparent px-0 text-center font-mono text-xs focus:ring-0"
                  min="0"
                  autoFocus
                />
                <button
                  onClick={handlePriorityChange}
                  disabled={setPriority.isPending}
                  className="flex h-6 w-6 cursor-pointer items-center justify-center rounded-sm text-ok hover:bg-ok-soft"
                >
                  <Check className="h-3 w-3" />
                </button>
                <button
                  onClick={() => {
                    setEditingPriority(false)
                    setPriorityValue(String(credential.priority))
                  }}
                  className="flex h-6 w-6 cursor-pointer items-center justify-center rounded-sm text-muted-foreground hover:bg-muted"
                >
                  <X className="h-3 w-3" />
                </button>
              </div>
            ) : (
              <>
                <button
                  onClick={() => setEditingPriority(true)}
                  className="flex h-7 w-7 cursor-pointer items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
                  title="编辑优先级"
                  aria-label="edit priority"
                >
                  <Pencil className="h-3 w-3" />
                </button>
                <div className="inline-flex items-center">
                  <button
                    onClick={() => {
                      const newPriority = Math.max(0, credential.priority - 1)
                      setPriority.mutate(
                        { id: credential.id, priority: newPriority },
                        {
                          onSuccess: res => toast.success(res.message),
                          onError: err => toast.error('操作失败: ' + (err as Error).message),
                        },
                      )
                    }}
                    disabled={setPriority.isPending || credential.priority === 0}
                    className="flex h-7 w-7 cursor-pointer items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:cursor-not-allowed disabled:opacity-30"
                    title="提高优先级"
                    aria-label="priority up"
                  >
                    <ChevronUp className="h-3.5 w-3.5" />
                  </button>
                  <button
                    onClick={() => {
                      const newPriority = credential.priority + 1
                      setPriority.mutate(
                        { id: credential.id, priority: newPriority },
                        {
                          onSuccess: res => toast.success(res.message),
                          onError: err => toast.error('操作失败: ' + (err as Error).message),
                        },
                      )
                    }}
                    disabled={setPriority.isPending}
                    className="flex h-7 w-7 cursor-pointer items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:cursor-not-allowed disabled:opacity-30"
                    title="降低优先级"
                    aria-label="priority down"
                  >
                    <ChevronDown className="h-3.5 w-3.5" />
                  </button>
                </div>
              </>
            )}
            <button
              onClick={() => setShowDeleteDialog(true)}
              disabled={!credential.disabled}
              className="flex h-7 w-7 cursor-pointer items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-bad-soft hover:text-bad disabled:cursor-not-allowed disabled:opacity-30"
              title={!credential.disabled ? '需要先禁用凭据才能删除' : '删除凭据'}
              aria-label="delete"
            >
              <Trash2 className="h-3.5 w-3.5" />
            </button>
          </div>
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
    </>
  )
}

interface Tier {
  key: 'free' | 'pro' | 'power' | 'unknown'
  cardBg: string
  labelText: string
}

function resolveTier(title: string | null | undefined): Tier {
  if (!title) {
    return {
      key: 'unknown',
      cardBg: 'bg-surface',
      labelText: 'text-muted-foreground',
    }
  }
  const t = title.toUpperCase()
  if (t.includes('POWER')) {
    return {
      key: 'power',
      cardBg:
        'bg-gradient-to-br from-amber-500/[0.09] via-surface to-surface dark:from-amber-400/[0.08]',
      labelText: 'text-amber-700 dark:text-amber-400',
    }
  }
  if (t.includes('PRO')) {
    return {
      key: 'pro',
      cardBg:
        'bg-gradient-to-br from-sky-500/[0.07] via-surface to-surface dark:from-sky-400/[0.07]',
      labelText: 'text-sky-700 dark:text-sky-400',
    }
  }
  if (t.includes('FREE')) {
    return {
      key: 'free',
      cardBg: 'bg-surface',
      labelText: 'text-muted-foreground',
    }
  }
  return {
    key: 'unknown',
    cardBg: 'bg-surface',
    labelText: 'text-muted-foreground',
  }
}

function SubtleAction({
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
      className="inline-flex h-7 cursor-pointer items-center gap-1 rounded-md px-2 text-xs font-medium text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:cursor-not-allowed disabled:opacity-30"
    >
      {icon}
      {label}
    </button>
  )
}
