import { useEffect, useMemo, useRef, useState } from 'react'
import {
  Area,
  AreaChart,
  Bar,
  CartesianGrid,
  Line,
  ComposedChart,
  Legend,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from 'recharts'
import { useQueryClient } from '@tanstack/react-query'
import { Activity, AlertTriangle, Check, ChevronDown, CircleDollarSign, Clock, RefreshCw, Search } from 'lucide-react'
import { cn } from '@/lib/utils'
import { useCredentials, useStatsSummary, useStatsTimeseries } from '@/hooks/use-credentials'
import type { StatGroup, StatsBucket } from '@/types/api'

// ━━━━━━━━━━ 时间范围 ━━━━━━━━━━
type HoursPreset = '24h' | '7d' | '30d'
type RangeKey = HoursPreset | 'today' | 'yesterday' | 'custom'
// 滚动窗口预设（hours）
const RANGES: Record<HoursPreset, { hours: number; bucket: StatsBucket }> = {
  '24h': { hours: 24, bucket: 'hour' },
  '7d': { hours: 168, bucket: 'hour' },
  '30d': { hours: 720, bucket: 'day' },
}
// SegBar 顺序与标签
const RANGE_OPTIONS: { key: RangeKey; label: string }[] = [
  { key: '24h', label: '24 小时' },
  { key: 'today', label: '今天' },
  { key: 'yesterday', label: '昨天' },
  { key: '7d', label: '7 天' },
  { key: '30d', label: '30 天' },
  { key: 'custom', label: '自定义' },
]
/** 本地当日 0 点的 Unix 秒 */
function startOfTodayTs(): number {
  const d = new Date()
  d.setHours(0, 0, 0, 0)
  return Math.floor(d.getTime() / 1000)
}


// ━━━━━━━━━━ 格式化 ━━━━━━━━━━
function fmtUsd(v: number): string {
  const abs = Math.abs(v)
  const d = abs >= 100 ? 2 : abs >= 1 ? 3 : 4
  return `${v < 0 ? '-' : ''}$${abs.toLocaleString('en-US', { minimumFractionDigits: d, maximumFractionDigits: d })}`
}
function fmtNum(v: number): string {
  return v.toLocaleString('en-US')
}
// 紧凑整数（去掉多余的 .00），用于 token / 请求数等窄轴刻度，避免 100.00M 这类长串被裁
function fmtCompact(v: number): string {
  const trim = (n: number) => `${Number(n.toFixed(1))}`
  if (v >= 1_000_000_000) return `${trim(v / 1_000_000_000)}B`
  if (v >= 1_000_000) return `${trim(v / 1_000_000)}M`
  if (v >= 1_000) return `${trim(v / 1_000)}k`
  return `${v}`
}
// 成本轴刻度专用紧凑写法（$1.2k / $3.4M）；KPI/tooltip/表格仍用精确的 fmtUsd
function fmtUsdAxis(v: number): string {
  const a = Math.abs(v)
  const s = v < 0 ? '-' : ''
  if (a >= 1_000_000) return `${s}$${Number((a / 1_000_000).toFixed(1))}M`
  if (a >= 1_000) return `${s}$${Number((a / 1_000).toFixed(1))}k`
  return `${s}$${Number(a.toFixed(a >= 1 ? 0 : 2))}`
}
function fmtMs(v: number): string {
  if (v >= 1000) return `${(v / 1000).toFixed(1)}s`
  return `${Math.round(v)}ms`
}
function fmtTime(ts: number, bucket: StatsBucket): string {
  const d = new Date(ts * 1000)
  const mm = String(d.getMonth() + 1).padStart(2, '0')
  const dd = String(d.getDate()).padStart(2, '0')
  if (bucket === 'day') return `${mm}-${dd}`
  const hh = String(d.getHours()).padStart(2, '0')
  return `${mm}-${dd} ${hh}:00`
}

const AXIS = 'hsl(var(--muted-foreground))'
const GRID = 'hsl(var(--border))'

const tooltipStyle = {
  background: 'hsl(var(--background))',
  border: '1px solid hsl(var(--border))',
  borderRadius: 12,
  fontSize: 12,
  boxShadow: '0 8px 30px rgba(0,0,0,0.12)',
}

export function StatsView() {
  const [range, setRange] = useState<RangeKey>('24h')
  // 自定义起止（datetime-local 字符串）
  const [customFrom, setCustomFrom] = useState('')
  const [customTo, setCustomTo] = useState('')
  // 数据过滤（模型 + 凭据，叠加生效，WHERE 下推到后端）
  const [modelFilter, setModelFilter] = useState<string[]>([])
  const [credFilter, setCredFilter] = useState<number[]>([])

  // 解析时间范围 → 查询参数（预设用 hours；自定义用 from/to）+ 分桶粒度
  const { tsRange, bucket } = useMemo<{
    tsRange: { hours?: number; from?: number; to?: number }
    bucket: StatsBucket
  }>(() => {
    if (range === 'today') {
      const s = startOfTodayTs()
      return { tsRange: { from: s, to: s + 86400 }, bucket: 'hour' }
    }
    if (range === 'yesterday') {
      const s = startOfTodayTs()
      return { tsRange: { from: s - 86400, to: s }, bucket: 'hour' }
    }
    if (range === 'custom') {
      const f = customFrom ? Math.floor(new Date(customFrom).getTime() / 1000) : 0
      const t = customTo ? Math.floor(new Date(customTo).getTime() / 1000) : 0
      if (f && t && t > f) {
        const span = t - f
        return { tsRange: { from: f, to: t }, bucket: span > 3 * 86400 ? 'day' : 'hour' }
      }
      return { tsRange: { hours: 24 }, bucket: 'hour' } // 未填全 → 回退默认
    }
    const r = RANGES[range as HoursPreset]
    return { tsRange: { hours: r.hours }, bucket: r.bucket }
  }, [range, customFrom, customTo])

  const queryClient = useQueryClient()
  const refresh = () => {
    queryClient.invalidateQueries({ queryKey: ['stats-timeseries'] })
    queryClient.invalidateQueries({ queryKey: ['stats-summary'] })
  }

  const filters = { models: modelFilter, credentials: credFilter }
  const { data: series = [], isLoading, isFetching } = useStatsTimeseries({
    ...tsRange,
    bucket,
    groupBy: 'none',
    ...filters,
  })
  const { data: summary } = useStatsSummary(tsRange, filters)
  // 未过滤的 facet（填充筛选器选项；过滤为空时与上面的 summary 同 key、自动复用）
  const { data: facets } = useStatsSummary(tsRange)
  const { data: creds } = useCredentials()

  // credential id → email 标签映射
  const credLabel = useMemo(() => {
    const m = new Map<string, string>()
    creds?.credentials?.forEach(c => m.set(String(c.id), c.email || `#${c.id}`))
    return (key: string) => m.get(key) || `#${key}`
  }, [creds])

  // 筛选器选项（来自未过滤 facet）：模型名 / 凭据 id
  const modelOptions = useMemo(() => (facets?.by_model ?? []).map(g => g.key), [facets])
  const credOptions = useMemo(() => (facets?.by_credential ?? []).map(g => g.key), [facets])
  const hasFilter = modelFilter.length > 0 || credFilter.length > 0

  // 总览曲线数据（groupBy=none 时 series 已是单序列）
  const overview = useMemo(
    () =>
      series.map(r => ({
        ...r,
        time: fmtTime(r.bucket, bucket),
        // 错误率分母含失败本身：失败 / (成功 + 失败)。
        errRate: r.requests + r.failures > 0 ? (r.failures / (r.requests + r.failures)) * 100 : 0,
        inputTotal: r.input_tokens + r.cache_read + r.cache_creation,
      })),
    [series, bucket],
  )

  const total = summary?.total

  return (
    <div className="space-y-5 sm:space-y-6">
      {/* ━━━ 控制条 ━━━ */}
      <section className="flex flex-col gap-3 sm:flex-row sm:flex-wrap sm:items-center sm:justify-between">
        <h1 className="text-2xl font-semibold tracking-tight sm:text-3xl">统计分析</h1>
        <div className="flex flex-col gap-2 sm:flex-row sm:items-center">
          {/* 窄屏时间栏可横向滚动，避免 6 个选项挤压换行 */}
          <div className="-mx-1 max-w-full overflow-x-auto px-1 no-scrollbar">
            <SegBar value={range} options={RANGE_OPTIONS} onChange={setRange} />
          </div>
          <button
            onClick={refresh}
            disabled={isFetching}
            title="刷新统计"
            className="inline-flex h-8 w-8 shrink-0 cursor-pointer items-center justify-center rounded-lg border border-border text-muted-foreground transition-colors hover:text-foreground disabled:cursor-default disabled:opacity-60"
          >
            <RefreshCw className={cn('h-4 w-4', isFetching && 'animate-spin')} />
          </button>
          {range === 'custom' && (
            <div className="flex flex-col gap-1.5 text-xs sm:flex-row sm:items-center">
              <input
                type="datetime-local"
                value={customFrom}
                max={customTo || undefined}
                onChange={e => setCustomFrom(e.target.value)}
                className="w-full rounded-lg border border-border bg-transparent px-2 py-1 text-xs outline-none focus:border-primary sm:w-auto"
              />
              <span className="hidden text-muted-foreground sm:inline">→</span>
              <input
                type="datetime-local"
                value={customTo}
                min={customFrom || undefined}
                onChange={e => setCustomTo(e.target.value)}
                className="w-full rounded-lg border border-border bg-transparent px-2 py-1 text-xs outline-none focus:border-primary sm:w-auto"
              />
            </div>
          )}
        </div>
      </section>

      {/* ━━━ 筛选：模型(chips) + 凭据(搜索下拉)，叠加生效（WHERE 下推后端） ━━━ */}
      {(modelOptions.length > 0 || credOptions.length > 0) && (
        <section className="flex flex-wrap items-center gap-x-3 gap-y-2">
          <span className="font-mono text-2xs text-muted-foreground">筛选</span>
          {modelOptions.map(m => {
            const on = modelFilter.includes(m)
            return (
              <button
                key={m}
                onClick={() =>
                  setModelFilter(prev => (on ? prev.filter(x => x !== m) : [...prev, m]))
                }
                className={cn(
                  'inline-flex cursor-pointer items-center rounded-full border px-2 py-0.5 text-2xs transition-colors',
                  on
                    ? 'border-primary bg-primary text-primary-foreground'
                    : 'border-border text-muted-foreground hover:text-foreground',
                )}
              >
                {m}
              </button>
            )
          })}
          {credOptions.length > 0 && (
            <GroupFilter
              label="凭据"
              candidates={credOptions}
              labelOf={credLabel}
              colorOf={() => '#525252'}
              selected={new Set(credFilter.map(String))}
              setSelected={s => setCredFilter([...s].map(Number))}
              totalCount={credOptions.length}
            />
          )}
          {hasFilter && (
            <button
              onClick={() => {
                setModelFilter([])
                setCredFilter([])
              }}
              className="cursor-pointer text-2xs text-muted-foreground underline-offset-2 hover:text-foreground hover:underline"
            >
              清除筛选
            </button>
          )}
        </section>
      )}

      {/* ━━━ KPI 卡片 ━━━ */}
      <section className="grid grid-cols-2 gap-2.5 sm:grid-cols-3 sm:gap-3 lg:grid-cols-6">
        <Kpi label="请求" value={fmtNum(total?.requests ?? 0)} icon={<Activity className="h-3.5 w-3.5" />} />
        <Kpi label="实际成本" value={fmtUsd(total?.actual_usd ?? 0)} icon={<CircleDollarSign className="h-3.5 w-3.5" />} />
        <Kpi label="官方价" value={fmtUsd(total?.official_usd ?? 0)} />
        <Kpi
          label="毛利"
          value={fmtUsd(total?.margin_usd ?? 0)}
          tone={total && total.margin_usd < 0 ? 'bad' : 'ok'}
        />
        <Kpi
          label="错误率"
          value={`${total && total.requests + total.failures > 0 ? ((total.failures / (total.requests + total.failures)) * 100).toFixed(1) : '0.0'}%`}
          icon={<AlertTriangle className="h-3.5 w-3.5" />}
          tone={total && total.failures > 0 ? 'bad' : undefined}
        />
        <Kpi label="平均首字" value={fmtMs(total?.avg_ttft_ms ?? 0)} icon={<Clock className="h-3.5 w-3.5" />} />
      </section>

      {isLoading && <div className="py-10 text-center font-mono text-xs text-muted-foreground">加载中…</div>}

      {!isLoading && series.length === 0 && (
        <div className="rounded-xl border border-dashed border-border py-16 text-center">
          <p className="text-sm font-medium">该时间范围内暂无统计数据</p>
          <p className="mt-1 font-mono text-xs text-muted-foreground">发起若干请求后曲线会自动出现</p>
        </div>
      )}

      {/* ━━━ 总览：四联曲线（数据受顶部筛选约束） ━━━ */}
      {!isLoading && series.length > 0 && (
        <div className="grid gap-3 lg:grid-cols-2">
          <ChartCard title="成本 / 官方价 / 毛利" subtitle="USD">
            <ResponsiveContainer width="100%" height={240}>
              <ComposedChart data={overview} margin={{ top: 8, right: 8, left: -8, bottom: 0 }}>
                <defs>
                  <linearGradient id="gOff" x1="0" y1="0" x2="0" y2="1">
                    <stop offset="0%" stopColor="#94a3b8" stopOpacity={0.35} />
                    <stop offset="100%" stopColor="#94a3b8" stopOpacity={0} />
                  </linearGradient>
                  <linearGradient id="gAct" x1="0" y1="0" x2="0" y2="1">
                    <stop offset="0%" stopColor="#525252" stopOpacity={0.4} />
                    <stop offset="100%" stopColor="#525252" stopOpacity={0} />
                  </linearGradient>
                </defs>
                <CartesianGrid strokeDasharray="3 3" stroke={GRID} vertical={false} />
                <XAxis dataKey="time" tick={{ fontSize: 11, fill: AXIS }} stroke={GRID} minTickGap={24} />
                <YAxis tick={{ fontSize: 11, fill: AXIS }} stroke={GRID} width={56} tickFormatter={fmtUsdAxis} />
                <Tooltip contentStyle={tooltipStyle} formatter={(v: number) => fmtUsd(v)} />
                <Legend wrapperStyle={{ fontSize: 12 }} />
                <Area name="官方价" type="monotone" dataKey="official_usd" stroke="#94a3b8" fill="url(#gOff)" strokeWidth={1.5} />
                <Area name="实际成本" type="monotone" dataKey="actual_usd" stroke="#525252" fill="url(#gAct)" strokeWidth={1.5} />
                <Line name="毛利" type="monotone" dataKey="margin_usd" stroke="#10b981" strokeWidth={1.8} dot={false} />
              </ComposedChart>
            </ResponsiveContainer>
          </ChartCard>

          <ChartCard title="请求量 / 错误率" subtitle="次 · %">
            <ResponsiveContainer width="100%" height={240}>
              <ComposedChart data={overview} margin={{ top: 8, right: 8, left: -8, bottom: 0 }}>
                <CartesianGrid strokeDasharray="3 3" stroke={GRID} vertical={false} />
                <XAxis dataKey="time" tick={{ fontSize: 11, fill: AXIS }} stroke={GRID} minTickGap={24} />
                <YAxis yAxisId="l" tick={{ fontSize: 11, fill: AXIS }} stroke={GRID} width={40} tickFormatter={fmtCompact} />
                <YAxis yAxisId="r" orientation="right" tick={{ fontSize: 11, fill: AXIS }} stroke={GRID} width={40} unit="%" />
                <Tooltip contentStyle={tooltipStyle} />
                <Legend wrapperStyle={{ fontSize: 12 }} />
                <Bar yAxisId="l" name="请求" dataKey="requests" fill="#3b82f6" radius={[3, 3, 0, 0]} maxBarSize={28} />
                <Bar yAxisId="l" name="失败" dataKey="failures" fill="#f97316" radius={[3, 3, 0, 0]} maxBarSize={28} />
                <Line yAxisId="r" name="错误率%" type="monotone" dataKey="errRate" stroke="#f97316" strokeWidth={1.8} dot={false} />
              </ComposedChart>
            </ResponsiveContainer>
          </ChartCard>

          <ChartCard title="Token 用量" subtitle="按类型堆叠">
            <ResponsiveContainer width="100%" height={240}>
              <AreaChart data={overview} margin={{ top: 8, right: 8, left: -8, bottom: 0 }}>
                <CartesianGrid strokeDasharray="3 3" stroke={GRID} vertical={false} />
                <XAxis dataKey="time" tick={{ fontSize: 11, fill: AXIS }} stroke={GRID} minTickGap={24} />
                <YAxis tick={{ fontSize: 11, fill: AXIS }} stroke={GRID} width={48} tickFormatter={fmtCompact} />
                <Tooltip contentStyle={tooltipStyle} formatter={(v: number) => fmtNum(v)} />
                <Legend wrapperStyle={{ fontSize: 12 }} />
                <Area name="输入" type="monotone" dataKey="input_tokens" stackId="t" stroke="#525252" fill="#525252" fillOpacity={0.5} />
                <Area name="缓存读" type="monotone" dataKey="cache_read" stackId="t" stroke="#06b6d4" fill="#06b6d4" fillOpacity={0.5} />
                <Area name="缓存写" type="monotone" dataKey="cache_creation" stackId="t" stroke="#f59e0b" fill="#f59e0b" fillOpacity={0.5} />
                <Area name="输出" type="monotone" dataKey="output_tokens" stackId="t" stroke="#10b981" fill="#10b981" fillOpacity={0.5} />
              </AreaChart>
            </ResponsiveContainer>
          </ChartCard>

          <ChartCard title="延迟" subtitle="首字(左轴) / 总耗时(右轴)，平均">
            <ResponsiveContainer width="100%" height={240}>
              <ComposedChart data={overview} margin={{ top: 8, right: 8, left: -8, bottom: 0 }}>
                <CartesianGrid strokeDasharray="3 3" stroke={GRID} vertical={false} />
                <XAxis dataKey="time" tick={{ fontSize: 11, fill: AXIS }} stroke={GRID} minTickGap={24} />
                {/* 双轴：首字(数百ms)与总耗时(数千ms)量级差大，分轴才看得清 */}
                <YAxis yAxisId="ttft" tick={{ fontSize: 11, fill: AXIS }} stroke={GRID} width={48} tickFormatter={fmtMs} />
                <YAxis yAxisId="elapsed" orientation="right" tick={{ fontSize: 11, fill: AXIS }} stroke={GRID} width={48} tickFormatter={fmtMs} />
                <Tooltip contentStyle={tooltipStyle} formatter={(v: number) => fmtMs(v)} />
                <Legend wrapperStyle={{ fontSize: 12 }} />
                <Line yAxisId="ttft" name="首字 TTFT" type="monotone" dataKey="avg_ttft_ms" stroke="#f59e0b" strokeWidth={1.8} dot={false} />
                <Line yAxisId="elapsed" name="总耗时" type="monotone" dataKey="avg_elapsed_ms" stroke="#8b5cf6" strokeWidth={1.8} dot={false} />
              </ComposedChart>
            </ResponsiveContainer>
          </ChartCard>
        </div>
      )}

      {/* ━━━ 区间明细表：按模型 + 按凭据（均受顶部筛选约束） ━━━ */}
      {!isLoading && summary && summary.by_model.length > 0 && (
        <BreakdownTable title="按模型" rows={summary.by_model} labelOf={k => k} />
      )}
      {!isLoading && summary && summary.by_credential.length > 0 && (
        <BreakdownTable title="按凭据" rows={summary.by_credential} labelOf={credLabel} />
      )}
    </div>
  )
}

// 带搜索的多选下拉（分组多时用，如按凭据）
function GroupFilter({
  candidates,
  labelOf,
  colorOf,
  selected,
  setSelected,
  totalCount,
  label,
}: {
  candidates: string[]
  labelOf: (k: string) => string
  colorOf: (k: string) => string
  selected: Set<string>
  setSelected: (s: Set<string>) => void
  totalCount: number
  label?: string
}) {
  const [open, setOpen] = useState(false)
  const [q, setQ] = useState('')
  const ref = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (!open) return
    const onDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener('mousedown', onDown)
    return () => document.removeEventListener('mousedown', onDown)
  }, [open])

  const filtered = candidates.filter(g => labelOf(g).toLowerCase().includes(q.trim().toLowerCase()))
  const toggle = (g: string) => {
    const n = new Set(selected)
    n.has(g) ? n.delete(g) : n.add(g)
    setSelected(n)
  }

  return (
    <div className="relative" ref={ref}>
      <button
        onClick={() => setOpen(o => !o)}
        className="inline-flex cursor-pointer items-center gap-1.5 rounded-lg border border-border px-2.5 py-1 text-xs text-muted-foreground transition-colors hover:text-foreground"
      >
        {label && <span className="text-foreground">{label}</span>}
        已选 <span className="tnum text-foreground">{selected.size}</span>/{candidates.length}
        <ChevronDown className="h-3 w-3" />
      </button>
      {open && (
        <div className="absolute z-30 mt-1 w-64 max-w-[calc(100vw-2rem)] rounded-xl border border-border bg-background p-2 shadow-pop">
          <div className="relative mb-2">
            <Search className="pointer-events-none absolute left-2 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
            <input
              value={q}
              onChange={e => setQ(e.target.value)}
              placeholder="搜索…"
              autoFocus
              className="w-full rounded-md border border-border bg-transparent py-1 pl-7 pr-2 text-xs outline-none focus:border-primary"
            />
          </div>
          <div className="mb-1 flex items-center justify-between px-1 font-mono text-2xs text-muted-foreground">
            <button className="cursor-pointer hover:text-foreground" onClick={() => setSelected(new Set(candidates))}>
              全选
            </button>
            <button className="cursor-pointer hover:text-foreground" onClick={() => setSelected(new Set())}>
              清空
            </button>
          </div>
          <div className="max-h-56 overflow-y-auto">
            {filtered.length === 0 ? (
              <div className="px-2 py-3 text-center text-2xs text-muted-foreground">无匹配</div>
            ) : (
              filtered.map(g => {
                const on = selected.has(g)
                return (
                  <button
                    key={g}
                    onClick={() => toggle(g)}
                    className="flex w-full cursor-pointer items-center gap-2 rounded-md px-2 py-1.5 text-xs hover:bg-muted"
                  >
                    <span
                      className={cn(
                        'flex h-3.5 w-3.5 shrink-0 items-center justify-center rounded border',
                        on ? 'border-transparent' : 'border-border',
                      )}
                      style={on ? { backgroundColor: colorOf(g) } : undefined}
                    >
                      {on && <Check className="h-2.5 w-2.5 text-white" />}
                    </span>
                    <span className="h-2 w-2 shrink-0 rounded-full" style={{ backgroundColor: colorOf(g) }} />
                    <span className="truncate">{labelOf(g)}</span>
                  </button>
                )
              })
            )}
          </div>
          {totalCount > candidates.length && (
            <div className="px-2 pt-1.5 font-mono text-2xs text-muted-foreground/60">
              仅前 {candidates.length}（共 {totalCount}）
            </div>
          )}
        </div>
      )}
    </div>
  )
}

