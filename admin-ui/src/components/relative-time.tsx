import { formatLastUsed } from '@/lib/utils'
import { pickTickResolution, useNow } from '@/lib/use-now'

interface RelativeTimeProps {
  value: string | null | undefined
}

export function RelativeTime({ value }: RelativeTimeProps) {
  useNow(pickTickResolution(value))
  return <>{formatLastUsed(value)}</>
}
