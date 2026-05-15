// 凭据状态响应
export interface CredentialsStatusResponse {
  total: number
  available: number
  currentId: number
  credentials: CredentialStatusItem[]
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