// 明细表（列排序 + 分页）
type TableSortKey =
  | 'name'
  | 'requests'
  | 'actual_usd'
  | 'official_usd'
  | 'margin_usd'
  | 'output_tokens'
  | 'err'
  | 'avg_ttft_ms'
const TABLE_PAGE_SIZE = 10

const errRateOf = (r: StatGroup) =>
  r.requests + r.failures > 0 ? (r.failures / (r.requests + r.failures)) * 100 : 0

function pageList(current: number, total: number): (number | 'dots')[] {
  if (total <= 7) return Array.from({ length: total }, (_, i) => i + 1)
  const out: (number | 'dots')[] = [1]
  const l = Math.max(2, current - 1)
  const r = Math.min(total - 1, current + 1)
  if (l > 2) out.push('dots')
  for (let i = l; i <= r; i++) out.push(i)
  if (r < total - 1) out.push('dots')
  out.push(total)
  return out
}

function TablePager({
  page,
  totalPages,
  onChange,
}: {
  page: number
  totalPages: number
  onChange: (p: number) => void
}) {
  const btn = 'inline-flex h-7 min-w-7 cursor-pointer items-center justify-center rounded-md border px-1.5 text-xs transition-colors'
  return (
    <div className="mt-3 flex flex-wrap items-center justify-center gap-1">
      <button
        onClick={() => onChange(Math.max(1, page - 1))}
        disabled={page === 1}
        className={cn(btn, 'border-border text-muted-foreground hover:bg-muted hover:text-foreground disabled:cursor-default disabled:opacity-40')}
      >
        ‹
      </button>
      {pageList(page, totalPages).map((p, i) =>
        p === 'dots' ? (
          <span key={`d${i}`} className="inline-flex h-7 min-w-7 items-center justify-center text-xs text-muted-foreground/50">
            …
          </span>
        ) : (
          <button
            key={p}
            onClick={() => onChange(p)}
            className={cn(
              btn,
              'tnum',
              p === page
                ? 'border-primary bg-primary text-primary-foreground'
                : 'border-border text-muted-foreground hover:bg-muted hover:text-foreground',
            )}
          >
            {p}
          </button>
        ),
      )}
      <button
        onClick={() => onChange(Math.min(totalPages, page + 1))}
        disabled={page === totalPages}
        className={cn(btn, 'border-border text-muted-foreground hover:bg-muted hover:text-foreground disabled:cursor-default disabled:opacity-40')}
      >
        ›
      </button>
    </div>
  )
}

