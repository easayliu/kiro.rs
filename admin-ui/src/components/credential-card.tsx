import { useState } from 'react'
import { toast } from 'sonner'
import { RefreshCw, RotateCcw, ChevronUp, ChevronDown, Wallet, Trash2, Loader2, Clock, Globe, Pencil, Check, X } from 'lucide-react'
import { cn, formatLastUsed } from '@/lib/utils'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Switch } from '@/components/ui/switch'
import { Input } from '@/components/ui/input'
import { Checkbox } from '@/components/ui/checkbox'
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
        onSuccess: (res) => {
          toast.success(res.message)
        },
        onError: (err) => {
          toast.error('操作失败: ' + (err as Error).message)
        },
      }
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
        onSuccess: (res) => {
          toast.success(res.message)
          setEditingPriority(false)
        },
        onError: (err) => {
          toast.error('操作失败: ' + (err as Error).message)
        },
      }
    )
  }

  const handleReset = () => {
    resetFailure.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
      },
      onError: (err) => {
        toast.error('操作失败: ' + (err as Error).message)
      },
    })
  }

  const handleForceRefresh = () => {
    forceRefresh.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
      },
      onError: (err) => {
        toast.error('刷新失败: ' + (err as Error).message)
      },
    })
  }

  const handleDelete = () => {
    if (!credential.disabled) {
      toast.error('请先禁用凭据再删除')
      setShowDeleteDialog(false)
      return
    }

    deleteCredential.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
        setShowDeleteDialog(false)
      },
      onError: (err) => {
        toast.error('删除失败: ' + (err as Error).message)
      },
    })
  }

  const hasFailures = credential.failureCount > 0 || credential.refreshFailureCount > 0
  const remainingPercent = balance ? Math.max(0, Math.min(100, 100 - balance.usagePercentage)) : 0
  const usageBarClass = !balance
    ? ''
    : balance.usagePercentage >= 90
      ? 'bg-red-500'
      : balance.usagePercentage >= 70
        ? 'bg-amber-500'
        : 'bg-emerald-500'

  return (
    <>
      <Card
        className={cn(
          'flex flex-col overflow-hidden transition-colors',
          credential.isCurrent && 'ring-2 ring-primary',
          credential.disabled && 'opacity-75'
        )}
      >
        <CardHeader className="p-4 pb-3">
          <div className="flex items-center justify-between gap-2">
            <div className="flex min-w-0 flex-1 items-center gap-2">
              <Checkbox
                checked={selected}
                onCheckedChange={onToggleSelect}
                className="shrink-0"
              />
              <CardTitle
                className="flex min-w-0 flex-1 items-center gap-1.5 text-sm font-semibold"
                title={credential.email || undefined}
              >
                <span className="truncate">{credential.email || `凭据 #${credential.id}`}</span>
                <span className="shrink-0 font-mono text-[10px] font-normal text-muted-foreground tabular-nums">
                  #{credential.id}
                </span>
                {credential.isCurrent && (
                  <Badge variant="success" className="h-5 shrink-0 px-1.5 text-[10px]">当前</Badge>
                )}
                {credential.disabled && (
                  <Badge variant="destructive" className="h-5 shrink-0 px-1.5 text-[10px]">
                    {credential.disabledReason || '已禁用'}
                  </Badge>
                )}
                {credential.hasProfileArn && (
                  <Badge variant="secondary" className="h-5 shrink-0 px-1.5 text-[10px]">ARN</Badge>
                )}
              </CardTitle>
            </div>
            <Switch
              checked={!credential.disabled}
              onCheckedChange={handleToggleDisabled}
              disabled={setDisabled.isPending}
              title={credential.disabled ? '启用凭据' : '禁用凭据'}
              className="shrink-0"
            />
          </div>
        </CardHeader>

        <CardContent className="flex flex-1 flex-col gap-3 p-4 pt-0">
          {/* 关键指标 */}
          <div className="grid grid-cols-2 gap-x-3 gap-y-2 rounded-md bg-muted/40 px-3 py-2.5 md:grid-cols-4 md:gap-2">
            <div className="min-w-0">
              <div className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">优先级</div>
              {editingPriority ? (
                <div className="mt-1 flex items-center gap-0.5">
                  <Input
                    type="number"
                    value={priorityValue}
                    onChange={(e) => setPriorityValue(e.target.value)}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter') handlePriorityChange()
                      if (e.key === 'Escape') {
                        setEditingPriority(false)
                        setPriorityValue(String(credential.priority))
                      }
                    }}
                    className="h-7 w-14 px-1.5 text-xs"
                    min="0"
                    autoFocus
                  />
                  <Button
                    size="sm"
                    variant="ghost"
                    className="h-7 w-7 p-0"
                    onClick={handlePriorityChange}
                    disabled={setPriority.isPending}
                    title="确认"
                  >
                    <Check className="h-3.5 w-3.5" />
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    className="h-7 w-7 p-0"
                    onClick={() => {
                      setEditingPriority(false)
                      setPriorityValue(String(credential.priority))
                    }}
                    title="取消"
                  >
                    <X className="h-3.5 w-3.5" />
                  </Button>
                </div>
              ) : (
                <button
                  type="button"
                  className="group mt-0.5 inline-flex items-center gap-1 text-base font-semibold leading-tight tabular-nums hover:text-primary"
                  onClick={() => setEditingPriority(true)}
                  title="点击编辑优先级"
                >
                  {credential.priority}
                  <Pencil className="h-2.5 w-2.5 opacity-0 transition-opacity group-hover:opacity-60" />
                </button>
              )}
            </div>
            <div className="min-w-0">
              <div className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">订阅</div>
              <div className="mt-0.5 truncate text-sm font-medium leading-tight" title={balance?.subscriptionTitle || undefined}>
                {loadingBalance ? (
                  <Loader2 className="inline h-3.5 w-3.5 animate-spin text-muted-foreground" />
                ) : (
                  balance?.subscriptionTitle || <span className="text-muted-foreground">—</span>
                )}
              </div>
            </div>
            <div className="min-w-0">
              <div className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground">成功次数</div>
              <div className="mt-0.5 text-base font-semibold leading-tight tabular-nums">{credential.successCount}</div>
            </div>
            <div className="min-w-0">
              <div className="text-[11px] font-medium uppercase tracking-wide text-muted-foreground" title="请求失败 / 刷新 Token 失败">失败 / 刷新</div>
              <div className={cn('mt-0.5 text-base font-semibold leading-tight tabular-nums', hasFailures && 'text-red-500')}>
                {credential.failureCount}<span className="text-muted-foreground/60"> / </span>{credential.refreshFailureCount}
              </div>
            </div>
          </div>

          {/* 剩余用量 */}
          <div className="space-y-1.5">
            <div className="flex items-center justify-between text-xs">
              <span className="text-muted-foreground">剩余用量</span>
              {loadingBalance ? (
                <span className="text-muted-foreground">
                  <Loader2 className="mr-1 inline h-3 w-3 animate-spin" />加载中
                </span>
              ) : balance ? (
                <span className="tabular-nums">
                  <span className="font-medium">{balance.remaining.toFixed(2)}</span>
                  <span className="text-muted-foreground"> / {balance.usageLimit.toFixed(2)}</span>
                  <span className="ml-2 text-muted-foreground">剩余 {remainingPercent.toFixed(1)}%</span>
                </span>
              ) : (
                <span className="text-muted-foreground">未知</span>
              )}
            </div>
            <div className="h-1.5 w-full overflow-hidden rounded-full bg-muted">
              {balance && (
                <div
                  className={cn('h-full transition-[width]', usageBarClass)}
                  style={{ width: `${remainingPercent}%` }}
                />
              )}
            </div>
          </div>

          {/* meta 行 */}
          <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-muted-foreground">
            <span className="inline-flex items-center gap-1">
              <Clock className="h-3 w-3" />
              {formatLastUsed(credential.lastUsedAt)}
            </span>
            {credential.hasProxy && (
              <span className="inline-flex min-w-0 items-center gap-1" title={credential.proxyUrl}>
                <Globe className="h-3 w-3 shrink-0" />
                <span className="truncate">{credential.proxyUrl}</span>
              </span>
            )}
          </div>

          {/* 操作按钮 */}
          <div className="mt-auto flex flex-wrap items-center gap-1.5 border-t pt-3">
            <Button
              size="sm"
              variant="default"
              className="h-8"
              onClick={() => onViewBalance(credential.id)}
            >
              <Wallet className="h-3.5 w-3.5" />
              余额
            </Button>
            <Button
              size="sm"
              variant="outline"
              className="h-8"
              onClick={handleForceRefresh}
              disabled={forceRefresh.isPending || credential.disabled}
              title={credential.disabled ? '已禁用的凭据无法刷新 Token' : '强制刷新 Token'}
            >
              <RefreshCw className={cn('h-3.5 w-3.5', forceRefresh.isPending && 'animate-spin')} />
              刷 Token
            </Button>
            <Button
              size="sm"
              variant="outline"
              className="h-8"
              onClick={handleReset}
              disabled={resetFailure.isPending || !hasFailures}
              title={!hasFailures ? '无失败计数可重置' : '重置失败计数'}
            >
              <RotateCcw className="h-3.5 w-3.5" />
              重置
            </Button>
            <div className="inline-flex items-center overflow-hidden rounded-md border">
              <Button
                size="sm"
                variant="ghost"
                className="h-8 w-8 rounded-none p-0"
                onClick={() => {
                  const newPriority = Math.max(0, credential.priority - 1)
                  setPriority.mutate(
                    { id: credential.id, priority: newPriority },
                    {
                      onSuccess: (res) => toast.success(res.message),
                      onError: (err) => toast.error('操作失败: ' + (err as Error).message),
                    }
                  )
                }}
                disabled={setPriority.isPending || credential.priority === 0}
                title="提高优先级（数字减 1）"
              >
                <ChevronUp className="h-3.5 w-3.5" />
              </Button>
              <div className="h-4 w-px bg-border" aria-hidden />
              <Button
                size="sm"
                variant="ghost"
                className="h-8 w-8 rounded-none p-0"
                onClick={() => {
                  const newPriority = credential.priority + 1
                  setPriority.mutate(
                    { id: credential.id, priority: newPriority },
                    {
                      onSuccess: (res) => toast.success(res.message),
                      onError: (err) => toast.error('操作失败: ' + (err as Error).message),
                    }
                  )
                }}
                disabled={setPriority.isPending}
                title="降低优先级（数字加 1）"
              >
                <ChevronDown className="h-3.5 w-3.5" />
              </Button>
            </div>
            <Button
              size="sm"
              variant="ghost"
              className="ml-auto h-8 w-8 p-0 text-destructive hover:bg-destructive/10 hover:text-destructive"
              onClick={() => setShowDeleteDialog(true)}
              disabled={!credential.disabled}
              title={!credential.disabled ? '需要先禁用凭据才能删除' : '删除凭据'}
            >
              <Trash2 className="h-3.5 w-3.5" />
            </Button>
          </div>
        </CardContent>
      </Card>

      {/* 删除确认对话框 */}
      <Dialog open={showDeleteDialog} onOpenChange={setShowDeleteDialog}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>确认删除凭据</DialogTitle>
            <DialogDescription>
              您确定要删除凭据 #{credential.id} 吗？此操作无法撤销。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setShowDeleteDialog(false)}
              disabled={deleteCredential.isPending}
            >
              取消
            </Button>
            <Button
              variant="destructive"
              onClick={handleDelete}
              disabled={deleteCredential.isPending || !credential.disabled}
            >
              确认删除
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}
