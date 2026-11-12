// A Matrix account's device and its encryption (SPEC §9.4, §12.12):
// whether uplink's device is verified, its key backup and recovery, the
// account's devices; recovering with the recovery key, and importing and
// exporting room keys, as Element's Security & Privacy settings do.

import { useState } from 'preact/hooks'
import type {
  KeyBackup,
  MatrixSecurity,
  MatrixVerification,
  Recovery,
  VerificationAnswer,
} from '../generated/protocol'
import { useServices } from '../context'
import { useTitle } from '../title'
import { dateTime } from '../format'
import type { Security } from '../stores/matrix'
import styles from './Security.module.css'

export function securityPath(account: string): string {
  return `/matrix/${encodeURIComponent(account)}/security`
}

export function SecurityView(props: { account: string }) {
  const { matrix } = useServices()
  const account = decodeURIComponent(props.account)
  const known = matrix.accounts.value?.find((a) => a.id === account)
  useTitle(...(known ? ['Security', known.mxid] : []))
  if (matrix.accounts.value === null) return <p class={styles.placeholder}>Loading…</p>
  if (!known) return <p class={styles.placeholder}>No Matrix account {account}.</p>
  const security = matrix.security(account)
  const state = security.state.value
  return (
    <div class={styles.page}>
      <h1>Security of {known.mxid}</h1>
      {security.error.value && (
        <p class={styles.error} role="alert">
          {security.error.value}
        </p>
      )}
      {state === null ? (
        <p class={styles.placeholder}>Loading…</p>
      ) : (
        <>
          <ThisDevice security={security} state={state} />
          {state.verifications.length > 0 && <Verifications security={security} state={state} />}
          {(!state.verified || state.recovery !== 'enabled') && <Recover security={security} />}
          <RoomKeys security={security} />
          <Devices security={security} state={state} />
        </>
      )}
    </div>
  )
}

const BACKUP: Record<KeyBackup, string> = {
  unknown: 'Not in use on this device',
  creating: 'Being created',
  enabling: 'Being enabled',
  resuming: 'Resuming',
  enabled: 'In use',
  downloading: 'Downloading keys',
  disabling: 'Being disabled',
}

const RECOVERY: Record<Recovery, string> = {
  unknown: 'Unknown',
  enabled: 'Set up, and this device has its secrets',
  disabled: 'Not set up',
  incomplete: 'This device lacks secrets: recover with the recovery key',
}

function ThisDevice(props: { security: Security; state: MatrixSecurity }) {
  const { state } = props
  const verifying = state.verifications.some((v) => live(v))
  return (
    <section aria-label="This device">
      <h2>This device</h2>
      <dl class={styles.facts}>
        <dt>Device</dt>
        <dd>
          <code>{state.device_id}</code>
        </dd>
        <dt>Verified</dt>
        <dd>
          {state.verified ? (
            <span class={styles.verified}>Verified by the account's cross-signing</span>
          ) : (
            <span class={styles.unverified}>
              Not verified: others may not trust it, and it cannot read what was sent before it was set up
            </span>
          )}
        </dd>
        {!state.verified && (
          <dd class={styles.wide}>
            <button type="button" disabled={verifying} onClick={() => props.security.verify(null)}>
              Verify with another device
            </button>
          </dd>
        )}
        <dt>Cross-signing keys</dt>
        <dd>{state.cross_signing ? 'On this device' : 'Not on this device'}</dd>
        <dt>Key backup</dt>
        <dd>{BACKUP[state.backup]}</dd>
        <dt>Recovery</dt>
        <dd>{RECOVERY[state.recovery]}</dd>
      </dl>
    </section>
  )
}

/** Unlocks Secret Storage: the device then signs itself, and has the key
 * backup's key. */
