// The app's icon (SPEC §12.14): the logo, with the count of what the
// alert-badged buckets hold on it, in red, as Gmail's and Element's
// count; else of what the count-badged ones hold, in the accent colour;
// else a dot, while only dot-badged ones hold anything: home's mark.

import { useEffect } from 'preact/hooks'
import type { Badges } from './generated/protocol'
import logo from '../public/logo.svg?raw'

/** The most an icon counts: more is `99+`. */
const MOST = 99

export type Mark = { kind: 'alert' | 'count'; text: string } | { kind: 'dot' } | null

/** What the icon shows of `badges`: home's mark (SPEC §12.14); nothing
 * until the server says. */
export function markOf(badges: Badges | null): Mark {
  if (badges === null) return null
  const { alert, count, dot } = badges.marks.home
  const text = (n: number) => (n > MOST ? `${MOST}+` : String(n))
  if (alert > 0) return { kind: 'alert', text: text(alert) }
  if (count > 0) return { kind: 'count', text: text(count) }
  return dot ? { kind: 'dot' } : null
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
    case 'alert':
    case 'count': {
      // Wide enough for its digits, at the bottom right, as Gmail's.
      const width = 8 + 9 * mark.text.length
      const fill = mark.kind === 'alert' ? '#d93025' : '#1a73e8'
      return (
        `<rect x="${32 - width}" y="13" width="${width}" height="19" rx="9.5" fill="${fill}"/>` +
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
