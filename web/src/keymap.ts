// Keyboard shortcuts (SPEC §12.5, §16.4): Gmail's scheme, hardcoded.
// Views bind the actions that make sense for them; the help overlay lists
// the whole map.

import { type RefObject } from 'preact'
import { useLayoutEffect, useRef } from 'preact/hooks'
import { tinykeys } from 'tinykeys'

export type Action =
  | 'next'
  | 'previous'
  | 'open'
  | 'back'
  | 'select'
  | 'selectAll'
  | 'selectNone'
  | 'archive'
  | 'markRead'
  | 'markUnread'
  | 'star'
  | 'label'
  | 'compose'
  | 'reply'
  | 'replyAll'
  | 'forward'
  | 'send'
  | 'close'
  | 'goInbox'
  | 'goStarred'
  | 'goSent'
  | 'goDrafts'
  | 'search'
  | 'help'

export interface Binding {
  action: Action
  /** tinykeys key bindings; a space separates the presses of a sequence. */
  keys: string[]
  /** What the help overlay shows for the keys. */
  label: string
  help: string
}

export const KEYMAP: Binding[] = [
  { action: 'next', keys: ['j'], label: 'j', help: 'Next message' },
  { action: 'previous', keys: ['k'], label: 'k', help: 'Previous message' },
  { action: 'open', keys: ['o', 'Enter'], label: 'o, Enter', help: 'Open message' },
  { action: 'back', keys: ['u'], label: 'u', help: 'Back to the list' },
  { action: 'select', keys: ['x'], label: 'x', help: 'Select message' },
  { action: 'selectAll', keys: ['[Shift]+* a'], label: '* a', help: 'Select all' },
  { action: 'selectNone', keys: ['[Shift]+* n'], label: '* n', help: 'Select none' },
  { action: 'archive', keys: ['e'], label: 'e', help: 'Archive' },
  { action: 'markRead', keys: ['Shift+i'], label: 'Shift+i', help: 'Mark as read' },
  { action: 'markUnread', keys: ['Shift+u'], label: 'Shift+u', help: 'Mark as unread' },
  { action: 'star', keys: ['s'], label: 's', help: 'Star or unstar' },
  { action: 'label', keys: ['l'], label: 'l', help: 'Label' },
  { action: 'compose', keys: ['c'], label: 'c', help: 'Compose' },
  { action: 'reply', keys: ['r'], label: 'r', help: 'Reply (in a room: write a message)' },
  { action: 'replyAll', keys: ['a'], label: 'a', help: 'Reply all' },
  { action: 'forward', keys: ['f'], label: 'f', help: 'Forward' },
  { action: 'send', keys: ['$mod+Enter'], label: 'Ctrl+Enter', help: 'Send (in compose)' },
  { action: 'close', keys: ['Escape'], label: 'Esc', help: 'Close compose (the draft is kept)' },
  { action: 'goInbox', keys: ['g i'], label: 'g i', help: 'Go to Inbox' },
  { action: 'goStarred', keys: ['g s'], label: 'g s', help: 'Go to Starred' },
  { action: 'goSent', keys: ['g t'], label: 'g t', help: 'Go to Sent' },
  { action: 'goDrafts', keys: ['g d'], label: 'g d', help: 'Go to Drafts' },
  { action: 'search', keys: ['/'], label: '/', help: 'Search' },
  { action: 'help', keys: ['[Shift]+?'], label: '?', help: 'Keyboard shortcuts' },
]

/** A mousedown handler for controls that should not take the focus from
 * the shortcuts when clicked (a focused button would also take Enter). */
export function keepFocus(event: MouseEvent) {
  event.preventDefault()
}

/** Binds `actions` to their keys while the component is mounted: on the
 * window, where typing in a field does not trigger them, or on `within`,
 * where it does. */
export function useKeymap(
  actions: Partial<Record<Action, () => void>>,
  within?: RefObject<HTMLElement>,
) {
  // The handlers change with every render; the bindings need not.
  const current = useRef(actions)
  current.current = actions
  const bound = Object.keys(actions).sort().join()
  // At once, not after paint, so that keys pressed right after a view
  // change reach its bindings.
  useLayoutEffect(() => {
    const map: Record<string, (event: KeyboardEvent) => void> = {}
    for (const binding of KEYMAP) {
      if (!(binding.action in current.current)) continue
      for (const keys of binding.keys) {
        map[keys] = (event) => {
          event.preventDefault()
          current.current[binding.action]!()
        }
      }
    }
    if (!within) return tinykeys(window, map)
    return tinykeys(within.current!, map, { ignore: (event) => event.repeat || event.isComposing })
  }, [bound, within])
}
