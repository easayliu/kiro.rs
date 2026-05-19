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

export interface DefaultRpmLimitResponse {
  /** 全局默认 RPM（null=未配置；0=显式不限流；正整数=限制） */
  rpmLimit: number | null
}
