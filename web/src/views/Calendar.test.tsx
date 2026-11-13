import { signal } from '@preact/signals'
import { render, screen, waitFor, within } from '@testing-library/preact'
import userEvent from '@testing-library/user-event'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { Client } from '../stores/client'
import { FakeServer } from '../test/server'
import { App } from './App'
import { today } from './Calendar'

let server: FakeServer
let client: Client

// The tests run in New York (vite.config.ts), where November is UTC−5.
beforeEach(() => {
  server = new FakeServer()
  server.addM365Account('work', [])
  server.addEvent('work', 'Standup', '2026-11-10T15:00:00Z', '2026-11-10T15:15:00Z', { recurring: true })
  server.addEvent('work', 'Review', '2026-11-10T15:00:00Z', '2026-11-10T16:00:00Z', { location: 'Room 4' })
  server.addEvent('work', 'Lunch', '2026-11-10T17:00:00Z', '2026-11-10T18:00:00Z')
  server.addEvent('work', 'Offsite', '2026-11-12T00:00:00Z', '2026-11-13T00:00:00Z', { all_day: true })
  server.addEvent('work', 'Late call', '2026-11-16T03:00:00Z', '2026-11-16T04:00:00Z')
  client = new Client(server)
  server.attach(client)
})

afterEach(() => {
  vi.useRealTimers()
})

async function start(path: string) {
  history.replaceState(null, '', path)
  render(<App client={client} status={signal('open')} />)
  client.reset()
  await screen.findByRole('region', { name: 'Calendar' })
  return userEvent.setup()
}

const day = (name: string) => screen.getByRole('region', { name })
const timed = (name: string) => within(within(day(name)).getByRole('list', { name: 'Timed' })).queryAllByRole('listitem')
const allDay = (name: string) =>
  within(within(day(name)).getByRole('list', { name: 'All day' }))
    .queryAllByRole('listitem')
    .map((e) => e.textContent)

