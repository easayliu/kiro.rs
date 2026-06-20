import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  CredentialsStatusResponse,
  BalanceResponse,
  ModelsResponse,
  SuccessResponse,
  SetDisabledRequest,
  SetPriorityRequest,
  SetRpmLimitRequest,
  SetConcurrencyLimitRequest,
  AddCredentialRequest,
  AddCredentialResponse,
  ProxyGroupsResponse,
  UpsertProxyGroupRequest,
  BatchSetCredentialGroupResponse,
  BatchDeleteCredentialsResponse,
  BatchSetPriorityResponse,
  BatchSetRpmLimitResponse,
  BatchSetConcurrencyLimitResponse,
  BatchSetDisabledResponse,
  BatchSetOverageResponse,
  DefaultRpmLimitResponse,
  DefaultConcurrencyLimitResponse,
  MeResponse,
  BillingStatsResponse,
  StatsBucket,
  StatsGroupBy,
  StatsTimeBucket,
  StatsSummaryResponse,
} from '@/types/api'

// 创建 axios 实例
const api = axios.create({
  baseURL: '/api/admin',
  headers: {
    'Content-Type': 'application/json',
  },
})

// 请求拦截器添加 API Key
api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) {
    config.headers['x-api-key'] = apiKey
  }
  return config
})

// 获取当前调用方角色
export async function getMe(): Promise<MeResponse> {
  const { data } = await api.get<MeResponse>('/me')
  return data
}

// 获取所有凭据状态
export async function getCredentials(): Promise<CredentialsStatusResponse> {
  const { data } = await api.get<CredentialsStatusResponse>('/credentials')
  return data
}

// 设置凭据禁用状态
export async function setCredentialDisabled(
  id: number,
  disabled: boolean
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/disabled`,
    { disabled } as SetDisabledRequest
  )
  return data
}

// 设置凭据优先级
export async function setCredentialPriority(
  id: number,
  priority: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/priority`,
    { priority } as SetPriorityRequest
  )
  return data
}

// 设置凭据 RPM 上限（null 表示清除凭据级覆盖回退全局；0 表示显式不限流）
export async function setCredentialRpmLimit(
  id: number,
  rpmLimit: number | null
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/rpm-limit`,
    { rpmLimit } as SetRpmLimitRequest
  )
  return data
}

// 设置凭据并发上限（null 表示清除凭据级覆盖回退全局；0 表示显式不限并发）
export async function setCredentialConcurrencyLimit(
  id: number,
  concurrencyLimit: number | null
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/concurrency-limit`,
    { concurrencyLimit } as SetConcurrencyLimitRequest
  )
  return data
}

// 切换 overage（超额计费）开关
export async function setCredentialOverage(
  id: number,
  enabled: boolean
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/overage`,
    { enabled }
  )
  return data
}

// 重置失败计数
export async function resetCredentialFailure(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/reset`)
  return data
}

