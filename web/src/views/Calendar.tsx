import { useEffect } from 'preact/hooks'
import { useLocation } from 'preact-iso'
import type { CalendarEvent } from '../generated/protocol'
import { useServices } from '../context'
import { useKeymap } from '../keymap'
import styles from './Calendar.module.css'

const DAY = 86_400_000
/** Days the agenda lists. */
const AGENDA_DAYS = 14
const HOUR_PX = 40

export type CalendarView = 'week' | 'agenda'

export function calendarPath(view: CalendarView, date: string): string {
  return `/calendar/${view}/${date}`
}

/** Local midnight of `YYYY-MM-DD`. */
function midnight(date: string): Date {
  const [y, m, d] = date.split('-').map(Number)
  return new Date(y, m - 1, d)
}

function dateString(day: Date): string {
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${day.getFullYear()}-${pad(day.getMonth() + 1)}-${pad(day.getDate())}`
}

function addDays(day: Date, days: number): Date {
  return new Date(day.getFullYear(), day.getMonth(), day.getDate() + days)
}

export function today(): string {
  return dateString(new Date())
}

/** The Monday of the week of `day`. */
function weekStart(day: Date): Date {
  return addDays(day, -((day.getDay() + 6) % 7))
}

function time(ms: number): string {
  const t = new Date(ms)
  return `${String(t.getHours()).padStart(2, '0')}:${String(t.getMinutes()).padStart(2, '0')}`
}

const dayName = (day: Date) =>
  day.toLocaleDateString('en-GB', { weekday: 'long', day: 'numeric', month: 'long' })

/** Whether the event is on the (local) day: an all-day event by its UTC
 * dates, which are the days it is on wherever one is; a timed one if it
 * overlaps the day. */
function isOn(event: CalendarEvent, day: Date): boolean {
  if (event.all_day) {
    const utcDay = Date.UTC(day.getFullYear(), day.getMonth(), day.getDate())
    return event.start <= utcDay && utcDay < event.end
  }
  const start = day.getTime()
  const end = addDays(day, 1).getTime()
  return event.start < end && (event.end > start || (event.end === event.start && event.start >= start))
}

/** The week or agenda from `date`, of every account's calendar. */
export function Calendar(props: { view?: string; date?: string }) {
  const { calendar } = useServices()
  const { route } = useLocation()
  const view: CalendarView = props.view === 'agenda' ? 'agenda' : 'week'
  const date = props.date ?? today()
  const first = view === 'week' ? weekStart(midnight(date)) : midnight(date)
  const days = Array.from({ length: view === 'week' ? 7 : AGENDA_DAYS }, (_, i) => addDays(first, i))
  const from = first.getTime()
  const to = addDays(first, days.length).getTime()
  // All-day events are on UTC days, which may start before the local ones.
  useEffect(() => calendar.show(from - DAY, to + DAY), [calendar, from, to])

  const go = (target: Date, inView = view) => route(calendarPath(inView, dateString(target)))
  useKeymap({
    next: () => go(addDays(first, view === 'week' ? 7 : AGENDA_DAYS)),
    previous: () => go(addDays(first, view === 'week' ? -7 : -AGENDA_DAYS)),
    today: () => go(new Date()),
    weekView: () => go(midnight(date), 'week'),
    agendaView: () => go(midnight(date), 'agenda'),
  })

  const events = calendar.events.value
  const title =
    view === 'week'
      ? `Week of ${first.toLocaleDateString('en-GB', { day: 'numeric', month: 'long', year: 'numeric' })}`
      : `From ${first.toLocaleDateString('en-GB', { day: 'numeric', month: 'long', year: 'numeric' })}`
  return (
    <section class={styles.calendar} aria-label="Calendar">
      <header class={styles.header}>
        <h1>{title}</h1>
        <nav class={styles.controls}>
          <a href={calendarPath(view, dateString(addDays(first, view === 'week' ? -7 : -AGENDA_DAYS)))}>‹</a>
          <a href={calendarPath(view, today())}>Today</a>
          <a href={calendarPath(view, dateString(addDays(first, view === 'week' ? 7 : AGENDA_DAYS)))}>›</a>
          <a href={calendarPath('week', date)} aria-current={view === 'week' ? 'page' : undefined}>
            Week
          </a>
          <a href={calendarPath('agenda', date)} aria-current={view === 'agenda' ? 'page' : undefined}>
            Agenda
          </a>
        </nav>
      </header>
      {events === null ? (
        <p class={styles.placeholder}>Loading…</p>
      ) : view === 'week' ? (
        <Week days={days} events={events} />
      ) : (
        <Agenda days={days} events={events} />
      )}
    </section>
  )
}

function Marks(props: { event: CalendarEvent }) {
  const { event } = props
  if (event.exception) return <span title="An occurrence changed from its series"> ↻*</span>
  if (event.recurring) return <span title="Recurring"> ↻</span>
  return null
}

function Week(props: { days: Date[]; events: readonly CalendarEvent[] }) {
  return (
    <div class={styles.week}>
      {props.days.map((day) => {
        const on = props.events.filter((e) => isOn(e, day))
        return (
          <section key={day.getTime()} class={styles.day} aria-label={dayName(day)}>
            <h2 class={dateString(day) === today() ? styles.today : undefined}>{dayName(day)}</h2>
            <ul class={styles.allDay} aria-label="All day">
              {on
                .filter((e) => e.all_day)
                .map((e) => (
                  <li key={e.id} class={styles.event} title={[e.location, e.preview].filter(Boolean).join('\n')}>
                    {e.subject}
                    <Marks event={e} />
                  </li>
                ))}
            </ul>
            <ul class={styles.hours} aria-label="Timed" style={{ height: `${24 * HOUR_PX}px` }}>
              {laidOut(on.filter((e) => !e.all_day), day).map(({ event: e, top, height, lane, lanes }) => (
                <li
                  key={e.id}
                  class={styles.event}
                  style={{
                    top: `${top}px`,
                    height: `${height}px`,
                    left: `${(lane / lanes) * 100}%`,
                    width: `${100 / lanes}%`,
                  }}
                  title={[e.location, e.preview].filter(Boolean).join('\n')}
                >
                  <time>{time(e.start)}</time> {e.subject}
                  <Marks event={e} />
                  {e.location && <span class={styles.location}> · {e.location}</span>}
                </li>
              ))}
            </ul>
          </section>
        )
      })}
    </div>
  )
}

/** Where the day's timed events go: by time, side by side where they
 * overlap. */
function laidOut(events: CalendarEvent[], day: Date) {
  const start = day.getTime()
  const end = addDays(day, 1).getTime()
  const px = (ms: number) => ((ms - start) / 3_600_000) * HOUR_PX
  const placed: { event: CalendarEvent; top: number; height: number; lane: number; lanes: number }[] = []
  // Clusters of events that overlap one another, each laid out in lanes.
  let cluster: typeof placed = []
  let clusterEnd = -Infinity
  let laneEnds: number[] = []
  const close = () => {
    for (const p of cluster) p.lanes = laneEnds.length
    placed.push(...cluster)
    cluster = []
    laneEnds = []
  }
  for (const event of [...events].sort((a, b) => a.start - b.start || b.end - a.end)) {
    const from = Math.max(event.start, start)
    const to = Math.min(Math.max(event.end, event.start + 15 * 60_000), end)
    if (from >= clusterEnd) close()
    let lane = laneEnds.findIndex((laneEnd) => laneEnd <= from)
    if (lane === -1) lane = laneEnds.push(to) - 1
    else laneEnds[lane] = to
    clusterEnd = Math.max(clusterEnd, to)
    cluster.push({ event, top: px(from), height: Math.max(px(to) - px(from), 16), lane, lanes: 1 })
  }
  close()
  return placed
}

function Agenda(props: { days: Date[]; events: readonly CalendarEvent[] }) {
  const days = props.days
    .map((day) => ({ day, events: props.events.filter((e) => isOn(e, day)) }))
    .filter((d) => d.events.length > 0)
  if (days.length === 0) return <p class={styles.placeholder}>Nothing on.</p>
  return (
    <div class={styles.agenda}>
      {days.map(({ day, events }) => (
        <section key={day.getTime()} aria-label={dayName(day)}>
          <h2 class={dateString(day) === today() ? styles.today : undefined}>{dayName(day)}</h2>
          <ul>
            {events.map((e) => (
              <li key={e.id} class={styles.agendaEvent}>
                <span class={styles.when}>{e.all_day ? 'All day' : `${time(e.start)}–${time(e.end)}`}</span>
                <span>
                  {e.subject}
                  <Marks event={e} />
                  {e.location && <span class={styles.location}> · {e.location}</span>}
                </span>
              </li>
            ))}
          </ul>
        </section>
      ))}
    </div>
  )
}