function BreakdownTable({
  title,
  rows,
  labelOf,
}: {
  title: string
  rows: StatGroup[]
  labelOf: (key: string) => string
}) {
  const [sortKey, setSortKey] = useState<TableSortKey>('official_usd')
  const [sortDir, setSortDir] = useState<'asc' | 'desc'>('desc')
  const [page, setPage] = useState(1)

  const sorted = useMemo(() => {
    const val = (r: StatGroup): number | string =>
      sortKey === 'name'
        ? labelOf(r.key)
        : sortKey === 'err'
          ? errRateOf(r)
          : (r[sortKey] as number)
    return [...rows].sort((a, b) => {
      const va = val(a)
      const vb = val(b)
      const c =
        typeof va === 'string' || typeof vb === 'string'
          ? String(va).localeCompare(String(vb))
          : va - vb
      return sortDir === 'asc' ? c : -c
    })
  }, [rows, sortKey, sortDir, labelOf])

  const totalPages = Math.max(1, Math.ceil(sorted.length / TABLE_PAGE_SIZE))
  const safePage = Math.min(page, totalPages)
  const pageRows = sorted.slice((safePage - 1) * TABLE_PAGE_SIZE, safePage * TABLE_PAGE_SIZE)
  useEffect(() => {
    setPage(1)
  }, [sortKey, sortDir, rows.length])

  const onSort = (k: TableSortKey) => {
    if (k === sortKey) setSortDir(d => (d === 'asc' ? 'desc' : 'asc'))
    else {
      setSortKey(k)
      setSortDir(k === 'name' ? 'asc' : 'desc')
    }
  }

  const cols: { key: TableSortKey; label: string; right?: boolean }[] = [
    { key: 'name', label: '名称' },
    { key: 'requests', label: '请求', right: true },
    { key: 'actual_usd', label: '实际成本', right: true },
    { key: 'official_usd', label: '官方价', right: true },
    { key: 'margin_usd', label: '毛利', right: true },
    { key: 'output_tokens', label: '输出 token', right: true },
    { key: 'err', label: '错误率', right: true },
    { key: 'avg_ttft_ms', label: '首字', right: true },
  ]

  return (
    <ChartCard title={title} subtitle={`区间汇总 · 共 ${rows.length}`}>
      <div className="-mx-1 overflow-x-auto">
        <table className="w-full min-w-[640px] text-sm">
          <thead>
            <tr className="border-b border-border text-left font-mono text-2xs uppercase tracking-wider text-muted-foreground">
              {cols.map(c => (
                <th key={c.key} className={cn('whitespace-nowrap px-2 py-2 font-medium', c.right && 'text-right')}>
                  <button
                    onClick={() => onSort(c.key)}
                    className={cn(
                      'inline-flex cursor-pointer items-center gap-0.5 hover:text-foreground',
                      c.right && 'flex-row-reverse',
                      sortKey === c.key && 'text-foreground',
                    )}
                  >
                    <span>{c.label}</span>
                    <span className="text-[8px] leading-none">
                      {sortKey === c.key ? (sortDir === 'asc' ? '▲' : '▼') : ''}
                    </span>
                  </button>
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {pageRows.map(r => (
              <tr key={r.key} className="border-b border-border/50 last:border-0">
                <td className="px-2 py-2 font-medium">{labelOf(r.key)}</td>
                <td className="tnum px-2 py-2 text-right">{fmtNum(r.requests)}</td>
                <td className="tnum px-2 py-2 text-right">{fmtUsd(r.actual_usd)}</td>
                <td className="tnum px-2 py-2 text-right">{fmtUsd(r.official_usd)}</td>
                <td className={cn('tnum px-2 py-2 text-right', r.margin_usd < 0 ? 'text-bad' : 'text-ok')}>
                  {fmtUsd(r.margin_usd)}
                </td>
                <td className="tnum px-2 py-2 text-right">{fmtCompact(r.output_tokens)}</td>
                <td className={cn('tnum px-2 py-2 text-right', r.failures > 0 && 'text-bad')}>
                  {errRateOf(r).toFixed(1)}%
                </td>
                <td className="tnum px-2 py-2 text-right">{fmtMs(r.avg_ttft_ms)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {totalPages > 1 && <TablePager page={safePage} totalPages={totalPages} onChange={setPage} />}
    </ChartCard>
  )
}

// ━━━━━━━━━━ 小组件 ━━━━━━━━━━
function SegBar<T extends string>({
  value,
  options,
  onChange,
}: {
  value: T
  options: { key: T; label: string }[]
  onChange: (v: T) => void
}) {
  return (
    <div className="inline-flex items-center gap-0.5 rounded-xl border border-border bg-muted/40 p-0.5">
      {options.map(o => (
        <button
          key={o.key}
          onClick={() => onChange(o.key)}
          className={cn(
            'shrink-0 cursor-pointer whitespace-nowrap rounded-lg px-2.5 py-1 text-xs font-medium transition-colors',
            value === o.key
              ? 'bg-background text-foreground shadow-sm'
              : 'text-muted-foreground hover:text-foreground',
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  )
}

function Kpi({
  label,
  value,
  icon,
  tone,
}: {
  label: string
  value: string
  icon?: React.ReactNode
  tone?: 'ok' | 'bad'
}) {
  return (
    <div className="rounded-xl bg-surface p-3 shadow-stripe dark:ring-1 dark:ring-border sm:p-4">
      <div className="flex items-center gap-1.5 font-mono text-2xs uppercase tracking-wider text-muted-foreground">
        {icon}
        {label}
      </div>
      <div
        className={cn(
          'tnum mt-1.5 text-lg font-semibold tracking-tight sm:text-xl',
          tone === 'bad' && 'text-bad',
          tone === 'ok' && 'text-ok',
        )}
      >
        {value}
      </div>
    </div>
  )
}

function ChartCard({
  title,
  subtitle,
  children,
}: {
  title: string
  subtitle?: string
  children: React.ReactNode
}) {
  return (
    <div className="rounded-xl bg-surface p-3 shadow-stripe dark:ring-1 dark:ring-border sm:p-4">
      <div className="mb-2 flex items-baseline justify-between">
        <h3 className="text-sm font-semibold tracking-tight">{title}</h3>
        {subtitle && <span className="font-mono text-2xs text-muted-foreground">{subtitle}</span>}
      </div>
      {children}
    </div>
  )
}
