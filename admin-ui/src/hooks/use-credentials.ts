import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  getMe,
  getCredentials,
  setCredentialDisabled,
  setCredentialPriority,
  setCredentialRpmLimit,
  setCredentialConcurrencyLimit,
  setCredentialOverage,
  batchSetOverage,
  resetCredentialFailure,
  forceRefreshToken,
  getCredentialBalance,
  getCredentialModels,
  addCredential,
  deleteCredential,
  batchDeleteCredentials,
  getLoadBalancingMode,
  setLoadBalancingMode,
  getGlobalCache,
  setGlobalCache,
  getCacheScope,
  setCacheScope,
  getCacheSkipRate,
  setCacheSkipRate,
  getOutputMultiplier,
  setOutputMultiplier,
  listProxyGroups,
  upsertProxyGroup,
  deleteProxyGroup,
  setCredentialGroup,
  batchSetCredentialGroup,
  batchSetPriority,
  batchSetRpmLimit,
  batchSetConcurrencyLimit,
  batchSetDisabled,
  getDefaultRpmLimit,
  setDefaultRpmLimit,
  getDefaultConcurrencyLimit,
  setDefaultConcurrencyLimit,
  getBillingStats,
  getStatsTimeseries,
  getStatsSummary,
} from '@/api/credentials'
import type {
  AddCredentialRequest,
  UpsertProxyGroupRequest,
  StatsBucket,
  StatsGroupBy,
} from '@/types/api'

// 查询当前调用方角色
export function useMe() {
  return useQuery({
    queryKey: ['me'],
    queryFn: getMe,
    staleTime: 5 * 60 * 1000,
    retry: false,
  })
}

// 是否为只读用户（guest）
export function useIsReadOnly(): boolean {
  const { data } = useMe()
  return data?.role === 'guest'
}

// 查询凭据列表
export function useCredentials() {
  return useQuery({
    queryKey: ['credentials'],
    queryFn: getCredentials,
    refetchInterval: 30000, // 每 30 秒刷新一次
  })
}

// 查询凭据余额
export function useCredentialBalance(id: number | null) {
  return useQuery({
    queryKey: ['credential-balance', id],
    queryFn: () => getCredentialBalance(id!),
    enabled: id !== null,
    retry: false, // 余额查询失败时不重试（避免重复请求被封禁的账号）
  })
}

// 查询凭据上游可用模型列表（懒加载：仅在子菜单打开后才请求）
export function useCredentialModels(id: number, enabled: boolean) {
  return useQuery({
    queryKey: ['credential-models', id],
    queryFn: () => getCredentialModels(id),
    enabled,
    retry: false, // 模型查询失败时不重试（避免重复请求被封禁的账号）
    staleTime: 10 * 60 * 1000, // 模型列表变化极少，缓存 10 分钟
  })
}

// 设置禁用状态
export function useSetDisabled() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, disabled }: { id: number; disabled: boolean }) =>
      setCredentialDisabled(id, disabled),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 设置优先级
export function useSetPriority() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, priority }: { id: number; priority: number }) =>
      setCredentialPriority(id, priority),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 设置凭据 RPM 上限
export function useSetRpmLimit() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, rpmLimit }: { id: number; rpmLimit: number | null }) =>
      setCredentialRpmLimit(id, rpmLimit),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 设置凭据并发上限
export function useSetConcurrencyLimit() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, concurrencyLimit }: { id: number; concurrencyLimit: number | null }) =>
      setCredentialConcurrencyLimit(id, concurrencyLimit),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 切换 overage（超额计费）开关
export function useSetOverage() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, enabled }: { id: number; enabled: boolean }) =>
      setCredentialOverage(id, enabled),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 重置失败计数
export function useResetFailure() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => resetCredentialFailure(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 强制刷新 Token
export function useForceRefreshToken() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => forceRefreshToken(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 添加新凭据
export function useAddCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: AddCredentialRequest) => addCredential(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 删除凭据
export function useDeleteCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => deleteCredential(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 批量删除凭据（一次请求，后端单事务批量删除）
export function useBatchDeleteCredentials() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (credentialIds: number[]) => batchDeleteCredentials(credentialIds),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 获取负载均衡模式
export function useLoadBalancingMode() {
  return useQuery({
    queryKey: ['loadBalancingMode'],
    queryFn: getLoadBalancingMode,
  })
}

// 设置负载均衡模式
export function useSetLoadBalancingMode() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setLoadBalancingMode,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['loadBalancingMode'] })
    },
  })
}

// 获取全局缓存模式
export function useGlobalCache() {
  return useQuery({
    queryKey: ['globalCache'],
    queryFn: getGlobalCache,
  })
}

// 设置全局缓存模式
export function useSetGlobalCache() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setGlobalCache,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['globalCache'] })
      queryClient.invalidateQueries({ queryKey: ['cacheScope'] })
    },
  })
}

// 获取缓存分桶策略
export function useCacheScope() {
  return useQuery({
    queryKey: ['cacheScope'],
    queryFn: getCacheScope,
  })
}

