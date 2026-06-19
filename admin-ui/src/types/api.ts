// 调用方角色
export type AdminRole = 'admin' | 'guest'

export interface MeResponse {
  role: AdminRole
}

// 凭据状态响应
export interface CredentialsStatusResponse {
  total: number
  available: number
  currentId: number
  credentials: CredentialStatusItem[]
  /** 全局默认 RPM 上限（不存在时表示未配置） */
  defaultRpmLimit?: number
  /** 全局默认并发上限（不存在时表示未配置） */
  defaultConcurrencyLimit?: number
}

// 单个凭据状态
export interface CredentialStatusItem {
  id: number
  priority: number
  disabled: boolean
  failureCount: number
  isCurrent: boolean
  expiresAt: string | null
  authMethod: string | null
  hasProfileArn: boolean
  email?: string
  refreshTokenHash?: string
  successCount: number
  lastUsedAt: string | null
  hasProxy: boolean
  proxyUrl?: string
  /** 凭据所属代理分组名称 */
  group?: string
  refreshFailureCount: number
  disabledReason?: string
  /** 上游 429 冷却到期时间（RFC3339）；不存在时表示未在冷却 */
  throttledUntil?: string
  /** 凭据级 RPM 上限覆盖（不存在=回退全局默认；0=显式不限流） */
  rpmLimit?: number
  /** 最近 60s 滑动窗口内的请求数（默认 0） */
  rpmCurrent?: number
  /** 凭据级并发上限覆盖（不存在=回退全局默认；0=显式不限并发） */
  concurrencyLimit?: number
  /** 当前在途请求数（默认 0） */
  concurrencyCurrent?: number
  /** overage（超额计费）上次下发状态（不存在=从未下发，状态未知） */
  overage?: boolean
  /** 缓存的余额（来自服务端 balance_cache，非实时查询；用于列表直接内联显示） */
  balance?: BalanceResponse
  /** 余额缓存时间（Unix 秒） */
  balanceCachedAt?: number
}

// 余额响应
export interface BalanceResponse {
  id: number
  subscriptionTitle: string | null
  currentUsage: number
  usageLimit: number
  remaining: number
  usagePercentage: number
  nextResetAt: number | null
  /** 超额计费状态（ENABLED / DISABLED，上游真实下发） */
  overageStatus: string | null
  /** 当前超额用量（已越过额度的部分） */
  currentOverages: number
  /** 已产生的超额费用 */
  overageCharges: number
  /** 超额单价（每单位费用） */
  overageRate: number
  /** 超额上限 */
  overageCap: number
  /** 货币（如 USD） */
  currency: string | null
}

// 凭据可用模型响应
export interface ModelsResponse {
  id: number
  /** 上游 ListAvailableModels 返回的模型 id 列表 */
  models: string[]
}

// 成功响应
export interface SuccessResponse {
  success: boolean
  message: string
}

// 错误响应
export interface AdminErrorResponse {
  error: {
    type: string
    message: string
  }
}

// 请求类型
export interface SetDisabledRequest {
  disabled: boolean
}

export interface SetPriorityRequest {
  priority: number
}

export interface SetRpmLimitRequest {
  /** null/undefined：清除覆盖回退全局；0：显式不限流；正整数：限制为 n 次/分钟 */
  rpmLimit: number | null
}

export interface SetConcurrencyLimitRequest {
  /** null/undefined：清除覆盖回退全局；0：显式不限并发；正整数：最多 n 个同时在途 */
  concurrencyLimit: number | null
}

// 添加凭据请求
export interface AddCredentialRequest {
  refreshToken: string
  email?: string
  authMethod?: 'social' | 'idc'
  clientId?: string
  clientSecret?: string
  priority?: number
  authRegion?: string
  apiRegion?: string
  machineId?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  /** 凭据所属代理分组名称（可选） */
  group?: string
}

