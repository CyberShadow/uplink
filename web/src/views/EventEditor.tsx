// The form that creates or edits an event of the user's own (SPEC §10.4):
// as Outlook's, its subject, attendees, times, how it recurs, and a TipTap
// editor for its description, as Compose has. Loaded lazily, as Compose
// is: TipTap is most of the bundle.

import { Editor } from '@tiptap/core'
import StarterKit from '@tiptap/starter-kit'
import { useEffect, useLayoutEffect, useRef, useState } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import type {
  EventDetails,
  EventField,
  EventFields,
  Recurrence,
  RecurrenceEnd,
  RecurrencePattern,
  ShowAs,
  WeekIndex,
  Weekday,
} from '../generated/protocol'
import { useServices } from '../context'
import { date as dateString } from '../format'
import { keepFocus, useKeymap } from '../keymap'
import { useTitle } from '../title'
import { calendarPath, today, viewOf } from './Calendar'
import styles from './EventEditor.module.css'

const DAY = 86_400_000
const MINUTE = 60_000

/** A new event (no `event`) on `date`, or the event's edit: of its
 * series, if `scope` says so. */
export default function EventEditor(props: { view?: string; date?: string; event?: string; scope?: string }) {
  const { calendar } = useServices()
  const id = props.event === undefined ? null : Number(props.event)
  const series = props.scope === 'series'
  if (props.scope !== undefined && !series) throw new Error(`no event edit of ${props.scope}`)
  // As they are when the form opens: what it edits from, as later changes
  // do not change what is typed.
  const [details, setDetails] = useState<EventDetails | 'gone' | null>(null)
  useEffect(() => {
    if (id === null) return
    let current = true
    void calendar.details(id).then((loaded) => {
      if (current) setDetails(loaded)
    })
    return () => {
      current = false
    }
  }, [calendar, id])
  const back = { view: viewOf(props.view), date: props.date ?? today(), event: id }
  if (id === null) return <EventForm back={back} initial={null} series={false} />
  if (details === null) return <p class={styles.placeholder}>Loading…</p>
  if (details === 'gone') return <p class={styles.placeholder}>This event is no longer on the calendar.</p>
  const initial = series ? details.series : details.editable
  if (initial === null) return <p class={styles.placeholder}>This event is not yours to change.</p>
  return <EventForm back={back} initial={{ event: id, account: details.event.account, fields: initial }} series={series} />
}

interface Back {
  view: ReturnType<typeof viewOf>
  date: string
  event: number | null
}

interface Initial {
  event: number
  account: string
  fields: EventFields
}

/** The next half hour from now, on `date` if today, else its 09:00. */
function defaultStart(date: string): number {
  if (date === today()) return Math.ceil(Date.now() / (30 * MINUTE)) * 30 * MINUTE
  const [y, m, d] = date.split('-').map(Number)
  return new Date(y, m - 1, d, 9).getTime()
}

function newFields(date: string): EventFields {
  const start = defaultStart(date)
  return {
    subject: '',
    location: '',
    body: '',
    start,
    end: start + 30 * MINUTE,
    all_day: false,
    required: '',
    optional: '',
    online_meeting: false,
    reminder: 15,
    show_as: 'busy',
    recurrence: null,
  }
}