function Recover(props: { security: Security }) {
  const [recovery, setRecovery] = useState('')
  const [busy, setBusy] = useState(false)
  return (
    <section aria-label="Recover">
      <h2>Recover</h2>
      <p class={styles.help}>
        With the account's recovery key (or Secret Storage passphrase), this device gets the account's cross-signing
        keys, so verifies itself, and the key backup's key, so reads what was sent before it.
      </p>
      <form
        class={styles.form}
        onSubmit={async (event) => {
          event.preventDefault()
          setBusy(true)
          if (await props.security.recover(recovery)) setRecovery('')
          setBusy(false)
        }}
      >
        <input
          type="password"
          autocomplete="off"
          aria-label="Recovery key or passphrase"
          placeholder="Recovery key or passphrase"
          value={recovery}
          onInput={(event) => setRecovery(event.currentTarget.value)}
        />
        <button type="submit" disabled={busy || recovery.trim() === ''}>
          {busy ? 'Recovering…' : 'Recover'}
        </button>
      </form>
    </section>
  )
}

function RoomKeys(props: { security: Security }) {
  return (
    <section aria-label="Room keys">
      <h2>Room keys</h2>
      <p class={styles.help}>
        Room keys decrypt messages. Export them to keep, or to give another client; import those another client
        exported, to read what this device has no keys for.
      </p>
      <ImportKeys security={props.security} />
      <ExportKeys security={props.security} />
    </section>
  )
}

function ImportKeys(props: { security: Security }) {
  const [file, setFile] = useState<File | null>(null)
  const [passphrase, setPassphrase] = useState('')
  const [busy, setBusy] = useState(false)
  const [done, setDone] = useState<string | null>(null)
  return (
    <form
      class={styles.form}
      aria-label="Import room keys"
      onSubmit={async (event) => {
        event.preventDefault()
        setBusy(true)
        setDone(null)
        const imported = await props.security.importKeys(file!, passphrase)
        setBusy(false)
        if (!imported) return
        setDone(`Imported ${imported.imported} new keys of ${imported.total}.`)
        setPassphrase('')
      }}
    >
      <input
        type="file"
        aria-label="Exported keys"
        onChange={(event) => setFile(event.currentTarget.files?.[0] ?? null)}
      />
      <input
        type="password"
        autocomplete="off"
        aria-label="Import passphrase"
        placeholder="Passphrase"
        value={passphrase}
        onInput={(event) => setPassphrase(event.currentTarget.value)}
      />
      <button type="submit" disabled={busy || file === null || passphrase === ''}>
        {busy ? 'Importing…' : 'Import'}
      </button>
      {done && <span role="status">{done}</span>}
    </form>
  )
}

function ExportKeys(props: { security: Security }) {
  const [passphrase, setPassphrase] = useState('')
  const [again, setAgain] = useState('')
  const [busy, setBusy] = useState(false)
  const matching = passphrase !== '' && passphrase === again
  return (
    <form
      class={styles.form}
      aria-label="Export room keys"
      onSubmit={async (event) => {
        event.preventDefault()
        setBusy(true)
        const exported = await props.security.exportKeys(passphrase)
        setBusy(false)
        if (!exported) return
        download(exported, `element-keys-${props.security.account}.txt`)
        setPassphrase('')
        setAgain('')
      }}
    >
      <input
        type="password"
        autocomplete="new-password"
        aria-label="Export passphrase"
        placeholder="New passphrase"
        value={passphrase}
        onInput={(event) => setPassphrase(event.currentTarget.value)}
      />
      <input
        type="password"
        autocomplete="new-password"
        aria-label="Export passphrase again"
        placeholder="Again"
        value={again}
        onInput={(event) => setAgain(event.currentTarget.value)}
      />
      <button type="submit" disabled={busy || !matching}>
        {busy ? 'Exporting…' : 'Export'}
      </button>
    </form>
  )
}

/** Saves `blob` as a file the browser downloads. */
function download(blob: Blob, name: string) {
  const url = URL.createObjectURL(blob)
  const link = document.createElement('a')
  link.href = url
  link.download = name
  link.click()
  URL.revokeObjectURL(url)
}

