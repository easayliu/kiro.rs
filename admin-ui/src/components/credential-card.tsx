import { useState } from 'react'
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
} from 'lucide-react'
import { cn } from '@/lib/utils'
import { RelativeTime } from '@/components/relative-time'
import { Button } from '@/components/ui/button'
import { Switch } from '@/components/ui/switch'
import { Input } from '@/components/ui/input'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
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
  const displayName = credential.email || `凭据 #${credential.id}`
  const initial = (credential.email?.[0] || '#').toUpperCase()

  const barColor =
    !balance
      ? 'bg-muted'
      : balance.usagePercentage >= 90
        ? 'bg-bad'
        : balance.usagePercentage >= 70
          ? 'bg-warn'
          : 'bg-ok'

  // Status label for the subtitle row
  const statusLabel = credential.disabled
    ? credential.disabledReason || '已禁用'
    : hasFailures
      ? '异常'
      : credential.isCurrent
        ? '活跃'
        : null
  const statusClass = credential.disabled
    ? 'text-bad'
    : hasFailures
      ? 'text-warn'
      : credential.isCurrent
        ? 'text-foreground'
        : 'text-muted-foreground'

  return (
    <>
      <div
        className={cn(
          'group relative flex flex-col overflow-hidden rounded-lg border bg-surface shadow-sm transition-shadow duration-200 hover:shadow-md',
          selected
            ? 'border-primary ring-2 ring-primary/20'
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
            <div className="mt-1 flex items-center gap-1.5 font-mono text-2xs text-muted-foreground">
              <span className="tnum">#{String(credential.id).padStart(3, '0')}</span>
              {balance?.subscriptionTitle && (
                <>
                  <span className="text-border">·</span>
                  <span className={cn('font-medium', tier.labelText)}>{balance.subscriptionTitle}</span>
                </>
              )}
              {statusLabel && (
                <>
                  <span className="text-border">·</span>
                  <span className={cn('font-medium', statusClass)}>{statusLabel}</span>
                </>
              )}
              {credential.hasProfileArn && (
                <>
                  <span className="text-border">·</span>
                  <span>ARN</span>
                </>
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

        {/* ─── BODY: usage + meta ─── */}
        <dl className="space-y-2.5 px-4 pb-4">
          {/* Usage row */}
          <div>
            <div className="mb-1.5 flex items-center justify-between text-xs">
              <dt className="font-medium text-muted-foreground">用量</dt>
              <dd className="tnum font-mono">
                {loadingBalance ? (
                  <span className="flex items-center gap-1 text-muted-foreground">
                    <Loader2 className="h-3 w-3 animate-spin" /> 加载中
                  </span>
                ) : balance ? (
                  <>
                    <span className={cn('font-semibold', isOverLimit ? 'text-bad' : 'text-foreground')}>
                      {balance.usagePercentage.toFixed(1)}%
                    </span>
                    <span className="ml-1.5 text-muted-foreground">
                      {balance.currentUsage.toFixed(2)}/{balance.usageLimit.toFixed(2)}
                    </span>
                  </>
                ) : (
                  <span className="text-muted-foreground">—</span>
                )}
              </dd>
            </div>
            <div className="h-1 w-full overflow-hidden rounded-full bg-muted">
              <div
                className={cn('h-full rounded-full transition-[width] duration-500 ease-out', barColor)}
                style={{ width: balance ? `${usedPercent}%` : '0%' }}
              />
            </div>
          </div>

          {/* Meta key-values — definition-list style grid */}
          <div className="grid grid-cols-3 gap-2 text-xs">
            <MetaCell label="优先级">
              {editingPriority ? (
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
                  className="group/p inline-flex items-center gap-1 tnum font-mono font-semibold text-foreground hover:text-primary"
                  title="点击编辑优先级"
                >
                  {credential.priority}
                  <Pencil className="h-2.5 w-2.5 opacity-0 transition-opacity group-hover/p:opacity-60" />
                </button>
              )}
            </MetaCell>
            <MetaCell label="成功/失败">
              <span className="tnum font-mono">
                <span className="text-foreground">{credential.successCount}</span>
                <span className="text-muted-foreground/60"> / </span>
                <span className={cn(hasFailures ? 'text-bad font-medium' : 'text-muted-foreground')}>
                  {credential.failureCount}
                </span>
              </span>
            </MetaCell>
            <MetaCell label="最近">
              <span className="tnum font-mono text-foreground">
                <RelativeTime value={credential.lastUsedAt} />
              </span>
            </MetaCell>
          </div>
        </dl>

        {/* ─── FOOTER: divided action cells ─── */}
        <div className="mt-auto grid grid-cols-3 divide-x divide-border border-t border-border">
          <FooterAction
            onClick={() => onViewBalance(credential.id)}
            icon={<Wallet className="h-4 w-4" />}
            label="余额"
          />
          <FooterAction
            onClick={handleForceRefresh}
            disabled={forceRefresh.isPending || credential.disabled}
            icon={<RefreshCw className={cn('h-4 w-4', forceRefresh.isPending && 'animate-spin')} />}
            label="Token"
            title={credential.disabled ? '已禁用的凭据无法刷新 Token' : '强制刷新 Token'}
          />
          <DropdownMenu>
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
              <DropdownMenuSeparator />
              <DropdownMenuItem
                onClick={() => setShowDeleteDialog(true)}
                disabled={!credential.disabled}
                className="text-bad focus:text-bad"
              >
                <Trash2 /> 删除凭据
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
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

// ─── Primitives ───

function MetaCell({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-0.5">
      <dt className="label-eyebrow text-[0.625rem]">{label}</dt>
      <dd>{children}</dd>
    </div>
  )
}

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