// 添加凭据响应
export interface AddCredentialResponse {
  success: boolean
  message: string
  credentialId: number
  email?: string
}

// ============ 代理分组管理 ============

export interface ProxyGroupItem {
  name: string
  proxyUrl: string
  proxyUsername?: string
  proxyPassword?: string
  description?: string
}

export interface ProxyGroupsResponse {
  groups: ProxyGroupItem[]
}

export interface UpsertProxyGroupRequest {
  proxyUrl: string
  proxyUsername?: string
  proxyPassword?: string
  description?: string
}

export interface SetCredentialGroupRequest {
  group: string | null
}

export interface BatchSetCredentialGroupRequest {
  credentialIds: number[]
  /** null/空字符串表示清空分组绑定 */
  group: string | null
}

export interface BatchSetCredentialGroupFailure {
  id: number
  error: string
}

export interface BatchSetCredentialGroupResponse {
  total: number
  succeeded: number[]
  failed: BatchSetCredentialGroupFailure[]
}

export interface BatchSetPriorityResponse {
  total: number
  succeeded: number[]
  failed: BatchSetCredentialGroupFailure[]
}

export interface BatchSetRpmLimitResponse {
  total: number
  succeeded: number[]
  failed: BatchSetCredentialGroupFailure[]
}

export interface BatchSetDisabledResponse {
  total: number
  succeeded: number[]
  failed: BatchSetCredentialGroupFailure[]
}

export interface BatchSetOverageResponse {
  total: number
  succeeded: number[]
  failed: BatchSetCredentialGroupFailure[]
}

export interface BatchSetConcurrencyLimitResponse {
  total: number
  succeeded: number[]
  failed: BatchSetCredentialGroupFailure[]
}

export interface DefaultRpmLimitResponse {
  /** 全局默认 RPM（null=未配置；0=显式不限流；正整数=限制） */
  rpmLimit: number | null
}

export interface DefaultConcurrencyLimitResponse {
  /** 全局默认并发上限（null=未配置；0=显式不限并发；正整数=限制） */
  concurrencyLimit: number | null
}

// 计费累计统计（进程维度，落盘到 billing_stats.json）
export interface BillingStatsResponse {
  /** 累计请求数 */
  requests: number
  /** 累计实际成本（USD，上游折扣后真实成本） */
  actual_cost_usd: number
  /** 累计官方折算价（USD，Anthropic 零售价） */
  official_price_usd: number
  /** 累计毛利（USD，official − actual，可为负） */
  margin_usd: number
}

// ━━━━━━━━━━ 时序统计（曲线 / 分析） ━━━━━━━━━━

/** 曲线分桶粒度 */
export type StatsBucket = 'hour' | 'day'
/** 曲线分组维度 */
export type StatsGroupBy = 'none' | 'model' | 'credential'

/** 一个时间桶的聚合点 */
export interface StatsTimeBucket {
  /** 桶起始时间（Unix 秒） */
  bucket: number
  /** 分组键（分组时存在）：model 名 或 credential id */
  group?: string
  requests: number
  actual_usd: number
  official_usd: number
  margin_usd: number
  input_tokens: number
  cache_read: number
  cache_creation: number
  output_tokens: number
  /** 该桶内 max_tokens 截断请求数 */
  truncated: number
  avg_ttft_ms: number
  avg_elapsed_ms: number
}

/** 一个分组（或全量）的区间汇总 */
export interface StatGroup {
  /** 分组键：model 名 / credential id；全量为空串 */
  key: string
  requests: number
  actual_usd: number
  official_usd: number
  margin_usd: number
  input_tokens: number
  cache_read: number
  cache_creation: number
  output_tokens: number
  truncated: number
  avg_ttft_ms: number
  avg_elapsed_ms: number
}

/** 区间汇总：全量 + 按模型 + 按凭据 */
export interface StatsSummaryResponse {
  total: StatGroup
  by_model: StatGroup[]
  by_credential: StatGroup[]
}
