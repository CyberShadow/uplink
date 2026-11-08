// Keyboard shortcuts (SPEC §12.5, §16.4): Gmail's scheme, hardcoded.
// Views bind the actions that make sense for them; the help overlay lists
// the whole map.

import { type RefObject } from 'preact'
import { useLayoutEffect, useRef } from 'preact/hooks'
import {
  defaultKeybindingsHandlerIgnore,
  matchKeybindingPress,
  parseKeybinding,
  type KeybindingPress,
} from 'tinykeys'

export type Action =
  | 'next'
  | 'previous'
  | 'open'
  | 'back'
  | 'select'
  | 'selectAll'
  | 'selectNone'
  | 'selectRead'
  | 'selectUnread'
  | 'selectStarred'
  | 'selectUnstarred'
  | 'archive'
  | 'archiveNewer'
  | 'archiveOlder'
  | 'markRead'
  | 'markUnread'
  | 'star'
  | 'label'
  | 'move'
  | 'important'
  | 'notImportant'
  | 'flag'
  | 'done'
  | 'delete'
  | 'spam'
  | 'add'
  | 'undo'
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
  | 'goAll'
  | 'goLabel'
  | 'goTodos'
  | 'goHistory'
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
  { action: 'selectRead', keys: ['[Shift]+* r'], label: '* r', help: 'Select read' },
  { action: 'selectUnread', keys: ['[Shift]+* u'], label: '* u', help: 'Select unread' },
  { action: 'selectStarred', keys: ['[Shift]+* s'], label: '* s', help: 'Select starred' },
  { action: 'selectUnstarred', keys: ['[Shift]+* t'], label: '* t', help: 'Select unstarred' },
  { action: 'archive', keys: ['e'], label: 'e', help: 'Archive' },
  { action: 'archiveNewer', keys: ['['], label: '[', help: 'Archive, and open the newer message (in a message)' },
  { action: 'archiveOlder', keys: [']'], label: ']', help: 'Archive, and open the older message (in a message)' },
  { action: 'markRead', keys: ['Shift+i'], label: 'Shift+i', help: 'Mark as read' },
  { action: 'markUnread', keys: ['Shift+u'], label: 'Shift+u', help: 'Mark as unread' },
  { action: 'star', keys: ['s'], label: 's', help: 'Star or unstar' },
  { action: 'label', keys: ['l'], label: 'l', help: 'Label' },
  { action: 'move', keys: ['v'], label: 'v', help: 'Move to a label (out of the one on show, or the Inbox)' },
  { action: 'important', keys: ['[Shift]++', '='], label: '+, =', help: 'Mark as important' },
  { action: 'notImportant', keys: ['-'], label: '-', help: 'Mark as not important' },
  { action: 'flag', keys: ['Shift+t'], label: 'Shift+t', help: 'Flag for follow-up (adds a to-do), or unflag' },
  { action: 'done', keys: ['e'], label: 'e', help: 'Mark done, or not done (in to-dos)' },
  { action: 'delete', keys: ['[Shift]+#'], label: '#', help: 'Delete (mail: move to Trash)' },
  { action: 'spam', keys: ['[Shift]+!'], label: '!', help: 'Report spam, or not spam' },
  { action: 'add', keys: ['a'], label: 'a', help: 'Add a to-do (in to-dos)' },
  { action: 'undo', keys: ['z'], label: 'z', help: 'Undo or redo (in history)' },
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
  { action: 'goAll', keys: ['g a'], label: 'g a', help: 'Go to All Mail' },
  { action: 'goLabel', keys: ['g l'], label: 'g l', help: 'Go to a label' },
  { action: 'goTodos', keys: ['g k'], label: 'g k', help: 'Go to To-dos' },
  { action: 'goHistory', keys: ['g h'], label: 'g h', help: 'Go to History (undo anything)' },
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
 * where it does. The window's bindings, from every component, make up
 * one keymap, so that a sequence's first press (`g`) keeps a shorter
 * binding of its second (`s`) from firing too. */
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
    const bindings: Bound[] = KEYMAP.filter((b) => b.action in current.current).flatMap((b) =>
      b.keys.map((keys) => ({
        presses: parseKeybinding(keys),
        fire: (event: KeyboardEvent) => {
          event.preventDefault()
          current.current[b.action]!()
        },
      })),
    )
    if (!within) {
      windowBindings.push(bindings)
      return () => {
        windowBindings.splice(windowBindings.indexOf(bindings), 1)
      }
    }
    const handler = sequences(
      () => [bindings],
      (event) => event.repeat || event.isComposing,
    )
    const target = within.current!
    target.addEventListener('keydown', handler)
    return () => target.removeEventListener('keydown', handler)
  }, [bound, within])
}

/** A binding's keys, parsed, as bound by a component. */
interface Bound {
  presses: KeybindingPress[]
  fire: (event: KeyboardEvent) => void
}

/** The window's bindings, by component, in the order they mounted. */
const windowBindings: Bound[][] = []
window.addEventListener('keydown', sequences(() => windowBindings, defaultKeybindingsHandlerIgnore))

/** How long a sequence waits for its next press. */
const SEQUENCE_MS = 1000

const MODIFIERS = ['Shift', 'Control', 'Alt', 'Meta', 'AltGraph']

/** A keydown handler for the bindings of components. Unlike tinykeys'
 * own, which leaves other bindings half-matched once one fires (so `* r`
 * then `* u` typed at once misses the second), it tracks one sequence:
 * the presses since the last that matched nothing, or fired. Of bindings
 * of the same keys, the latest component's fires. */
function sequences(
  components: () => Bound[][],
  ignore: (event: KeyboardEvent) => boolean,
): (event: KeyboardEvent) => void {
  let typed: KeyboardEvent[] = []
  let timer: ReturnType<typeof setTimeout> | undefined
  const matching = (events: KeyboardEvent[]) =>
    components()
      .flat()
      .filter(
        (b) =>
          b.presses.length >= events.length && events.every((event, i) => matchKeybindingPress(event, b.presses[i])),
      )
  return (event) => {
    // Choosing an autocompletion sends a keydown that is a plain Event.
    if (!(event instanceof KeyboardEvent) || ignore(event) || MODIFIERS.includes(event.key)) return
    clearTimeout(timer)
    // The sequence so far, continued; or else, a new one.
    for (const events of [[...typed, event], [event]]) {
      const candidates = matching(events)
      if (candidates.length === 0) continue
      const complete = candidates.findLast((b) => b.presses.length === events.length)
      if (complete) {
        typed = []
        complete.fire(event)
      } else {
        typed = events
        timer = setTimeout(() => (typed = []), SEQUENCE_MS)
      }
      return
    }
    typed = []
  }
}