// 强制刷新 Token
export async function forceRefreshToken(
  id: number
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/credentials/${id}/refresh`)
  return data
}

// 获取凭据余额
export async function getCredentialBalance(id: number): Promise<BalanceResponse> {
  const { data } = await api.get<BalanceResponse>(`/credentials/${id}/balance`)
  return data
}

// 查询凭据上游可用模型列表
export async function getCredentialModels(id: number): Promise<ModelsResponse> {
  const { data } = await api.get<ModelsResponse>(`/credentials/${id}/models`)
  return data
}

// 添加新凭据
export async function addCredential(
  req: AddCredentialRequest
): Promise<AddCredentialResponse> {
  const { data } = await api.post<AddCredentialResponse>('/credentials', req)
  return data
}

// 删除凭据
export async function deleteCredential(id: number): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(`/credentials/${id}`)
  return data
}

// 批量删除凭据（仅删除已禁用项，后端单事务批量删除）
export async function batchDeleteCredentials(
  credentialIds: number[],
): Promise<BatchDeleteCredentialsResponse> {
  const { data } = await api.post<BatchDeleteCredentialsResponse>(
    '/credentials/delete/batch',
    { credentialIds },
  )
  return data
}

// 获取负载均衡模式
export async function getLoadBalancingMode(): Promise<{ mode: 'priority' | 'balanced' }> {
  const { data } = await api.get<{ mode: 'priority' | 'balanced' }>('/config/load-balancing')
  return data
}

// 设置负载均衡模式
export async function setLoadBalancingMode(mode: 'priority' | 'balanced'): Promise<{ mode: 'priority' | 'balanced' }> {
  const { data } = await api.put<{ mode: 'priority' | 'balanced' }>('/config/load-balancing', { mode })
  return data
}

// 获取全局缓存模式
export async function getGlobalCache(): Promise<{ enabled: boolean }> {
  const { data } = await api.get<{ enabled: boolean }>('/config/global-cache')
  return data
}

// 设置全局缓存模式
export async function setGlobalCache(enabled: boolean): Promise<{ enabled: boolean }> {
  const { data } = await api.put<{ enabled: boolean }>('/config/global-cache', { enabled })
  return data
}

// 缓存分桶策略（两种都按用户身份 metadata.user_id 分桶，PerCredential 在此之上再按凭据切分）
export type CacheScope = 'global' | 'per_credential'

// 获取缓存分桶策略
export async function getCacheScope(): Promise<{ scope: CacheScope }> {
  const { data } = await api.get<{ scope: CacheScope }>('/config/cache-scope')
  return data
}

// 设置缓存分桶策略
export async function setCacheScope(scope: CacheScope): Promise<{ scope: CacheScope }> {
  const { data } = await api.put<{ scope: CacheScope }>('/config/cache-scope', { scope })
  return data
}

// 获取缓存查找跳过率
export async function getCacheSkipRate(): Promise<{ rate: number | null }> {
  const { data } = await api.get<{ rate: number | null }>('/config/cache-skip-rate')
  return data
}

// 设置缓存查找跳过率（0.0-1.0，传 null 关闭）
export async function setCacheSkipRate(rate: number | null): Promise<{ rate: number | null }> {
  const { data } = await api.put<{ rate: number | null }>('/config/cache-skip-rate', { rate })
  return data
}

// ============ 代理分组管理 ============

export async function listProxyGroups(): Promise<ProxyGroupsResponse> {
  const { data } = await api.get<ProxyGroupsResponse>('/config/proxy-groups')
  return data
}

export async function upsertProxyGroup(
  name: string,
  req: UpsertProxyGroupRequest
): Promise<SuccessResponse> {
  const { data } = await api.put<SuccessResponse>(
    `/config/proxy-groups/${encodeURIComponent(name)}`,
    req
  )
  return data
}

export async function deleteProxyGroup(name: string): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(
    `/config/proxy-groups/${encodeURIComponent(name)}`
  )
  return data
}

export async function setCredentialGroup(
  id: number,
  group: string | null
): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(
    `/credentials/${id}/group`,
    { group }
  )
  return data
}

export async function batchSetCredentialGroup(
  credentialIds: number[],
  group: string | null,
): Promise<BatchSetCredentialGroupResponse> {
  const { data } = await api.post<BatchSetCredentialGroupResponse>(
    '/credentials/group/batch',
    { credentialIds, group },
  )
  return data
}

export async function batchSetPriority(
  credentialIds: number[],
  priority: number,
): Promise<BatchSetPriorityResponse> {
  const { data } = await api.post<BatchSetPriorityResponse>(
    '/credentials/priority/batch',
    { credentialIds, priority },
  )
  return data
}

export async function batchSetRpmLimit(
  credentialIds: number[],
  rpmLimit: number | null,
): Promise<BatchSetRpmLimitResponse> {
  const { data } = await api.post<BatchSetRpmLimitResponse>(
    '/credentials/rpm-limit/batch',
    { credentialIds, rpmLimit },
  )
  return data
}

export async function batchSetConcurrencyLimit(
  credentialIds: number[],
  concurrencyLimit: number | null,
): Promise<BatchSetConcurrencyLimitResponse> {
  const { data } = await api.post<BatchSetConcurrencyLimitResponse>(
    '/credentials/concurrency-limit/batch',
    { credentialIds, concurrencyLimit },
  )
  return data
}

export async function batchSetDisabled(
  credentialIds: number[],
  disabled: boolean,
): Promise<BatchSetDisabledResponse> {
  const { data } = await api.post<BatchSetDisabledResponse>(
    '/credentials/disabled/batch',
    { credentialIds, disabled },
  )
  return data
}

export async function batchSetOverage(
  credentialIds: number[],
  enabled: boolean,
): Promise<BatchSetOverageResponse> {
  const { data } = await api.post<BatchSetOverageResponse>(
    '/credentials/overage/batch',
    { credentialIds, enabled },
  )
  return data
}

export async function getDefaultRpmLimit(): Promise<DefaultRpmLimitResponse> {
  const { data } = await api.get<DefaultRpmLimitResponse>('/config/default-rpm-limit')
  return data
}

export async function setDefaultRpmLimit(rpmLimit: number | null): Promise<DefaultRpmLimitResponse> {
  const { data } = await api.put<DefaultRpmLimitResponse>('/config/default-rpm-limit', { rpmLimit })
  return data
}

export async function getDefaultConcurrencyLimit(): Promise<DefaultConcurrencyLimitResponse> {
  const { data } = await api.get<DefaultConcurrencyLimitResponse>('/config/default-concurrency-limit')
  return data
}

export async function setDefaultConcurrencyLimit(
  concurrencyLimit: number | null,
): Promise<DefaultConcurrencyLimitResponse> {
  const { data } = await api.put<DefaultConcurrencyLimitResponse>(
    '/config/default-concurrency-limit',
    { concurrencyLimit },
  )
  return data
}

// 获取计费累计统计
export async function getBillingStats(): Promise<BillingStatsResponse> {
  const { data } = await api.get<BillingStatsResponse>('/billing-stats')
  return data
}

// 获取时序曲线（按 model / credential 分组，可叠加 models/credentials 过滤）
export async function getStatsTimeseries(params: {
  hours?: number
  from?: number
  to?: number
  bucket: StatsBucket
  groupBy: StatsGroupBy
  models?: string[]
  credentials?: number[]
}): Promise<StatsTimeBucket[]> {
  const { data } = await api.get<StatsTimeBucket[]>('/stats/timeseries', {
    params: {
      hours: params.hours,
      from: params.from,
      to: params.to,
      bucket: params.bucket,
      group_by: params.groupBy,
      models: params.models?.length ? params.models.join(',') : undefined,
      credentials: params.credentials?.length ? params.credentials.join(',') : undefined,
    },
  })
  return data
}

// 获取区间汇总（全量 + 按模型 + 按凭据，可叠加 models/credentials 过滤）
export async function getStatsSummary(
  range: { hours?: number; from?: number; to?: number },
  filters?: { models?: string[]; credentials?: number[] },
): Promise<StatsSummaryResponse> {
  const { data } = await api.get<StatsSummaryResponse>('/stats/summary', {
    params: {
      hours: range.hours,
      from: range.from,
      to: range.to,
      models: filters?.models?.length ? filters.models.join(',') : undefined,
      credentials: filters?.credentials?.length ? filters.credentials.join(',') : undefined,
    },
  })
  return data
}
