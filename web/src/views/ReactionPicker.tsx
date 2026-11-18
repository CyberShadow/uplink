// The picker of a line's reaction, as chat clients have it: the quick
// reactions, a box to search with (or to type any reaction in), and
// every emoji by its group, after the images of the packs the room
// offers, with a bar to go to each group.

import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'preact/hooks'
import type { ImagePack, Room, RoomId } from '../generated/protocol'
import { useServices } from '../context'
import { type Emoji, GROUPS, searchEmoji } from '../emoji'
import { keepFocus } from '../keymap'
import { packImagePath } from '../matrix'
import { useEmoji } from './Autocomplete'
import { useOpensAbove } from './popover'
import styles from './ReactionPicker.module.css'

/** Reactions offered at once, as Element's quick ones. */
const QUICK = ['👍', '👎', '😄', '🎉', '😕', '❤️', '🚀', '👀']

/** What a reaction can be chosen as: an emoji, or a pack's image (its
 * key the image's URL, by its `:shortcode:`). */
interface Choice {
  key: string
  shortcode: string | null
  /** As its button is named: "React with" it. */
  name: string
  /** Its `:shortcodes:`, the first shown on hover. */
  codes: readonly string[]
  /** Its image's path, a pack's image's. */
  image: string | null
}

interface Section {
  id: string
  name: string
  /** As the bar shows it: one of its choices. */
  icon: Choice
  choices: Choice[]
}

function emojiChoice(emoji: Emoji): Choice {
  return { key: emoji.unicode, shortcode: null, name: emoji.unicode, codes: emoji.shortcodes.map((c) => `:${c}:`), image: null }
}

function packSections(room: RoomId, packs: readonly ImagePack[], words: string): Section[] {
  return packs.flatMap((pack, at) => {
    const choices = pack.images
      .filter((i) => i.shortcode.toLowerCase().includes(words) || (i.body ?? '').toLowerCase().includes(words))
      .map((i) => {
        const code = `:${i.shortcode}:`
        return { key: i.url, shortcode: code, name: code, codes: [code], image: packImagePath(room, i.url) }
      })
    return choices.length > 0 ? [{ id: `pack-${at}`, name: pack.name ?? 'Your emoji', icon: choices[0], choices }] : []
  })
}

/** Browsed, each group; searched, those matching, best first. */
function emojiSections(emoji: readonly Emoji[], words: string): Section[] {
  if (words !== '') {
    const choices = searchEmoji(emoji, words, Infinity).map(emojiChoice)
    return choices.length > 0 ? [{ id: 'found', name: 'Emoji', icon: choices[0], choices }] : []
  }
  return GROUPS.flatMap((group, at) =>
    group === null
      ? []
      : [
          {
            id: `group-${at}`,
            name: group.name,
            icon: emojiChoice(emoji.find((e) => e.unicode === group.icon)!),
            choices: emoji.filter((e) => e.group === at).map(emojiChoice),
          },
        ],
  )
}

interface Box {
  top: number
  bottom: number
  left: number
  right: number
}

/** Of the boxes, the one Up (`direction` -1) or Down (1) goes to from
 * `from`: on the nearest line that way, the one most across from it
 * (the first of those as much: from the box, the first of a row), or
 * least far off; if any. */
export function vertically(from: Box, boxes: readonly Box[], direction: 1 | -1): number | undefined {
  // Past `from` that way, by how far; how much across from it, or, less
  // than none, how far off.
  const beyond = (b: Box) => (direction === 1 ? b.top - from.bottom : from.top - b.bottom)
  const across = (b: Box) => Math.min(b.right, from.right) - Math.max(b.left, from.left)
  const ahead = boxes.map((b, at) => ({ b, at })).filter(({ b }) => beyond(b) >= -1)
  if (ahead.length === 0) return undefined
  const nearest = ahead.reduce((n, a) => (beyond(a.b) < beyond(n.b) ? a : n)).b
  return ahead
    .filter(({ b }) => b.top < nearest.bottom && b.bottom > nearest.top)
    .reduce((n, a) => (across(a.b) > across(n.b) ? a : n)).at
}

/** Shown in `room`, by its button (its parent, which it is placed by),
 * giving the focus to its first reaction if `focus`; `choose` takes a
 * reaction, and `leave` is the focus leaving it. */
