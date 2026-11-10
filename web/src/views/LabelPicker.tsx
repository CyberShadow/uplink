import { useLayoutEffect, useRef, useState } from 'preact/hooks'
import type { Account, Label } from '../generated/protocol'
import type { Thread } from '../stores/mail'
import { useServices } from '../context'
import { labelName, navLabels } from './Nav'
import styles from './Overlay.module.css'

type Check = 'all' | 'some' | 'none'

/** A filterable list of an account's labels, one to choose. */
function Picker(props: {
  account: Account
  name: string
  placeholder: string
  labels: Label[]
  /** Shows a checkbox per label. */
  check?: (label: Label) => Check
  choose: (label: Label) => void
  close: () => void
}) {
  const [filter, setFilter] = useState('')
  const [highlight, setHighlight] = useState(0)
  const input = useRef<HTMLInputElement>(null)
  // At once, not after paint: keys typed right after the shortcut are the
  // filter's.
  useLayoutEffect(() => input.current?.focus(), [])

  const labels = props.labels.filter((l) => labelName(props.account, l).toLowerCase().includes(filter.toLowerCase()))
  const choose = (label: Label) => {
    props.choose(label)
    props.close()
  }
  const { check } = props

  return (
    <div class={styles.backdrop} onClick={props.close}>
      <div class={styles.dialog} role="dialog" aria-label={props.name} onClick={(event) => event.stopPropagation()}>
        <input
          ref={input}
          class={styles.filter}
          placeholder={props.placeholder}
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
            else if (event.key === 'Enter' && labels[highlight]) choose(labels[highlight])
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
              aria-checked={check && { all: true, some: 'mixed' as const, none: false }[check(label)]}
              class={index === highlight ? styles.highlighted : undefined}
              onClick={() => choose(label)}
            >
              {check && <span class={styles.check}>{{ all: '☑', some: '⊟', none: '☐' }[check(label)]}</span>}
              {labelName(props.account, label)}
            </li>
          ))}
          {labels.length === 0 && <li class={styles.empty}>No labels</li>}
        </ul>
      </div>
    </div>
  )
}

/** The account the targets share; labels belong to one. */
function useTargetAccount(targets: Thread[]): Account | null {
  const { mail } = useServices()
  const accounts = new Set(targets.map((t) => t.account))
  if (accounts.size > 1) return null
  const [account] = accounts
  return mail.accounts.value!.find((a) => a.id === account)!
}

function MixedAccounts(props: { name: string; close: () => void }) {
  return (
    <div class={styles.backdrop} onClick={props.close}>
      <div class={styles.dialog} role="dialog" aria-label={props.name}>
        <p>Labels belong to one account; select messages of one account to label them.</p>
      </div>
    </div>
  )
}

/** Toggles a user label on `targets` (`l`); folders are moved to instead. */
export function LabelPicker(props: { targets: Thread[]; close: () => void }) {
  const { mail } = useServices()
  const account = useTargetAccount(props.targets)
  if (!account) return <MixedAccounts name="Label" close={props.close} />
  const check = (label: Label): Check => {
    const having = props.targets.filter((t) => t.entries.some((e) => e.labels.includes(label.id))).length
    return having === 0 ? 'none' : having === props.targets.length ? 'all' : 'some'
  }
  return (
    <Picker
      account={account}
      name="Label"
      placeholder="Label as…"
      labels={navLabels(account).user.filter((l) => !l.folder)}
      check={check}
      choose={(label) => mail.toggleLabel(props.targets, label)}
      close={props.close}
    />
  )
}

/** Moves `targets` out of the label on show, or else the Inbox, into a
 * user label or the Inbox (`v`); or, if the account has folders, into
 * another folder. */
export function MovePicker(props: { targets: Thread[]; from: string | null; close: () => void }) {
  const { mail } = useServices()
  const account = useTargetAccount(props.targets)
  if (!account) return <MixedAccounts name="Move to" close={props.close} />
  const inbox = mail.systemLabel(account.id, 'INBOX')
  const { system, user } = navLabels(account)
  const folders = account.labels.some((l) => l.folder)
  const targets = folders ? [...system, ...user].filter((l) => l.folder) : [inbox, ...user]
  const from = targets.find((l) => l.external_id === props.from) ?? inbox
  return (
    <Picker
      account={account}
      name="Move to"
      placeholder="Move to…"
      labels={targets.filter((l) => l !== from)}
      choose={(label) => mail.move(props.targets, from, label)}
      close={props.close}
    />
  )
}

/** Goes to one of the account's labels (`g l`). */
export function GoToLabel(props: { account: Account; go: (label: Label) => void; close: () => void }) {
  const { system, user } = navLabels(props.account)
  return (
    <Picker
      account={props.account}
      name="Go to label"
      placeholder="Go to label…"
      labels={[...system, ...user]}
      choose={props.go}
      close={props.close}
    />
  )
}
