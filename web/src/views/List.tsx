// A view's list (SPEC §12.5): it has the focus as a whole, as Gmail's,
// which is where the focus rests in its view, and its keys (the view's,
// on the window) move a cursor among its rows. One Tab stop, with the
// controls of the cursor's row, which Tab goes through next.

import { type ComponentChildren, type RefObject } from 'preact'
import { useRegion, useStops } from '../regions'
import { useRest } from '../rest'
import styles from './List.module.css'

/** What a row is, in a `List`. */
const ROW = '[role=option]'

/** The id of the row at `index`, of those of the lists on show. */
export function rowId(index: number): string {
  return `row-${index}`
}

export function List(props: {
  /** A list of its own, or what holds several (home's buckets), whose
   * rows are one list to the keys. */
  tag: 'ul' | 'div'
  list: RefObject<HTMLElement>
  class: string
  label: string
  /** The id of the cursor's row, if any: the one with `aria-current`. */
  cursor: string | undefined
  /** Whether rows are selected apart from the cursor. */
  multi: boolean
  children: ComponentChildren
}) {
  const { list } = props
  useRest(list)
  useRegion('list', list, () => list.current!.focus({ preventScroll: true }))
  useStops(list, ROW, () => list.current!.querySelector('[aria-current]'), 'always')
  const Tag = props.tag
  return (
    <Tag
      ref={list as RefObject<HTMLUListElement & HTMLDivElement>}
      class={`${styles.list} ${props.class}`}
      role="listbox"
      tabIndex={0}
      aria-label={props.label}
      aria-multiselectable={props.multi}
      aria-activedescendant={props.cursor}
    >
      {props.children}
    </Tag>
  )
}
