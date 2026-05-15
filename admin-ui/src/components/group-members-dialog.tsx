import { useEffect, useMemo, useState } from 'react'
import { toast } from 'sonner'
import { Search, X, Check, AlertTriangle } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { useCredentials, useBatchSetCredentialGroup } from '@/hooks/use-credentials'
import { cn, extractErrorMessage } from '@/lib/utils'

interface GroupMembersDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  /** 当前编辑的分组名（null 表示弹窗未指向任何分组，不渲染内容） */
  groupName: string | null
}

export function GroupMembersDialog({ open, onOpenChange, groupName }: GroupMembersDialogProps) {
  const { data } = useCredentials()
  const batchSetGroup = useBatchSetCredentialGroup()

  const allCreds = useMemo(() => data?.credentials || [], [data])

  // 初始勾选集合：当前属于 groupName 的凭据
  const initialSelectedIds = useMemo(() => {
    if (!groupName) return new Set<number>()
    return new Set(allCreds.filter(c => c.group === groupName).map(c => c.id))
  }, [allCreds, groupName])

  const [selectedIds, setSelectedIds] = useState<Set<number>>(new Set())
  const [search, setSearch] = useState('')

  // 弹窗每次打开（或分组切换）时重置勾选状态
  useEffect(() => {
    if (open) {
      setSelectedIds(new Set(initialSelectedIds))
      setSearch('')
    }
  }, [open, initialSelectedIds])

  const filteredCreds = useMemo(() => {
    const q = search.trim().toLowerCase()
    if (!q) return allCreds
    return allCreds.filter(
      c =>
        (c.email || '').toLowerCase().includes(q) ||
        String(c.id).includes(q) ||
        (c.group || '').toLowerCase().includes(q),
    )
  }, [allCreds, search])

  const toggle = (id: number) => {
    setSelectedIds(prev => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }

  const toggleVisibleAll = () => {
    const visibleIds = filteredCreds.map(c => c.id)
    const allOn = visibleIds.every(id => selectedIds.has(id))
    setSelectedIds(prev => {
      const next = new Set(prev)
      if (allOn) visibleIds.forEach(id => next.delete(id))
      else visibleIds.forEach(id => next.add(id))
      return next
    })
  }

  // diff：要加入本组 / 要移出本组
  const { toAdd, toRemove } = useMemo(() => {
    const toAdd: number[] = []
    const toRemove: number[] = []
    for (const c of allCreds) {
      const wasInGroup = c.group === groupName
      const isSelected = selectedIds.has(c.id)
      if (isSelected && !wasInGroup) toAdd.push(c.id)
      else if (!isSelected && wasInGroup) toRemove.push(c.id)
    }
    return { toAdd, toRemove }
  }, [allCreds, selectedIds, groupName])

  const hasChanges = toAdd.length > 0 || toRemove.length > 0

  const handleSubmit = async () => {
    if (!groupName || !hasChanges) return

    try {
      let totalSucceeded = 0
      const failures: { id: number; error: string }[] = []

      if (toAdd.length > 0) {
        const res = await batchSetGroup.mutateAsync({ credentialIds: toAdd, group: groupName })
        totalSucceeded += res.succeeded.length
        failures.push(...res.failed)
      }
      if (toRemove.length > 0) {
        const res = await batchSetGroup.mutateAsync({ credentialIds: toRemove, group: null })
        totalSucceeded += res.succeeded.length
        failures.push(...res.failed)
      }

      const total = toAdd.length + toRemove.length
      if (failures.length === 0) {
        toast.success(`已更新 ${totalSucceeded}/${total} 个凭据的分组绑定`)
        onOpenChange(false)
      } else {
        toast.warning(
          `更新完成：成功 ${totalSucceeded}/${total}，失败 ${failures.length} —— ${failures
            .slice(0, 3)
            .map(f => `#${f.id} ${f.error}`)
            .join('；')}${failures.length > 3 ? '…' : ''}`,
        )
      }
    } catch (err) {
      toast.error(`保存失败: ${extractErrorMessage(err)}`)
    }
  }

  const allVisibleChecked =
    filteredCreds.length > 0 && filteredCreds.every(c => selectedIds.has(c.id))

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-2xl max-h-[85vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>
            管理分组成员
            {groupName && (
              <span className="ml-2 font-mono text-sm text-muted-foreground">{groupName}</span>
            )}
          </DialogTitle>
          <DialogDescription>
            勾选要纳入此分组的凭据；取消勾选会把原本属于本组的凭据移出（清空 group 绑定）。
          </DialogDescription>
        </DialogHeader>

        <div className="flex flex-col min-h-0 flex-1 gap-3 py-2">
          {/* 搜索 */}
          <div className="relative">
            <Search className="pointer-events-none absolute left-3 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
            <Input
              type="search"
              placeholder="搜索邮箱 / ID / 分组"
              value={search}
              onChange={e => setSearch(e.target.value)}
              className="pl-9 pr-9"
            />
            {search && (
              <button
                onClick={() => setSearch('')}
                className="absolute right-2 top-1/2 flex h-6 w-6 -translate-y-1/2 cursor-pointer items-center justify-center rounded-md text-muted-foreground hover:bg-muted hover:text-foreground"
                aria-label="clear"
              >
                <X className="h-3.5 w-3.5" />
              </button>
            )}
          </div>

          {/* 工具条 */}
          <div className="flex items-center justify-between font-mono text-2xs text-muted-foreground">
            <button
              onClick={toggleVisibleAll}
              disabled={filteredCreds.length === 0}
              className="inline-flex cursor-pointer items-center gap-1.5 rounded-md px-1.5 py-1 hover:text-foreground disabled:cursor-not-allowed disabled:opacity-40"
            >
              <span
                className={cn(
                  'flex h-4 w-4 items-center justify-center rounded border',
                  allVisibleChecked
                    ? 'border-primary bg-primary text-primary-foreground'
                    : 'border-border bg-background',
                )}
              >
                {allVisibleChecked && <Check className="h-3 w-3" />}
              </span>
              {allVisibleChecked ? '取消可见全选' : '全选可见'}
            </button>
            <div>
              已选 <span className="text-foreground">{selectedIds.size}</span>
              {hasChanges && (
                <>
                  <span className="mx-1.5 text-border">·</span>
                  <span className="text-foreground">
                    {toAdd.length > 0 && `+${toAdd.length}`}
                    {toAdd.length > 0 && toRemove.length > 0 && ' '}
                    {toRemove.length > 0 && `-${toRemove.length}`}
                  </span>
                </>
              )}
            </div>
          </div>

          {/* 凭据列表 */}
          <div className="min-h-0 flex-1 overflow-y-auto rounded-lg border border-border bg-surface">
            {filteredCreds.length === 0 ? (
              <div className="py-8 text-center text-sm text-muted-foreground">
                {search ? '没有匹配的凭据' : '暂无凭据'}
              </div>
            ) : (
              <ul className="divide-y divide-border">
                {filteredCreds.map(c => {
                  const checked = selectedIds.has(c.id)
                  const otherGroup = c.group && c.group !== groupName ? c.group : null
                  return (
                    <li key={c.id}>
                      <button
                        onClick={() => toggle(c.id)}
                        className={cn(
                          'flex w-full cursor-pointer items-center gap-3 px-3 py-2 text-left transition-colors hover:bg-muted',
                          checked && 'bg-primary/[0.04]',
                        )}
                      >
                        <span
                          className={cn(
                            'flex h-4 w-4 shrink-0 items-center justify-center rounded border transition-colors',
                            checked
                              ? 'border-primary bg-primary text-primary-foreground'
                              : 'border-border bg-background',
                          )}
                        >
                          {checked && <Check className="h-3 w-3" />}
                        </span>
                        <div className="min-w-0 flex-1">
                          <div className="flex items-center gap-1.5 text-sm">
                            <span className="tnum font-mono text-2xs text-muted-foreground">
                              #{String(c.id).padStart(3, '0')}
                            </span>
                            <span className="truncate font-medium">{c.email || `凭据 #${c.id}`}</span>
                          </div>
                          <div className="mt-0.5 flex items-center gap-1.5 font-mono text-2xs text-muted-foreground">
                            {c.disabled && <span className="text-bad">已禁用</span>}
                            {otherGroup && (
                              <span
                                className="inline-flex items-center gap-0.5 text-warn"
                                title={`当前属于 ${otherGroup}，勾选会迁移到 ${groupName}`}
                              >
                                <AlertTriangle className="h-2.5 w-2.5" />
                                已在 {otherGroup}
                              </span>
                            )}
                            {c.proxyUrl && (
                              <span className="truncate" title={c.proxyUrl}>
                                独立代理 · {c.proxyUrl}
                              </span>
                            )}
                          </div>
                        </div>
                      </button>
                    </li>
                  )
                })}
              </ul>
            )}
          </div>
        </div>

        <DialogFooter className="gap-2 sm:gap-2">
          <Button
            variant="outline"
            onClick={() => onOpenChange(false)}
            disabled={batchSetGroup.isPending}
          >
            取消
          </Button>
          <Button
            onClick={handleSubmit}
            disabled={!hasChanges || batchSetGroup.isPending || !groupName}
          >
            {batchSetGroup.isPending
              ? '保存中…'
              : hasChanges
                ? `保存（+${toAdd.length} / -${toRemove.length}）`
                : '无变更'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
