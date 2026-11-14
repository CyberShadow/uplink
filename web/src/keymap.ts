// Keyboard shortcuts (SPEC §12.5, §16.4): Gmail's scheme, hardcoded.
// Views bind the actions that make sense for them; the help overlay lists
// the whole map.

import { type RefObject } from 'preact'
import { signal } from '@preact/signals'
import { useEffect, useLayoutEffect, useRef, useState } from 'preact/hooks'
import {
  matchKeybindingPress,
  parseKeybinding,
  type KeybindingPress,
} from 'tinykeys'

export type Action =
  | 'next'
  | 'previous'
  | 'lineDown'
  | 'lineUp'
  | 'pageUp'
  | 'pageDown'
  | 'jumpToUnread'
  | 'nextMention'
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
  | 'edit'
  | 'thread'
  | 'react'
  | 'pin'
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
  | 'up'
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
  | 'dayView'
  | 'nextEvent'
  | 'previousEvent'
  | 'join'
  | 'weekView'
  | 'agendaView'
  | 'search'
  | 'switch'
  | 'navNext'
  | 'navPrevious'
  | 'navNextUnread'
  | 'navPreviousUnread'
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
export const MOD = MAC ? 'Cmd' : 'Ctrl'
export const ALT = MAC ? 'Option' : 'Alt'

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

/** Of a move in the nav, by Alt (Option) and an arrow, from within
 * fields too, as Element's and Slack's moves through rooms. */
function nav(action: Action, key: string, label: string, help: string): Binding {
  return { action, keys: [`Alt+${key}`], label: `${ALT}+${label}`, help, repeats: true }
}

