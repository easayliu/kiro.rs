import type { BalanceResponse } from '@/types/api'

const API_KEY_STORAGE_KEY = 'adminApiKey'
const BALANCE_CACHE_KEY = 'kiro-admin:balance-cache'
const BALANCE_CACHE_TTL_MS = 10 * 60 * 1000 // 10 分钟

interface BalanceCachePayload {
  ts: number
  entries: [number, BalanceResponse][]
}

export const storage = {
  getApiKey: () => localStorage.getItem(API_KEY_STORAGE_KEY),
  setApiKey: (key: string) => localStorage.setItem(API_KEY_STORAGE_KEY, key),
  removeApiKey: () => localStorage.removeItem(API_KEY_STORAGE_KEY),

  loadBalanceCache(): Map<number, BalanceResponse> {
    try {
      const raw = localStorage.getItem(BALANCE_CACHE_KEY)
      if (!raw) return new Map()
      const parsed = JSON.parse(raw) as BalanceCachePayload | null
      if (!parsed || typeof parsed.ts !== 'number' || !Array.isArray(parsed.entries)) {
        return new Map()
      }
      if (Date.now() - parsed.ts > BALANCE_CACHE_TTL_MS) {
        localStorage.removeItem(BALANCE_CACHE_KEY)
        return new Map()
      }
      return new Map(parsed.entries)
    } catch {
      return new Map()
    }
  },

  saveBalanceCache(map: Map<number, BalanceResponse>) {
    try {
      if (map.size === 0) {
        localStorage.removeItem(BALANCE_CACHE_KEY)
        return
      }
      const payload: BalanceCachePayload = {
        ts: Date.now(),
        entries: Array.from(map.entries()),
      }
      localStorage.setItem(BALANCE_CACHE_KEY, JSON.stringify(payload))
    } catch {
      // 忽略 quota / private 模式错误
    }
  },

  clearBalanceCache() {
    try {
      localStorage.removeItem(BALANCE_CACHE_KEY)
    } catch {
      // ignore
    }
  },
}
