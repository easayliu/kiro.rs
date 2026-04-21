import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  CredentialsStatusResponse,
  BalanceResponse,
  SuccessResponse,
  SetDisabledRequest,
  SetPriorityRequest,
  AddCredentialRequest,
  AddCredentialResponse,
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