export const KEYMAP: Binding[] = [
  { action: 'next', keys: ['j'], label: 'j', help: 'Next conversation (in a room: the newer message; in the calendar: the next day, week or agenda)', repeats: true },
  { action: 'previous', keys: ['k'], label: 'k', help: 'Previous conversation (in a room: the older message; in the calendar: the previous day, week or agenda)', repeats: true },
  { action: 'lineDown', keys: ['ArrowDown'], label: '↓', help: 'Down a line, as j (in a list)', repeats: true },
  { action: 'lineUp', keys: ['ArrowUp'], label: '↑', help: 'Up a line, as k (in a list)', repeats: true },
  { action: 'pageUp', keys: ['PageUp'], label: 'PgUp', help: "Scroll up a page (in a room, its box's too)", repeats: true },
  { action: 'pageDown', keys: ['PageDown'], label: 'PgDn', help: "Scroll down a page (in a room, its box's too)", repeats: true },
  { action: 'jumpToUnread', keys: ['Shift+PageUp'], label: 'Shift+PgUp', help: 'Jump to the first unread message (in a room)' },
  { action: 'nextMention', keys: ['Shift+PageDown'], label: 'Shift+PgDn', help: 'Jump to the next mention not yet seen (in a room)' },
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
  {
    action: 'delete',
    // macOS's Delete key is Backspace, as Apple Mail's delete; elsewhere,
    // Backspace is left to the browser (as Back, where it is).
    keys: MAC ? ['[Shift]+#', 'Backspace'] : ['[Shift]+#', 'Delete'],
    label: '#, Delete',
    help: 'Delete (mail: move to Trash; in a room, the message, if your own, once asked; in the calendar: the event open, if your own, of a series the occurrence)',
  },
  { action: 'edit', keys: ['e'], label: 'e', help: 'Edit the message (in a room, your own); in the calendar: the event open, if your own' },
  { action: 'thread', keys: ['t'], label: 't', help: 'Open the message\'s thread, or reply in one (in a room)' },
  { action: 'react', keys: ['[Shift]++'], label: '+', help: 'React to the message (in a room)' },
  { action: 'pin', keys: ['p'], label: 'p', help: 'Pin or unpin the message (in a room)' },
  { action: 'spam', keys: ['[Shift]+!'], label: '!', help: 'Report spam, or not spam' },
  { action: 'add', keys: ['a'], label: 'a', help: 'Add a to-do (in to-dos)' },
  { action: 'undo', keys: ['z'], label: 'z', help: 'Undo the last action; again, the one before it (in history: undo or redo the one focused)' },
  { action: 'compose', keys: ['c'], label: 'c', help: 'Compose (in the calendar: a new event)' },
  { action: 'reply', keys: ['r'], label: 'r', help: 'Reply (in a room: to the message, or else write one)' },
  { action: 'write', keys: ['i', 'Enter'], label: 'i, Enter', help: 'Write a message (in a room, whose box Esc leaves)' },
  { action: 'replyAll', keys: ['a'], label: 'a', help: 'Reply all' },
  { action: 'forward', keys: ['f'], label: 'f', help: 'Forward' },
  { action: 'send', keys: ['$mod+Enter'], label: `${MOD}+Enter`, help: 'Send (in compose; in an event: save it)' },
  { action: 'close', keys: ['Escape'], label: 'Esc', help: 'Close compose (the draft is kept), or an event unsaved' },
  {
    action: 'up',
    keys: ['Escape'],
    label: 'Esc',
    help: 'Up, again and again: out of what has the focus, then closes a panel, back to the list, selects none (in a room behind: marks it read), and home',
  },
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
  { action: 'nextEvent', keys: ['n'], label: 'n', help: 'Open the next event (in the calendar; with none open, the first yet to end)', repeats: true },
  { action: 'previousEvent', keys: ['p'], label: 'p', help: 'Open the previous event (in the calendar)', repeats: true },
  { action: 'join', keys: ['Shift+j'], label: 'Shift+j', help: 'Join the online meeting (in the calendar: the one open, or else the one on or starting in 10 minutes)' },
  { action: 'dayView', keys: ['d'], label: 'd', help: 'Day view (in the calendar)' },
  { action: 'weekView', keys: ['w'], label: 'w', help: 'Week view (in the calendar)' },
  { action: 'agendaView', keys: ['a'], label: 'a', help: 'Agenda view (in the calendar)' },
  { action: 'search', keys: ['/'], label: '/', help: 'Search' },
  { action: 'switch', keys: ['$mod+k'], label: `${MOD}+K`, help: 'Switch to a view or room, the most recent first (also while typing)' },
  nav('navNext', 'ArrowDown', '↓', 'The next view in the nav, as Element\'s next room'),
  nav('navPrevious', 'ArrowUp', '↑', 'The previous view in the nav'),
  nav('navNextUnread', 'Shift+ArrowDown', 'Shift+↓', 'The next view in the nav with unread'),
  nav('navPreviousUnread', 'Shift+ArrowUp', 'Shift+↑', 'The previous view in the nav with unread'),
  { action: 'help', keys: ['[Shift]+?'], label: '?', help: 'Keyboard shortcuts' },
]

/** Focuses a box that shows, as one writes in it next (a room's): but
 * not where the pointer is a finger, as on a phone, where that would
 * raise the on-screen keyboard over what one came to read, and apps
 * leave it to a tap on the box. */
export function focusShown(box: HTMLElement) {
  if (!TOUCH) box.focus()
}

const TOUCH = matchMedia('(pointer: coarse)').matches

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

/** A list's moves to the next and previous of it: by j/k, and by the
 * arrows, as Slack's and Discord's lists move (not where an arrow
 * scrolls, as in an open conversation). */
export function lines(next: () => void, previous: () => void): Actions {
  return { next, previous, lineDown: next, lineUp: previous }
}

/** Gives `element` the focus once shown, as a dialog's field or button,
 * and once it is gone, back to where it was: to the line acted on, or the
 * box being written in. */
