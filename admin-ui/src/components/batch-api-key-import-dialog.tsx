import { useRef, useState, useMemo } from 'react'
import { toast } from 'sonner'
import { CheckCircle2, XCircle, AlertCircle, Loader2, Upload } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Textarea } from '@/components/ui/textarea'
import { useCredentials, useAddCredential, useDeleteCredential } from '@/hooks/use-credentials'
import { getCredentialBalance, setCredentialDisabled } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'

interface BatchApiKeyImportDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

interface VerificationResult {
  index: number
  status: 'pending' | 'checking' | 'verifying' | 'verified' | 'duplicate' | 'failed'
  error?: string
  usage?: string
  email?: string
  maskedKey: string
  credentialId?: number
  rollbackStatus?: 'success' | 'failed' | 'skipped'
  rollbackError?: string
}

// 与后端 token_manager 的 refresh_token_hash 脱敏规则保持一致：
// ASCII 且长度 > 16 时取「前 4...后 4」，否则 "***"。用于和已有 API Key 凭据去重比对。
function maskApiKey(key: string): string {
  // eslint-disable-next-line no-control-regex
  const isAscii = /^[\x00-\x7F]*$/.test(key)
  return isAscii && key.length > 16 ? `${key.slice(0, 4)}...${key.slice(-4)}` : '***'
}

// 单条解析结果：key 为 API Key 本体，region 为可选的凭据级 API region。
interface ParsedApiKey {
  key: string
  region?: string
}

// 从多行文本解析出 API Key 列表：按行拆分、去空白、去空行、按 key 去重（保留首次出现）。
// 每行支持 `ksk_xxx | region` 格式：`|` 后为该 key 走的 API region（如 eu-central-1），
// 省略 `|` 段时不带 region（后端回退默认 us-east-1）。region 只影响该凭据的请求落地区域，
// 无需在表单里另填，也不改动全局配置。
function parseApiKeys(raw: string): ParsedApiKey[] {
  const seen = new Set<string>()
  const keys: ParsedApiKey[] = []
  for (const line of raw.split(/\r?\n/)) {
    const trimmed = line.trim()
    if (!trimmed) continue
    // 仅按首个 `|` 切分，key 里不含 `|`，多余的 `|` 归入 region 段一并 trim。
    const sep = trimmed.indexOf('|')
    const key = (sep === -1 ? trimmed : trimmed.slice(0, sep)).trim()
    const region = sep === -1 ? '' : trimmed.slice(sep + 1).trim()
    if (!key || seen.has(key)) continue
    seen.add(key)
    keys.push(region ? { key, region } : { key })
  }
  return keys
}