/** `YYYY-MM-DDTHH:MM`, local, as a `datetime-local` input has it. */
function localInput(ms: number): string {
  const d = new Date(ms)
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${dateString(d)}T${pad(d.getHours())}:${pad(d.getMinutes())}`
}

/** An all-day event's UTC date of `ms`. */
const utcDate = (ms: number) => new Date(ms).toISOString().slice(0, 10)

/** The first day of the event, as its recurrence is of: of an all-day
 * event, its UTC date; else the local one. */
function firstDay(fields: EventFields): { month: number; day: number; weekday: number; lastWeek: boolean } {
  const d = new Date(fields.start)
  const [year, month, day, weekday] = fields.all_day
    ? [d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate(), d.getUTCDay()]
    : [d.getFullYear(), d.getMonth(), d.getDate(), d.getDay()]
  const daysInMonth = new Date(Date.UTC(year, month + 1, 0)).getUTCDate()
  return { month: month + 1, day, weekday, lastWeek: day + 7 > daysInMonth }
}

const WEEKDAYS: Weekday[] = ['sunday', 'monday', 'tuesday', 'wednesday', 'thursday', 'friday', 'saturday']
const INDICES: WeekIndex[] = ['first', 'second', 'third', 'fourth', 'last']
const MONTHS = ['January', 'February', 'March', 'April', 'May', 'June', 'July', 'August', 'September', 'October', 'November', 'December']

type Repeat = 'none' | RecurrencePattern['type']

/** Which of its month's such weekdays the day is: a fifth is the last, as
 * Outlook has it. */
function indexOf(day: number): WeekIndex {
  return INDICES[Math.min(Math.floor((day - 1) / 7), 4)]
}

/** The pattern of `repeat` from the event's first day; a week's, on its
 * days already chosen, if any. */
function pattern(repeat: Exclude<Repeat, 'none'>, fields: EventFields, days: Weekday[] | null): RecurrencePattern {
  const first = firstDay(fields)
  const weekday = [WEEKDAYS[first.weekday]]
  const index = indexOf(first.day)
  switch (repeat) {
    case 'daily':
      return { type: 'daily' }
    case 'weekly':
      return { type: 'weekly', days: days ?? weekday }
    case 'absolute_monthly':
      return { type: 'absolute_monthly', day: first.day }
    case 'relative_monthly':
      return { type: 'relative_monthly', index, days: weekday }
    case 'absolute_yearly':
      return { type: 'absolute_yearly', month: first.month, day: first.day }
    case 'relative_yearly':
      return { type: 'relative_yearly', month: first.month, index, days: weekday }
  }
}

/** The choices of how to repeat, in words, of the event's first day. */
function repeats(fields: EventFields): [Repeat, string][] {
  const first = firstDay(fields)
  const weekday = WEEKDAYS[first.weekday][0].toUpperCase() + WEEKDAYS[first.weekday].slice(1)
  const nth = `the ${indexOf(first.day)} ${weekday}`
  return [
    ['none', "Doesn't repeat"],
    ['daily', 'Daily'],
    ['weekly', 'Weekly'],
    ['absolute_monthly', `Monthly on day ${first.day}`],
    ['relative_monthly', `Monthly on ${nth}`],
    ['absolute_yearly', `Yearly on ${first.day} ${MONTHS[first.month - 1]}`],
    ['relative_yearly', `Yearly on ${nth} of ${MONTHS[first.month - 1]}`],
  ]
}

const UNITS: Record<RecurrencePattern['type'], string> = {
  daily: 'days',
  weekly: 'weeks',
  absolute_monthly: 'months',
  relative_monthly: 'months',
  absolute_yearly: 'years',
  relative_yearly: 'years',
}

const SHOW_AS: [ShowAs, string][] = [
  ['busy', 'Busy'],
  ['tentative', 'Tentative'],
  ['free', 'Free'],
  ['oof', 'Out of office'],
  ['workingElsewhere', 'Working elsewhere'],
]

/** Reminders, in minutes before, as Outlook offers them. */
const REMINDERS: [number | null, string][] = [
  [null, 'None'],
  [0, 'At the start'],
  [5, '5 minutes before'],
  [15, '15 minutes before'],
  [30, '30 minutes before'],
  [60, '1 hour before'],
  [120, '2 hours before'],
  [24 * 60, '1 day before'],
  [7 * 24 * 60, '1 week before'],
]

/** The fields edited: those that differ from where the form started
 * (the body, once typed in, as the editor writes its HTML anew). */
function editedFields(initial: EventFields, fields: EventFields, bodyEdited: boolean): EventField[] {
  const edited: [EventField, boolean][] = [
    ['subject', fields.subject !== initial.subject],
    ['location', fields.location !== initial.location],
    ['body', bodyEdited],
    ['time', fields.start !== initial.start || fields.end !== initial.end || fields.all_day !== initial.all_day],
    ['attendees', fields.required !== initial.required || fields.optional !== initial.optional],
    ['online_meeting', fields.online_meeting !== initial.online_meeting],
    ['reminder', fields.reminder !== initial.reminder],
    ['show_as', fields.show_as !== initial.show_as],
    ['recurrence', JSON.stringify(fields.recurrence) !== JSON.stringify(initial.recurrence)],
  ]
  return edited.filter(([, is]) => is).map(([field]) => field)
}

/** Of the times, why they can't be saved, if they can't. */
function invalidTimes(fields: EventFields): string | null {
  if (fields.end < fields.start || (fields.all_day && fields.end === fields.start)) return 'It ends before it starts.'
  return null
}

function EventForm(props: { back: Back; initial: Initial | null; series: boolean }) {
  const { ops, mail } = useServices()
  const { route } = useLocation()
  const { back, initial } = props
  const accounts = (mail.accounts.value ?? []).filter((a) => a.provider === 'm365')
  const [account, setAccount] = useState(initial?.account ?? accounts[0]?.id)
  const [fields, setFields] = useState(() => initial?.fields ?? newFields(back.date))
  const [bodyEdited, setBodyEdited] = useState(false)
  // What the editor starts from: it has the body after.
  const [startBody] = useState(fields.body)
  const [saving, setSaving] = useState(false)
  const form = useRef<HTMLFormElement>(null)
  const subject = useRef<HTMLInputElement>(null)
  const body = useRef<HTMLDivElement>(null)
  const set = (changed: Partial<EventFields>) => setFields((f) => ({ ...f, ...changed }))
  useTitle(fields.subject || '(no subject)', initial ? 'Edit event' : 'New event')

  useLayoutEffect(() => {
    const instance = new Editor({
      element: body.current!,
      extensions: [StarterKit],
      content: startBody,
      onUpdate: ({ editor }) => {
        setBodyEdited(true)
        set({ body: editor.getHTML() })
      },
    })
    subject.current!.focus()
    return () => instance.destroy()
  }, [startBody])

  const close = () => {
    if (history.length > 1) history.back()
    else route(calendarPath(back.view, back.date, back.event ?? undefined))
  }
  const timeZone = Intl.DateTimeFormat().resolvedOptions().timeZone
  const invalid = invalidTimes(fields)
  const save = async () => {
    if (invalid !== null || saving || account === undefined) return
    const edited = initial ? editedFields(initial.fields, fields, bodyEdited) : null
    if (edited?.length === 0) return close()
    setSaving(true)
    const done = initial
      ? await ops.dispatch({ type: 'update_event', event: initial.event, series: props.series, fields, edited: edited!, time_zone: timeZone })
      : await ops.dispatch({ type: 'create_event', account, fields, time_zone: timeZone })
    setSaving(false)
    // Refused: the form stays, as the notice says why.
    if (!done) return
    const date = fields.all_day ? utcDate(fields.start) : dateString(new Date(fields.start))
    route(calendarPath(back.view, date, initial?.event), true)
  }
  useKeymap({ send: save, close }, { within: form })

  /** Moves the start, the end with it, as Outlook does; of a series, its
   * pattern after it. */
  const moveStart = (start: number) => {
    setFields((f) => {
      const moved = { ...f, start, end: start + (f.end - f.start) }
      const recurrence = f.recurrence && {
        ...f.recurrence,
        pattern: pattern(f.recurrence.pattern.type, moved, f.recurrence.pattern.type === 'weekly' ? f.recurrence.pattern.days : null),
      }
      return { ...moved, recurrence }
    })
  }
  const setAllDay = (allDay: boolean) => {
    setFields((f) => {
      if (allDay) {
        // The days it is on, here, as UTC dates.
        const first = new Date(f.start)
        const last = new Date(Math.max(f.start, f.end - 1))
        const start = Date.UTC(first.getFullYear(), first.getMonth(), first.getDate())
        const end = Date.UTC(last.getFullYear(), last.getMonth(), last.getDate()) + DAY
        return { ...f, all_day: true, start, end }
      }
      const d = new Date(f.start)
      const start = new Date(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate(), 9).getTime()
      return { ...f, all_day: false, start, end: start + 30 * MINUTE }
    })
  }
  const setRepeat = (repeat: Repeat) => {
    set({
      recurrence:
        repeat === 'none'
          ? null
          : {
              pattern: pattern(repeat, fields, null),
              interval: fields.recurrence?.interval ?? 1,
              until: fields.recurrence?.until ?? { type: 'never' },
            },
    })
  }
  const setRecurrence = (changed: Partial<Recurrence>) => set({ recurrence: { ...fields.recurrence!, ...changed } })
  // A series recurs, and only a series: an occurrence's or a single
  // event's edit does not offer it.
  const recurs = initial === null || props.series
  const { recurrence } = fields

  const text = (name: 'subject' | 'location' | 'required' | 'optional', label: string, ref?: typeof subject) => (
    <label class={styles.field}>
      <span>{label}</span>
      <input ref={ref} type="text" value={fields[name]} onInput={(e) => set({ [name]: e.currentTarget.value })} />
    </label>
  )

  return (
    <form
      ref={form}
      class={styles.editor}
      aria-label={initial ? 'Edit event' : 'New event'}
      onSubmit={(event) => {
        event.preventDefault()
        void save()
      }}
    >
      <h1>{initial ? (props.series ? 'Edit the series' : 'Edit event') : 'New event'}</h1>
      {initial === null && accounts.length > 1 && (
        <label class={styles.field}>
          <span>Calendar</span>
          <select value={account} onChange={(e) => setAccount(e.currentTarget.value)}>
            {accounts.map((a) => (
              <option key={a.id} value={a.id}>
                {a.address}
              </option>
            ))}
          </select>
        </label>
      )}
      {text('subject', 'Subject', subject)}
      {text('required', 'Required')}
      {text('optional', 'Optional')}
      <div class={styles.times}>
        {fields.all_day ? (
          <>
            <label>
              <span>From</span>
              <input
                type="date"
                required
                value={utcDate(fields.start)}
                onInput={(e) => e.currentTarget.value && moveStart(Date.parse(e.currentTarget.value))}
              />
            </label>
            <label>
              <span>To</span>
              <input
                type="date"
                required
                value={utcDate(fields.end - DAY)}
                onInput={(e) => e.currentTarget.value && set({ end: Date.parse(e.currentTarget.value) + DAY })}
              />
            </label>
          </>
        ) : (
          <>
            <label>
              <span>From</span>
              <input
                type="datetime-local"
                required
                value={localInput(fields.start)}
                onInput={(e) => e.currentTarget.value && moveStart(new Date(e.currentTarget.value).getTime())}
              />
            </label>
            <label>
              <span>To</span>
              <input
                type="datetime-local"
                required
                value={localInput(fields.end)}
                onInput={(e) => e.currentTarget.value && set({ end: new Date(e.currentTarget.value).getTime() })}
              />
            </label>
          </>
        )}
        <label class={styles.check}>
          <input type="checkbox" checked={fields.all_day} onChange={(e) => setAllDay(e.currentTarget.checked)} />
          All day
        </label>
      </div>
      {invalid && (
        <p class={styles.error} role="alert">
          {invalid}
        </p>
      )}
      {recurs && (
        <fieldset class={styles.recurrence}>
          <legend>Repeat</legend>
          <select
            aria-label="Repeat"
            value={recurrence?.pattern.type ?? 'none'}
            onChange={(e) => setRepeat(e.currentTarget.value as Repeat)}
          >
            {repeats(fields)
              // A series can't be made a single event.
              .filter(([repeat]) => repeat !== 'none' || !props.series)
              .map(([repeat, label]) => (
                <option key={repeat} value={repeat}>
                  {label}
                </option>
              ))}
          </select>
          {recurrence && (
            <>
              <label>
                every{' '}
                <input
                  type="number"
                  min={1}
                  class={styles.number}
                  aria-label="Interval"
                  value={recurrence.interval}
                  onInput={(e) => setRecurrence({ interval: Math.max(1, Number(e.currentTarget.value)) })}
                />{' '}
                {UNITS[recurrence.pattern.type]}
              </label>
              {recurrence.pattern.type === 'weekly' && (
                <WeekdayChoice
                  days={recurrence.pattern.days}
                  set={(days) => setRecurrence({ pattern: { type: 'weekly', days } })}
                />
              )}
              <RecurrenceEndChoice
                until={recurrence.until}
                start={fields.all_day ? utcDate(fields.start) : dateString(new Date(fields.start))}
                set={(until) => setRecurrence({ until })}
              />
            </>
          )}
        </fieldset>
      )}
      {text('location', 'Location')}
      <div class={styles.options}>
        <label class={styles.check}>
          <input
            type="checkbox"
            checked={fields.online_meeting}
            onChange={(e) => set({ online_meeting: e.currentTarget.checked })}
          />
          Teams meeting
        </label>
        <label>
          Reminder{' '}
          <select
            value={String(fields.reminder)}
            onChange={(e) => set({ reminder: e.currentTarget.value === 'null' ? null : Number(e.currentTarget.value) })}
          >
            {REMINDERS.map(([minutes, label]) => (
              <option key={String(minutes)} value={String(minutes)}>
                {label}
              </option>
            ))}
            {!REMINDERS.some(([m]) => m === fields.reminder) && (
              <option value={String(fields.reminder)}>{fields.reminder} minutes before</option>
            )}
          </select>
        </label>
        <label>
          Show as{' '}
          <select value={fields.show_as} onChange={(e) => set({ show_as: e.currentTarget.value as ShowAs })}>
            {SHOW_AS.map(([value, label]) => (
              <option key={value} value={value}>
                {label}
              </option>
            ))}
          </select>
        </label>
      </div>
      <div class={styles.body} ref={body} aria-label="Description" />
      <div class={styles.actions}>
        <button type="submit" disabled={invalid !== null || saving || account === undefined}>
          {initial || fields.required + fields.optional === '' ? 'Save' : 'Send'}
        </button>
        <button type="button" onMouseDown={keepFocus} onClick={close}>
          Cancel
        </button>
      </div>
    </form>
  )
}

function WeekdayChoice(props: { days: Weekday[]; set: (days: Weekday[]) => void }) {
  return (
    <span class={styles.weekdays} role="group" aria-label="On">
      {WEEKDAYS.map((day) => (
        <label key={day} title={day}>
          <input
            type="checkbox"
            checked={props.days.includes(day)}
            onChange={(e) =>
              props.set(e.currentTarget.checked ? WEEKDAYS.filter((d) => d === day || props.days.includes(d)) : props.days.filter((d) => d !== day))
            }
          />
          {day.slice(0, 2)}
        </label>
      ))}
    </span>
  )
}

function RecurrenceEndChoice(props: { until: RecurrenceEnd; start: string; set: (until: RecurrenceEnd) => void }) {
  const { until } = props
  return (
    <label>
      until{' '}
      <select
        aria-label="Until"
        value={until.type}
        onChange={(e) => {
          const type = e.currentTarget.value as RecurrenceEnd['type']
          props.set(type === 'never' ? { type } : type === 'on' ? { type, date: props.start } : { type, count: 10 })
        }}
      >
        <option value="never">further notice</option>
        <option value="on">a date</option>
        <option value="after">a number of times</option>
      </select>
      {until.type === 'on' && (
        <input
          type="date"
          aria-label="Last date"
          required
          min={props.start}
          value={until.date}
          onInput={(e) => e.currentTarget.value && props.set({ type: 'on', date: e.currentTarget.value })}
        />
      )}
      {until.type === 'after' && (
        <input
          type="number"
          min={1}
          class={styles.number}
          aria-label="Occurrences"
          value={until.count}
          onInput={(e) => props.set({ type: 'after', count: Math.max(1, Number(e.currentTarget.value)) })}
        />
      )}
    </label>
  )
}
