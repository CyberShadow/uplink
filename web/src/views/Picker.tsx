import type { ComponentChildren } from 'preact'
import { useLayoutEffect, useRef, useState } from 'preact/hooks'
import styles from './Overlay.module.css'

/** Whether `text` has every word of `filter`, ignoring case. */
export function matches(text: string, filter: string): boolean {
  const haystack = text.toLowerCase()
  return filter
    .toLowerCase()
    .split(/\s+/)
    .every((word) => haystack.includes(word))
}

/** An item's checkbox: whether every target has it, some, or none. */
export type Check = 'all' | 'some' | 'none'

/** A dialog of `items`, filtered as typed, one to choose. */
export function Picker<T>(props: {
  name: string
  placeholder: string
  /** The filter's and the list's accessible names. */
  filterName: string
  listName: string
  empty: string
  items: T[]
  id: (item: T) => string
  /** What the filter matches. */
  text: (item: T) => string
  render: (item: T) => ComponentChildren
  /** Shows a checkbox per item. */
  check?: (item: T) => Check
  /** An item the filter makes, offered after those it matches: `null`
   * if it makes none. */
  create?: (filter: string) => T | null
  choose: (item: T) => void
  close: () => void
}) {
  const [filter, setFilter] = useState('')
  const [highlight, setHighlight] = useState(0)
  const input = useRef<HTMLInputElement>(null)
  const list = useRef<HTMLUListElement>(null)
  // At once, not after paint: keys typed right after the shortcut are the
  // filter's.
  useLayoutEffect(() => input.current?.focus(), [])
  useLayoutEffect(() => list.current!.children[highlight]?.scrollIntoView({ block: 'nearest' }), [highlight])

  const created = props.create?.(filter) ?? null
  const items = [...props.items.filter((item) => matches(props.text(item), filter)), ...(created ? [created] : [])]
  const choose = (item: T) => {
    props.choose(item)
    props.close()
  }
  const move = (by: number) => setHighlight((h) => Math.max(0, Math.min(h + by, items.length - 1)))
  /** The options a page of the list shows, less one, which stays in view
   * after a page's move, as in a native list. */
  const page = () => {
    const option = list.current!.children[0] as HTMLElement
    return Math.max(1, Math.floor(list.current!.clientHeight / option.offsetHeight) - 1)
  }
  const { check } = props

  return (
    <div class={styles.backdrop} onClick={props.close}>
      <div
        class={`${styles.dialog} ${styles.picker}`}
        role="dialog"
        aria-label={props.name}
        onClick={(event) => event.stopPropagation()}
      >
        <input
          ref={input}
          class={styles.filter}
          placeholder={props.placeholder}
          aria-label={props.filterName}
          value={filter}
          onInput={(event) => {
            setFilter(event.currentTarget.value)
            setHighlight(0)
          }}
          onKeyDown={(event) => {
            if (event.key === 'Escape') props.close()
            else if (event.key === 'ArrowDown') move(1)
            else if (event.key === 'ArrowUp') move(-1)
            else if (event.key === 'PageDown') move(page())
            else if (event.key === 'PageUp') move(-page())
            else if (event.key === 'Enter' && items[highlight]) choose(items[highlight])
            else return
            event.preventDefault()
          }}
        />
        <ul ref={list} class={styles.options} role="listbox" aria-label={props.listName}>
          {items.map((item, index) => (
            <li
              key={props.id(item)}
              role="option"
              aria-selected={index === highlight}
              aria-checked={check && { all: true, some: 'mixed' as const, none: false }[check(item)]}
              class={index === highlight ? styles.highlighted : undefined}
              onClick={() => choose(item)}
            >
              {check && <span class={styles.check}>{{ all: '☑', some: '⊟', none: '☐' }[check(item)]}</span>}
              {props.render(item)}
            </li>
          ))}
          {items.length === 0 && <li class={styles.empty}>{props.empty}</li>}
        </ul>
      </div>
    </div>
  )
}