export function BatchApiKeyImportDialog({ open, onOpenChange }: BatchApiKeyImportDialogProps) {
  const [textInput, setTextInput] = useState('')
  const [importing, setImporting] = useState(false)
  const [progress, setProgress] = useState({ current: 0, total: 0 })
  const [currentProcessing, setCurrentProcessing] = useState<string>('')
  const [results, setResults] = useState<VerificationResult[]>([])
  const fileInputRef = useRef<HTMLInputElement>(null)

  const handleFileSelect = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0]
    // 重置 value，确保连续选择同一文件也能再次触发 onChange
    e.target.value = ''
    if (!file) return
    try {
      const text = await file.text()
      setTextInput(text)
      setResults([])
      toast.success(`已读取文件: ${file.name}`)
    } catch (error) {
      toast.error('读取文件失败: ' + extractErrorMessage(error))
    }
  }

  const { data: existingCredentials } = useCredentials()
  const { mutateAsync: addCredential } = useAddCredential()
  const { mutateAsync: deleteCredential } = useDeleteCredential()

  const rollbackCredential = async (id: number): Promise<{ success: boolean; error?: string }> => {
    try {
      await setCredentialDisabled(id, true)
    } catch (error) {
      return { success: false, error: `禁用失败: ${extractErrorMessage(error)}` }
    }
    try {
      await deleteCredential(id)
      return { success: true }
    } catch (error) {
      return { success: false, error: `删除失败: ${extractErrorMessage(error)}` }
    }
  }

  const resetForm = () => {
    setTextInput('')
    setProgress({ current: 0, total: 0 })
    setCurrentProcessing('')
    setResults([])
  }

  // 预览解析结果
  const { parsedKeys, invalidCount } = useMemo(() => {
    const keys = parseApiKeys(textInput)
    return {
      parsedKeys: keys,
      invalidCount: keys.filter(k => !k.key.startsWith('ksk_')).length,
    }
  }, [textInput])

  const handleImport = async () => {
    const keys = parseApiKeys(textInput)
    if (keys.length === 0) {
      toast.error('没有可导入的 API Key')
      return
    }

    try {
      setImporting(true)
      setProgress({ current: 0, total: keys.length })

      const initialResults: VerificationResult[] = keys.map(({ key }, i) => ({
        index: i + 1,
        status: 'pending',
        maskedKey: maskApiKey(key),
      }))
      setResults(initialResults)

      // 已有 API Key 凭据的脱敏串集合，用于去重（api_key 凭据的 refreshTokenHash 即脱敏 key）
      const existingMasked = new Set(
        existingCredentials?.credentials
          .map(c => c.refreshTokenHash)
          .filter((hash): hash is string => Boolean(hash)) || []
      )

      let successCount = 0
      let duplicateCount = 0
      let failCount = 0

      for (let i = 0; i < keys.length; i++) {
        const { key, region } = keys[i]
        const masked = maskApiKey(key)

        setCurrentProcessing(`正在处理 ${masked}`)
        setResults(prev => {
          const next = [...prev]
          next[i] = { ...next[i], status: 'checking' }
          return next
        })

        // 格式校验
        if (!key.startsWith('ksk_')) {
          failCount++
          setResults(prev => {
            const next = [...prev]
            next[i] = { ...next[i], status: 'failed', error: '格式错误：API Key 应以 ksk_ 开头' }
            return next
          })
          setProgress({ current: i + 1, total: keys.length })
          continue
        }

        // 去重
        if (existingMasked.has(masked)) {
          duplicateCount++
          setResults(prev => {
            const next = [...prev]
            next[i] = { ...next[i], status: 'duplicate', error: '该 API Key 已存在' }
            return next
          })
          setProgress({ current: i + 1, total: keys.length })
          continue
        }

        setResults(prev => {
          const next = [...prev]
          next[i] = { ...next[i], status: 'verifying' }
          return next
        })

        let addedCredId: number | null = null

        try {
          const addedCred = await addCredential({
            kiroApiKey: key,
            authMethod: 'api_key',
            ...(region ? { apiRegion: region } : {}),
          })

          addedCredId = addedCred.credentialId

          await new Promise(resolve => setTimeout(resolve, 1000))

          const balance = await getCredentialBalance(addedCred.credentialId)

          successCount++
          existingMasked.add(masked)
          setCurrentProcessing(`验活成功: ${addedCred.email || masked}`)
          setResults(prev => {
            const next = [...prev]
            next[i] = {
              ...next[i],
              status: 'verified',
              usage: `${balance.currentUsage}/${balance.usageLimit}`,
              email: addedCred.email || undefined,
              credentialId: addedCred.credentialId,
            }
            return next
          })
        } catch (error) {
          let rollbackStatus: VerificationResult['rollbackStatus'] = 'skipped'
          let rollbackError: string | undefined

          if (addedCredId) {
            const result = await rollbackCredential(addedCredId)
            if (result.success) {
              rollbackStatus = 'success'
            } else {
              rollbackStatus = 'failed'
              rollbackError = result.error
            }
          }

          failCount++
          setResults(prev => {
            const next = [...prev]
            next[i] = {
              ...next[i],
              status: 'failed',
              error: extractErrorMessage(error),
              rollbackStatus,
              rollbackError,
            }
            return next
          })
        }

        setProgress({ current: i + 1, total: keys.length })
      }

      const parts: string[] = []
      if (successCount > 0) parts.push(`成功 ${successCount}`)
      if (duplicateCount > 0) parts.push(`重复 ${duplicateCount}`)
      if (failCount > 0) parts.push(`失败 ${failCount}`)

      if (failCount === 0 && duplicateCount === 0) {
        toast.success(`成功导入并验活 ${successCount} 个 API Key`)
      } else {
        toast.info(`导入完成：${parts.join('，')}`)
      }
    } catch (error) {
      toast.error('导入失败: ' + extractErrorMessage(error))
    } finally {
      setImporting(false)
    }
  }

  const getStatusIcon = (status: VerificationResult['status']) => {
    switch (status) {
      case 'pending':
        return <div className="w-5 h-5 rounded-full border-2 border-gray-300" />
      case 'checking':
      case 'verifying':
        return <Loader2 className="w-5 h-5 animate-spin text-muted-foreground" />
      case 'verified':
        return <CheckCircle2 className="w-5 h-5 text-ok" />
      case 'duplicate':
        return <AlertCircle className="w-5 h-5 text-warn" />
      case 'failed':
        return <XCircle className="w-5 h-5 text-bad" />
    }
  }

  const getStatusText = (result: VerificationResult) => {
    switch (result.status) {
      case 'pending': return '等待中'
      case 'checking': return '检查重复...'
      case 'verifying': return '验活中...'
      case 'verified': return '验活成功'
      case 'duplicate': return '重复凭据'
      case 'failed':
        if (result.rollbackStatus === 'success') return '验活失败（已排除）'
        if (result.rollbackStatus === 'failed') return '验活失败（未排除）'
        return '验活失败（未创建）'
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(newOpen) => {
        if (!newOpen && importing) return
        if (!newOpen) resetForm()
        onOpenChange(newOpen)
      }}
    >
      <DialogContent className="sm:max-w-2xl max-h-[80vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>批量导入 API Key（自动验活）</DialogTitle>
        </DialogHeader>

        <div className="flex-1 overflow-y-auto space-y-4 py-4">
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <label className="text-sm font-medium">Kiro API Key（每行一个）</label>
              <input
                ref={fileInputRef}
                type="file"
                accept=".txt,text/plain"
                className="hidden"
                onChange={handleFileSelect}
                disabled={importing}
              />
              <Button
                type="button"
                variant="outline"
                size="sm"
                onClick={() => fileInputRef.current?.click()}
                disabled={importing}
              >
                <Upload className="w-4 h-4" />
                从文件导入
              </Button>
            </div>
            <Textarea
              placeholder={'每行粘贴一个 Kiro API Key，例如：\nksk_xxxxxxxxxxxxxxxx\nksk_yyyyyyyyyyyyyyyy | eu-central-1'}
              value={textInput}
              onChange={(e) => setTextInput(e.target.value)}
              disabled={importing}
              className="min-h-[200px] font-mono"
            />
            <p className="text-xs text-muted-foreground">
              💡 导入时逐条验活并拉取额度，失败的凭据会被自动排除
            </p>
            <p className="text-xs text-muted-foreground">
              🌍 指定区域：<code>ksk_xxx | eu-central-1</code>（`|` 后为该 key 的 API region，省略则默认 us-east-1）
            </p>
          </div>

          {/* 解析预览 */}
          {parsedKeys.length > 0 && !importing && results.length === 0 && (
            <div className="text-sm text-muted-foreground">
              识别到 {parsedKeys.length} 个 Key（已按行去重）
              {parsedKeys.some(k => k.region) && (
                <span>，其中 {parsedKeys.filter(k => k.region).length} 个指定了区域</span>
              )}
              {invalidCount > 0 && (
                <span className="text-warn">，其中 {invalidCount} 个不以 ksk_ 开头，将被标记失败</span>
              )}
            </div>
          )}

          {(importing || results.length > 0) && (
            <>
              <div className="space-y-2">
                <div className="flex justify-between text-sm">
                  <span>{importing ? '导入进度' : '导入完成'}</span>
                  <span>{progress.current} / {progress.total}</span>
                </div>
                <div className="w-full bg-secondary rounded-full h-2">
                  <div
                    className="bg-primary h-2 rounded-full transition-all"
                    style={{ width: `${progress.total > 0 ? (progress.current / progress.total) * 100 : 0}%` }}
                  />
                </div>
                {importing && currentProcessing && (
                  <div className="text-xs text-muted-foreground">{currentProcessing}</div>
                )}
              </div>

              <div className="flex gap-4 text-sm">
                <span className="text-ok">
                  ✓ 成功: {results.filter(r => r.status === 'verified').length}
                </span>
                <span className="text-warn">
                  ⚠ 重复: {results.filter(r => r.status === 'duplicate').length}
                </span>
                <span className="text-bad">
                  ✗ 失败: {results.filter(r => r.status === 'failed').length}
                </span>
              </div>

              <div className="border rounded-md divide-y max-h-[300px] overflow-y-auto">
                {results.map((result) => (
                  <div key={result.index} className="p-3">
                    <div className="flex items-start gap-3">
                      {getStatusIcon(result.status)}
                      <div className="flex-1 min-w-0">
                        <div className="flex items-center gap-2">
                          <span className="text-sm font-medium">
                            {result.email || result.maskedKey}
                          </span>
                          <span className="text-xs text-muted-foreground">
                            {getStatusText(result)}
                          </span>
                        </div>
                        {result.usage && (
                          <div className="text-xs text-muted-foreground mt-1">用量: {result.usage}</div>
                        )}
                        {result.error && (
                          <div className="text-xs text-bad mt-1">{result.error}</div>
                        )}
                        {result.rollbackError && (
                          <div className="text-xs text-bad mt-1">回滚失败: {result.rollbackError}</div>
                        )}
                      </div>
                    </div>
                  </div>
                ))}
              </div>
            </>
          )}
        </div>

        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            onClick={() => { onOpenChange(false); resetForm() }}
            disabled={importing}
          >
            {importing ? '导入中...' : results.length > 0 ? '关闭' : '取消'}
          </Button>
          {results.length === 0 && (
            <Button
              type="button"
              onClick={handleImport}
              disabled={importing || parsedKeys.length === 0}
            >
              开始导入并验活
            </Button>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
