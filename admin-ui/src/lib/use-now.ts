import { useSyncExternalStore } from 'react'

export type TickResolution = 'second' | 'minute' | 'hour'

const INTERVAL_MS: Record<TickResolution, number> = {
  second: 1_000,
  minute: 60_000,
  hour: 3_600_000,
}

const listeners = new Map<TickResolution, Set<() => void>>()
const timers = new Map<TickResolution, ReturnType<typeof setInterval>>()

function subscribe(resolution: TickResolution, onChange: () => void): () => void {
  let set = listeners.get(resolution)
  if (!set) {
    set = new Set()
    listeners.set(resolution, set)
  }
  set.add(onChange)

  if (!timers.has(resolution)) {
    const id = setInterval(() => {
      listeners.get(resolution)?.forEach((fn) => fn())
    }, INTERVAL_MS[resolution])
    timers.set(resolution, id)
  }

  return () => {
    const subs = listeners.get(resolution)
    if (!subs) return
    subs.delete(onChange)
    if (subs.size === 0) {
      listeners.delete(resolution)
      const id = timers.get(resolution)
      if (id !== undefined) {
        clearInterval(id)
        timers.delete(resolution)
      }
    }
  }
}

// 每个 resolution 的 subscribe/getSnapshot 预先绑定成稳定引用，
// 避免 useSyncExternalStore 在每次渲染时 cleanup+resubscribe、重启 timer。
const subscribers: Record<TickResolution, (cb: () => void) => () => void> = {
  second: (cb) => subscribe('second', cb),
  minute: (cb) => subscribe('minute', cb),
  hour: (cb) => subscribe('hour', cb),
}

const snapshotGetters: Record<TickResolution, () => number> = {
  second: () => Math.floor(Date.now() / INTERVAL_MS.second),
  minute: () => Math.floor(Date.now() / INTERVAL_MS.minute),
  hour: () => Math.floor(Date.now() / INTERVAL_MS.hour),
}

function getServerSnapshot(): number {
  return 0
}

/**
 * 订阅一个按 resolution 节拍跳动的"当前时间"，触发订阅组件重渲染。
 * 所有订阅同一 resolution 的组件共享同一个 setInterval；没有订阅者时 timer 自动销毁。
 */
export function useNow(resolution: TickResolution = 'minute'): number {
  return useSyncExternalStore(
    subscribers[resolution],
    snapshotGetters[resolution],
    getServerSnapshot,
  )
}

/**
 * 根据距今时间差，挑选合适的 tick 精度，避免"3 天前"这种显示每秒重渲染。
 */
export function pickTickResolution(lastUsedAt: string | null | undefined): TickResolution {
  if (!lastUsedAt) return 'hour'
  const diff = Date.now() - new Date(lastUsedAt).getTime()
  if (diff < INTERVAL_MS.minute) return 'second'
  if (diff < INTERVAL_MS.hour) return 'minute'
  return 'hour'
}