describe('calendar', () => {
  it('shows the week from Monday, at local times', async () => {
    await start('/calendar/week/2026-11-11')
    expect(screen.getByRole('heading', { level: 1 }).textContent).toBe('Week of 2026-11-09')
    await waitFor(() => expect(timed('Tuesday 2026-11-10')).toHaveLength(3))
    const [review, standup, lunch] = timed('Tuesday 2026-11-10')
    expect(standup.textContent).toBe('10:00 Standup ↻')
    expect(review.textContent).toBe('10:00 Review · Room 4')
    expect(lunch.textContent).toBe('12:00 Lunch')
    // Standup and Review overlap, so are side by side; Lunch has the day
    // to itself.
    expect([standup.style.left, standup.style.width]).toEqual(['50%', '50%'])
    expect([review.style.left, review.style.width]).toEqual(['0%', '50%'])
    expect(lunch.style.width).toBe('100%')
    expect(lunch.style.top).toBe(`${12 * 48}px`)
    // An all-day event is on its date, though that starts earlier in UTC.
    expect(allDay('Thursday 2026-11-12')).toEqual(['Offsite'])
    expect(allDay('Wednesday 2026-11-11')).toEqual([])
    expect(allDay('Friday 2026-11-13')).toEqual([])
    // 03:00 UTC on Monday is Sunday evening here.
    expect(timed('Sunday 2026-11-15').map((e) => e.textContent)).toEqual(['22:00 Late call'])
  })

  it('moves by week with j and k, to today with t, and between views with a and w', async () => {
    const user = await start('/calendar/week/2026-11-11')
    await user.keyboard('j')
    expect(location.pathname).toBe('/calendar/week/2026-11-16')
    await screen.findByRole('heading', { name: 'Week of 2026-11-16' })
    expect(timed('Monday 2026-11-16')).toEqual([])
    await user.keyboard('kk')
    expect(location.pathname).toBe('/calendar/week/2026-11-02')
    await user.keyboard('a')
    expect(location.pathname).toBe('/calendar/agenda/2026-11-02')
    await screen.findByRole('heading', { name: 'From 2026-11-02' })
    await waitFor(() => expect(screen.getByRole('region', { name: 'Tuesday 2026-11-10' })).toBeTruthy())
    await user.keyboard('j')
    expect(location.pathname).toBe('/calendar/agenda/2026-11-16')
    await user.keyboard('w')
    expect(location.pathname).toBe('/calendar/week/2026-11-16')
    await user.keyboard('t')
    expect(location.pathname).toBe(`/calendar/week/${today()}`)
  })

  it('lists the days with events in the agenda', async () => {
    await start('/calendar/agenda/2026-11-10')
    const tuesday = await screen.findByRole('region', { name: 'Tuesday 2026-11-10' })
    expect(within(tuesday).getAllByRole('listitem').map((e) => e.textContent)).toEqual([
      '10:00–10:15Standup ↻',
      '10:00–11:00Review · Room 4',
      '12:00–13:00Lunch',
    ])
    expect(
      within(day('Thursday 2026-11-12'))
        .getAllByRole('listitem')
        .map((e) => e.textContent),
    ).toEqual(['All dayOffsite'])
    expect(screen.queryByRole('region', { name: 'Wednesday 2026-11-11' })).toBeNull()
  })

  it('shows what changed when a sync says the calendar did', async () => {
    await start('/calendar/week/2026-11-11')
    await waitFor(() => expect(timed('Wednesday 2026-11-11')).toEqual([]))
    server.addEvent('work', 'Planning', '2026-11-11T19:00:00Z', '2026-11-11T20:00:00Z', { exception: true })
    server.calendarSynced('work')
    await waitFor(() => expect(timed('Wednesday 2026-11-11').map((e) => e.textContent)).toEqual(['14:00 Planning ↻*']))
  })

  it('is a link in the nav, and g c away', async () => {
    const user = await start('/calendar')
    expect(location.pathname).toBe('/calendar')
    await screen.findByRole('heading', { name: /^Week of / })
    await user.click(screen.getByRole('link', { name: 'To-dos' }))
    await screen.findByRole('region', { name: 'To-dos' })
    await user.keyboard('gc')
    expect(location.pathname).toBe(`/calendar/week/${today()}`)
    expect(screen.getByRole('link', { name: 'Calendar' }).getAttribute('aria-current')).toBe('page')
  })

  it('heads the days alike, ticks the hours, and marks the time today', async () => {
    // Tuesday 11:30 here.
    vi.useFakeTimers({ toFake: ['Date'] })
    vi.setSystemTime(new Date('2026-11-10T16:30:00Z'))
    await start('/calendar/week/2026-11-11')
    const week = await screen.findByRole('region', { name: 'Calendar' })
    const heads = await within(week).findAllByRole('heading', { level: 2 })
    expect(heads.map((h) => h.textContent)).toEqual(['Mon 9', 'Tue 10', 'Wed 11', 'Thu 12', 'Fri 13', 'Sat 14', 'Sun 15'])
    expect(heads[1].getAttribute('aria-current')).toBe('date')
    expect(heads[1].title).toBe('Tuesday 2026-11-10')
    expect(screen.getByText('08:00')).toBeTruthy()
    const now = within(day('Tuesday 2026-11-10')).getByTitle('Now, 11:30')
    expect(now.style.top).toBe(`${11.5 * 48}px`)
    expect(within(day('Monday 2026-11-09')).queryByTitle(/^Now/)).toBeNull()
  })

  it('shows what an event is, opened, and closes it with Esc', async () => {
    const planning = server.addEvent(
      'work',
      'Planning',
      '2026-11-11T19:00:00Z',
      '2026-11-11T20:00:00Z',
      { response: 'notResponded', show_as: 'tentative', location: 'Room 4' },
      {
        organizer: { name: 'Bob', address: 'bob@example.com' },
        attendees: [
          { person: { name: 'Alice', address: 'alice@example.com' }, kind: 'required', response: 'none' },
          { person: { name: 'Room 4', address: 'room4@example.com' }, kind: 'resource', response: 'accepted' },
        ],
        body_text: 'Notes: https://example.org/notes',
        join_url: 'https://meet.example.org/123',
        web_link: 'https://outlook.example.org/item',
      },
    )
    const user = await start('/calendar/week/2026-11-11')
    await waitFor(() => expect(timed('Wednesday 2026-11-11')).toHaveLength(1))
    await user.click(screen.getByRole('link', { name: /Planning/ }))
    expect(location.pathname).toBe(`/calendar/week/2026-11-11/${planning.id}`)
    const pane = await screen.findByRole('complementary', { name: 'Event' })
    await within(pane).findByRole('heading', { name: 'Planning' })
    expect(within(pane).getByText('Wednesday 2026-11-11, 14:00–15:00')).toBeTruthy()
    expect(within(pane).getByRole('link', { name: 'Join online meeting' }).getAttribute('href')).toBe(
      'https://meet.example.org/123',
    )
    expect(within(pane).getByRole('link', { name: 'https://example.org/notes' })).toBeTruthy()
    expect(within(pane).getByRole('link', { name: 'Open in Outlook' })).toBeTruthy()
    const facts = [...pane.querySelectorAll('dt, dd')].map((e) => e.textContent)
    expect(facts).toEqual(['Where', 'Room 4', 'Organizer', 'Bob', 'You', 'Not responded', 'Shown as', 'Tentative'])
    expect(
      within(within(pane).getByRole('region', { name: 'Attendees' }))
        .getAllByRole('listitem')
        .map((e) => e.textContent),
    ).toEqual(['Alice', 'Room 4 (resource) · accepted'])
    expect(within(pane).getByText(/"subject": "Planning"/)).toBeTruthy()
    // As the week does, it shows as it changes.
    server.calendar[server.calendar.length - 1].subject = 'Planning, moved'
    server.calendarSynced('work')
    await within(pane).findByRole('heading', { name: 'Planning, moved' })
    await user.keyboard('{Escape}')
    expect(location.pathname).toBe('/calendar/week/2026-11-11')
    expect(screen.queryByRole('complementary', { name: 'Event' })).toBeNull()
  })

  it('shows an event cancelled or declined as such, and one gone as gone', async () => {
    server.addEvent('work', 'Canceled: Retro', '2026-11-11T19:00:00Z', '2026-11-11T20:00:00Z', {
      cancelled: true,
      response: 'accepted',
    })
    server.addEvent('work', 'Offsite prep', '2026-11-11T21:00:00Z', '2026-11-11T22:00:00Z', { response: 'declined' })
    await start('/calendar/week/2026-11-11')
    const [retro, prep] = await waitFor(() => {
      const events = within(day('Wednesday 2026-11-11')).getAllByRole('link')
      expect(events).toHaveLength(2)
      return events
    })
    expect(retro.title).toBe('Canceled: Retro\nCancelled · Accepted · shown as Busy')
    expect(prep.title).toBe('Offsite prep\nDeclined · shown as Busy')

    history.replaceState(null, '', '/calendar/agenda/2026-11-11/99')
    dispatchEvent(new PopStateEvent('popstate'))
    await screen.findByText('This event is no longer on the calendar.')
  })

  it('opens an event found from home or search in the agenda', async () => {
    await start('/calendar/agenda/2026-11-10/2')
    const pane = await screen.findByRole('complementary', { name: 'Event' })
    await within(pane).findByRole('heading', { name: 'Review' })
    expect(within(pane).getByText('Tuesday 2026-11-10, 10:00–11:00')).toBeTruthy()
    const review = within(day('Tuesday 2026-11-10')).getByRole('link', { name: /Review/ })
    expect(review.getAttribute('href')).toBe('/calendar/agenda/2026-11-10/2')
  })

  it('ticks the hours in the time zones configured too, left of the browser\'s', async () => {
    await start('/calendar/week/2026-11-11')
    const week = await screen.findByRole('region', { name: 'Calendar' })
    await waitFor(() => expect(within(week).getByText('08:00')).toBeTruthy())
    server.configureTimeZones([
      { label: 'Office', zone: 'Europe/Berlin' },
      { label: 'India', zone: 'Asia/Kolkata' },
    ])
    // Monday 08:00 here (UTC−5) is 14:00 in Berlin and 18:30 in India.
    await waitFor(() => expect(within(week).getByText('Office')).toBeTruthy())
    const columns = [...week.querySelectorAll('[aria-hidden] ol')].map((ol) => [...ol.children].map((li) => li.textContent))
    expect(columns.map((ticks) => ticks[7])).toEqual(['14', '18:30', '08'])
    expect([...week.querySelectorAll('[class*=corner] span')].map((e) => e.textContent)).toEqual(['Office', 'India', 'GMT-5'])
  })
})
