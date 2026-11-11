// Keyboard shortcuts (SPEC §12.5, §16.4): Gmail's scheme, hardcoded.
// Views bind the actions that make sense for them; the help overlay lists
// the whole map.

import { type RefObject } from 'preact'
import { useEffect, useLayoutEffect, useRef } from 'preact/hooks'
import {
  matchKeybindingPress,
  parseKeybinding,
  type KeybindingPress,
} from 'tinykeys'

export type Action =
  | 'next'
  | 'previous'
  | 'pageUp'
  | 'pageDown'
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
  | 'write'
  | 'replyAll'
  | 'forward'
  | 'send'
  | 'close'
  | 'goHome'
  | 'goInbox'
  | 'goStarred'
  | 'goSent'
  | 'goDrafts'
  | 'goAll'
  | 'goLabel'
  | 'goTodos'
  | 'goHistory'
  | 'goCalendar'
  | 'today'
  | 'weekView'
  | 'agendaView'
  | 'search'
  | 'switch'
  | 'help'

/** What a component does by actions' keys. */
export type Actions = Partial<Record<Action, () => void>>

export interface Binding {
  action: Action
  /** tinykeys key bindings; a space separates the presses of a sequence. */
  keys: string[]
  /** What the help overlay shows for the keys. */
  label: string
  help: string
  /** Held down, it repeats, as moving does; others, done again by a held
   * key's repeats, would act on more than meant. */
  repeats?: true
}

/** As tinykeys tells macOS, for `$mod`, which is Cmd there and Ctrl
 * elsewhere. */
const MAC = /Mac|iPod|iPhone|iPad/.test(navigator.platform)
const MOD = MAC ? 'Cmd' : 'Ctrl'

/** A go-to's keys: `g` and `key`; and, from within fields too, Alt+Shift
 * and `key`, the modifiers least taken by browsers and systems. On macOS,
 * where Option types characters and browsers keep Cmd+Shift (Slack's
 * Cmd+Shift go-tos work only in its app), Ctrl+Option, as Google Docs'
 * menus; by where `key` is, as Option changes what it types. */
function go(key: string): Pick<Binding, 'keys' | 'label'> {
  const upper = key.toUpperCase()
  return MAC
    ? { keys: [`g ${key}`, `Control+Alt+Key${upper}`], label: `g ${key}, Ctrl+Option+${upper}` }
    : { keys: [`g ${key}`, `Alt+Shift+${key}`], label: `g ${key}, Alt+Shift+${upper}` }
}

export const KEYMAP: Binding[] = [
  { action: 'next', keys: ['j'], label: 'j', help: 'Next conversation (in the calendar: the next week or agenda)', repeats: true },
  { action: 'previous', keys: ['k'], label: 'k', help: 'Previous conversation (in the calendar: the previous week or agenda)', repeats: true },
  { action: 'pageUp', keys: ['PageUp'], label: 'PgUp', help: "Scroll up a page (in a room, its box's too)", repeats: true },
  { action: 'pageDown', keys: ['PageDown'], label: 'PgDn', help: "Scroll down a page (in a room, its box's too)", repeats: true },
  { action: 'open', keys: ['o', 'Enter'], label: 'o, Enter', help: 'Open conversation' },
  { action: 'back', keys: ['u'], label: 'u', help: 'Back to the list' },
  { action: 'select', keys: ['x'], label: 'x', help: 'Select conversation' },
  { action: 'selectAll', keys: ['[Shift]+* a'], label: '* a', help: 'Select all' },
  { action: 'selectNone', keys: ['[Shift]+* n'], label: '* n', help: 'Select none' },
  { action: 'selectRead', keys: ['[Shift]+* r'], label: '* r', help: 'Select read' },
  { action: 'selectUnread', keys: ['[Shift]+* u'], label: '* u', help: 'Select unread' },
  { action: 'selectStarred', keys: ['[Shift]+* s'], label: '* s', help: 'Select starred' },
  { action: 'selectUnstarred', keys: ['[Shift]+* t'], label: '* t', help: 'Select unstarred' },
  { action: 'archive', keys: ['e'], label: 'e', help: 'Archive' },
  { action: 'archiveNewer', keys: ['['], label: '[', help: 'Archive, and open the newer conversation (in one)' },
  { action: 'archiveOlder', keys: [']'], label: ']', help: 'Archive, and open the older conversation (in one)' },
  { action: 'markRead', keys: ['Shift+i'], label: 'Shift+i', help: 'Mark as read' },
  { action: 'markUnread', keys: ['Shift+u'], label: 'Shift+u', help: 'Mark as unread' },
  { action: 'star', keys: ['s'], label: 's', help: 'Star or unstar (a room: favourite it)' },
  { action: 'label', keys: ['l'], label: 'l', help: 'Label (in a room: tag it)' },
  { action: 'move', keys: ['v'], label: 'v', help: 'Move to a label (out of the one on show, or the Inbox)' },
  { action: 'important', keys: ['[Shift]++', '='], label: '+, =', help: 'Mark as important' },
  { action: 'notImportant', keys: ['-'], label: '-', help: 'Mark as not important' },
  { action: 'flag', keys: ['Shift+t'], label: 'Shift+t', help: 'Flag for follow-up (adds a to-do), or unflag' },
  { action: 'done', keys: ['e'], label: 'e', help: 'Mark done, or not done (in to-dos); at home: archive, read the room, or finish the to-do' },
  { action: 'delete', keys: ['[Shift]+#'], label: '#', help: 'Delete (mail: move to Trash)' },
  { action: 'spam', keys: ['[Shift]+!'], label: '!', help: 'Report spam, or not spam' },
  { action: 'add', keys: ['a'], label: 'a', help: 'Add a to-do (in to-dos)' },
  { action: 'undo', keys: ['z'], label: 'z', help: 'Undo the last action; again, the one before it (in history: undo or redo the one focused)' },
  { action: 'compose', keys: ['c'], label: 'c', help: 'Compose' },
  { action: 'reply', keys: ['r'], label: 'r', help: 'Reply (in a room: write a message)' },
  { action: 'write', keys: ['i', 'Enter'], label: 'i, Enter', help: 'Write a message (in a room, whose box Esc leaves)' },
  { action: 'replyAll', keys: ['a'], label: 'a', help: 'Reply all' },
  { action: 'forward', keys: ['f'], label: 'f', help: 'Forward' },
  { action: 'send', keys: ['$mod+Enter'], label: `${MOD}+Enter`, help: 'Send (in compose)' },
  { action: 'close', keys: ['Escape'], label: 'Esc', help: 'Close compose (the draft is kept)' },
  { action: 'goHome', ...go('h'), help: 'Go home' },
  { action: 'goInbox', ...go('i'), help: 'Go to Inbox' },
  { action: 'goStarred', ...go('s'), help: 'Go to Starred' },
  { action: 'goSent', ...go('t'), help: 'Go to Sent' },
  { action: 'goDrafts', ...go('d'), help: 'Go to Drafts' },
  { action: 'goAll', ...go('a'), help: 'Go to All Mail' },
  { action: 'goLabel', ...go('l'), help: 'Go to a label' },
  { action: 'goTodos', ...go('k'), help: 'Go to To-dos' },
  { action: 'goHistory', ...go('z'), help: 'Go to History (undo anything)' },
  { action: 'goCalendar', ...go('c'), help: 'Go to the calendar' },
  { action: 'today', keys: ['t'], label: 't', help: 'Today (in the calendar)' },
  { action: 'weekView', keys: ['w'], label: 'w', help: 'Week view (in the calendar)' },
  { action: 'agendaView', keys: ['a'], label: 'a', help: 'Agenda view (in the calendar)' },
  { action: 'search', keys: ['/'], label: '/', help: 'Search' },
  { action: 'switch', keys: ['$mod+k'], label: `${MOD}+K`, help: 'Switch to a view or room, the most recent first (also while typing)' },
  { action: 'help', keys: ['[Shift]+?'], label: '?', help: 'Keyboard shortcuts' },
]

