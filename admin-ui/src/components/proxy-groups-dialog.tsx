import { useState, useMemo, useEffect } from 'react'
import { toast } from 'sonner'
import { Plus, Trash2, Pencil, X, Check, Users } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  useProxyGroups,
  useUpsertProxyGroup,
  useDeleteProxyGroup,
} from '@/hooks/use-credentials'
import { extractErrorMessage, cn } from '@/lib/utils'
import type { ProxyGroupItem } from '@/types/api'
import { GroupMembersDialog } from '@/components/group-members-dialog'
import { useCredentials } from '@/hooks/use-credentials'

interface ProxyGroupsDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

type EditState =
  | { mode: 'idle' }
  | { mode: 'create' }
  | { mode: 'edit'; name: string }

interface FormState {
  name: string
  proxyUrl: string
  proxyUsername: string
  proxyPassword: string
  description: string
}

const emptyForm: FormState = {
  name: '',
  proxyUrl: '',
  proxyUsername: '',
  proxyPassword: '',
  description: '',
}

export function ProxyGroupsDialog({ open, onOpenChange }: ProxyGroupsDialogProps) {
  const { data, isLoading } = useProxyGroups()
  const upsertGroup = useUpsertProxyGroup()
  const deleteGroup = useDeleteProxyGroup()

  const [editState, setEditState] = useState<EditState>({ mode: 'idle' })
  const [form, setForm] = useState<FormState>(emptyForm)
  const [membersDialogGroup, setMembersDialogGroup] = useState<string | null>(null)

  const { data: credentialsData } = useCredentials()

  // Reset state when dialog closes
  useEffect(() => {
    if (!open) {
      setEditState({ mode: 'idle' })
      setForm(emptyForm)
    }
  }, [open])

  const groups = useMemo(() => data?.groups || [], [data])

  // 计算每个分组成员数
  const memberCountByGroup = useMemo(() => {
    const map = new Map<string, number>()
    for (const c of credentialsData?.credentials || []) {
      if (c.group) map.set(c.group, (map.get(c.group) || 0) + 1)
    }
    return map
  }, [credentialsData])

  const startCreate = () => {
    setForm(emptyForm)
    setEditState({ mode: 'create' })
  }

  const startEdit = (group: ProxyGroupItem) => {
    setForm({
      name: group.name,
      proxyUrl: group.proxyUrl,
      proxyUsername: group.proxyUsername || '',
      proxyPassword: group.proxyPassword || '',
      description: group.description || '',
    })
    setEditState({ mode: 'edit', name: group.name })
  }

  const cancelEdit = () => {
    setEditState({ mode: 'idle' })
    setForm(emptyForm)
  }

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()
    if (editState.mode === 'idle') return

    const name = form.name.trim()
    if (!name) {
      toast.error('分组名称不能为空')
      return
    }
    if (!form.proxyUrl.trim()) {
      toast.error('代理 URL 不能为空（输入 "direct" 表示显式不走代理）')
      return
    }

    // 编辑场景下不允许改 name；新增时检查重名
    if (editState.mode === 'create' && groups.some(g => g.name === name)) {
      toast.error(`分组 "${name}" 已存在`)
      return
    }

    const targetName = editState.mode === 'edit' ? editState.name : name

    upsertGroup.mutate(
      {
        name: targetName,
        req: {
          proxyUrl: form.proxyUrl.trim(),
          proxyUsername: form.proxyUsername.trim() || undefined,
          proxyPassword: form.proxyPassword.trim() || undefined,
          description: form.description.trim() || undefined,
        },
      },
      {
        onSuccess: res => {
          toast.success(res.message)
          cancelEdit()
        },
        onError: err => toast.error(`保存失败: ${extractErrorMessage(err)}`),
      },
    )
  }

  const handleDelete = (group: ProxyGroupItem) => {
    if (!confirm(`确定要删除代理分组 "${group.name}" 吗？\n\n引用该分组的凭据会回退到全局代理。`)) {
      return
    }
    deleteGroup.mutate(group.name, {
      onSuccess: res => {
        toast.success(res.message)
        if (editState.mode === 'edit' && editState.name === group.name) {
          cancelEdit()
        }
      },
      onError: err => toast.error(`删除失败: ${extractErrorMessage(err)}`),
    })
  }

  const isEditing = editState.mode !== 'idle'

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-2xl max-h-[85vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>代理分组</DialogTitle>
          <DialogDescription>
            为一组凭据共享同一份代理配置。解析顺序：凭据独立代理 &gt; 分组代理 &gt; 全局代理。
          </DialogDescription>
        </DialogHeader>

        <div className="flex flex-col min-h-0 flex-1 gap-4 py-2">
          {/* 列表区 */}
          <div className="flex items-center justify-between">
            <span className="font-mono text-2xs uppercase tracking-wider text-muted-foreground">
              共 {groups.length} 个分组
            </span>
            <Button
              size="sm"
              variant="outline"
              onClick={startCreate}
              disabled={isEditing}
            >
              <Plus className="h-3.5 w-3.5" />
              新建分组
            </Button>
          </div>

          <div className="min-h-0 flex-1 overflow-y-auto space-y-2 pr-1">
            {isLoading ? (
              <div className="py-8 text-center text-sm text-muted-foreground">加载中…</div>
            ) : groups.length === 0 && editState.mode !== 'create' ? (
              <div className="py-8 text-center text-sm text-muted-foreground">
                还没有代理分组，点右上角"新建分组"开始
              </div>
            ) : (
              groups.map(g => {
                const isEditingThis = editState.mode === 'edit' && editState.name === g.name
                if (isEditingThis) return null
                return (
                  <div
                    key={g.name}
                    className="flex items-center justify-between gap-3 rounded-lg border border-border bg-surface p-3"
                  >
                    <div className="min-w-0 flex-1">
                      <div className="flex items-center gap-2">
                        <span className="font-mono text-sm font-semibold">{g.name}</span>
                        <span className="font-mono text-2xs text-muted-foreground truncate">
                          {g.proxyUrl}
                        </span>
                        <span className="ml-auto tnum font-mono text-2xs text-muted-foreground">
                          {memberCountByGroup.get(g.name) || 0} 个成员
                        </span>
                      </div>
                      {(g.description || g.proxyUsername) && (
                        <div className="mt-1 font-mono text-2xs text-muted-foreground truncate">
                          {g.proxyUsername && <span>auth · {g.proxyUsername}</span>}
                          {g.description && (
                            <>
                              {g.proxyUsername && <span className="mx-1.5">·</span>}
                              <span>{g.description}</span>
                            </>
                          )}
                        </div>
                      )}
                    </div>
                    <div className="flex shrink-0 items-center gap-1">
                      <Button
                        size="sm"
                        variant="ghost"
                        onClick={() => setMembersDialogGroup(g.name)}
                        disabled={isEditing}
                        aria-label="管理成员"
                        title="批量加入/移出凭据"
                      >
                        <Users className="h-3.5 w-3.5" />
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        onClick={() => startEdit(g)}
                        disabled={isEditing}
                        aria-label="编辑"
                      >
                        <Pencil className="h-3.5 w-3.5" />
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        onClick={() => handleDelete(g)}
                        disabled={isEditing || deleteGroup.isPending}
                        aria-label="删除"
                        className="text-bad hover:text-bad"
                      >
                        <Trash2 className="h-3.5 w-3.5" />
                      </Button>
                    </div>
                  </div>
                )
              })
            )}

            {/* 编辑/新建表单内嵌在列表中 */}
            {isEditing && (
              <form
                onSubmit={handleSubmit}
                className={cn(
                  'rounded-lg border bg-surface p-3 space-y-3',
                  editState.mode === 'create' ? 'border-primary/50' : 'border-primary/40',
                )}
              >
                <div className="flex items-center justify-between">
                  <span className="text-xs font-semibold">
                    {editState.mode === 'create' ? '新建分组' : `编辑 · ${editState.name}`}
                  </span>
                  <button
                    type="button"
                    onClick={cancelEdit}
                    className="flex h-6 w-6 cursor-pointer items-center justify-center rounded-md text-muted-foreground hover:bg-muted hover:text-foreground"
                    aria-label="取消"
                  >
                    <X className="h-3.5 w-3.5" />
                  </button>
                </div>

                <div className="space-y-2">
                  <label className="text-xs font-medium text-muted-foreground">
                    分组名称 <span className="text-bad">*</span>
                  </label>
                  <Input
                    placeholder="例如 us-pool"
                    value={form.name}
                    onChange={e => setForm(f => ({ ...f, name: e.target.value }))}
                    disabled={editState.mode === 'edit' || upsertGroup.isPending}
                  />
                  {editState.mode === 'edit' && (
                    <p className="text-2xs text-muted-foreground">
                      分组名是凭据引用 key，不可修改；如需重命名请删除后重建
                    </p>
                  )}
                </div>

                <div className="space-y-2">
                  <label className="text-xs font-medium text-muted-foreground">
                    代理 URL <span className="text-bad">*</span>
                  </label>
                  <Input
                    placeholder='例如 socks5://host:1080 或 "direct"'
                    value={form.proxyUrl}
                    onChange={e => setForm(f => ({ ...f, proxyUrl: e.target.value }))}
                    disabled={upsertGroup.isPending}
                  />
                </div>

                <div className="grid grid-cols-2 gap-2">
                  <div className="space-y-2">
                    <label className="text-xs font-medium text-muted-foreground">用户名</label>
                    <Input
                      placeholder="可选"
                      value={form.proxyUsername}
                      onChange={e => setForm(f => ({ ...f, proxyUsername: e.target.value }))}
                      disabled={upsertGroup.isPending}
                    />
                  </div>
                  <div className="space-y-2">
                    <label className="text-xs font-medium text-muted-foreground">密码</label>
                    <Input
                      type="password"
                      placeholder="可选"
                      value={form.proxyPassword}
                      onChange={e => setForm(f => ({ ...f, proxyPassword: e.target.value }))}
                      disabled={upsertGroup.isPending}
                    />
                  </div>
                </div>

                <div className="space-y-2">
                  <label className="text-xs font-medium text-muted-foreground">说明</label>
                  <Input
                    placeholder="可选，仅用于前端显示"
                    value={form.description}
                    onChange={e => setForm(f => ({ ...f, description: e.target.value }))}
                    disabled={upsertGroup.isPending}
                  />
                </div>

                <div className="flex justify-end gap-2 pt-1">
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    onClick={cancelEdit}
                    disabled={upsertGroup.isPending}
                  >
                    取消
                  </Button>
                  <Button type="submit" size="sm" disabled={upsertGroup.isPending}>
                    <Check className="h-3.5 w-3.5" />
                    {upsertGroup.isPending ? '保存中…' : '保存'}
                  </Button>
                </div>
              </form>
            )}
          </div>
        </div>
      </DialogContent>
      <GroupMembersDialog
        open={membersDialogGroup !== null}
        onOpenChange={open => {
          if (!open) setMembersDialogGroup(null)
        }}
        groupName={membersDialogGroup}
      />
    </Dialog>
  )
}
