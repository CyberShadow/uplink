// Completions in the composer (SPEC §12.11), as Element's: a command
// after a leading `/`, a member after `@`, an emoji after `:` and two
// characters. The list shows above the box; Up and Down choose, Tab or
// Enter takes, Escape dismisses.

import { useEffect, useState } from 'preact/hooks'
import type { MemberInfo, Room } from '../generated/protocol'
import { COMMANDS } from '../commands'
import { useServices } from '../context'
import { type Emoji, loadEmoji, searchEmoji } from '../emoji'
import { displayName } from '../matrix'
import styles from './Room.module.css'

/** What is being typed that completes: its kind, the text typed so far
 * after its sigil, and where it begins. */
export interface Completing {
  kind: 'command' | 'mention' | 'emoji'
  query: string
  start: number
}

export function completing(text: string, caret: number): Completing | null {
  const before = text.slice(0, caret)
  const command = /^\/(\w*)$/.exec(before)
  if (command) return { kind: 'command', query: command[1], start: 0 }
  const mention = /(^|\s)@([^\s@]*)$/.exec(before)
  if (mention) return { kind: 'mention', query: mention[2], start: caret - mention[2].length - 1 }
  const emoji = /(^|\s):([\w+-]{2,})$/.exec(before)
  if (emoji) return { kind: 'emoji', query: emoji[2], start: caret - emoji[2].length - 1 }
  return null
}

export interface Suggestion {
  key: string
  label: string
  detail: string
  /** What replaces the text typed. */
  insert: string
}

/** A Markdown link to the user, which mentions them (SPEC §9.3); its
 * text is their name, but brackets, which would end it. */
export function mention(user: string, name: string): string {
  return `[${name.replace(/[[\]]/g, '')}](https://matrix.to/#/${user})`
}

/** The members whose name has a word beginning with `query`, or whose
 * id does, but the account; then `@room`, if `query` begins it, which
 * notifies the whole room (as its power levels let the sender). */
export function memberSuggestions(members: readonly MemberInfo[], query: string, me: string): Suggestion[] {
  const q = query.toLowerCase()
  const found: Suggestion[] = members
    .filter((m) => m.membership === 'joined' && m.user !== me)
    .filter((m) => {
      const name = displayName(m.user, m.name).toLowerCase()
      return name.split(/\s+/).some((word) => word.startsWith(q)) || m.user.slice(1).toLowerCase().startsWith(q)
    })
    .slice(0, 8)
    .map((m) => {
      const name = displayName(m.user, m.name)
      return { key: m.user, label: name, detail: m.user, insert: `${mention(m.user, name)} ` }
    })
  if (q !== '' && 'room'.startsWith(q)) {
    found.push({ key: '@room', label: '@room', detail: 'Notify the whole room', insert: '@room ' })
  }
  return found
}

export function commandSuggestions(query: string): Suggestion[] {
  return COMMANDS.filter((c) => c.name.startsWith(query)).map((c) => ({
    key: c.name,
    label: `/${c.name} ${c.args}`.trim(),
    detail: c.description,
    insert: `/${c.name} `,
  }))
}

/** Each with the shortcode that matched, else its first. */
export function emojiSuggestions(emoji: readonly Emoji[], query: string): Suggestion[] {
  const q = query.toLowerCase()
  return searchEmoji(emoji, query).map((e) => ({
    key: e.unicode,
    label: `${e.unicode} :${
      e.shortcodes.find((c) => c.startsWith(q)) ?? e.shortcodes.find((c) => c.includes(q)) ?? e.shortcodes[0]
    }:`,
    detail: e.label,
    insert: e.unicode,
  }))
}

/** Emoji, once loaded: loaded as soon as `wanted`. */
export function useEmoji(wanted: boolean): readonly Emoji[] | null {
  const [emoji, setEmoji] = useState<readonly Emoji[] | null>(null)
  useEffect(() => {
    if (wanted && emoji === null) loadEmoji().then(setEmoji)
  }, [wanted, emoji])
  return emoji
}

/** The suggestions for what is being typed. */
export function useSuggestions(room: Room, me: string, typing: Completing | null): Suggestion[] {
  const { matrix } = useServices()
  const emoji = useEmoji(typing?.kind === 'emoji')
  switch (typing?.kind) {
    case undefined:
      return []
    case 'command':
      return commandSuggestions(typing.query)
    case 'mention':
      return memberSuggestions(matrix.members(room.id).state.value?.members ?? [], typing.query, me)
    case 'emoji':
      return emoji === null ? [] : emojiSuggestions(emoji, typing.query)
  }
}

export function Suggestions(props: {
  suggestions: readonly Suggestion[]
  selected: number
  choose: (suggestion: Suggestion) => void
}) {
  return (
    <ul class={styles.suggestions} role="listbox" aria-label="Suggestions">
      {props.suggestions.map((s, i) => (
        <li
          key={s.key}
          role="option"
          aria-selected={i === props.selected}
          // Keeping the box's focus.
          onMouseDown={(event) => {
            event.preventDefault()
            props.choose(s)
          }}
        >
          <span class={styles.suggestion}>{s.label}</span>
          <span class={styles.muted}>{s.detail}</span>
        </li>
      ))}
    </ul>
  )
}
