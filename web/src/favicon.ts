// The app's icon (SPEC §12.14): the logo, with the count of what the
// count-badged buckets hold on it, as Gmail's and Element's count; or a
// dot, while only dot-badged ones hold anything.

import { useEffect } from 'preact/hooks'
import type { Badges } from './generated/protocol'
import logo from '../public/logo.svg?raw'

/** The most an icon counts: more is `99+`, as a bucket names at most
 * `BADGE_LIMIT` (100). */
const MOST = 99

export type Mark = { kind: 'count'; text: string } | { kind: 'dot' } | null

/** What the icon shows of `badges`; nothing until the server says. */
export function markOf(badges: Badges | null): Mark {
  if (badges === null) return null
  const count = badges.buckets.filter((b) => b.badge === 'count').reduce((n, b) => n + b.items.length, 0)
  if (count > 0) return { kind: 'count', text: count > MOST ? `${MOST}+` : String(count) }
  return badges.buckets.some((b) => b.badge === 'dot' && b.items.length > 0) ? { kind: 'dot' } : null
}

/** The icon's URL: the logo, `mark` drawn over it, in its 32×32. */
export function iconOf(mark: Mark): string {
  if (mark === null) return '/logo.svg'
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32">${logo}${overlay(mark)}</svg>`
  return `data:image/svg+xml,${encodeURIComponent(svg)}`
}

function overlay(mark: Exclude<Mark, null>): string {
  switch (mark.kind) {
    case 'dot':
      return '<circle cx="25" cy="7" r="6" fill="#1a73e8" stroke="#fff" stroke-width="2"/>'
    case 'count': {
      // Wide enough for its digits, at the bottom right, as Gmail's.
      const width = 8 + 9 * mark.text.length
      return (
        `<rect x="${32 - width}" y="13" width="${width}" height="19" rx="9.5" fill="#d93025"/>` +
        `<text x="${32 - width / 2}" y="28" fill="#fff" font-family="sans-serif" font-size="16" ` +
        `font-weight="bold" text-anchor="middle">${mark.text}</text>`
      )
    }
  }
}

/** Shows the badges on the page's icon. */
export function useFavicon(badges: Badges | null) {
  const href = iconOf(markOf(badges))
  useEffect(() => {
    document.querySelector<HTMLLinkElement>('link[rel=icon]')!.href = href
  }, [href])
}
