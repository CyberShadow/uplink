import type { Account, Label } from '../generated/protocol'
import type { Thread } from '../stores/mail'
import { useServices } from '../context'
import { labelName, navLabels } from './Nav'
import { type Check, Picker } from './Picker'
import styles from './Overlay.module.css'

/** A filterable list of an account's labels, one to choose. */
function LabelsPicker(props: {
  account: Account
  name: string
  placeholder: string
  labels: Label[]
  check?: (label: Label) => Check
  choose: (label: Label) => void
  close: () => void
}) {
  const name = (label: Label) => labelName(props.account, label)
  return (
    <Picker
      name={props.name}
      placeholder={props.placeholder}
      filterName="Filter labels"
      listName="Labels"
      empty="No labels"
      items={props.labels}
      id={(label) => String(label.id)}
      text={name}
      render={name}
      check={props.check}
      choose={props.choose}
      close={props.close}
    />
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
    <LabelsPicker
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
    <LabelsPicker
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
    <LabelsPicker
      account={props.account}
      name="Go to label"
      placeholder="Go to label…"
      labels={[...system, ...user]}
      choose={props.go}
      close={props.close}
    />
  )
}