// 设置缓存分桶策略
export function useSetCacheScope() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setCacheScope,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['cacheScope'] })
      queryClient.invalidateQueries({ queryKey: ['globalCache'] })
    },
  })
}

// 获取缓存查找跳过率
export function useCacheSkipRate() {
  return useQuery({
    queryKey: ['cacheSkipRate'],
    queryFn: getCacheSkipRate,
  })
}

// 设置缓存查找跳过率
export function useSetCacheSkipRate() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setCacheSkipRate,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['cacheSkipRate'] })
    },
  })
}

// 获取输出 token 上报倍率
export function useOutputMultiplier() {
  return useQuery({
    queryKey: ['outputMultiplier'],
    queryFn: getOutputMultiplier,
  })
}

// 设置输出 token 上报倍率
export function useSetOutputMultiplier() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setOutputMultiplier,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['outputMultiplier'] })
    },
  })
}

// ============ 代理分组 ============

export function useProxyGroups() {
  return useQuery({
    queryKey: ['proxyGroups'],
    queryFn: listProxyGroups,
  })
}

export function useUpsertProxyGroup() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ name, req }: { name: string; req: UpsertProxyGroupRequest }) =>
      upsertProxyGroup(name, req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['proxyGroups'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

export function useDeleteProxyGroup() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (name: string) => deleteProxyGroup(name),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['proxyGroups'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

export function useSetCredentialGroup() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, group }: { id: number; group: string | null }) =>
      setCredentialGroup(id, group),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

export function useBatchSetCredentialGroup() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ credentialIds, group }: { credentialIds: number[]; group: string | null }) =>
      batchSetCredentialGroup(credentialIds, group),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

export function useBatchSetPriority() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ credentialIds, priority }: { credentialIds: number[]; priority: number }) =>
      batchSetPriority(credentialIds, priority),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

export function useBatchSetRpmLimit() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ credentialIds, rpmLimit }: { credentialIds: number[]; rpmLimit: number | null }) =>
      batchSetRpmLimit(credentialIds, rpmLimit),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

export function useBatchSetConcurrencyLimit() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ credentialIds, concurrencyLimit }: { credentialIds: number[]; concurrencyLimit: number | null }) =>
      batchSetConcurrencyLimit(credentialIds, concurrencyLimit),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

export function useBatchSetDisabled() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ credentialIds, disabled }: { credentialIds: number[]; disabled: boolean }) =>
      batchSetDisabled(credentialIds, disabled),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

export function useBatchSetOverage() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ credentialIds, enabled }: { credentialIds: number[]; enabled: boolean }) =>
      batchSetOverage(credentialIds, enabled),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

export function useDefaultRpmLimit() {
  return useQuery({
    queryKey: ['default-rpm-limit'],
    queryFn: getDefaultRpmLimit,
    staleTime: 30 * 1000,
  })
}

export function useSetDefaultRpmLimit() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (rpmLimit: number | null) => setDefaultRpmLimit(rpmLimit),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['default-rpm-limit'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

export function useDefaultConcurrencyLimit() {
  return useQuery({
    queryKey: ['default-concurrency-limit'],
    queryFn: getDefaultConcurrencyLimit,
    staleTime: 30 * 1000,
  })
}

export function useSetDefaultConcurrencyLimit() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (concurrencyLimit: number | null) => setDefaultConcurrencyLimit(concurrencyLimit),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['default-concurrency-limit'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 计费累计统计（每 30 秒轮询刷新）
export function useBillingStats() {
  return useQuery({
    queryKey: ['billing-stats'],
    queryFn: getBillingStats,
    refetchInterval: 30000,
  })
}

// 时序曲线（hours 或自定义 from/to；可叠加 models/credentials 过滤，60 秒轮询）
export function useStatsTimeseries(params: {
  hours?: number
  from?: number
  to?: number
  bucket: StatsBucket
  groupBy: StatsGroupBy
  models?: string[]
  credentials?: number[]
  enabled?: boolean
}) {
  const mk = (params.models ?? []).join(',')
  const ck = (params.credentials ?? []).join(',')
  return useQuery({
    queryKey: [
      'stats-timeseries',
      params.hours ?? null,
      params.from ?? null,
      params.to ?? null,
      params.bucket,
      params.groupBy,
      mk,
      ck,
    ],
    queryFn: () => getStatsTimeseries(params),
    enabled: params.enabled ?? true,
    refetchInterval: 60000,
  })
}

// 区间汇总（hours 或自定义 from/to；可叠加 models/credentials 过滤，60 秒轮询）
export function useStatsSummary(
  range: { hours?: number; from?: number; to?: number },
  filters?: { models?: string[]; credentials?: number[] },
  enabled = true,
) {
  const mk = (filters?.models ?? []).join(',')
  const ck = (filters?.credentials ?? []).join(',')
  return useQuery({
    queryKey: ['stats-summary', range.hours ?? null, range.from ?? null, range.to ?? null, mk, ck],
    queryFn: () => getStatsSummary(range, filters),
    enabled,
    refetchInterval: 60000,
  })
}