/** A mousedown handler for controls that should not take the focus from
 * the shortcuts when clicked (a focused button would also take Enter). */
export function keepFocus(event: MouseEvent) {
  event.preventDefault()
}

/** `keepFocus` for controls that are also dragged: as preventing the
 * mousedown's default would keep a drag from starting, the control takes
 * the focus, and gives it back once the press ends, clicked or dragged. */
export function returnFocus(event: MouseEvent) {
  const previous = document.activeElement!
  const pressed = event.currentTarget as HTMLElement
  const back = () => {
    window.removeEventListener('mouseup', back)
    window.removeEventListener('dragend', back)
    if (previous === document.body) pressed.blur()
    else (previous as HTMLElement).focus({ preventScroll: true })
  }
  window.addEventListener('mouseup', back)
  window.addEventListener('dragend', back)
}

/** Closes an overlay, while `close` is given, by Escape: wherever the
 * focus is (in the box being written in, say), and before the key
 * reaches anything else. */
export function useEscape(close: (() => void) | null) {
  useEffect(() => {
    if (!close) return
    const escape = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return
      event.stopPropagation()
      close()
    }
    window.addEventListener('keydown', escape, true)
    return () => window.removeEventListener('keydown', escape, true)
  }, [close])
}

/** Scrolls `scroller` by a page, as the browser's page keys do: by what
 * it shows, less a little to read on from. */
export function pages(scroller: RefObject<HTMLElement>): Actions {
  const by = (sign: number) => () =>
    scroller.current!.scrollBy({ top: sign * scroller.current!.clientHeight * PAGE })
  return { pageUp: by(-1), pageDown: by(1) }
}

/** What a page's scroll moves of what is shown. */
const PAGE = 0.875

/** Binds `actions` to their keys while the component is mounted: on the
 * window, where typing in a field does not trigger them (but for presses
 * with Ctrl, Cmd or Alt held), or on `within`,
 * where it does. The window's bindings, from every component, make up
 * one keymap, so that a sequence's first press (`g`) keeps a shorter
 * binding of its second (`s`) from firing too. */
export function useKeymap(
  actions: Actions,
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
        repeats: b.repeats === true && !keys.includes(' '),
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
      (event) => event.isComposing,
    )
    const target = within.current!
    target.addEventListener('keydown', handler)
    return () => target.removeEventListener('keydown', handler)
  }, [bound, within])
}

/** A binding's keys, parsed, as bound by a component. */
interface Bound {
  presses: KeybindingPress[]
  /** Fires on a held key's repeats; a single press. */
  repeats: boolean
  fire: (event: KeyboardEvent) => void
}

/** The window's bindings, by component, in the order they mounted. */
const windowBindings: Bound[][] = []
// tinykeys' own ignores fields, and repeats: those, `sequences` handles.
window.addEventListener(
  'keydown',
  sequences(
    () => windowBindings,
    (event) => {
      const target = event.target as Element
      return (
        event.isComposing ||
        // What is typed into a field is its own; but not Ctrl, Cmd or Alt
        // held.
        (!event.ctrlKey &&
          !event.metaKey &&
          !event.altKey &&
          target !== event.currentTarget &&
          target.matches('[contenteditable],input,select,textarea'))
      )
    },
  ),
)

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
    if (event.repeat) {
      components()
        .flat()
        .findLast((b) => b.repeats && matchKeybindingPress(event, b.presses[0]))
        ?.fire(event)
      return
    }
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
