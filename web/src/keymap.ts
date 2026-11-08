// Keyboard shortcuts (SPEC §12.5, §16.4): Gmail's scheme, hardcoded.
// Views bind the actions that make sense for them; the help overlay lists
// the whole map.

import { useEffect, useRef } from 'preact/hooks'
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
  { action: 'goInbox', keys: ['g i'], label: 'g i', help: 'Go to Inbox' },
  { action: 'goStarred', keys: ['g s'], label: 'g s', help: 'Go to Starred' },
  { action: 'goSent', keys: ['g t'], label: 'g t', help: 'Go to Sent' },
  { action: 'goDrafts', keys: ['g d'], label: 'g d', help: 'Go to Drafts' },
  { action: 'search', keys: ['/'], label: '/', help: 'Search' },
  { action: 'help', keys: ['[Shift]+?'], label: '?', help: 'Keyboard shortcuts' },
]

/** Binds `actions` to their keys while the component is mounted. */
export function useKeymap(actions: Partial<Record<Action, () => void>>) {
  // The handlers change with every render; the bindings need not.
  const current = useRef(actions)
  current.current = actions
  const bound = Object.keys(actions).sort().join()
  useEffect(() => {
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
    return tinykeys(window, map)
  }, [bound])
}