function Devices(props: { security: Security; state: MatrixSecurity }) {
  const { state } = props
  const verifying = state.verifications.some((v) => live(v))
  return (
    <section aria-label="Devices">
      <h2>Devices</h2>
      <table class={styles.devices}>
        <thead>
          <tr>
            <th>Device</th>
            <th>Last seen</th>
            <th>Trust</th>
          </tr>
        </thead>
        <tbody>
          {state.devices.map((device) => (
            <tr key={device.device_id}>
              <td>
                {device.name ?? device.device_id}
                {device.current && <span class={styles.tag}>This device</span>}
                <div class={styles.muted}>
                  <code>{device.device_id}</code>
                </div>
              </td>
              <td class={styles.muted}>
                {device.last_seen_ts === null ? '—' : dateTime(device.last_seen_ts)}
                {device.last_seen_ip && <div>{device.last_seen_ip}</div>}
              </td>
              <td>
                {device.verified ? (
                  <span class={styles.verified}>Verified</span>
                ) : (
                  <span class={styles.unverified}>Not verified</span>
                )}
                {!device.current && !state.verified && (
                  <button
                    type="button"
                    class={styles.inline}
                    aria-label={`Verify with ${device.name ?? device.device_id}`}
                    disabled={verifying}
                    onClick={() => props.security.verify(device.device_id)}
                  >
                    Verify with it
                  </button>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  )
}

/** Whether the verification goes on. */
export function live(verification: MatrixVerification): boolean {
  return verification.phase.type !== 'done' && verification.phase.type !== 'cancelled'
}

/** Whether the verification waits for the user. */
export function awaitsUser(verification: MatrixVerification): boolean {
  return (verification.phase.type === 'requested' && !verification.we_started) || verification.phase.type === 'compare'
}

/** Verifications going on, or ended since the last began: what each
 * asks of the user, as Element shows it. */
function Verifications(props: { security: Security; state: MatrixSecurity }) {
  const { security, state } = props
  const deviceName = (id: string | null) => {
    const device = state.devices.find((d) => d.device_id === id)
    return device ? (device.name ?? device.device_id) : (id ?? 'another device')
  }
  return (
    <section aria-label="Verification">
      <h2>Verification</h2>
      {state.verifications.map((v) => {
        const answer = (answer: VerificationAnswer) => () => security.answer(v.flow_id, answer)
        const cancel = (
          <button type="button" onClick={answer('cancel')}>
            Cancel
          </button>
        )
        const other = deviceName(v.other_device)
        return (
          <div key={v.flow_id} class={styles.verification}>
            {v.phase.type === 'requested' && v.we_started && (
              <p>
                Waiting for {v.other_device ? other : 'another device of the account'} to accept… {cancel}
              </p>
            )}
            {v.phase.type === 'requested' && !v.we_started && (
              <p>
                {other} asks to verify this device.{' '}
                <button type="button" onClick={answer('accept')}>
                  Accept
                </button>{' '}
                <button type="button" onClick={answer('cancel')}>
                  Decline
                </button>
              </p>
            )}
            {(v.phase.type === 'ready' || v.phase.type === 'started') && <p>Starting to compare with {other}… {cancel}</p>}
            {v.phase.type === 'compare' && (
              <>
                <p>Check that {other} shows these, in this order:</p>
                <ol class={styles.emoji} aria-label="Emoji to compare">
                  {v.phase.emoji.map((e, i) => (
                    <li key={i}>
                      <span class={styles.symbol}>{e.symbol}</span>
                      {e.description}
                    </li>
                  ))}
                </ol>
                <p class={styles.muted}>Or these numbers: {v.phase.decimals.join(' ')}</p>
                <p>
                  <button type="button" onClick={answer('confirm')}>
                    They match
                  </button>{' '}
                  <button type="button" onClick={answer('mismatch')}>
                    They don't match
                  </button>
                </p>
              </>
            )}
            {v.phase.type === 'confirmed' && <p>Waiting for {other} to confirm…</p>}
            {v.phase.type === 'done' && <p class={styles.verified}>Verified with {other}.</p>}
            {v.phase.type === 'cancelled' && (
              <p class={styles.unverified}>
                {v.phase.by_us ? 'Cancelled' : `Cancelled by ${other}`}: {v.phase.reason}
              </p>
            )}
          </div>
        )
      })}
    </section>
  )
}