export function useTakeFocus(element: RefObject<HTMLElement>) {
  // At once, not after paint: keys typed right after what showed it are
  // its.
  useLayoutEffect(() => {
    const previous = document.activeElement as HTMLElement
    element.current!.focus()
    return () => previous.focus()
  }, [element])
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

/** Where a component's bindings count, but for the window, and not in
 * fields (but for presses with Ctrl, Cmd or Alt held): `focus`, on the
 * window too, while the focus is within it, as a list's keys for its
 * row that has the focus; or `within`, on its own keydowns, where fields
 * count too. */
export type Where = { focus: RefObject<HTMLElement> } | { within: RefObject<HTMLElement> }

/** Binds `actions` to their keys while the component is mounted, as
 * `where` says, or else on the window. The window's bindings, from every
 * component (those with `focus` too), make up one keymap, so that a
 * sequence's first press (`g`) keeps a shorter binding of its second
 * (`s`) from firing too. */
export function useKeymap(actions: Actions, where?: Where) {
  // The handlers change with every render; the bindings need not.
  const current = useRef(actions)
  current.current = actions
  const bound = Object.keys(actions).sort().join()
  const scope = where && 'focus' in where ? where.focus : null
  const within = where && 'within' in where ? where.within : null
  // Where its bindings rank: as of its first render, which comes after its
  // parent's, as its effects do not.
  const [rank] = useState(() => nextRank++)
  // At once, not after paint, so that keys pressed right after a view
  // change reach its bindings.
  useLayoutEffect(() => {
    const bindings: Bound[] = KEYMAP.filter((b) => b.action in current.current).flatMap((b) =>
      b.keys.map((keys) => ({
        presses: parseKeybinding(keys),
        shown: keys.split(' ').map((press) => press.replace('[Shift]+', '')),
        binding: b,
        repeats: b.repeats === true && !keys.includes(' '),
        scope,
        fire: (event: KeyboardEvent) => {
          event.preventDefault()
          current.current[b.action]!()
        },
      })),
    )
    if (!within) {
      const at = windowBindings.findIndex((b) => ranks.get(b)! > rank)
      ranks.set(bindings, rank)
      windowBindings.splice(at === -1 ? windowBindings.length : at, 0, bindings)
      return () => {
        windowBindings.splice(windowBindings.indexOf(bindings), 1)
      }
    }
    const handler = sequences(
      () => [bindings],
      null,
      null,
      (event) => event.isComposing,
    )
    const element = within.current!
    element.addEventListener('keydown', handler)
    return () => element.removeEventListener('keydown', handler)
  }, [bound, scope, within, rank])
}

/** A binding's keys, parsed, as bound by a component. */
interface Bound {
  presses: KeybindingPress[]
  /** The presses, as shown while a sequence waits for one. */
  shown: string[]
  binding: Binding
  /** Fires on a held key's repeats; a single press. */
  repeats: boolean
  /** Counts only while the focus is within it. */
  scope: RefObject<HTMLElement> | null
  fire: (event: KeyboardEvent) => void
}

/** The window's bindings, by component, in the order they first
 * rendered: a component's after its parent's, and after those on show
 * before it; the nearest last. */
const windowBindings: Bound[][] = []
const ranks = new WeakMap<Bound[], number>()
let nextRank = 0

/** The actions the views on show bind, for the help to list first. */
export function boundActions(): Set<Action> {
  return new Set(windowBindings.flat().map((b) => b.binding.action))
}
/** Whether a press is a field's own: what is typed into one, but with
 * Ctrl, Cmd or Alt held. */
const typed = (event: KeyboardEvent) =>
  !event.ctrlKey &&
  !event.metaKey &&
  !event.altKey &&
  (event.target as Element).matches('[contenteditable],input,select,textarea')
// tinykeys' own ignores fields, and repeats: those, `sequences` handles.
const windowKeys = sequences(
  () => windowBindings,
  (pending) => (pendingSequence.value = pending),
  // Escape leaves what has the focus (a line, a link), as fields leave
  // themselves, before it goes `up` from nothing; but after what is
  // bound where the focus is (a panel opened from a line closes first).
  (event) => {
    const focused = document.activeElement as HTMLElement
    if (event.key !== 'Escape' || focused === document.body) return false
    focused.blur()
    return true
  },
  (event) =>
    event.isComposing ||
    typed(event) ||
    // As are the keys that activate a control that has the focus: a link
    // in a message, Tabbed to.
    (ACTIVATE.includes(event.key) && (event.target as Element).matches('a[href],button,summary')),
)
window.addEventListener('keydown', windowKeys)

/** Lets the window's bindings have the keydowns of a frame's `window`, of
 * the app's origin, as if its own: a mail's, which has no scripts to
 * pass them on, and keeps them while it has the focus. The app's document
 * has the frame for its focused element, which bindings with `focus`
 * count as within them, and which Escape leaves. */
export function takeKeys(frame: Window) {
  frame.addEventListener('keydown', windowKeys)
}

const ACTIVATE = ['Enter', ' ']

/** How long a sequence waits for its next press; on the window, until
 * what may follow is shown, after which it waits as long as it takes. */
const SEQUENCE_MS = 1000
const WHICH_MS = 500

/** The presses of a window's sequence begun, once it has waited a while,
 * and what may follow: shown (`WhichKey`), as Emacs' which-key does. */
export const pendingSequence = signal<Pending | null>(null)

export interface Pending {
  typed: string
  next: Array<{ key: string; help: string }>
}

const MODIFIERS = ['Shift', 'Control', 'Alt', 'Meta', 'AltGraph']

/** A keydown handler for the bindings of components. Unlike tinykeys'
 * own, which leaves other bindings half-matched once one fires (so `* r`
 * then `* u` typed at once misses the second), it tracks one sequence:
 * the presses since the last that matched nothing, or fired. Of bindings
 * of the same keys, one of a scope that has the focus fires, as nearer;
 * else, the latest component's. */
function sequences(
  components: () => Bound[][],
  /** Shows a sequence begun, or none. */
  show: ((pending: Pending | null) => void) | null,
  /** Takes a press before an unscoped binding of it fires, if it does. */
  leave: ((event: KeyboardEvent) => boolean) | null,
  ignore: (event: KeyboardEvent) => boolean,
): (event: KeyboardEvent) => void {
  let typed: KeyboardEvent[] = []
  let timer: ReturnType<typeof setTimeout> | undefined
  let which: ReturnType<typeof setTimeout> | undefined
  const reset = () => {
    typed = []
    clearTimeout(timer)
    clearTimeout(which)
    show?.(null)
  }
  /** The bindings that count where the focus is. */
  const live = () =>
    components()
      .flat()
      .filter((b) => b.scope === null || b.scope.current!.contains(document.activeElement))
  const matching = (events: KeyboardEvent[]) =>
    live().filter(
      (b) =>
        b.presses.length >= events.length && events.every((event, i) => matchKeybindingPress(event, b.presses[i])),
    )
  const nearest = (bindings: Bound[]) => bindings.findLast((b) => b.scope !== null) ?? bindings.at(-1)
  return (event) => {
    // Choosing an autocompletion sends a keydown that is a plain Event,
    // without a key; a frame's are of its own `KeyboardEvent`.
    if (event.key === undefined || MODIFIERS.includes(event.key)) return
    // Typing in a field gives up a sequence begun, too.
    if (ignore(event)) {
      reset()
      return
    }
    if (event.repeat) {
      nearest(live().filter((b) => b.repeats && matchKeybindingPress(event, b.presses[0])))?.fire(event)
      return
    }
    // Esc gives up a sequence begun.
    if (typed.length > 0 && event.key === 'Escape') {
      reset()
      event.preventDefault()
      return
    }
    const before = typed
    reset()
    // The sequence so far, continued; or else, a new one.
    for (const events of [[...before, event], [event]]) {
      const candidates = matching(events)
      if (candidates.length === 0) continue
      const complete = nearest(candidates.filter((b) => b.presses.length === events.length))
      if (complete) {
        if (complete.scope === null && leave?.(event)) event.preventDefault()
        else complete.fire(event)
        return
      }
      typed = events
      timer = setTimeout(reset, SEQUENCE_MS)
      if (show) {
        which = setTimeout(() => {
          clearTimeout(timer)
          const next = new Map(
            candidates.map((b) => {
              const key = b.shown[events.length]
              return [`${key} ${b.binding.help}`, { key, help: b.binding.help }]
            }),
          )
          show({ typed: events.map((e) => e.key).join(' '), next: [...next.values()] })
        }, WHICH_MS)
      }
      return
    }
  }
}