export function ReactionPicker(props: {
  room: Room
  focus: boolean
  choose: (key: string, shortcode: string | null) => void
  leave: () => void
}) {
  const { matrix } = useServices()
  const [typed, setTyped] = useState('')
  // Of the room as it is when it opens.
  const [packs, setPacks] = useState<ImagePack[] | null>(null)
  useEffect(() => {
    let current = true
    void matrix.imagePacks(props.room.id).then((packs) => current && setPacks(packs))
    return () => {
      current = false
    }
  }, [matrix, props.room.id])
  const emoji = useEmoji(true)
  const picker = useRef<HTMLDivElement>(null)
  const input = useRef<HTMLInputElement>(null)
  const scroller = useRef<HTMLDivElement>(null)
  // As large from the first, whatever it lists.
  const above = useOpensAbove(picker, true)
  useLayoutEffect(() => {
    if (props.focus) picker.current!.querySelector('button')!.focus()
  }, [props.focus])
  const words = typed.trim().replace(/^:|:$/g, '').toLowerCase()
  const sections = useMemo(
    () => [...packSections(props.room.id, packs ?? [], words), ...(emoji ? emojiSections(emoji, words) : [])],
    [props.room.id, packs, emoji, words],
  )
  // Back at the top, as what it lists changes.
  useLayoutEffect(() => {
    scroller.current!.scrollTop = 0
  }, [words])
  const choose = (choice: Choice) => props.choose(choice.key, choice.shortcode)
  const text = typed.trim()

  return (
    <div
      ref={picker}
      class={above ? `${styles.picker} ${styles.above}` : styles.picker}
      role="dialog"
      aria-label="React"
      // Clicked between its controls, it has the focus, which so stays
      // in it.
      tabIndex={-1}
      // Left by the focus (Tab, a click elsewhere), it closes.
      onFocusOut={(event) => {
        if (!picker.current!.contains(event.relatedTarget as Node | null)) props.leave()
      }}
      onKeyDown={(event) => {
        const pressed = event.target as HTMLElement
        const arrow = { ArrowLeft: -1, ArrowRight: 1 }[event.key]
        const vertical = ({ ArrowUp: -1, ArrowDown: 1 } as const)[event.key]
        const controls = () => [...picker.current!.querySelectorAll<HTMLElement>('button, input')]
        // The arrows move between its controls (not between the lines):
        // in the box, Left and Right move its caret, but from an empty one.
        if (arrow !== undefined && (pressed !== input.current || typed === '')) {
          const all = controls()
          all[Math.max(0, Math.min(all.indexOf(pressed) + arrow, all.length - 1))].focus()
          event.preventDefault()
          event.stopPropagation()
        } else if (vertical !== undefined && pressed !== picker.current) {
          const all = controls()
          const to = vertically(
            pressed.getBoundingClientRect(),
            all.map((c) => c.getBoundingClientRect()),
            vertical,
          )
          if (to !== undefined) all[to].focus()
          event.preventDefault()
          event.stopPropagation()
        } else if (pressed !== input.current && [...event.key].length === 1 && !event.ctrlKey && !event.metaKey && !event.altKey) {
          // A key typed on a reaction is the box's, not a shortcut.
          event.stopPropagation()
          input.current!.focus()
        }
      }}
    >
      <div class={styles.quick} role="group" aria-label="Quick reactions">
        {QUICK.map((key) => (
          <button key={key} type="button" aria-label={`React with ${key}`} onMouseDown={keepFocus} onClick={() => props.choose(key, null)}>
            {key}
          </button>
        ))}
      </div>
      <form
        onSubmit={(event) => {
          event.preventDefault()
          if (text === '') return
          // A shortcode typed whole is its image, or its emoji; else, the
          // first found; else, what is typed.
          const found = sections.flatMap((s) => s.choices)
          const chosen = found.find((c) => c.codes.includes(text)) ?? found.at(0)
          if (chosen) choose(chosen)
          else props.choose(text, null)
        }}
      >
        <input
          ref={input}
          aria-label="Reaction"
          placeholder="Search, or type a reaction…"
          value={typed}
          onInput={(event) => setTyped(event.currentTarget.value)}
        />
      </form>
      {words === '' && (
        <nav class={styles.bar} aria-label="Groups">
          {sections.map((section, at) => (
            <button
              key={section.id}
              type="button"
              aria-label={section.name}
              title={section.name}
              onMouseDown={keepFocus}
              onClick={() => {
                // Its section, listed as the bar lists them.
                scroller.current!.scrollTop = scroller.current!.querySelectorAll<HTMLElement>('section')[at].offsetTop
              }}
            >
              <ChoiceIcon choice={section.icon} />
            </button>
          ))}
        </nav>
      )}
      <div class={styles.choices} ref={scroller}>
        {sections.map((section) => (
          <section key={section.id} aria-label={section.name}>
            <h4>{section.name}</h4>
            {section.choices.map((choice) => (
              <button
                key={choice.name}
                type="button"
                aria-label={`React with ${choice.name}`}
                title={choice.codes[0]}
                onMouseDown={keepFocus}
                onClick={() => choose(choice)}
              >
                <ChoiceIcon choice={choice} />
              </button>
            ))}
          </section>
        ))}
        {text !== '' && (
          <section aria-label="As text">
            <h4>As text</h4>
            <button type="button" class={styles.text} aria-label={`React with ${text}`} onMouseDown={keepFocus} onClick={() => props.choose(text, null)}>
              {text}
            </button>
          </section>
        )}
      </div>
    </div>
  )
}

function ChoiceIcon(props: { choice: Choice }) {
  const { choice } = props
  return choice.image === null ? <>{choice.key}</> : <img src={choice.image} alt="" loading="lazy" />
}
