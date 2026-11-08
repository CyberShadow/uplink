import { useLayoutEffect, useRef, useState } from 'preact/hooks'
import type { Entry, Label } from '../generated/protocol'
import { useServices } from '../context'
import styles from './Overlay.module.css'

/** Toggles a user label on `targets`, which must share an account. */
export function LabelPicker(props: { targets: Entry[]; close: () => void }) {
  const { mail } = useServices()
  const [filter, setFilter] = useState('')
  const [highlight, setHighlight] = useState(0)
  const input = useRef<HTMLInputElement>(null)
  // At once, not after paint: keys typed right after `l` are the filter's.
  useLayoutEffect(() => input.current?.focus(), [])

  const accounts = new Set(props.targets.map((e) => e.account))
  if (accounts.size > 1) {
    return (
      <div class={styles.backdrop} onClick={props.close}>
        <div class={styles.dialog} role="dialog" aria-label="Label">
          <p>Labels belong to one account; select messages of one account to label them.</p>
        </div>
      </div>
    )
  }
  const [account] = accounts
  const labels = (mail.accounts.value?.find((a) => a.id === account)?.labels ?? [])
    .filter((l) => !l.system && l.name.toLowerCase().includes(filter.toLowerCase()))
    .sort((a, b) => a.name.localeCompare(b.name))

  const toggle = (label: Label) => {
    mail.toggleLabel(props.targets, label)
    props.close()
  }
  const state = (label: Label) => {
    const having = props.targets.filter((e) => e.labels.includes(label.id)).length
    return having === 0 ? 'none' : having === props.targets.length ? 'all' : 'some'
  }

  return (
    <div class={styles.backdrop} onClick={props.close}>
      <div
        class={styles.dialog}
        role="dialog"
        aria-label="Label"
        onClick={(event) => event.stopPropagation()}
      >
        <input
          ref={input}
          class={styles.filter}
          placeholder="Label as…"
          aria-label="Filter labels"
          value={filter}
          onInput={(event) => {
            setFilter(event.currentTarget.value)
            setHighlight(0)
          }}
          onKeyDown={(event) => {
            if (event.key === 'Escape') props.close()
            else if (event.key === 'ArrowDown') setHighlight((h) => Math.min(h + 1, labels.length - 1))
            else if (event.key === 'ArrowUp') setHighlight((h) => Math.max(h - 1, 0))
            else if (event.key === 'Enter' && labels[highlight]) toggle(labels[highlight])
            else return
            event.preventDefault()
          }}
        />
        <ul class={styles.options} role="listbox" aria-label="Labels">
          {labels.map((label, index) => (
            <li
              key={label.id}
              role="option"
              aria-selected={index === highlight}
              aria-checked={{ all: true, some: 'mixed' as const, none: false }[state(label)]}
              class={index === highlight ? styles.highlighted : undefined}
              onClick={() => toggle(label)}
            >
              <span class={styles.check}>{{ all: '☑', some: '⊟', none: '☐' }[state(label)]}</span>
              {label.name}
            </li>
          ))}
          {labels.length === 0 && <li class={styles.empty}>No labels</li>}
        </ul>
      </div>
    </div>
  )
}
