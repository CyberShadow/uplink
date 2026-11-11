// Emoji by their shortcodes, as Element knows them: Emojibase's data,
// with its Slack-style (iamcal) shortcodes, loaded on first use.

export interface Emoji {
  unicode: string
  label: string
  shortcodes: readonly string[]
}

let loading: Promise<readonly Emoji[]> | null = null

/** Every emoji with a shortcode, in Emojibase's order. */
export function loadEmoji(): Promise<readonly Emoji[]> {
  loading ??= (async () => {
    const [{ default: data }, { default: shortcodes }] = await Promise.all([
      import('emojibase-data/en/compact.json'),
      import('emojibase-data/en/shortcodes/iamcal.json'),
    ])
    return data.flatMap((emoji) => {
      const codes = shortcodes[emoji.hexcode as keyof typeof shortcodes] as string | string[] | undefined
      if (codes === undefined) return []
      return [{ unicode: emoji.unicode, label: emoji.label, shortcodes: typeof codes === 'string' ? [codes] : codes }]
    })
  })()
  return loading
}

/** Up to `limit` emoji for a shortcode typed so far: those whose
 * shortcodes begin with it (the shortest first), then those with it
 * within a shortcode or their label. */
export function searchEmoji(emoji: readonly Emoji[], query: string, limit = 8): Emoji[] {
  const q = query.toLowerCase()
  const rank = (e: Emoji): number => {
    const prefixed = e.shortcodes.filter((c) => c.startsWith(q))
    if (prefixed.length > 0) return Math.min(...prefixed.map((c) => c.length))
    if (e.shortcodes.some((c) => c.includes(q)) || e.label.toLowerCase().includes(q)) return 1000
    return Infinity
  }
  return emoji
    .map((e, order) => ({ e, rank: rank(e), order }))
    .filter((r) => r.rank !== Infinity)
    .sort((a, b) => a.rank - b.rank || a.order - b.order)
    .slice(0, limit)
    .map((r) => r.e)
}

/** The emoji whose shortcode is exactly `code`. */
export function byShortcode(emoji: readonly Emoji[], code: string): Emoji | undefined {
  return emoji.find((e) => e.shortcodes.includes(code))
}
