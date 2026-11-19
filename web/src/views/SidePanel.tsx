// A panel beside a room's timeline (SPEC §12.11): its header, which
// closes it, and what it shows; and, as a region of the window, the keys
// that move within it and out of it, the same for each.

import { type ComponentChildren } from 'preact'
import { useRef } from 'preact/hooks'
import { keepFocus, useKeymap } from '../keymap'
import { enterRegion, stopKeys, useRegion } from '../regions'
// Its header as the room info's.
import styles from './RoomInfo.module.css'

/** A panel's controls, for its keys to move among. */
export const CONTROLS = 'a[href], button:not(:disabled), input, select, textarea, summary'

export function SidePanel(props: {
  class: string
  /** What it is, as read out: its close button is named by it. */
  label: string
  title: string
  /** What its keys move among: a selector, of what is not in its header. */
  stops: string
  /** Goes back to the panel it was opened from, if any. */
  back: { label: string; go: () => void } | null
  /** Buttons of its header, before its close button. */
  actions?: ComponentChildren
  close: () => void
  children: ComponentChildren
}) {
  const aside = useRef<HTMLElement>(null)
  const stops = () =>
    [...aside.current!.querySelectorAll<HTMLElement>(props.stops)].filter((stop) => !stop.closest('header'))
  // Back to where the focus was in it; else, to its first.
  useRegion('panel', aside, (last) => (last ?? stops().at(0))?.focus())
  useKeymap(
    {
      ...stopKeys(stops),
      left: () => enterRegion('timeline'),
      // Esc, from within, closes it for the timeline.
      up: () => {
        props.close()
        enterRegion('timeline')
      },
    },
    { focus: aside },
  )
  return (
    <aside ref={aside} class={props.class} aria-label={props.label}>
      <header class={styles.header}>
        {props.back && (
          <button type="button" onMouseDown={keepFocus} onClick={props.back.go} aria-label={props.back.label}>
            ‹
          </button>
        )}
        <h2>{props.title}</h2>
        {props.actions}
        <button
          type="button"
          onMouseDown={keepFocus}
          onClick={() => {
            // Closed with the focus in it (its box, say), the room's box
            // has it, as before it opened.
            const focused = aside.current!.contains(document.activeElement)
            props.close()
            if (focused) enterRegion('box')
          }}
          aria-label={`Close ${props.label.toLowerCase()}`}
        >
          ×
        </button>
      </header>
      {props.children}
    </aside>
  )
}
